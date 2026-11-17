//! 合成器时钟原语：显示器刷新率 + 下一次可呈现时刻 + 等待合成器节拍（R07-10 / 架构决策 B3B6 §6）。
//!
//! 旧实现由 `SetTimer(hwnd, id, 1ms)` + `WM_TIMER` 驱动连续帧（R07-10 的缺陷本体）：
//! `WM_TIMER` 是**低优先级**消息，
//! 只保证「不早于」请求的间隔，既不与 DWM 的合成节拍对齐，也不会跟着显示器刷新率走 ——
//! 60 Hz 下每次拍频落在任意相位上（帧间隔抖动），120 Hz 显示器上最多也只能拿到一半的拍子。
//! 合成器时钟（compositor clock）是 DWM 每个合成周期递增一次的节拍，Chrome 的 Viz
//! （`DCLayerTree::WaitForCompositorClock`）与 Qt 6 的 Windows 后端都以它作为帧源：
//! [`FrameClock::wait_next_frame`] 阻塞到下一拍，[`FrameClock::sample`] 给出这一拍的相位与刷新率。
//!
//! **为什么用 `DwmGetCompositionTimingInfo` 而不是 `DwmFlush`**：`DwmFlush` 阻塞到「DWM 完成一次
//! 合成」，它把 DWM 的合成线程和调用方绑在一起（DWM 慢，我们也慢），而且只回答「这次合成做完了」，
//! **不提供下一次 vblank 的相位**。`DwmGetCompositionTimingInfo` 是纯读的 O(1) 查询，给出最近一次
//! vblank 的 QPC 时刻（`qpcVBlank`）与一个刷新周期的 QPC 长度（`qpcRefreshPeriod`），于是
//! `相位 = (now - qpcVBlank) % qpcRefreshPeriod`：既能算出「还有多久可以呈现」，也能预测该帧的
//! 呈现时刻给动画推进 `dt`。
//!
//! **为什么 `DCompositionWaitForCompositorClock` 传 `None`**：它的句柄数组是给「同时等合成器节拍
//! 与自己的事件句柄」的调用方准备的（`count` 个句柄都由合成器时钟监听）。本模块只需要节拍本身，
//! 不参与等待其它句柄，所以 `count == 0`：此时「节拍到了」就是 `WAIT_OBJECT_0`（官方复合时钟文档
//! 的示例：`WAIT_OBJECT_0 + count` = 合成器节拍）。
//!
//! 本文件只提供 **Windows 原语**：没有 trait 抽象，也**不接管**事件循环（泵的接线在别处）。探测
//! 失败一律退回「按缓存刷新率推算的整帧间隔」而**不返回错误**：帧节奏不能因为一次时序探测失败
//! 就停摆。
//!
//! 系统要求：`DCompositionWaitForCompositorClock` 的文档最低版本是 **Windows 11（build 22000）**。
//! 本模块静态导入该导出，因此在更早的系统上进程会在**加载期**失败；需要兼容 Windows 10 时必须
//! 换成 `GetProcAddress` 动态解析 + 定时器兜底（Zed/GPUI 就是这么做的）。

use std::{fmt, mem::size_of, time::Duration};

use windows::{
    Win32::{
        Foundation::{HWND, RECT, WAIT_OBJECT_0},
        Graphics::{
            DirectComposition::DCompositionWaitForCompositorClock,
            Dwm::{DWM_TIMING_INFO, DwmGetCompositionTimingInfo},
            Dxgi::{
                Common::DXGI_FORMAT_B8G8R8A8_UNORM, CreateDXGIFactory1, DXGI_MODE_DESC1,
                IDXGIFactory1, IDXGIOutput1,
            },
        },
        System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
        UI::WindowsAndMessaging::GetWindowRect,
    },
    core::{IUnknown, Interface},
};

/// 刷新率下限：低于它的「刷新率」只可能是采样错误（省电面板也不低于 24 Hz）。
const MIN_REFRESH_HZ: f32 = 24.0;

/// 刷新率上限：360 Hz 覆盖目前所有消费级面板，超出只可能是采样错误。
const MAX_REFRESH_HZ: f32 = 360.0;

/// 两级探测都失败时的兜底刷新率（60 Hz：绝大多数显示器的缺省档）。
const FALLBACK_REFRESH_HZ: f32 = 60.0;

/// `wait_next_frame` 的超时上限（毫秒）：即使调用方给了更长的超时，也不许无限期卡在等待里。
const MAX_WAIT_MS: u32 = 1_000;

/// 一次采样得到的合成器时钟状态。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ClockSample {
    /// 本次采样认定的显示器刷新率（已 clamp 到 [`MIN_REFRESH_HZ`]..=[`MAX_REFRESH_HZ`]）。
    pub refresh_hz: f32,
    /// 距离下一次可呈现时刻的时长（已 clamp 到 `[0, frame_interval]`）。
    pub until_next_present: Duration,
}

/// 刷新率的来源（`Debug`/诊断用；帧节奏本身不依赖它）。
#[derive(Debug)]
pub(crate) enum RefreshSource {
    /// `DwmGetCompositionTimingInfo` 的 QPC 周期（备选：`rateRefresh` 千分比）。
    Dwm,
    /// DXGI 输出当前桌面模式的 `RefreshRate`。
    Dxgi,
    /// 两级探测都失败：60 Hz 兜底。
    Fallback,
}

/// 一个窗口的合成器时钟（采样 + 等待节拍）。
///
/// 一个窗口一个实例，由拥有该窗口的线程独占（泵 / 合成线程）；`hwnd` 只用于 DXGI 兜底时定位
/// 显示器，DWM 采样始终是桌面级的。
pub(crate) struct FrameClock {
    /// DXGI 兜底探测用（窗口所在显示器）；`HWND::default()` = 桌面级（主显示器）。
    hwnd: HWND,
    /// 最近一次采样 / 兜底值，恒在 [`MIN_REFRESH_HZ`]..=[`MAX_REFRESH_HZ`] 内。
    refresh_hz: f32,
    /// `refresh_hz` 的来源（[`RefreshSource`]）。
    source: RefreshSource,
}

impl fmt::Debug for FrameClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameClock")
            .field("hwnd", &self.hwnd)
            .field("refresh_hz", &self.refresh_hz)
            .field("source", &self.source)
            .finish()
    }
}

impl FrameClock {
    /// 为一个窗口建立时钟（`hwnd` 可传 `HWND::default()`：此时 DXGI 兜底按桌面原点找主显示器）。
    ///
    /// 建好就采一次：120/144 Hz 显示器不能等第一次 [`Self::sample`] 才发现。
    pub(crate) fn new(hwnd: HWND) -> Self {
        let mut clock = Self {
            hwnd,
            refresh_hz: FALLBACK_REFRESH_HZ,
            source: RefreshSource::Fallback,
        };
        // 采样已经把刷新率与来源写进缓存；构造期没有 `ClockSample` 的消费方。
        clock.sample();
        clock
    }

    /// 显示器刷新率（Hz，恒在 [`MIN_REFRESH_HZ`]..=[`MAX_REFRESH_HZ`] 内）。
    pub(crate) fn refresh_hz(&self) -> f32 {
        self.refresh_hz
    }

    /// 一帧的目标间隔 = `1 / refresh_hz`（动画 `dt` 的量化粒度，也是相位未知时的兜底估计）。
    pub(crate) fn frame_interval(&self) -> Duration {
        Duration::from_secs_f32(1.0 / self.refresh_hz())
    }

    /// 重新采样合成器时钟（`DwmGetCompositionTimingInfo`，O(1) 只读查询），返回本帧的采样。
    ///
    /// 同时更新缓存的刷新率：显示器切换 / 热插拔 / 动态刷新率（DRR）之后自动跟上。
    /// 探测失败**不是错误**：退回按缓存刷新率推算的整帧间隔，帧节奏不停。
    pub(crate) fn sample(&mut self) -> ClockSample {
        let until_next_present = self.resample();
        ClockSample {
            refresh_hz: self.refresh_hz,
            until_next_present,
        }
    }

    /// 阻塞到下一个合成器节拍（`DCompositionWaitForCompositorClock`）。
    ///
    /// `timeout` 被 clamp 到 `(0, 1 s]`：合成线程不能因为合成器停摆（显示器关闭、会话断开、
    /// DWM 退出）而无限期卡住；`false` = 这一拍没有节拍，调用方按「没有可呈现时刻」处理。
    ///
    /// 返回值是 `WaitForMultipleObjects` 风格的等待结果：官方复合时钟文档的示例里
    /// `WAIT_OBJECT_0 + count` 表示合成器节拍到了。本函数不传句柄（`count == 0`），所以节拍就是
    /// `WAIT_OBJECT_0`；其余值（`WAIT_TIMEOUT`、显示关闭时的 `STATUS_GRAPHICS_PRESENT_OCCLUDED`）
    /// 都算「没有节拍」。
    pub(crate) fn wait_next_frame(&self, timeout: Duration) -> bool {
        let timeout_ms = timeout.as_millis().clamp(1, u128::from(MAX_WAIT_MS)) as u32;
        // SAFETY: 无指针参数；`None` = 只等合成器时钟，不参与等待其它句柄。
        let wait_result = unsafe { DCompositionWaitForCompositorClock(None, timeout_ms) };
        wait_result == WAIT_OBJECT_0.0
    }

    /// 重新探测「刷新率 + 相位」，返回距下一次可呈现时刻的时长。
    fn resample(&mut self) -> Duration {
        // 主来源：DWM 合成时序（进程内只读查询，O(1)，每个连续帧采一次也没有成本）。
        let dwm = dwm_sample();
        // DWM 缺席时才问 DXGI：它要建工厂 / 枚举适配器与输出，比 DWM 查询贵得多，而且只给得出
        // 当前桌面模式的刷新率（没有相位）。
        let dxgi = if dwm.is_none() {
            dxgi_refresh_hz(self.hwnd)
        } else {
            None
        };
        let dwm_hz = dwm.map(|(refresh_hz, _)| refresh_hz);
        self.refresh_hz = choose_refresh(dwm_hz, dxgi);
        self.source = match (dwm_hz, dxgi) {
            (Some(_), _) => RefreshSource::Dwm,
            (None, Some(_)) => RefreshSource::Dxgi,
            (None, None) => RefreshSource::Fallback,
        };
        // 相位缺省 = 整整一帧：没有相位信息时宁可保守地等一整帧，也不要在「马上可呈现」的假
        // 信号上空转。再 clamp 到本帧间隔是 `ClockSample` 的契约。
        dwm.map_or_else(
            || self.frame_interval(),
            |(_, until_next_present)| until_next_present.min(self.frame_interval()),
        )
    }
}

/// 采样 DWM 的合成时序：`(刷新率, 距下一次可呈现时刻)`。
///
/// 任一环节拿不到 ⇒ `None`（调用方退回 DXGI / 兜底）：DWM 的时序字段在「合成被关 / 服务会话 /
/// 刚启动还没跑过一拍」时是 0，不能拿它当相位用。
fn dwm_sample() -> Option<(f32, Duration)> {
    let mut info = DWM_TIMING_INFO {
        cbSize: size_of::<DWM_TIMING_INFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` 是本函数的栈上值，`cbSize` 已按结构体实际大小填好。Win8.1 起 `hwnd` **必须**
    // 是 `HWND::default()`（传窗口句柄返回 E_INVALIDARG）—— 这个查询本来就是桌面级（合成器级）的。
    unsafe { DwmGetCompositionTimingInfo(HWND::default(), &mut info) }.ok()?;

    // `DWM_TIMING_INFO` 是 `packed(1)`（dwmapi.h 的 `pshpack1`）：字段只能按值读，不能取引用。
    let rate_refresh = info.rateRefresh;
    let refresh_period = info.qpcRefreshPeriod;
    let vblank_qpc = info.qpcVBlank;
    let refresh_count = info.cRefresh;

    let mut qpc_frequency = 0i64;
    // SAFETY: `qpc_frequency` 是本函数的栈上输出参数。
    unsafe { QueryPerformanceFrequency(&mut qpc_frequency) }.ok()?;
    if qpc_frequency <= 0 {
        return None;
    }

    // 刷新率：QPC 周期是主来源；`rateRefresh`（千分比）只在周期缺失时备选。
    let refresh_hz = refresh_hz_from_period(qpc_frequency, refresh_period)
        .or_else(|| refresh_hz_from_ratio(rate_refresh.uiNumerator, rate_refresh.uiDenominator))?;
    let frame_interval = Duration::from_secs_f32(1.0 / refresh_hz);

    // `cRefresh == 0`：DWM 还没报告过任何 vblank；`qpcVBlank == 0` / `qpcRefreshPeriod == 0`：
    // 没有可用的相位。三者都按「整整一帧之后」处理。
    if refresh_count == 0 || vblank_qpc == 0 || refresh_period == 0 {
        return Some((refresh_hz, frame_interval));
    }
    let mut now_qpc = 0i64;
    // SAFETY: `now_qpc` 是本函数的栈上输出参数。
    unsafe { QueryPerformanceCounter(&mut now_qpc) }.ok()?;
    let ticks = ticks_until_next_present(now_qpc, vblank_qpc, refresh_period)?;
    Some((
        refresh_hz,
        Duration::from_secs_f64(ticks as f64 / qpc_frequency as f64),
    ))
}

/// QPC 周期（每帧的 QPC tick 数）+ QPC 频率 → Hz。
///
/// `period == 0`（DWM 没有周期信息）、`frequency <= 0`（QPC 不可用）或结果落在
/// [`MIN_REFRESH_HZ`]..=[`MAX_REFRESH_HZ`] 之外 ⇒ `None`：越界值一定是采样坏了（例如
/// `period == 1` 会算成 1e7 Hz），不能拿它去推帧间隔。
fn refresh_hz_from_period(qpc_frequency: i64, qpc_refresh_period: u64) -> Option<f32> {
    if qpc_refresh_period == 0 || qpc_frequency <= 0 {
        return None;
    }
    // f64 相除：`qpc_frequency` 是 1e7 量级，越界判断要在不丢精度的域里做。
    let refresh_hz = qpc_frequency as f64 / qpc_refresh_period as f64;
    if !(f64::from(MIN_REFRESH_HZ)..=f64::from(MAX_REFRESH_HZ)).contains(&refresh_hz) {
        return None;
    }
    Some(refresh_hz as f32)
}

/// DWM 的 `UNSIGNED_RATIO`（`rateRefresh`，千分比）+ DXGI 的 `DXGI_RATIONAL`（`RefreshRate`）→ Hz。
///
/// 分母为 0（未填）或结果越界 ⇒ `None`（与 [`refresh_hz_from_period`] 同一套合法性判据）。
fn refresh_hz_from_ratio(numerator: u32, denominator: u32) -> Option<f32> {
    if denominator == 0 {
        return None;
    }
    let refresh_hz = f64::from(numerator) / f64::from(denominator);
    if !(f64::from(MIN_REFRESH_HZ)..=f64::from(MAX_REFRESH_HZ)).contains(&refresh_hz) {
        return None;
    }
    Some(refresh_hz as f32)
}

/// 距下一次 vblank 的 QPC tick 数：`(now_qpc, vblank_qpc, period)` → `0..=period`。
///
/// 相位取模后**向前**推进到下一个 vblank 边界：`now` 已经越过若干个 vblank（采样陈旧）或反而
/// 落后于 `vblank_qpc`（QPC 回退）时，`rem_euclid` 都把它折回一个周期内，不会 panic。`now` 正好
/// 落在 vblank 上 ⇒ `0`（这一刻就是可呈现时刻；另一选法是返回整周期，这里取 0 并在此说明）。
fn ticks_until_next_present(now_qpc: i64, vblank_qpc: u64, period: u64) -> Option<u64> {
    if period == 0 {
        return None;
    }
    // i128 防溢出：陈旧采样 + QPC 回退时 `now - vblank` 可以超出 i64 范围。
    let period = i128::from(period);
    let phase = (i128::from(now_qpc) - i128::from(vblank_qpc)).rem_euclid(period);
    // `phase == 0` ⇒ `period.rem_euclid(period) == 0`（正好在 vblank 上）；其余 ⇒ `period - phase`。
    u64::try_from((period - phase).rem_euclid(period)).ok()
}

/// 来源优先级：DWM 采样值 > DXGI 采样值 > 60.0 兜底（每级都先 clamp 到合法区间）。
fn choose_refresh(dwm: Option<f32>, dxgi: Option<f32>) -> f32 {
    match dwm.or(dxgi) {
        Some(refresh_hz) if refresh_hz.is_finite() => {
            refresh_hz.clamp(MIN_REFRESH_HZ, MAX_REFRESH_HZ)
        }
        // NaN / ∞ 只可能来自坏采样，与「没有采样」同样处理。
        _ => FALLBACK_REFRESH_HZ,
    }
}

/// DXGI 兜底：窗口所在显示器的**当前桌面模式**刷新率。
///
/// DXGI 没有「当前模式」查询接口（`DXGI_OUTPUT_DESC` / `DXGI_OUTPUT_DESC1` 里都没有刷新率）。
/// 官方给的做法是 `IDXGIOutput1::FindClosestMatchingMode1`：传一个**全部未指定**的模式
/// （分辨率 0、刷新率 0/0、scaling / scanline 未指定），未指定的成员会被对齐到该 output 的
/// **桌面设置**，于是返回的 `DXGI_MODE_DESC1::RefreshRate` 就是当前桌面模式。
/// 唯一约束：`pConcernedDevice` 传 NULL 时 `Format` 不能是 `DXGI_FORMAT_UNKNOWN`，
/// 所以给扫描输出常用的 8 位 BGRA。
fn dxgi_refresh_hz(hwnd: HWND) -> Option<f32> {
    let center = window_center(hwnd);
    // SAFETY: 纯 COM 创建调用，工厂句柄是输出；没有 DXGI / 没有显示设备时返回 Err。
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    // 适配器 / 输出的数量没有查询接口：越界枚举返回 DXGI_ERROR_NOT_FOUND，所以一直枚举到失败。
    let mut adapter_index = 0;
    loop {
        // SAFETY: 索引枚举，适配器句柄在本次循环内持有。
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(adapter_index) }) else {
            return None;
        };
        adapter_index += 1;
        let mut output_index = 0;
        loop {
            // SAFETY: 索引枚举，输出句柄在本次循环内持有。
            let Ok(output) = (unsafe { adapter.EnumOutputs(output_index) }) else {
                break;
            };
            output_index += 1;
            // SAFETY: `GetDesc` 只写调用方提供的结构（由 wrapper 返回）。
            let Ok(desc) = (unsafe { output.GetDesc() }) else {
                continue;
            };
            if !desc.AttachedToDesktop.as_bool() || !rect_contains(desc.DesktopCoordinates, center)
            {
                continue;
            }
            let Ok(output1) = output.cast::<IDXGIOutput1>() else {
                continue;
            };
            let requested = DXGI_MODE_DESC1 {
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                ..Default::default()
            };
            let mut closest = DXGI_MODE_DESC1::default();
            // SAFETY: 两个结构都是本函数的栈上值；`pConcernedDevice` 传 NULL。
            let matched = unsafe {
                output1.FindClosestMatchingMode1(&requested, &mut closest, None::<&IUnknown>)
            };
            if matched.is_err() {
                continue;
            }
            let refresh_rate = closest.RefreshRate;
            if let Some(refresh_hz) =
                refresh_hz_from_ratio(refresh_rate.Numerator, refresh_rate.Denominator)
            {
                return Some(refresh_hz);
            }
        }
    }
}

/// 窗口中心的桌面坐标（物理像素）。窗口不可用（`HWND::default()` / 已销毁）时退回桌面原点：
/// 主显示器的左上角恒为 `(0, 0)`，正好覆盖「没有窗口时按桌面级探测」的情形。
fn window_center(hwnd: HWND) -> (i32, i32) {
    let mut rect = RECT::default();
    // SAFETY: `rect` 是本函数的栈上输出参数。
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return (0, 0);
    }
    (
        rect.left + (rect.right - rect.left) / 2,
        rect.top + (rect.bottom - rect.top) / 2,
    )
}

/// 桌面坐标点是否落在显示器矩形内（右 / 下边界取开区间，与 Win32 的半开矩形习惯一致）。
fn rect_contains(rect: RECT, point: (i32, i32)) -> bool {
    (rect.left..rect.right).contains(&point.0) && (rect.top..rect.bottom).contains(&point.1)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::HWND;

    use super::{
        FALLBACK_REFRESH_HZ, FrameClock, MAX_REFRESH_HZ, MIN_REFRESH_HZ, choose_refresh,
        refresh_hz_from_period, refresh_hz_from_ratio, ticks_until_next_present,
    };

    #[test]
    fn period_converts_to_hz() {
        // 10 MHz QPC：166_666 tick ≈ 16.67 ms ⇒ 60.00 Hz；83_333 tick ⇒ 120.0 Hz。
        let sixty = refresh_hz_from_period(10_000_000, 166_666).expect("60 Hz 周期必须可换算");
        assert!((sixty - 60.0).abs() < 0.5, "{sixty}");
        let one_twenty = refresh_hz_from_period(10_000_000, 83_333).expect("120 Hz 周期必须可换算");
        assert!((one_twenty - 120.0).abs() < 0.5, "{one_twenty}");

        // period 0：DWM 没有周期信息；频率非正：QPC 不可用。
        assert_eq!(refresh_hz_from_period(10_000_000, 0), None);
        assert_eq!(refresh_hz_from_period(0, 166_666), None);
        assert_eq!(refresh_hz_from_period(-10_000_000, 166_666), None);

        // 离谱值（period == 1 ⇒ 1e7 Hz）与下限越界都必须被拒绝，不能拿它去推帧间隔。
        assert_eq!(refresh_hz_from_period(10_000_000, 1), None);
        assert_eq!(refresh_hz_from_period(10_000_000, 10_000_000), None);
    }

    #[test]
    fn ratio_converts_to_hz() {
        // 60_000/1_000 是 DWM `rateRefresh` 的千分比形态（60 Hz）；DXGI 给的是 60/1 形态。
        assert_eq!(refresh_hz_from_ratio(60_000, 1_000), Some(60.0));
        assert_eq!(refresh_hz_from_ratio(120_000, 1_000), Some(120.0));
        assert_eq!(refresh_hz_from_ratio(60, 1), Some(60.0));

        // 分母 0 = 比例未填。
        assert_eq!(refresh_hz_from_ratio(60_000, 0), None);

        // 越界：1000 Hz（超过 MAX）与 1 Hz（低于 MIN）都拒绝。
        assert_eq!(refresh_hz_from_ratio(1_000_000, 1_000), None);
        assert_eq!(refresh_hz_from_ratio(1, 1), None);
    }

    #[test]
    fn ticks_until_next_present_wraps_forward() {
        const PERIOD: u64 = 166_666;
        const VBLANK: u64 = 100_000;
        // `now_qpc` 是有符号的（QPC 可以回退）；两个常量都远在 i64 范围内。
        let vblank = VBLANK as i64;
        let period = PERIOD as i64;

        // vblank 之后半个周期 ⇒ 距下一个边界还有半个周期。
        assert_eq!(
            ticks_until_next_present(vblank + period / 2, VBLANK, PERIOD),
            Some(PERIOD / 2)
        );

        // 正好落在 vblank 上 ⇒ 0（这一刻就是可呈现时刻；另一选法是整周期，见函数文档）。
        assert_eq!(ticks_until_next_present(vblank, VBLANK, PERIOD), Some(0));

        // 采样陈旧 / QPC 回退（now 早于 vblank）⇒ 不 panic，相位折回一个周期内。
        let behind =
            ticks_until_next_present(vblank - 1, VBLANK, PERIOD).expect("时钟回退也要给出时长");
        assert!(behind <= PERIOD, "{behind}");
        assert_eq!(behind, 1, "落后 1 tick ⇒ 距离 vblank 还有 1 tick");

        // 已经越过若干 vblank ⇒ 仍然是「到下一个边界」的余量，而不是负值或整周期。
        assert_eq!(
            ticks_until_next_present(vblank + 2 * period + 1, VBLANK, PERIOD),
            Some(PERIOD - 1)
        );

        // period 0 ⇒ 没有相位可言。
        assert_eq!(ticks_until_next_present(vblank, VBLANK, 0), None);
    }

    #[test]
    fn refresh_source_priority_is_dwm_then_dxgi_then_60() {
        // DWM 优先：DWM 说 144 就以 144 为准（混合刷新率显示器上 DXGI 可能选到另一块屏）。
        assert_eq!(choose_refresh(Some(144.0), Some(60.0)), 144.0);
        // DWM 缺席 ⇒ DXGI。
        assert_eq!(choose_refresh(None, Some(75.0)), 75.0);
        // 两级都缺席 ⇒ 60 兜底。
        assert_eq!(choose_refresh(None, None), FALLBACK_REFRESH_HZ);
        assert_eq!(FALLBACK_REFRESH_HZ, 60.0);

        // 越界采样被 clamp 到合法区间，而不是原样带进帧间隔。
        assert_eq!(choose_refresh(Some(1.0), None), MIN_REFRESH_HZ);
        assert_eq!(choose_refresh(Some(10_000.0), None), MAX_REFRESH_HZ);
        // NaN / ∞ 与「没有采样」同路径。
        assert_eq!(choose_refresh(Some(f32::NAN), None), FALLBACK_REFRESH_HZ);
        assert_eq!(
            choose_refresh(Some(f32::INFINITY), None),
            FALLBACK_REFRESH_HZ
        );
    }

    #[test]
    fn frame_interval_matches_refresh() {
        let mut clock = FrameClock::new(HWND::default());

        let refresh_hz = clock.refresh_hz();
        assert!(
            (MIN_REFRESH_HZ..=MAX_REFRESH_HZ).contains(&refresh_hz),
            "{refresh_hz}"
        );
        let interval = clock.frame_interval();
        assert!(interval >= Duration::from_secs_f32(1.0 / MAX_REFRESH_HZ));
        assert!(interval <= Duration::from_secs_f32(1.0 / MIN_REFRESH_HZ));

        // 真机 vblank 相位不可断言，只能断言采样被 clamp 在 [0, 帧间隔] 内。
        let sample = clock.sample();
        assert!(
            (MIN_REFRESH_HZ..=MAX_REFRESH_HZ).contains(&sample.refresh_hz),
            "{sample:?}"
        );
        assert!(
            sample.until_next_present <= clock.frame_interval(),
            "{sample:?}"
        );

        // `Debug` 是刷新率来源（`RefreshSource`）的唯一出口，必须能打印出来。
        let debug = format!("{clock:?}");
        assert!(
            debug.contains("FrameClock") && debug.contains("source"),
            "{debug}"
        );
    }

    #[test]
    fn wait_next_frame_is_bounded_and_honours_a_short_timeout() {
        let clock = FrameClock::new(HWND::default());
        let started = Instant::now();
        let ticked = clock.wait_next_frame(Duration::from_millis(1));
        let elapsed = started.elapsed();

        // 真机 / 远程桌面 / 无桌面会话下这一拍可能来也可能不来：不断言真假，只断言有返回值。
        // 防呆：超时被 clamp 到 1 s，任何实现都不该在这里卡住半秒以上（无界等待的 bug 会挂这条）。
        assert!(
            elapsed <= Duration::from_millis(500),
            "ticked={ticked} elapsed={elapsed:?}"
        );
    }
}

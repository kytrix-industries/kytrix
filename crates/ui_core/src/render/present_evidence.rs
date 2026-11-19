//! 上屏证据与呈现节奏的**平台无关**逻辑（R3 / A4）。
//!
//! 三端各有一条薄壳，把平台原生时间戳喂进本模块，由本模块决定
//! **`presented_at` 怎么换算/钳制、`latency` 怎么算、什么时候发布反馈**：
//!
//! - macOS（`render/macos/renderer.rs`）：`CAMetalDisplayLink` 的排定时刻 + tick 序号
//!   （[`presentation_instant`] / [`FeedbackQueue`] / [`PacingGate`]）；
//! - Windows（`render/windows/renderer.rs`）：`IDXGISwapChain::GetFrameStatistics` 的
//!   `SyncQPCTime` / 刷新计数（[`qpc_instant`] / [`presented_qpc_from_frame_statistics`] /
//!   [`refresh_period_qpc`]）；
//! - Linux（`platform/linux/presentation.rs` + `platform/linux/surface.rs`）：
//!   `wp_presentation_feedback.presented` 的 `(tv_sec_hi, tv_sec_lo, tv_nsec)` 呈现时钟
//!   （[`presented_event_instant`]）与跨线程邮箱 [`PresentationInbox`]；事件在 UI 线程派发，
//!   由泵按 `FrameIndex` 回填到窗口的呈现历史（`Window::note_frame_presented`）。
//!
//! 本模块**三平台无条件编译**；各端专用的项按 `cfg` 只在该端生产构建 + 所有平台的
//! `cfg(test)` 构建里存在——这样 Windows 开发机的 `cargo test` 能跑到 Linux / macOS 的判据
//! （「三平台全绿」必须有本机可执行证据，AR5 P1-3）。
//!
//! 语义约定（三端共享契约在 [`crate::render::frame::FrameFeedback`]）：
//! - `completed_at` = GPU 线程**观察到 GPU 工作完成**的时刻；
//! - `presented_at` = 真实上屏时刻的**下界**，偏差方向恒为**偏早**（见 [`clamp_presented_at`]）；
//! - `latency` = 发布 → GPU 完成（与 `presented_at` 无关）。

#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::collections::VecDeque;
/// 时间类型在 Windows（帧统计换算）/ macOS（display link 推断）/ Linux（presentation-time
/// 呈现时钟换算）的生产构建与所有平台测试构建里使用。
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
use std::time::{Duration, Instant};

#[cfg(any(target_os = "macos", test))]
use crate::render::{
    RenderStats,
    frame::{FrameFeedback, MAX_FRAMES_IN_FLIGHT},
    timer::GpuTimerSample,
};
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use crate::window::FrameIndex;

/// display link 的空闲滞回窗口（R06-8）。
///
/// 最后一次工作之后 display link 继续跑这么久再暂停。理由：孤立更新（一次行情推送、一次
/// hover、一次 a11y 动作回投）之间通常只隔几十~几百毫秒；一旦暂停，恢复后要等 display link
/// 的下一个 vsync 回调才拿到 drawable ⇒ 每次孤立更新白付一帧延迟（表现为"偶发的、随机的
/// 一帧卡顿"，在行情/交易软件里最刺眼）。250 ms ≈ 15 个 vsync：覆盖散户行情（≥1 Hz）与交互
/// 反馈，同时"空闲 0 唤醒"的目标不变（最后一次工作后最多 250 ms 必定 `isPaused = true`）。
#[cfg(any(target_os = "macos", test))]
pub(crate) const IDLE_GRACE: Duration = Duration::from_millis(250);

/// display link 需要跑的全部理由（`sync_pacing` 的唯一输入；纯函数便于单测）。
///
/// - `continuous`：应用要连续帧（动画；Debug HUD 是纯观察者，不参与）；
/// - `pending_frame`：有未取走的帧请求（GPU 线程还没拿走的）；
/// - `in_flight`：还有在飞帧（它们要等 vsync 完成）；
/// - `awaiting_presentation`：**还在等上屏证据**。这一条不能省：display link 是 tick 推断的
///   唯一来源，暂停之后不会再有回调，最后在飞的那一帧永远等不到证据（反馈不发布、
///   `presented_at` 永远为空）。
#[cfg(any(target_os = "macos", test))]
pub(crate) fn pacing_work(
    continuous: bool,
    pending_frame: bool,
    in_flight: usize,
    awaiting_presentation: bool,
) -> bool {
    continuous || pending_frame || in_flight > 0 || awaiting_presentation
}

/// display link 的暂停状态机（§5.3；纯逻辑便于单测）。
///
/// 有工作 ⇒ 立刻恢复；没有工作 ⇒ **不立即**暂停，先保持 [`IDLE_GRACE`] 的运行窗口（R06-8）。
/// `update` 每轮主线程泵调用一次，返回本轮是否应当暂停。
#[cfg(any(target_os = "macos", test))]
#[derive(Debug)]
pub(crate) struct PacingGate {
    /// 最近一次「有工作」的时刻（`None` = 从未有过工作 ⇒ 初始即暂停）。
    last_work: Option<Instant>,
    /// 当前是否已暂停（与 `CAMetalDisplayLink::isPaused` 同义；初始值见 `WindowRenderer::new`）。
    paused: bool,
    /// 已发生的「运行 → 暂停」转换次数。**只用于单测断言**（滞回窗口内不得出现往返），
    /// 生产构建里不存在这个字段（否则会产生 dead_code 警告）。
    #[cfg(test)]
    pauses: u64,
}

#[cfg(any(target_os = "macos", test))]
impl PacingGate {
    /// 初始状态：暂停（macOS 的 `WindowRenderer::new` 里 display link 就是 `setPaused(true)`）。
    pub(crate) fn new() -> Self {
        Self {
            last_work: None,
            paused: true,
            #[cfg(test)]
            pauses: 0,
        }
    }

    /// 记录一次「有工作」：立刻恢复（`publish` 也走这里，保证门面状态与 display link 一致）。
    pub(crate) fn note_work(&mut self, now: Instant) {
        self.last_work = Some(now);
        self.paused = false;
    }

    /// 推进状态机并返回本轮的暂停决策。
    pub(crate) fn update(&mut self, work: bool, now: Instant) -> bool {
        if work {
            self.note_work(now);
            return false;
        }
        let idle = self.last_work.map(|at| now.saturating_duration_since(at));
        let should_pause = idle.is_none_or(|idle| idle >= IDLE_GRACE);
        if should_pause && !self.paused {
            #[cfg(test)]
            {
                self.pauses = self.pauses.saturating_add(1);
            }
        }
        self.paused = should_pause;
        should_pause
    }

    /// 已发生的暂停转换次数（R06-8 的计数断言面）。
    #[cfg(test)]
    const fn pauses(&self) -> u64 {
        self.pauses
    }
}

/// 「目标呈现时刻」与采样时刻的最大合理差距：display link 只提前 1–2 个 vsync 给出排定时刻，
/// 超过它就是垃圾值 / 时钟异常，按「拿不到」处理（见 [`presentation_instant`]）。
#[cfg(any(target_os = "macos", test))]
const MAX_TARGET_LEAD: Duration = Duration::from_millis(250);

/// display link 给出的**目标上屏时刻**（host time，秒）→ `Instant`。
///
/// `targetPresentationTimestamp` 与 `CACurrentMediaTime()` 是同一时基（`mach_absolute_time` 的
/// 秒形式），与 `Instant` 只差「采样此刻」这一个点：先算两者的差，再从 `observed` 推出去。
///
/// 与「呈现回调」不同，目标时刻在**未来**（display link 提前一个 vsync 把本 vsync 的排定时刻交给
/// 我们），因此两个方向都要支持：`target - now_host` 为正就往 `observed` 之后推。非法值
/// （`0.0` / 非有限 / 差值超过 [`MAX_TARGET_LEAD`]）返回 `None`：调用方如实把 `presented_at` 留空，
/// 而不是拿别的时刻顶替（假数据）。
///
/// 入参是 `CFTimeInterval` 的裸 `f64`（不做类型依赖），因此本函数在非 macOS 平台也能单测。
#[cfg(any(target_os = "macos", test))]
pub(crate) fn presentation_instant(
    target: f64,
    observed: Instant,
    now_host: f64,
) -> Option<Instant> {
    if !target.is_finite() || target <= 0.0 || !now_host.is_finite() {
        return None;
    }
    let lead = target - now_host;
    if !lead.is_finite() || lead.abs() > MAX_TARGET_LEAD.as_secs_f64() {
        return None;
    }
    if lead >= 0.0 {
        observed.checked_add(Duration::from_secs_f64(lead))
    } else {
        observed.checked_sub(Duration::from_secs_f64(-lead))
    }
}

/// 把 tick 推断的排定上屏时刻钳到 GPU 完成时刻之后：**上屏时刻的下界（偏差方向恒为偏早）**。
///
/// GPU 晚于排定时刻才画完（错过本 vsync）时，像素最早只能在完成后**下一个** vsync 上屏，
/// 而报告值 = 本帧的排定时刻会得出「像素比 GPU 工作更早上屏」的荒谬值。这里取
/// `max(排定时刻, completed_at)`：
///
/// - 工作正常（GPU 在排定时刻前完成）：返回值 = 排定时刻，误差 ≤1 vsync，**偏早**；
/// - 错过本 vsync：返回值 = `completed_at`，真实上屏在之后 ≤1 vsync，**偏早最多 1 vsync**。
///
/// 因此它**永远不是真实上屏时刻**，只是「真实时刻 − [0, 1 vsync]」的**下界**；对外契约
/// （[`FrameFeedback::presented_at`]）的措辞必须按这个语义使用。`None` 原样透传。
///
/// **cfg 说明**：三端生产构建都用它 —— Windows（帧统计路径）与 macOS（tick 推断）在发布
/// 反馈前收口，Linux 在平台层把 `wp_presentation` 事件回填到窗口呈现历史时收口
/// （[`crate::window::Window::note_frame_presented`]）；所有平台测试构建无条件可见。
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
pub(crate) fn clamp_presented_at(
    presented: Option<Instant>,
    completed_at: Instant,
) -> Option<Instant> {
    presented.map(|presented| presented.max(completed_at))
}

// ─────────────────────────── Windows：DXGI_FRAME_STATISTICS（A4） ───────────────────────────

/// 上屏证据与**采样时刻**的最大合理距离（秒）。
///
/// `GetFrameStatistics.SyncQPCTime` 是**最近一次 vblank** 的 QPC 时刻，与采样时刻只差一个刷新
/// 周期；超过 1 秒说明时钟域异常（QPC 回绕 / 换显示器 / DWM 重建），按「拿不到」处理。
/// 注意它约束的是「换算结果与 `now` 的距离」，**不是**与锚点的距离：锚点只在进程内取一次，
/// 之后 QPC 继续前进，二者相距可以任意远（线性映射本身与距离无关）。
#[cfg(any(windows, test))]
const MAX_QPC_LEAD_SECS: f64 = 1.0;

/// `QueryPerformanceCounter` 读数 → `Instant`（QPC 锚点线性映射）。
///
/// 入参是裸整数（QPC ticks / 频率），因此本函数在非 Windows 的测试构建里也能单测（A4）。
/// `anchor_qpc` / `anchor` 必须来自**同一次**采样（进程级锚点
/// `(QueryPerformanceCounter, Instant::now())`；只在进程内取一次，之后一直复用）。
///
/// 返回 `None` 的情形（契约要求不可信时如实留空，绝不拿别的时刻顶替）：
/// - `frequency <= 0`（系统没给有效频率）；
/// - 换算结果晚于 `now`（上屏时刻不可能在采样时刻之后）或早于 `now −`[`MAX_QPC_LEAD_SECS`]
///   （证据与采样时刻相距过远 ⇒ 时钟域/锚点异常）。
///
/// 返回值仍是**时间戳本身**（不是下界）；与 `completed_at` 的关系由调用方经
/// [`clamp_presented_at`] 收口。
#[cfg(any(windows, test))]
pub(crate) fn qpc_instant(
    presented_qpc: i64,
    anchor_qpc: i64,
    anchor: Instant,
    now: Instant,
    frequency: i64,
) -> Option<Instant> {
    if frequency <= 0 {
        return None;
    }
    let ticks = presented_qpc.checked_sub(anchor_qpc)?;
    let seconds = ticks as f64 / frequency as f64;
    if !seconds.is_finite() {
        return None;
    }
    // **不要**限制 `ticks`/`seconds` 的绝对值：锚点（`OnceLock`）只在进程内取一次，之后 QPC
    // 持续前进，`presented_qpc − anchor_qpc` 随进程 uptime 单调增长。曾经在这里加过
    // 「|seconds| ≤ 10s」的上限，导致进程运行约 10 秒后 `presented_at` **永久变 None**
    // （RA-1：20s acceptance 有值率 38.64%，与「首帧+10s 终断」模型吻合）。线性映射本身与
    // 锚点距离无关，合法性判据是下面两条：结果不晚于采样时刻、且离采样时刻不超过一个刷新
    // 量级（1s）。
    let instant = if seconds >= 0.0 {
        anchor.checked_add(Duration::from_secs_f64(seconds))?
    } else {
        anchor.checked_sub(Duration::from_secs_f64(-seconds))?
    };
    // 上屏是**已经发生**的事：晚于采样时刻不可信；与采样时刻相距超过 1 s 的「上屏证据」
    // 也不可信（`GetFrameStatistics` 的 sync 采样只落后一个刷新周期）。
    let age = now.saturating_duration_since(instant);
    (instant <= now && age <= Duration::from_secs_f64(MAX_QPC_LEAD_SECS)).then_some(instant)
}

/// 相邻两次 `GetFrameStatistics` 采样 → 每个 vblank 的 QPC 周期（ticks）。
///
/// `DXGI_FRAME_STATISTICS` **没有** `SyncRefreshPeriod` 字段（`SyncGPUTime` 恒 0；MS docs 与
/// windows crate 0.62 均已核对）：刷新周期只能用 `ΔSyncQPCTime / ΔSyncRefreshCount` 实测。要求：
/// - `SyncRefreshCount` 前进（计数不前进 ⇒ 这段时间没有新的 vblank 证据，返回 `None`）；
/// - `SyncQPCTime` 前进；
/// - 换算出的周期在 [1 ms, 100 ms]（24–1000 Hz），否则视为垃圾采样。
#[cfg(any(windows, test))]
pub(crate) fn refresh_period_qpc(
    previous: (u32, i64),
    current: (u32, i64),
    frequency: i64,
) -> Option<i64> {
    if frequency <= 0 {
        return None;
    }
    let count_delta = current.0.checked_sub(previous.0)?;
    let qpc_delta = current.1.checked_sub(previous.1)?;
    if count_delta == 0 || qpc_delta <= 0 {
        return None;
    }
    let period = qpc_delta / i64::from(count_delta);
    let seconds = period as f64 / frequency as f64;
    if !(0.001..=0.1).contains(&seconds) {
        return None;
    }
    Some(period)
}

/// 由一帧 `DXGI_FRAME_STATISTICS` 还原「最近一次已上屏内容」的呈现时刻（QPC ticks）。
///
/// `PresentRefreshCount` 是**最后一次上屏**发生在哪个 vblank；`SyncQPCTime` 是
/// `SyncRefreshCount` 那个 vblank 的 QPC 时刻。两者相差 `lag` 个 vblank，因此
/// `presented = SyncQPCTime − lag × period`。
///
/// **契约方向**：本函数返回的是「已经上屏的某个内容」的时刻，调用方只能把它用在
/// **不早于**那次上屏的帧上（`render/windows/renderer.rs` 在 present 之后立即采样，因此
/// 「最后一次已上屏内容」必然早于本帧），并在发布前经 [`clamp_presented_at`] 钳到该帧的
/// `completed_at` 之后：两个下界取较大者仍是下界。
///
/// 返回 `None`：`PresentRefreshCount > SyncRefreshCount`（计数不一致）、`period_qpc <= 0`、
/// 乘法/减法溢出、结果非正。`lag == 0` 时不需要周期（呈现时刻就是 `SyncQPCTime`）。
#[cfg(any(windows, test))]
pub(crate) fn presented_qpc_from_frame_statistics(
    sync_refresh_count: u32,
    present_refresh_count: u32,
    sync_qpc_time: i64,
    period_qpc: Option<i64>,
) -> Option<i64> {
    let lag = sync_refresh_count.checked_sub(present_refresh_count)?;
    let offset = match (lag, period_qpc) {
        (0, _) => 0,
        (_, Some(period)) if period > 0 => i64::from(lag).checked_mul(period)?,
        // `lag > 0` 但没有可信周期：算不出呈现时刻（不能猜一个刷新率去乘）。
        _ => return None,
    };
    let presented = sync_qpc_time.checked_sub(offset)?;
    (presented > 0).then_some(presented)
}

// ─────────────────────────── Linux：wp_presentation（A4） ───────────────────────────

/// 呈现时间戳与 `clock_gettime` 锚点的最大合理距离（秒）。
///
/// `presented` 事件在 vblank 之后几微秒~几毫秒到达；超过 1 秒说明时钟域选错或
/// `clock_gettime` 读数异常（`CLOCK_REALTIME` 被 NTP 步进时也会出现），按「拿不到」处理。
#[cfg(any(target_os = "linux", test))]
const MAX_PRESENTATION_DELTA_SECS: f64 = 1.0;

/// wp_presentation 认可的 POSIX `clockid_t`：`CLOCK_REALTIME` / `CLOCK_MONOTONIC`。
///
/// 协议原文（`presentation.clock_id`）：「On POSIX platforms, the identifier value is one of
/// the clockid_t values accepted by clock_gettime()」。sway / Mutter / KWin 都发
/// `CLOCK_MONOTONIC`；`CLOCK_REALTIME` 只在旧实现里出现，换算方式相同（真正保证正确性的是用
/// **同一时钟域**的 `clock_gettime` 与 `Instant::now()` 取锚点，见 [`presented_event_instant`]）。
/// 其余时钟域（`MONOTONIC_RAW` / `BOOTTIME` 等）一律拒绝：它们与 `Instant` 的时基关系没有保证。
#[cfg(any(target_os = "linux", test))]
pub(crate) const CLOCK_REALTIME: i32 = 0;
/// 见 [`CLOCK_REALTIME`]；绝大多数 compositor 的 presentation clock。
#[cfg(any(target_os = "linux", test))]
pub(crate) const CLOCK_MONOTONIC: i32 = 1;

/// `(tv_sec_hi, tv_sec_lo, tv_nsec)` + 呈现时钟 → `Instant`。
///
/// `clock_anchor_secs` 是事件到达时对**同一时钟域**调 `clock_gettime(clock_id)` 的读数（秒），
/// `anchor` 是**同一时刻**的 `Instant::now()`：两者构成线性锚点，把呈现时间戳搬到单调时钟上。
/// 这个映射只依赖「同一进程内两个时钟读数点的差值」，不依赖 compositor 的绝对时钟值。
///
/// 返回 `None`（不可信就如实留空）：未知 `clock_id`、`tv_nsec > 999_999_999`、
/// 锚点非有限、与锚点相差超过 ±[`MAX_PRESENTATION_DELTA_SECS`]、换算结果晚于 `now`。
///
/// 与 `completed_at` 的关系不是本函数职责：Linux 由平台层在
/// [`crate::window::Window::note_frame_presented`] 回填时经 [`clamp_presented_at`] 收口
/// （真实呈现时刻 ≥ GPU 完成时刻，映射误差只允许把它推早）。
#[cfg(any(target_os = "linux", test))]
pub(crate) fn presented_event_instant(
    tv_sec_hi: u32,
    tv_sec_lo: u32,
    tv_nsec: u32,
    clock_id: i32,
    clock_anchor_secs: f64,
    anchor: Instant,
    now: Instant,
) -> Option<Instant> {
    if !matches!(clock_id, CLOCK_REALTIME | CLOCK_MONOTONIC) {
        return None;
    }
    if tv_nsec > 999_999_999 || !clock_anchor_secs.is_finite() {
        return None;
    }
    let secs = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
    let presented = secs as f64 + f64::from(tv_nsec) / 1e9;
    let delta = presented - clock_anchor_secs;
    if !delta.is_finite() || delta.abs() > MAX_PRESENTATION_DELTA_SECS {
        return None;
    }
    let instant = if delta >= 0.0 {
        anchor.checked_add(Duration::from_secs_f64(delta))?
    } else {
        anchor.checked_sub(Duration::from_secs_f64(-delta))?
    };
    // 呈现已经发生：晚于采样时刻的值不可信（时钟域/锚点选错）。
    (instant <= now).then_some(instant)
}

/// `wp_presentation` 的一帧呈现结果（UI 线程派发事件 → 泵按 `FrameIndex` 回填）。
#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresentationOutcome {
    /// 已上屏：呈现时刻（仍是**下界证据**，回填时经 [`clamp_presented_at`]）。
    Presented(Instant),
    /// 内容被合成器丢弃（surface 销毁 / 被后来者取代）：如实 `None`。
    Discarded,
}

/// 邮箱容量上限：正常 ≤ 在飞帧数；超过说明反馈消费端停了，丢弃最旧并计数（诊断面）。
#[cfg(any(target_os = "linux", test))]
const MAX_PRESENTATION_EVENTS: usize = 16;

/// 呈现反馈的进程内邮箱：Wayland 事件在 **UI 线程**派发，泵按 `FrameIndex` 回填窗口历史。
///
/// 事件按 `FrameIndex` 归档（反馈对象创建时把目标 frame 写进 user data），因此到达顺序无关；
/// 每条反馈恰好一个终态（presented / discarded），重复/迟到的事件按「先到者为准」忽略。
///
/// Linux 生产路径没有「等上屏证据的 GPU 线程」（帧节奏由 `wl_callback` 门禁承担）⇒
/// [`Self::set_waker`] 只在测试构建里存在，生产不注册 waker。
#[cfg(any(target_os = "linux", test))]
pub(crate) struct PresentationInbox {
    events: std::sync::Mutex<VecDeque<(FrameIndex, PresentationOutcome)>>,
    waker: std::sync::Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
    /// 容量上限丢弃的反馈条数（诊断；正常恒为 0）。
    dropped: std::sync::atomic::AtomicU64,
}

#[cfg(any(target_os = "linux", test))]
impl PresentationInbox {
    pub(crate) fn new() -> Self {
        Self {
            events: std::sync::Mutex::new(VecDeque::with_capacity(MAX_PRESENTATION_EVENTS)),
            waker: std::sync::Mutex::new(None),
            dropped: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 注册 GPU 线程的唤醒（测试 / 未来「等上屏证据」的消费者；Linux 生产路径不设置）。
    #[cfg(test)]
    pub(crate) fn set_waker(&self, waker: std::sync::Arc<dyn Fn() + Send + Sync>) {
        *self
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(waker);
    }

    /// 记录一条终态事件并唤醒等待方（若注册过）。同一帧的重复事件按先到者为准。
    pub(crate) fn note(&self, index: FrameIndex, outcome: PresentationOutcome) {
        {
            let mut events = self
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if events.iter().any(|(queued, _)| *queued == index) {
                return;
            }
            if events.len() >= MAX_PRESENTATION_EVENTS {
                let _ = events.pop_front();
                self.dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            events.push_back((index, outcome));
        }
        let waker = self
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(waker) = waker {
            waker();
        }
    }

    /// 取走某一帧的终态（`None` = 还没到 / 已被容量上限丢弃）。
    pub(crate) fn take(&self, index: FrameIndex) -> Option<PresentationOutcome> {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let position = events.iter().position(|(queued, _)| *queued == index)?;
        events.remove(position).map(|(_, outcome)| outcome)
    }

    /// 已排队、尚未被取走的条数（诊断）。
    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// 容量上限丢弃的条数（诊断；正常恒为 0）。
    #[cfg(test)]
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// 一帧的提交记账（反馈的统计与发布基准）。
///
/// **两个时刻必须分开**（AR3 §6.3 / P2-12）：
/// - [`Self::completed_at`]：GPU 线程**观察到 GPU 工作完成**的时刻，与共享契约
///   （`render/frame.rs`）以及 Windows/Linux 两端同义；
/// - [`Self::presented_at`]：本帧**被排定的上屏时刻**（提交时由 display link 的
///   `targetPresentationTimestamp` 换算的 **tick 推断值**），不是逐帧真实呈现时间戳。
#[cfg(any(target_os = "macos", test))]
struct SubmittedFrame {
    index: FrameIndex,
    stats: RenderStats,
    samples: Vec<GpuTimerSample>,
    published_at: Instant,
    /// 本帧所用 drawable 的 tick 序号（`DrawableUpdate::sequence`）：更新的 tick 到达 ⇒ 本帧已上屏。
    tick: u64,
    /// 完成通知到达的时刻（`None` = 仍在飞）。
    completed_at: Option<Instant>,
    /// tick 推断的上屏时刻（`None` = 系统没给可用的目标时刻）；**不是**完成时刻的占位。
    ///
    /// 发布前还要经 [`clamp_presented_at`] 钳到 GPU 完成之后；它的语义是真实上屏时刻的
    /// **下界**（偏差方向恒为偏早、最多 1 个 vsync），不是「可信上屏时刻」的近似值。
    presented_at: Option<Instant>,
    /// 上屏证据（一个更新的 tick）已经到达：只有到这时才允许把 `presented_at` 发布出去。
    presented: bool,
}

/// 反馈队列的容量上限（R5b / AR5 P2-3）。
///
/// 正常情况下队列长度天然 ≤ [`MAX_FRAMES_IN_FLIGHT`]（提交需要 display link tick 提供的
/// drawable）；遮挡等「长期没有 tick」的场景下滞留帧理论上会累积，这里给硬上限兜底，避免
/// `VecDeque` 无界增长。`+1` 覆盖「最后一帧已完成、只等上屏证据」的那一帧。
#[cfg(any(target_os = "macos", test))]
const MAX_FEEDBACK_FRAMES: usize = MAX_FRAMES_IN_FLIGHT + 1;

/// 按提交顺序发布反馈的队列（`render/frame.rs` 的跨平台契约）。
///
/// Apple 只保证同一队列 command buffer 的**开始**顺序，完成回调可以乱序到达。按回调顺序发布
/// 会让 HUD 在乱序时用更旧帧的 `latency` 覆盖新值。这里把完成数据记到各自的提交槽上，只有
/// `CompletionOrder` 的连续前缀推进时（[`FeedbackQueue::pop_ready`]）才从队首按提交顺序弹出并
/// 发布；被跳过的帧（不产生 GPU 工作）不在队列里，不影响顺序。
#[cfg(any(target_os = "macos", test))]
pub(crate) struct FeedbackQueue {
    submitted: VecDeque<SubmittedFrame>,
    /// 容量上限触发的丢弃次数（诊断；正常恒为 0）。
    dropped: u64,
    /// 被丢弃且**尚未完成**的帧：它们的完成回调晚到时按 FIFO 消费，不触发 `debug_assert`。
    ///
    /// 这些条目只可能来自上限兜底路径；完成回调按提交顺序到达（`CompletionOrder` 语义），
    /// 因此 FIFO 匹配即可。
    abandoned: VecDeque<FrameIndex>,
}

#[cfg(any(target_os = "macos", test))]
impl FeedbackQueue {
    pub(crate) fn new() -> Self {
        Self {
            submitted: VecDeque::with_capacity(MAX_FEEDBACK_FRAMES),
            dropped: 0,
            abandoned: VecDeque::new(),
        }
    }

    /// 提交成功：按提交顺序入队本帧的统计、tick 序号、tick 推断的上屏时刻与发布基准。
    ///
    /// 队列达到 [`MAX_FEEDBACK_FRAMES`] 时丢弃**最旧**的反馈并计数（只在「没有 tick、上游
    /// 不变式被破坏」的兜底路径上发生）；从未完成的被丢弃帧的完成回调晚到时按 FIFO 忽略。
    pub(crate) fn push(
        &mut self,
        index: FrameIndex,
        stats: RenderStats,
        samples: Vec<GpuTimerSample>,
        published_at: Instant,
        tick: u64,
        presented_at: Option<Instant>,
    ) {
        if self.submitted.len() >= MAX_FEEDBACK_FRAMES {
            let dropped = self.submitted.pop_front();
            self.dropped = self.dropped.saturating_add(1);
            if self.dropped == 1 {
                eprintln!(
                    "kytrix: feedback queue hit {MAX_FEEDBACK_FRAMES} frames without a display-link tick; \
                     dropping the oldest feedback (subsequent drops are only counted)"
                );
            }
            if let Some(frame) = dropped
                && frame.completed_at.is_none()
            {
                self.abandoned.push_back(frame.index);
            }
        }
        self.submitted.push_back(SubmittedFrame {
            index,
            stats,
            samples,
            published_at,
            tick,
            completed_at: None,
            presented_at,
            presented: false,
        });
    }

    /// 完成通知到达：记录 GPU 采样与完成时刻；**不**发布（可能还在等更早的帧或上屏证据）。
    pub(crate) fn complete(&mut self, index: FrameIndex, sample: Option<GpuTimerSample>) {
        let position = self.submitted.iter().position(|frame| frame.index == index);
        let Some(position) = position else {
            // 容量上限丢弃的帧：完成回调晚到是**预期**行为（按提交顺序 FIFO 消费）。
            if self.abandoned.front() == Some(&index) {
                let _ = self.abandoned.pop_front();
                return;
            }
            debug_assert!(
                false,
                "completion for frame {} without a submission",
                index.get()
            );
            return;
        };
        let frame = &mut self.submitted[position];
        if let Some(sample) = sample {
            frame.samples.push(sample);
        }
        frame.completed_at = Some(Instant::now());
    }

    /// 观察到第 `latest` 个 display link tick：tick 序号更早的帧**已经上屏**。
    ///
    /// 判据来自 display link 本身而不是墙钟：一个 tick 到达说明**上一个 vsync 的呈现截止时刻
    /// 已经过去**，而该 vsync 的 drawable 正是那一帧的呈现目标。因此不需要（也不可能）拿到逐帧
    /// 真实的 `presentedTime`，只需要「排定时刻 + 已经到达的下一个 tick」两件事。
    pub(crate) fn observe_tick(&mut self, latest: u64) {
        for frame in &mut self.submitted {
            if !frame.presented && frame.tick < latest {
                frame.presented = true;
            }
        }
    }

    /// 还在等上屏证据的帧里最早的 tick 序号（`None` = 队列为空 / 全部有证据）。
    ///
    /// 交给 `Shared::set_awaiting_tick`：它同时决定 `wait_for_work` 的唤醒条件与 display link
    /// 能否暂停。
    pub(crate) fn awaiting_tick(&self) -> Option<u64> {
        self.submitted
            .iter()
            .find(|frame| !frame.presented)
            .map(|frame| frame.tick)
    }

    /// `CompletionOrder` 的连续前缀推进到 `frontier`：按提交顺序弹出「已完成 + 上屏证据已到」的
    /// 帧并发布。
    ///
    /// 两个条件必须同时成立，缺一个都不发布：
    /// 1. GPU 工作完成（完成回调到达，`completed_at` 有值）；
    /// 2. 上屏证据（此后观察到一个更新的 display link tick，[`Self::observe_tick`]）。
    ///
    /// 旧实现在等不到 `presentedTime` 时睡 32 ms（`PRESENTED_WAIT`）后用完成时刻兜底发布：
    /// 那既是假数据（把 GPU 完成冒充上屏），又让 GPU 线程每 32 ms 空转一次。现在两个条件分别由
    /// **完成回调**与 **display link tick** 驱动，没有任何定时轮询。
    ///
    /// 发布口径（AR3 §6.3 + B6-2 统一）：`completed_at` = GPU 工作完成、`latency` = 发布 → GPU
    /// 完成（三端同义）；`presented_at` = 经 [`clamp_presented_at`] 钳过的**上屏时刻下界**
    /// （`None` = 系统没给目标时刻）。
    pub(crate) fn pop_ready(
        &mut self,
        frontier: FrameIndex,
        mut publish: impl FnMut(FrameFeedback),
    ) {
        while let Some(front) = self.submitted.front() {
            if front.index > frontier {
                break;
            }
            let Some(completed_at) = front.completed_at else {
                debug_assert!(false, "completion frontier passed an incomplete frame");
                break;
            };
            // 上屏证据还没到：停在这里，保序（后面的帧也不能先发布）。
            if !front.presented {
                break;
            }
            let presented_at = clamp_presented_at(front.presented_at, completed_at);
            let frame = self.submitted.pop_front().expect("front exists");
            publish(FrameFeedback {
                index: frame.index,
                stats: frame.stats,
                samples: frame.samples,
                // 共享契约（B6-2 统一）：`latency` 三端同义 = 发布 → **GPU 完成**。
                latency: completed_at.saturating_duration_since(frame.published_at),
                // GPU 工作完成时刻（共享契约）；**不是**上屏时刻。
                completed_at,
                // tick 推断的上屏时刻下界；系统没给目标时刻时如实为 `None`（绝不拿完成时刻顶替）。
                presented_at,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        CLOCK_MONOTONIC, CLOCK_REALTIME, FeedbackQueue, IDLE_GRACE, MAX_PRESENTATION_EVENTS,
        MAX_TARGET_LEAD, PacingGate, PresentationInbox, PresentationOutcome, clamp_presented_at,
        pacing_work, presentation_instant, presented_event_instant,
        presented_qpc_from_frame_statistics, qpc_instant, refresh_period_qpc,
    };
    use crate::{render::RenderStats, window::FrameIndex};

    fn index(value: u32) -> FrameIndex {
        FrameIndex::from_raw(value)
    }

    fn stats(draw_calls: u32) -> RenderStats {
        RenderStats {
            draw_calls,
            instances: draw_calls * 10,
            geometry_uploaded_bytes: 0,
        }
    }

    /// display link 需要跑的四个理由一个都不能少（连续帧 / 有未取走的帧请求 / 有在飞帧 /
    /// **还在等上屏证据**），否则没有 drawable、也没有下一个 tick。
    #[test]
    fn pacing_work_covers_every_reason_to_keep_running() {
        assert!(pacing_work(true, false, 0, false), "连续帧必须跑");
        assert!(pacing_work(false, true, 0, false), "有未取走的帧请求必须跑");
        assert!(pacing_work(false, false, 1, false), "有在飞帧必须跑");
        assert!(
            pacing_work(false, false, 0, true),
            "等上屏证据时不能暂停：tick 是证据的唯一来源"
        );
        assert!(!pacing_work(false, false, 0, false), "空闲：没有工作");
    }

    /// **R06-8**：空闲**不立即**暂停。最后一个工作时刻之后 `IDLE_GRACE` 内到达的孤立更新
    /// 直接命中运行态，**不得**产生 pause→resume 往返（旧实现每次孤立更新各一次，恢复时白付
    /// 一帧 display link 延迟）。
    #[test]
    fn isolated_updates_do_not_pause_the_display_link() {
        let start = Instant::now();
        let at = |millis: u64| start + Duration::from_millis(millis);
        let mut gate = PacingGate::new();
        // 从未有过工作：初始就是暂停（与 `display_link.setPaused(true)` 一致）。
        assert!(gate.update(false, at(0)), "从未工作过 ⇒ 暂停");

        // 一帧工作之后：16 ms、200 ms 的两次孤立更新都在滞回窗口内。
        assert!(!gate.update(true, at(100)), "有工作必须跑");
        assert!(
            !gate.update(false, at(116)),
            "滞回窗口内（{IDLE_GRACE:?}）不得暂停"
        );
        assert!(!gate.update(true, at(300)), "孤立更新必须直接出帧");
        assert!(!gate.update(false, at(316)));
        assert!(!gate.update(true, at(500)));
        assert_eq!(
            gate.pauses(),
            0,
            "3 次孤立更新之间不得出现 pause→resume 往返（旧实现 = 3 次，每次白付一帧）"
        );

        // 最后一个工作时刻 (500 ms) + IDLE_GRACE 之后才暂停，且只计一次。
        let idle_deadline = at(500) + IDLE_GRACE;
        assert!(
            !gate.update(false, idle_deadline - Duration::from_millis(1)),
            "滞回窗口的最后一毫秒仍然必须跑"
        );
        assert!(gate.update(false, idle_deadline), "窗口结束 ⇒ 暂停");
        assert_eq!(gate.pauses(), 1, "暂停转换只允许计一次");
        assert!(gate.update(false, at(2_000)), "已暂停就继续保持暂停");
        assert_eq!(gate.pauses(), 1, "保持暂停不得重复计数");

        // 新工作恢复；恢复后的下一轮仍然在滞回窗口内。
        assert!(!gate.update(true, at(2_100)), "新工作必须恢复 display link");
        assert!(!gate.update(false, at(2_116)), "恢复之后同样有滞回窗口");
        assert_eq!(gate.pauses(), 1, "恢复不是暂停，不计入 pauses");
    }

    /// 滞回状态机的单次裁决成本必须可忽略（每轮主线程都会跑一次；数值打日志供报告引用）。
    #[test]
    fn the_pacing_decision_costs_nanoseconds() {
        const ROUNDS: u32 = 1_000_000;
        let start = Instant::now();
        let mut gate = PacingGate::new();
        let began = Instant::now();
        for round in 0..ROUNDS {
            // 交替「工作 / 空闲」：强制走状态机的两条分支（不是被优化成常量的空转）。
            let work = round % 64 == 0;
            std::hint::black_box(
                gate.update(work, start + Duration::from_micros(u64::from(round))),
            );
        }
        let per_round = began.elapsed() / ROUNDS;
        eprintln!("pacing_gate: {per_round:?} / decision（{ROUNDS} 轮）");
        assert!(
            per_round < Duration::from_micros(1),
            "单次暂停裁决必须远低于 1 µs（实际 {per_round:?}）"
        );
    }

    /// 目标呈现时刻是 host time（秒）且**在未来**（display link 提前一个 vsync 给出）：换算成
    /// `Instant` 必须往后推；过去值往前推；垃圾值返回 `None`（调用方如实留空 `presented_at`）。
    #[test]
    fn the_target_presentation_time_is_converted_into_the_monotonic_clock() {
        let observed = Instant::now();
        let now_host = 500.0;
        let lead = presentation_instant(now_host + 0.008, observed, now_host).expect("instant");
        assert_eq!(
            lead.saturating_duration_since(observed),
            Duration::from_millis(8),
            "本 vsync 的目标时刻在未来：必须往后推 8 ms"
        );
        let late = presentation_instant(now_host - 0.010, observed, now_host).expect("instant");
        assert_eq!(
            observed.saturating_duration_since(late),
            Duration::from_millis(10),
            "已经过去的排定时刻：往前推 10 ms"
        );
        // 非法值（系统没给 / NaN / 时钟异常）：如实返回 None，不拿别的时刻顶替。
        assert!(presentation_instant(0.0, observed, now_host).is_none());
        assert!(presentation_instant(f64::NAN, observed, now_host).is_none());
        assert!(presentation_instant(f64::INFINITY, observed, now_host).is_none());
        assert!(
            presentation_instant(
                now_host + MAX_TARGET_LEAD.as_secs_f64() * 2.0,
                observed,
                now_host
            )
            .is_none(),
            "超过一个合理提前量的目标时刻是垃圾值"
        );
    }

    /// AR3 §6.3 / P2-12：tick 推断的上屏时刻只进 `presented_at`，
    /// - `latency`（HUD 的 `Latency`）仍是**发布 → GPU 完成**（三端同义，与 Windows/Linux 一致）；
    /// - `completed_at` 保持 GPU 工作完成时刻，不被上屏时刻覆盖。
    #[test]
    fn the_tick_inferred_presentation_fills_presented_at_but_not_latency() {
        let mut feedback = FeedbackQueue::new();
        let published_at = Instant::now() - Duration::from_millis(100);
        // 目标呈现时刻（tick 推断）：发布之后、GPU 完成之后的某一刻。
        let target = Instant::now() + Duration::from_millis(3);
        feedback.push(
            index(1),
            stats(1),
            Vec::new(),
            published_at,
            7,
            Some(target),
        );
        feedback.complete(index(1), None);
        assert_eq!(feedback.awaiting_tick(), Some(7), "在等更新的 tick");
        // 还没有更新的 tick：不发布（上屏证据缺失）。
        let mut published = Vec::new();
        feedback.pop_ready(index(1), |frame| published.push(frame));
        assert!(published.is_empty(), "没有上屏证据时不得发布");
        feedback.observe_tick(8);
        assert_eq!(feedback.awaiting_tick(), None, "证据已到");
        feedback.pop_ready(index(1), |frame| published.push(frame));
        assert_eq!(published.len(), 1);
        // 契约（三端一致）：`latency` = 发布 → **GPU 完成**；上屏由 `presented_at` 承载。
        assert_eq!(
            published[0].latency,
            published[0]
                .completed_at
                .saturating_duration_since(published_at),
            "latency = 发布 → GPU 完成"
        );
        assert_eq!(
            published[0].presented_at,
            Some(target.max(published[0].completed_at)),
            "presented_at 是 tick 推断的上屏时刻（不小于 GPU 完成）"
        );
        assert!(
            published[0].presentation_is_consistent(),
            "共享契约：presented_at 不早于 completed_at"
        );
    }

    /// 上屏证据按 **tick 序号**判定：更新的 tick 只把**更早 tick** 的帧标记为已上屏，同一 tick 的
    /// 帧还要等下一个 tick。这是「上一帧已上屏」判定的全部逻辑（不依赖任何真实 `presentedTime`）。
    #[test]
    fn tick_evidence_marks_only_frames_from_older_ticks_as_presented() {
        let mut feedback = FeedbackQueue::new();
        let base = Instant::now() - Duration::from_millis(50);
        feedback.push(index(1), stats(1), Vec::new(), base, 10, Some(base));
        feedback.push(index(2), stats(2), Vec::new(), base, 11, Some(base));
        feedback.push(index(3), stats(3), Vec::new(), base, 11, Some(base));
        feedback.observe_tick(10);
        assert_eq!(feedback.awaiting_tick(), Some(10), "同一 tick 不算证据");
        feedback.observe_tick(11);
        assert_eq!(
            feedback.awaiting_tick(),
            Some(11),
            "tick 10 的帧已上屏；tick 11 的帧还在等下一个 tick"
        );
        assert!(feedback.submitted[0].presented);
        assert!(!feedback.submitted[1].presented);
        feedback.observe_tick(12);
        assert_eq!(feedback.awaiting_tick(), None, "全部拿到上屏证据");
    }

    /// 系统没给可用的目标呈现时刻（`targetPresentationTimestamp` 为 0 / 异常）：tick 推断值如实为
    /// `None`，**不**拿 GPU 完成时刻顶替；证据到达后照样发布，只是 `presented_at` 为空。
    #[test]
    fn an_unusable_target_time_publishes_a_none_presentation() {
        let mut feedback = FeedbackQueue::new();
        let published_at = Instant::now();
        feedback.push(index(1), stats(1), Vec::new(), published_at, 4, None);
        feedback.complete(index(1), None);
        feedback.observe_tick(5);
        let mut published = Vec::new();
        feedback.pop_ready(index(1), |frame| published.push(frame));
        assert_eq!(published.len(), 1);
        assert_eq!(
            published[0].presented_at, None,
            "拿不到可信上屏时刻就留空，不造假"
        );
        assert!(published[0].presentation_is_consistent(), "None 是合法值");
    }

    /// GPU 在排定时刻之后才画完（错过本 vsync）：推断的上屏时刻取 GPU 完成时刻作为下界，
    /// 否则会得出「像素比 GPU 工作更早上屏」的荒谬值（误差上界仍是 1 个 vsync）。
    #[test]
    fn an_early_target_is_clamped_to_the_gpu_completion() {
        let mut feedback = FeedbackQueue::new();
        let published_at = Instant::now() - Duration::from_millis(20);
        let early_target = published_at + Duration::from_millis(1);
        feedback.push(
            index(1),
            stats(1),
            Vec::new(),
            published_at,
            2,
            Some(early_target),
        );
        feedback.complete(index(1), None);
        feedback.observe_tick(3);
        let mut published = Vec::new();
        feedback.pop_ready(index(1), |frame| published.push(frame));
        assert_eq!(published.len(), 1);
        assert_eq!(
            published[0].presented_at,
            Some(published[0].completed_at),
            "排定时刻早于 GPU 完成时按完成时刻取下界"
        );
        assert!(published[0].presentation_is_consistent());
        // R5(a) 的措辞就是行为：`presented_at` 是**下界**（恒不早于完成），不是真实上屏时刻。
        assert_eq!(
            clamp_presented_at(Some(early_target), published[0].completed_at),
            Some(published[0].completed_at)
        );
    }

    /// 证据真的到达之前，反馈绝不发布（旧实现会在这里等 32 ms 然后用完成时刻兜底）：
    /// 只有 `observe_tick` 给出的更新 tick 才能放行，顺序保持（后面的帧也不能先出）。
    #[test]
    fn no_frame_is_published_without_tick_evidence() {
        let mut feedback = FeedbackQueue::new();
        let published_at = Instant::now() - Duration::from_millis(80);
        feedback.push(
            index(1),
            stats(1),
            Vec::new(),
            published_at,
            5,
            Some(published_at),
        );
        feedback.complete(index(1), None);
        assert_eq!(feedback.awaiting_tick(), Some(5), "在等更新的 tick");
        let mut published = Vec::new();
        feedback.pop_ready(index(1), |frame| published.push(frame));
        assert!(published.is_empty(), "等上屏证据期间不发布");
        assert_eq!(feedback.submitted.len(), 1);
        feedback.observe_tick(6);
        feedback.pop_ready(index(1), |frame| published.push(frame));
        assert_eq!(published.len(), 1);
        assert!(feedback.submitted.is_empty(), "发布后队列清空");
    }

    /// 队列为空 / 陌生 tick 到达时必须是无操作（不能 panic，也不能发明帧）。
    #[test]
    fn tick_evidence_without_frames_is_a_no_op() {
        let mut feedback = FeedbackQueue::new();
        feedback.observe_tick(99);
        assert!(feedback.submitted.is_empty());
        assert_eq!(feedback.awaiting_tick(), None);
    }

    /// 完成回调乱序到达：只要连续前缀都完成、且都拿到上屏证据 —— 发布顺序仍然只由提交顺序决定。
    #[test]
    fn out_of_order_completions_publish_in_submission_order() {
        let mut feedback = FeedbackQueue::new();
        let base = Instant::now();
        feedback.push(
            index(1),
            stats(1),
            Vec::new(),
            base - Duration::from_millis(100),
            21,
            Some(base),
        );
        feedback.push(index(2), stats(2), Vec::new(), base, 21, Some(base));
        feedback.push(index(3), stats(3), Vec::new(), base, 21, Some(base));
        // 第 2、3 帧先完成：连续前缀（第 1 帧）未到，不能发布。
        feedback.complete(index(2), None);
        feedback.complete(index(3), None);
        // 第 1 帧完成：`CompletionOrder` 的前沿一次推进到 3。
        feedback.complete(index(1), None);
        // 更新的 tick 到达：三帧都拿到上屏证据，按提交顺序发布 1、2、3。
        feedback.observe_tick(22);
        let mut published = Vec::new();
        feedback.pop_ready(index(3), |frame| published.push(frame));

        assert_eq!(
            published
                .iter()
                .map(|frame| frame.index.get())
                .collect::<Vec<_>>(),
            vec![1, 2, 3],
            "反馈按提交顺序发布"
        );
        assert_eq!(published[0].stats.draw_calls, 1);
        assert_eq!(published[1].stats.draw_calls, 2);
        assert_eq!(published[2].stats.draw_calls, 3);
        assert!(
            published[0].latency >= Duration::from_millis(90),
            "第 1 帧的 latency 用自己的发布基准，不是更旧帧的完成时刻"
        );
        assert!(
            published[1].latency < Duration::from_millis(90),
            "第 2 帧不能被更旧帧的 latency 污染"
        );
        assert!(feedback.submitted.is_empty(), "发布后队列清空");
    }

    /// **R5b / AR5 P2-3**：没有 tick 的长遮挡场景下反馈队列必须有硬上限，超限丢**最旧**并计数；
    /// 被丢弃帧的完成回调晚到必须被忽略（不能 `debug_assert` 炸掉），健康路径的丢弃计数保持 0。
    #[test]
    fn the_feedback_queue_is_bounded_without_ticks_and_counts_drops() {
        let mut queue = FeedbackQueue::new();
        let now = Instant::now();
        let total = MAX_FEEDBACK_ROUNDS;
        for value in 0..total {
            queue.push(
                index(value + 1),
                stats(value + 1),
                Vec::new(),
                now,
                1,
                Some(now),
            );
        }
        assert_eq!(
            queue.submitted.len(),
            super::MAX_FEEDBACK_FRAMES,
            "长期不 tick 时队列必须有硬上限（旧实现无界增长）"
        );
        let dropped = total - super::MAX_FEEDBACK_FRAMES as u32;
        assert_eq!(queue.dropped, u64::from(dropped), "丢弃次数必须计数");
        assert_eq!(
            queue.abandoned.len(),
            dropped as usize,
            "未完成即被丢弃的帧要记下（完成回调晚到不 panic）"
        );
        // 丢的是最旧的，保留最新的。
        assert_eq!(
            queue.submitted.front().expect("front").index.get(),
            dropped + 1
        );
        assert_eq!(queue.submitted.back().expect("back").index.get(), total);
        // 被丢弃帧的完成回调晚到：按 FIFO 消费，不 panic、不复活条目。
        let first_abandoned = queue.abandoned.front().copied().expect("abandoned");
        queue.complete(first_abandoned, None);
        assert_eq!(queue.abandoned.len(), dropped as usize - 1);

        // 健康路径（有 tick）：上限不触发、丢弃计数不增长、发布语义不变。
        let mut healthy = FeedbackQueue::new();
        healthy.push(index(1), stats(1), Vec::new(), now, 1, Some(now));
        healthy.complete(index(1), None);
        healthy.observe_tick(2);
        assert_eq!(healthy.dropped, 0);
        let mut published = Vec::new();
        healthy.pop_ready(index(1), |frame| published.push(frame));
        assert_eq!(published.len(), 1);
    }

    /// 触发上限所需的提交数（`4 ×` 上限，足以让「丢最旧」与计数都可观测）。
    const MAX_FEEDBACK_ROUNDS: u32 = (super::MAX_FEEDBACK_FRAMES as u32) * 4;

    // ───────────────────── Windows：QPC 映射 / 帧统计解析（A4） ─────────────────────

    /// QPC 锚点线性映射：锚点前后都支持；频率非法 / 超范围 / 映射到未来 ⇒ `None`。
    #[test]
    fn the_qpc_counter_is_mapped_onto_the_monotonic_clock() {
        const FREQUENCY: i64 = 10_000_000;
        let anchor = Instant::now() - Duration::from_millis(100);
        let now = Instant::now();
        let anchor_qpc = 1_000_000_000;
        // 锚点之后 16.667 ms（一个 60 Hz vblank）→ Instant 往后推。
        let ahead = qpc_instant(
            anchor_qpc + FREQUENCY / 60,
            anchor_qpc,
            anchor,
            now,
            FREQUENCY,
        )
        .expect("qpc ahead");
        assert_eq!(
            ahead.saturating_duration_since(anchor),
            Duration::from_nanos(16_666_600),
            "一个 60 Hz vblank 必须映射成 16.6666 ms"
        );
        // 锚点之前 5 ms（窗口里的呈现事件可能比取锚点更早）→ 往前推。
        let behind = qpc_instant(
            anchor_qpc - FREQUENCY / 200,
            anchor_qpc,
            anchor,
            now,
            FREQUENCY,
        )
        .expect("qpc behind");
        assert_eq!(
            anchor.saturating_duration_since(behind),
            Duration::from_millis(5)
        );
        // 垃圾值：频率 0、超出 ±1 s、i64 回绕、映射到采样时刻之后。
        assert!(qpc_instant(anchor_qpc, anchor_qpc, anchor, now, 0).is_none());
        assert!(
            qpc_instant(
                anchor_qpc + i64::MAX / 2,
                anchor_qpc,
                anchor,
                now,
                FREQUENCY
            )
            .is_none()
        );
        assert!(qpc_instant(i64::MIN, anchor_qpc, anchor, now, FREQUENCY).is_none());
        // 证据与采样时刻相距超过 1 s（锚点再旧也无妨，关键是结果要离 now 足够近）。
        assert!(
            qpc_instant(
                anchor_qpc,
                anchor_qpc,
                now - Duration::from_secs(2),
                now,
                FREQUENCY
            )
            .is_none(),
            "离采样时刻 2 s 的「上屏证据」不是本拍的统计"
        );
        let future = qpc_instant(
            anchor_qpc + FREQUENCY / 10,
            anchor_qpc,
            now - Duration::from_millis(50),
            now,
            FREQUENCY,
        );
        assert!(future.is_none(), "映射到采样时刻之后的呈现时刻不可信");
    }

    /// **RA-1 回归防线**：`OnceLock` 锚点只在进程内取一次，进程运行任意久之后仍必须能映射。
    ///
    /// 回归：曾经在 `qpc_instant` 里加「`|presented_qpc − anchor_qpc| ≤ 10 s`」的绝对上限，
    /// 进程跑约 10 秒后该判据恒真 ⇒ Windows `presented_at` 永久 `None`（20 s acceptance
    /// 有值率只有 38.64%）。线性映射与锚点距离无关，合法性只看「不晚于 now 且离 now ≤1 s」。
    #[test]
    fn an_anchor_sixty_seconds_old_still_maps_onto_the_monotonic_clock() {
        const FREQUENCY: i64 = 10_000_000;
        let now = Instant::now();
        // 锚点在 60 s 前；本帧的 vblank 在 5 ms 前 ⇒ 结果必须映射成功且与锚点相距 60 s。
        let anchor = now - Duration::from_secs(60);
        let anchor_qpc = 1_000_000_000;
        let instant = qpc_instant(
            anchor_qpc + FREQUENCY * 60 - FREQUENCY / 200,
            anchor_qpc,
            anchor,
            now,
            FREQUENCY,
        )
        .expect("锚点早 60 s 仍必须能映射（RA-1 回归）");
        assert_eq!(
            instant.saturating_duration_since(anchor),
            Duration::from_secs(59) + Duration::from_millis(995)
        );
        assert!(instant <= now);
        // 换成 10 分钟后同样成立（绝对距离不参与判据）。
        let anchor = now - Duration::from_secs(600);
        let instant = qpc_instant(
            anchor_qpc + FREQUENCY * 600 - FREQUENCY / 500,
            anchor_qpc,
            anchor,
            now,
            FREQUENCY,
        )
        .expect("锚点早 10 min 仍必须能映射");
        assert_eq!(
            instant.saturating_duration_since(anchor),
            Duration::from_secs(599) + Duration::from_millis(998)
        );
    }

    /// 刷新周期只能从相邻采样实测（`DXGI_FRAME_STATISTICS` 没有 `SyncRefreshPeriod` 字段）。
    #[test]
    fn the_refresh_period_is_derived_from_consecutive_frame_statistics() {
        const FREQUENCY: i64 = 10_000_000;
        let period = FREQUENCY / 60;
        assert_eq!(
            refresh_period_qpc((100, 1_000_000), (101, 1_000_000 + period), FREQUENCY),
            Some(period)
        );
        // 计数不前进 / QPC 回退 / 周期超出 [1 ms, 100 ms] ⇒ None（不可信采样）。
        assert!(refresh_period_qpc((100, 1_000_000), (100, 1_016_667), FREQUENCY).is_none());
        assert!(refresh_period_qpc((100, 1_016_667), (101, 1_000_000), FREQUENCY).is_none());
        assert!(refresh_period_qpc((100, 0), (101, 1), FREQUENCY).is_none());
        assert!(
            refresh_period_qpc((100, 0), (101, FREQUENCY / 5 + 1), FREQUENCY).is_none(),
            "5 Hz 不在合理刷新范围内"
        );
    }

    /// `PresentRefreshCount`/`SyncRefreshCount` → 最近一次已上屏内容的 QPC 时刻。
    #[test]
    fn frame_statistics_yield_the_presentation_lower_bound() {
        const PERIOD: i64 = 166_667;
        // 刚在本次 sync 上屏：呈现时刻就是 SyncQPCTime（不需要周期）。
        assert_eq!(
            presented_qpc_from_frame_statistics(500, 500, 9_000_000, None),
            Some(9_000_000)
        );
        // 上屏发生在 2 个 vblank 之前：SyncQPCTime − 2 × period。
        assert_eq!(
            presented_qpc_from_frame_statistics(500, 498, 9_000_000, Some(PERIOD)),
            Some(9_000_000 - 2 * PERIOD)
        );
        // 计数不一致 / 缺周期 / 周期 0 / 减法下溢 ⇒ None（绝不猜）。
        assert!(presented_qpc_from_frame_statistics(498, 500, 9_000_000, None).is_none());
        assert!(presented_qpc_from_frame_statistics(500, 498, 9_000_000, None).is_none());
        assert!(presented_qpc_from_frame_statistics(500, 498, 9_000_000, Some(0)).is_none());
        assert!(
            presented_qpc_from_frame_statistics(500, 498, i64::MIN + 1, Some(PERIOD)).is_none()
        );
        // 回绕后结果非正 ⇒ None。
        assert!(presented_qpc_from_frame_statistics(2, 0, 1, Some(PERIOD)).is_none());
    }

    // ───────────────────── Linux：wp_presentation 呈现时钟（A4） ─────────────────────

    /// 呈现时间戳（`(tv_sec_hi, tv_sec_lo, tv_nsec)` + 时钟域）→ `Instant`：合理值换算、
    /// 垃圾值 / 未知时钟域 / 未来值 ⇒ `None`。
    #[test]
    fn the_presentation_clock_is_mapped_onto_the_monotonic_clock() {
        let anchor = Instant::now() - Duration::from_millis(100);
        let now = Instant::now();
        let clock_anchor = 1_000.5_f64;
        // 1000.6 s（CLOCK_MONOTONIC）比锚点晚 100 ms。
        let instant = presented_event_instant(
            0,
            1_000,
            600_000_000,
            CLOCK_MONOTONIC,
            clock_anchor,
            anchor,
            now,
        )
        .expect("presented instant");
        assert_eq!(
            instant.saturating_duration_since(anchor),
            Duration::from_millis(100)
        );
        // 锚点之前 50 ms 也能换算（事件到达与取锚点不是同一纳秒）。
        let earlier = presented_event_instant(
            0,
            1_000,
            450_000_000,
            CLOCK_REALTIME,
            clock_anchor,
            anchor,
            now,
        )
        .expect("presented instant before anchor");
        assert_eq!(
            anchor.saturating_duration_since(earlier),
            Duration::from_millis(50)
        );
        // tv_sec_hi/lo 拼成 64 位秒数。
        let high = presented_event_instant(1, 0, 0, CLOCK_MONOTONIC, 4_294_967_296.0, anchor, now)
            .expect("hi/lo seconds");
        assert_eq!(high, anchor);
        // 垃圾值：未知时钟域 / 纳秒越界 / 锚点非有限 / 与锚点相差超过 1 s / 未来时间戳。
        for (clock, nsec, clock_anchor, secs) in [
            (4, 0, 1_000.5, 1_000_u32), // CLOCK_MONOTONIC_RAW
            (CLOCK_MONOTONIC, 1_000_000_000, 1_000.5, 1_000),
            (CLOCK_MONOTONIC, 0, f64::NAN, 1_000),
            (CLOCK_MONOTONIC, 0, 1_000.5, 5_000),
        ] {
            assert!(
                presented_event_instant(0, secs, nsec, clock, clock_anchor, anchor, now).is_none(),
                "clock={clock} nsec={nsec} anchor={clock_anchor} secs={secs} 必须是 None"
            );
        }
        assert!(
            presented_event_instant(
                0,
                1_000,
                900_000_000,
                CLOCK_MONOTONIC,
                1_000.5,
                now - Duration::from_millis(100),
                now
            )
            .is_none(),
            "晚于采样时刻的呈现时间戳不可信"
        );
    }

    // ───────────────────── Linux：呈现反馈邮箱（A4） ─────────────────────

    /// 反馈邮箱：乱序到达按 FrameIndex 取用；discarded 是终态；重复事件先到者为准。
    #[test]
    fn the_presentation_inbox_maps_out_of_order_feedback_by_frame() {
        let inbox = PresentationInbox::new();
        let presented = Instant::now() - Duration::from_millis(8);
        // 乱序：先到 3，再到 1；2 被丢弃。
        inbox.note(index(3), PresentationOutcome::Presented(presented));
        inbox.note(
            index(1),
            PresentationOutcome::Presented(presented - Duration::from_millis(1)),
        );
        inbox.note(index(2), PresentationOutcome::Discarded);
        assert_eq!(inbox.pending(), 3);
        assert_eq!(
            inbox.take(index(1)),
            Some(PresentationOutcome::Presented(
                presented - Duration::from_millis(1)
            ))
        );
        assert_eq!(inbox.take(index(2)), Some(PresentationOutcome::Discarded));
        assert_eq!(
            inbox.take(index(3)),
            Some(PresentationOutcome::Presented(presented))
        );
        assert_eq!(inbox.take(index(3)), None, "终态只取一次");
        // 重复事件按先到者为准（协议保证一条反馈只发一次终态）。
        inbox.note(index(4), PresentationOutcome::Presented(presented));
        inbox.note(index(4), PresentationOutcome::Discarded);
        assert_eq!(
            inbox.take(index(4)),
            Some(PresentationOutcome::Presented(presented))
        );
    }

    /// 容量上限兜底：反馈消费端停了不能无界增长；丢最旧并计数。
    #[test]
    fn the_presentation_inbox_is_bounded_and_counts_drops() {
        let inbox = PresentationInbox::new();
        let presented = Instant::now();
        for value in 0..(MAX_PRESENTATION_EVENTS as u32 + 4) {
            inbox.note(index(value + 1), PresentationOutcome::Presented(presented));
        }
        assert_eq!(inbox.pending(), MAX_PRESENTATION_EVENTS);
        assert_eq!(inbox.dropped(), 4);
        assert_eq!(inbox.take(index(1)), None, "最旧的被丢弃");
        assert_eq!(
            inbox.take(index(5)),
            Some(PresentationOutcome::Presented(presented)),
            "丢的是最旧的 4 条，保留 5..=20"
        );
        // 正常路径：不丢、计数为 0。
        let healthy = PresentationInbox::new();
        healthy.note(index(1), PresentationOutcome::Discarded);
        assert_eq!(healthy.pending(), 1);
        assert_eq!(healthy.dropped(), 0);
    }

    /// 注册的 waker 在**每次**事件写入后都会被调用一次（GPU 线程靠它从 condvar 醒来）。
    #[test]
    fn the_presentation_inbox_wakes_the_gpu_thread() {
        use std::sync::{
            Arc,
            atomic::{AtomicU32, Ordering},
        };
        let inbox = PresentationInbox::new();
        let wakes = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&wakes);
        inbox.set_waker(Arc::new(move || {
            counter.fetch_add(1, Ordering::AcqRel);
        }));
        inbox.note(index(1), PresentationOutcome::Discarded);
        inbox.note(index(2), PresentationOutcome::Presented(Instant::now()));
        assert_eq!(wakes.load(Ordering::Acquire), 2);
    }

    /// 呈现时钟判据的**双向**边界：早于锚点 1 s 内接受（事件在锚点之前生成，正常路径），
    /// 更早 / 更晚 / 未来时间戳一律拒绝 —— 时钟域选错或 NTP 步进时绝不把假时刻写进证据。
    #[test]
    fn the_presentation_clock_rejects_timestamps_outside_the_plausible_window() {
        let anchor = Instant::now();
        let now = anchor;
        let clock_anchor = 2_000.0_f64;
        // 锚点前 1 s 恰好落在容差边界（`> 1 s` 才拒绝）⇒ 接受。
        assert_eq!(
            presented_event_instant(0, 1_999, 0, CLOCK_MONOTONIC, clock_anchor, anchor, now),
            Some(anchor - Duration::from_secs(1)),
            "边界内（-1 s）必须接受"
        );
        // 两个方向超出 1 s 容差 ⇒ 拒绝。
        for secs in [1_998_u32, 2_002] {
            assert!(
                presented_event_instant(0, secs, 0, CLOCK_MONOTONIC, clock_anchor, anchor, now)
                    .is_none(),
                "secs={secs} 距锚点超过 1 s 必须拒绝"
            );
        }
        // 纳秒边界：999_999_999 合法，1_000_000_000 越界。
        assert!(
            presented_event_instant(
                0,
                1_999,
                999_999_999,
                CLOCK_MONOTONIC,
                clock_anchor,
                anchor,
                now
            )
            .is_some(),
            "999_999_999 ns 合法"
        );
        assert!(
            presented_event_instant(
                0,
                1_999,
                1_000_000_000,
                CLOCK_MONOTONIC,
                clock_anchor,
                anchor,
                now
            )
            .is_none(),
            "nsec ≥ 1e9 非法"
        );
        // 晚于采样时刻（未来）不可信。
        assert!(
            presented_event_instant(
                0,
                2_000,
                500_000_000,
                CLOCK_MONOTONIC,
                clock_anchor,
                anchor,
                now
            )
            .is_none(),
            "晚于采样时刻的呈现时间戳不可信"
        );
    }

    /// A4 Linux 回填的**下界收口**（`Window::note_frame_presented` 内调 `clamp_presented_at`）：
    /// compositor 的事件时间戳可能早于 GPU 完成时刻（时钟步进 / 映射误差），而上屏不可能早于
    /// 内容画完 ⇒ 回填值必须抬到 `completed_at`。变异「去掉 clamp 调用」会让本用例红。
    #[test]
    fn a_presentation_timestamp_older_than_the_gpu_completion_is_clamped() {
        use crate::window::{FramePresentation, Window, WindowId, WindowOptions};
        let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
        let published = Instant::now();
        let completed = published + Duration::from_millis(2);
        let frame = index(1);
        window.note_frame_presentation(FramePresentation {
            index: frame,
            published_at: published,
            completed_at: completed,
            presented_at: None,
            latency: Duration::from_millis(2),
        });
        // 事件时间戳比 GPU 完成早 5 ms：下界收口到 completed_at（变异去掉 clamp ⇒ 这里只剩
        // 事件时间戳，断言红）。
        let early = completed
            .checked_sub(Duration::from_millis(5))
            .expect("测试时间戳");
        window.note_frame_presented(frame, Some(early));
        assert_eq!(
            window
                .last_frame_presentation()
                .expect("本帧还在历史里")
                .presented_at,
            Some(completed),
            "presented 不得早于 GPU 完成（下界收口）"
        );
        // 正常路径（事件晚于完成）：保留事件时间戳。
        let later = completed + Duration::from_millis(3);
        window.note_frame_presented(frame, Some(later));
        assert_eq!(
            window
                .last_frame_presentation()
                .expect("本帧还在历史里")
                .presented_at,
            Some(later)
        );
        // `discarded` / 不可信：如实 None（不能用完成时刻顶替）。
        window.note_frame_presented(frame, None);
        assert_eq!(
            window
                .last_frame_presentation()
                .expect("本帧还在历史里")
                .presented_at,
            None
        );
    }
}

//! 消息泵：`run`（建窗 → 挂渲染器 → 显示）、每轮固定步骤、唤醒与帧驱动、帧节拍。
//!
//! ## 帧节奏（R07-10 / A2 / A5）
//!
//! 连续帧（**动画**）与「被帧预算门禁推迟的帧」由 [`FramePacer`] 驱动：一条**只在需要时存在**
//! 的节拍线程按窗口所在 output 的刷新周期唤醒一次泵。A2 删除了进程级合成器时钟（旧实现阻塞在
//! `DCompositionWaitForCompositorClock` 上等一拍才编码 —— 端到端固定多约 1 个 vsync）：
//! present 节奏由 GPU 线程的 DXGI waitable 门禁承担，UI 侧只需要按刷新率推进动画与补帧。
//!
//! 为什么不用 `WM_TIMER`：`WM_TIMER` 是**最低优先级**消息，输入密集时被饿（1000 Hz 鼠标下
//! 动画反而更抖）；且 `SetTimer` 的最小粒度受系统定时器分辨率限制（默认 15.6 ms）。`WM_TIMER`
//! 现在只保留给 a11y 补发兜底（[`A11Y_TIMER_ID`]），空闲时没有定时器。
//!
//! 为什么需要一条独立线程：节拍线程要 `sleep` 到下一拍，而 UI 线程是唯一的消息泵 —— 把它放进
//! 泵里就等于暂停输入（Chrome 同样是独立合成线程等节拍、UI 线程消费消息）。

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use windows::Win32::{
    Foundation::HWND,
    System::SystemInformation::GetTickCount64,
    UI::{
        Input::KeyboardAndMouse::{GetCapture, GetDoubleClickTime, ReleaseCapture, SetCapture},
        WindowsAndMessaging::*,
    },
};

use super::native_window::{apply_display_recovery, collect_events, sync_native_windows};
use crate::{
    app::App,
    error::EngineError,
    platform::{
        dispatcher::Dispatcher,
        windows::{composition, dpi_awareness, window::NativeWindow},
    },
    render::backend::Renderer,
    window::{Window, WindowId, WindowOptions},
};

/// 消费者：`wndproc` 的 `WM_TIMER` 分支与 `pump_loop` 的定时器管理。
/// a11y 补发兜底定时器 id（**唯一**保留的 `WM_TIMER` 用途；连续帧走节拍线程）。
pub(super) const A11Y_TIMER_ID: usize = 1;

/// a11y 补发兜底定时器的周期（毫秒）。
///
/// 与 `App` 侧的 `A11Y_THROTTLE`（100 ms）一致：被节流的一次交付最多等一个周期就必须重试，
/// 否则屏幕阅读器会永远收不到最后一笔更新。
const A11Y_RETRY_MS: u32 = 100;

/// 刷新率提示不可得时的节拍周期（60 Hz 兜底）。
///
/// 只影响**动画 tick 的频率**（内容帧率仍由每窗口 GPU 门禁决定）；`App` 侧的帧预算门禁在刷新率
/// 未知时保持禁用，因此不会用这个兜底值卡帧。
const FALLBACK_PACE: Duration = Duration::from_millis(16);

/// 节拍线程的唤醒周期上限（毫秒）：极低刷新率 / 未知刷新率时也不能睡死，同时是停止后的最长
/// 退出延迟（`shutdown` 的 join 不能长时间阻塞 UI 线程）。
const MAX_PACE_WAIT_MS: u64 = 100;

/// 停止检查分片（毫秒）：`sleep` 期间定期检查 stop 标志，保证 `shutdown` 能及时 join。
const PACE_STOP_SLICE_MS: u64 = 20;

/// 节拍线程的周期：窗口所在 output 的刷新率提示（A5/A3 的 per-output 来源）。
///
/// 提示不可得时退回 60 Hz：动画在未知刷新率的显示器上仍以可用频率前进。取**首窗**的提示
/// （多窗口同屏是常态；混合刷新率时每个窗口的帧预算仍按各自 output 独立节流，节拍线程只是
/// 唤醒泵的粗粒度定时器 —— A3 已接受 Windows 上动画节拍随首窗的限制并在报告登记）。
fn pace_interval(app: &App, windows: &[NativeWindow]) -> Duration {
    pace_interval_from(
        windows
            .first()
            .and_then(|native| app.window(native.id()))
            .and_then(Window::frame_refresh_hz),
    )
}

/// 刷新率提示 → 节拍周期（纯函数，可单测）：未知 ⇒ 60 Hz 兜底；周期上限 [`MAX_PACE_WAIT_MS`]。
fn pace_interval_from(refresh_hz: Option<f32>) -> Duration {
    refresh_hz.map_or(FALLBACK_PACE, |hz| {
        Duration::from_secs_f32(1.0 / hz).min(Duration::from_millis(MAX_PACE_WAIT_MS))
    })
}

/// 消费者：`run`。
/// 用系统设置初始化输入阈值（R02-15）。
///
/// 四个系统值**分开**读（R02-38）：双击矩形是 `SM_CXDOUBLECLK`/`SM_CYDOUBLECLK`，拖拽阈值是
/// `SM_CXDRAG`/`SM_CYDRAG`，两者是独立的用户设置。旧实现把 `SM_CXDRAG` 同时当作双击容差与拖拽
/// 阈值，于是「双击容差」被「拖拽阈值」绑死。
pub(super) fn install_input_thresholds(app: &mut App) {
    // SAFETY: 两个都是无副作用、无指针参数的系统查询。
    let (double_click, double_x, double_y, drag_x, drag_y) = unsafe {
        (
            GetDoubleClickTime(),
            GetSystemMetrics(SM_CXDOUBLECLK),
            GetSystemMetrics(SM_CYDOUBLECLK),
            GetSystemMetrics(SM_CXDRAG),
            GetSystemMetrics(SM_CYDRAG),
        )
    };
    let defaults = crate::interaction::InputThresholds::platform_default();
    let double_click_time = if double_click == 0 {
        defaults.double_click_time
    } else {
        std::time::Duration::from_millis(u64::from(double_click))
    };
    app.set_input_thresholds(crate::interaction::InputThresholds {
        double_click_time,
        double_click_distance: positive(double_x.max(double_y))
            .unwrap_or(defaults.double_click_distance),
        drag_distance: positive(drag_x.max(drag_y)).unwrap_or(defaults.drag_distance),
    });
}

/// 系统度量值 → 正数阈值（`0` / 负数（未设置）时返回 `None`，调用方保留平台缺省）。
fn positive(value: i32) -> Option<f32> {
    (value > 0).then_some(value as f32)
}

/// 一格滚轮的行数（`SPI_GETWHEELSCROLLLINES`，R02-10）。
///
/// 返回 `0.0` 表示「系统值不可用或按页滚动」（`WHEEL_PAGESCROLL = UINT_MAX`）：引擎无法用行
/// 表达一页，交给 [`crate::platform::event::lines_per_tick`] 回退到 Win32 缺省的 3 行。
pub(super) fn wheel_lines_from_system() -> f32 {
    let mut lines: u32 = 3;
    // SAFETY: `pvparam` 指向栈上的 `u32`；`SPI_GETWHEELSCROLLLINES` 只写这个值，不保留指针。
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWHEELSCROLLLINES,
            0,
            Some((&raw mut lines).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if ok.is_err() || lines == u32::MAX {
        return 0.0;
    }
    lines as f32
}

/// 当前消息的时间（`GetMessageTime()`，毫秒，与 `GetTickCount` 同源、**32 位回绕**）。
///
/// **必须**在处理消息期间调用（系统记录的是「最近一次 `GetMessage`/`PeekMessage` 取到的
/// 消息」的时间），正是 `WndProc` 所处的时机。消息循环之外调用拿到的是**上一条已取出消息**
/// 的时间（陈旧值）：合成事件必须用 [`tick_now`]，不能用本函数。
pub(super) fn message_ticks() -> u32 {
    // SAFETY: 无参数、无线程副作用的查询。
    unsafe { GetMessageTime() as u32 }
}

/// 当前单调 tick（`GetTickCount64` 截成 **32 位**，与 `MSG.time` 同源、同宽、同回绕）。
///
/// R02-14：公开事件时间戳的换算是 `timestamp_from_ticks_32(now, tick_now(), MSG.time)`。
/// 「现在」必须取**同一来源同一宽度**的 tick：
///
/// - 旧实现用 `QueryPerformanceCounter` 的毫秒当「现在 tick」—— QPC 的原点与宽度都和
///   `GetTickCount` 不同，所有时间戳被系统性平移 K ms（`Instant::now() - event.timestamp()`
///   的延迟统计全错），跨 API 不可比；
/// - `MSG.time` 是 32 位、49.7 天回绕。截成 32 位后 `wrapping_sub` 在回绕两侧仍得到正确
///   差值（见 `timestamp_from_ticks_32` 的单测），长跑（交易终端常态）不再静默退化。
///
/// 引擎合成的平台事件（`PointerEnter` / `PointerCancel`）也用它：它们可能发生在消息循环
/// **之外**（泵在 `take_capture_requests` 里调 `SetCapture`/`ReleaseCapture`），此时
/// `GetMessageTime()` 是上一条消息的陈旧时间。
pub(super) fn tick_now() -> u32 {
    // SAFETY: 无参数、无副作用的计时器查询（64 位值截断到 32 位与 `MSG.time` 同域）。
    unsafe { GetTickCount64() as u32 }
}

/// 消费者：`pump_loop` 与 `wndproc` 的 `WM_SETCURSOR`。
/// 引擎光标 → 系统光标。`Default` 交给类光标（`IDC_ARROW`），不调 `SetCursor`。
pub(super) fn cursor_handle(cursor: crate::paint::style::Cursor) -> HCURSOR {
    use windows::Win32::UI::WindowsAndMessaging::{
        IDC_HAND, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENS, IDC_SIZEWE,
    };

    use crate::paint::style::Cursor;

    let id = match cursor {
        Cursor::Default => IDC_ARROW,
        Cursor::Pointer => IDC_HAND,
        Cursor::Text => IDC_IBEAM,
        Cursor::NotAllowed => IDC_NO,
        Cursor::Grab | Cursor::Grabbing => IDC_SIZEALL,
        Cursor::ResizeHorizontal => IDC_SIZEWE,
        Cursor::ResizeVertical => IDC_SIZENS,
    };
    // SAFETY: `LoadCursorW` 取共享的系统光标句柄（不拥有、不释放）；`id` 是本函数的静态常量。
    unsafe { LoadCursorW(None, id) }.unwrap_or_default()
}

/// 消费者：`pump_loop` 的 ②c 与 `plan_release_capture`。
/// 引擎**主动**释放指针捕获（拖拽结束）：只有本窗口确实持有捕获时才置
/// `suppress_capture_cancel`，并告诉调用方需要在不持借用时执行 `ReleaseCapture`。
///
/// 两处细节都不能省：
/// - 自己调 `ReleaseCapture` 也会收到 `WM_CAPTURECHANGED`，不置位就会在每次松手后误投
///   `PointerCancel`（复审 F1）；
/// - 不持有捕获时 `ReleaseCapture` 不会产生该消息，若仍然置位，残留的 suppress 会把
///   **下一次真实**的会话丢失当成「自己释放」而吞掉取消（复审 F2）。
///
/// **本函数只做记账，不调用任何 Win32 API**（AR3 §1.2 P1）：`ReleaseCapture` 会**同步**派发
/// `WM_CAPTURECHANGED`、重入生产 `WndProc`，而重入路径会从 `GWLP_USERDATA` 再构造一个
/// `&mut WndProcState` —— 在持有 `&mut WndProcState` 时调用它就是别名 UB（与 R07-7 修 IMM32
/// 时同一条纪律）。调用方必须把返回的 `true` 变成动作、**离开借用作用域后**再执行。
///
/// 返回 `true` = 已置 suppress、需要调用 `ReleaseCapture()`。
pub(super) fn plan_release_capture(hwnd: HWND, state: &mut super::WndProcState) -> bool {
    // SAFETY: `GetCapture` 只读查询当前线程的捕获窗口（不派发消息）。
    if unsafe { GetCapture() } != hwnd {
        return false;
    }
    state.suppress_capture_cancel = true;
    true
}

/// 消费者：`pump_loop` 的 ②c。
/// ②c 收集到的「会同步重入 `WndProc` 的本地动作」——**两阶段执行**（R07-7 / AR3 §1.2）。
///
/// `ReleaseCapture` 会同步派发 `WM_CAPTURECHANGED`；`SetCapture` 会把同一消息派给**失去**捕获
/// 的窗口（也可能是本窗口）；`SetCursor` 也可能引发 `WM_SETCURSOR`。重入的 `WndProc` 会再构造
/// 一次 `&mut WndProcState`，所以这些调用既不能落在 `&mut WndProcState` 借用里，也不该落在
/// `&mut NativeWindow` 借用里：先在短借用里只做记账、把动作收集起来，离开借用后统一执行
/// （[`apply_interaction_actions`]）。
enum InteractionAction {
    /// 本窗口的引擎光标 → 系统光标。
    SetCursor(HCURSOR),
    /// 让目标窗口取得指针捕获。
    SetCapture(HWND),
    /// 释放本窗口持有的指针捕获（suppress 已在计划阶段置位）。
    ReleaseCapture(HWND),
}

/// 执行 [`InteractionAction`]（**调用方不得持有任何 `WndProcState` / `NativeWindow` 借用**）。
fn apply_interaction_actions(actions: Vec<InteractionAction>) {
    for action in actions {
        match action {
            InteractionAction::SetCursor(cursor) => {
                // SAFETY: 系统光标句柄由 `LoadCursorW` 借用，进程不拥有；`SetCursor` 只影响本线程。
                unsafe { SetCursor(Some(cursor)) };
            }
            InteractionAction::SetCapture(hwnd) => {
                // SAFETY: `hwnd` 是本进程创建的窗口；捕获在拖拽结束/释放/失焦时释放。
                unsafe { SetCapture(hwnd) };
            }
            InteractionAction::ReleaseCapture(hwnd) => {
                debug_assert_eq!(
                    // SAFETY: 只读查询当前线程的捕获窗口（不派发消息）。
                    unsafe { GetCapture() },
                    hwnd,
                    "计划与执行之间捕获窗口变了：suppress / 释放的目标会错位"
                );
                // SAFETY: `hwnd` 由计划阶段确认持有捕获；只释放本线程的捕获，同线程调用会同步
                // 派发 `WM_CAPTURECHANGED`（这正是它必须在这里、而不是在借用里执行的原因）。
                let _ = unsafe { ReleaseCapture() };
            }
        }
    }
}

/// 帧节拍线程（R07-10 / A5）。
///
/// 生命周期跟随「`App::needs_continuous_frames()`（动画）**或**有被帧预算门禁推迟的帧」：
/// 需要时起、不需要时停 —— 空闲时**没有线程、没有定时器、没有周期唤醒**（I3）。
/// A2 之前它阻塞在 `DCompositionWaitForCompositorClock` 上；现在按窗口 refresh 周期 sleep，
/// present 对齐由 GPU 线程的 DXGI waitable 负责。
pub(super) struct FramePacer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// 启动线程时用的刷新周期：周期变了（换屏 / 改刷新率）就重启线程。
    interval: Duration,
    /// 节拍线程最近一次唤醒是「tick」而不是普通 wake：泵用它决定本帧走 force 通道。
    ///
    /// 为什么 tick 要 force（A5 对齐）：数据泵以与刷新率相同的频率写状态时，预算门禁的
    /// `next_allowed` 是「上一帧起点 + 1/refresh」，而数据泵/节拍线程的相位独立 ⇒ 总有请求
    /// 落在 `next_allowed` 前几百微秒，被推迟到**下一个节拍**（多等一整拍，实测 60 Hz 写入
    /// 掉到 ~40 fps）。节拍线程的 tick 本身就是「这一拍该出帧」的信号（与 mac display link
    /// tick 同义），走 force 既保证每拍一帧、又不会超过 refresh 节拍（节拍线程只在
    /// 动画/有推迟帧时存在）。普通 wake（输入/行情写入）仍走预算门禁。
    tick: Arc<AtomicBool>,
}

impl FramePacer {
    pub(super) fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
            interval: FALLBACK_PACE,
            tick: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 启动/停止/改周期（幂等）。
    pub(super) fn sync(&mut self, active: bool, interval: Duration, dispatcher: &Arc<Dispatcher>) {
        if !active {
            self.shutdown();
            return;
        }
        if self.thread.is_some() && self.interval == interval {
            return;
        }
        self.shutdown();
        let stop = Arc::new(AtomicBool::new(false));
        let tick = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_tick = Arc::clone(&tick);
        let thread_dispatcher = Arc::clone(dispatcher);
        self.thread = Some(std::thread::spawn(move || {
            pace(thread_stop, thread_tick, interval, thread_dispatcher);
        }));
        self.stop = stop;
        self.tick = tick;
        self.interval = interval;
    }

    /// 取走「本轮流唤醒来自节拍线程」的标记（泵在 ④ 的帧门禁里用它选择 force 通道）。
    pub(super) fn take_tick(&self) -> bool {
        self.tick.swap(false, Ordering::AcqRel)
    }

    /// 停止并 join（`Drop` 也会调）。
    pub(super) fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // 等待上限是 `PACE_STOP_SLICE_MS`：不能因为线程还在 sleep 就跳过 join
            //（跳过会让节拍线程活过 `Dispatcher`）。
            let _ = thread.join();
        }
        self.interval = FALLBACK_PACE;
        self.tick.store(false, Ordering::Release);
    }
}

impl Default for FramePacer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for FramePacer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 消费者：`FramePacer::sync` 的线程体。
///
/// 每 `interval` 唤醒一次泵（`Dispatcher::wake` 是电平信号，天然合并）。被抢占 / 系统睡眠恢复
/// 之后**重新对齐相位**而不是补拍：补拍会在恢复瞬间产生一串连续唤醒（忙循环），而少掉的动画
/// tick 只意味着动画按真实时间前进（`tick_animations` 用真实时钟），不会卡住。
fn pace(
    stop: Arc<AtomicBool>,
    tick: Arc<AtomicBool>,
    interval: Duration,
    dispatcher: Arc<Dispatcher>,
) {
    let interval = interval.clamp(
        Duration::from_millis(1),
        Duration::from_millis(MAX_PACE_WAIT_MS),
    );
    let slice = Duration::from_millis(PACE_STOP_SLICE_MS);
    let mut next = Instant::now() + interval;
    loop {
        // 分片睡眠：每片检查一次 stop，保证 `shutdown` 的 join 延迟 ≤ 一片。
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let now = Instant::now();
            if now >= next {
                break;
            }
            let remaining = next.saturating_duration_since(now);
            std::thread::sleep(remaining.min(slice));
        }
        if stop.load(Ordering::Acquire) {
            return;
        }
        // 落后超过一个周期（被抢占 / 唤醒恢复）⇒ 从当前时刻重新对齐，不补拍。
        let now = Instant::now();
        next += interval;
        if next <= now {
            next = now + interval;
        }
        // 先置 tick 再唤醒：泵在 ④ 读到标记时本轮的 wake 位一定已置起（Release/Acquire 配对）。
        tick.store(true, Ordering::Release);
        dispatcher.wake();
    }
}

/// 消费者：`platform::windows` 的 `pub(crate) use event_loop::run`。
/// 引擎自有事件循环：建窗口 → 挂渲染器 → 显示 → 泵 → 关闭。
pub(crate) fn run(mut app: App) -> Result<(), EngineError> {
    // 发布基线：只支持 Windows 11（build 22000+）。A2 删除了 dcomp 合成器时钟之后启动路径
    // 不再有 GetProcAddress 解析失败面，但 Win11 仍是产品支持基线（DirectComposition /
    // 窗口行为假设只在 Win11 上验证）；版本门禁保留在启动期，早失败好过运行期行为异常。
    crate::platform::windows::version::require_windows11()?;
    // R07-1：进程清单（`ui_core.manifest.xml`）声明 PerMonitorV2，启动再**校验实际值** ——
    // 清单可能被宿主忽略；不是 per-monitor v2 就带诊断失败，绝不静默按 96 dpi 排版。
    dpi_awareness::install()?;
    install_input_thresholds(&mut app);
    let dispatcher = app.dispatcher();
    // 线程消息兜底的目标：首窗关闭后 `PostMessageW` 会失败，`wake()` 必须还能叫醒 `GetMessageW`。
    // SAFETY: 无参数，返回调用线程的 id（只用于 `PostThreadMessageW`）。
    dispatcher.bind_thread(unsafe { windows::Win32::System::Threading::GetCurrentThreadId() });

    let requests: Vec<(WindowId, WindowOptions)> = app
        .windows()
        .map(|(id, window)| (id, window.options().clone()))
        .collect();
    // `App` 交给 `Rc<RefCell<_>>`：系统的 size / move 模态循环里，`WndProc` 要直接驱动一帧
    // 真实帧（见 `pump_loop` 的 ① 与 `ResizeFrameDriver`）。驱动的引用是 `Weak`，关闭顺序不受影响。
    let app = Rc::new(RefCell::new(app));
    let mut windows: Vec<NativeWindow> = Vec::with_capacity(requests.len());
    for (id, options) in requests {
        // a11y adapter 与引擎 `A11yBridge` 共享消费端代际（AT 重激活 ⇒ 下一笔全树，E1）。
        let a11y_epoch = app
            .borrow()
            .window(id)
            .map_or_else(crate::a11y::adapter::A11yConsumerEpoch::new, |window| {
                window.a11y_consumer_epoch()
            });
        let mut native =
            NativeWindow::create(id, &options, dispatcher.clone(), Some(&app), a11y_epoch)?;
        let client = native.client_size();
        let renderer = Renderer::new_for_hwnd(native.composition(), client, options.background)?;
        // A5：帧预算的刷新率提示（窗口所在 output；查询失败 ⇒ 门禁保持禁用）。
        let refresh_hz = native.refresh_hz();
        {
            let mut app = app.borrow_mut();
            if let Some(window) = app.window_mut(id) {
                window.set_renderer(renderer);
                window.set_scale(native.scale());
                window.set_size(native.logical_size());
                // 渲染目标就是真实客户区：逻辑尺寸取整后的 covering 值可能比它大 1 px。
                window.set_surface(client);
                if let Some(hz) = refresh_hz {
                    window.set_frame_refresh_hz(hz);
                }
            }
        }
        // 首帧已经由 `new_for_hwnd` prime 并 present（内容层与背景层都已 attach；客户区在第一次
        // present 之前完全透明），这里把几何对齐成「内容 = 客户区」（内容 1:1、背景层铺满），
        // 再显示窗口。
        {
            let target = native.composition();
            let mut target = composition::lock(&target);
            target.set_geometry(client, client)?;
        }
        native.note_composition_geometry((client, client));
        native.show();
        windows.push(native);
    }
    if let Some(first) = windows.first() {
        dispatcher.attach(first.hwnd_raw());
        crate::platform::windows::clipboard::set_owner(first.hwnd_raw());
    }
    // 首帧：显示之后请求一帧（I3 的「空闲」只发生在没有 dirty 时）。
    dispatcher.wake();
    pump(app, windows, dispatcher)
}

/// 消费者：`run`。
pub(super) fn pump(
    app: Rc<RefCell<App>>,
    mut windows: Vec<NativeWindow>,
    dispatcher: Arc<Dispatcher>,
) -> Result<(), EngineError> {
    let result = pump_loop(&app, &mut windows, &dispatcher);
    // 关闭顺序：先 drop `App`（`Window` → `Renderer::drop` 里的 GPU 线程 join 握手，期间窗口
    // 必须仍然存活），再销毁原生窗口。驱动持有的是 `Weak<RefCell<App>>`，不会延长 `App` 的
    // 生命周期，所以 `drop(app)` 就是「最后一个强引用」。
    drop(app);
    drop(windows);
    result
}

/// 合成几何缓存键：客户区只在 `WM_SIZE` / `WM_DPICHANGED` 之后变化，而这两个消息已经在本轮
/// 泵的 ①/② 阶段把**逻辑尺寸**与**比例**写进 `NativeWindow`（R07-11）。
#[derive(Clone, Copy, PartialEq)]
struct GeometryKey {
    logical: crate::math::Size,
    scale: crate::math::Scale,
}

/// 一次客户区探测的结果（`Minimized` = `GetClientRect` 是 0×0，不能当几何）。
enum ClientProbe {
    Minimized,
    Measured((u32, u32)),
}

/// 合成几何的客户区缓存（R07-11）。
///
/// 泵的 ②b′ 每轮每窗都要「当前客户区」来对齐 DirectComposition 的内容 visual，而客户区只能由
/// `GetClientRect` 得到。旧实现**每轮每窗**都调一次 `IsIconic` + `GetClientRect`（2 次系统
/// 调用），但那两个值只在窗口尺寸 / DPI 变化时才变 —— 而尺寸与比例本轮已经落在
/// [`NativeWindow::logical_size`] / [`NativeWindow::scale`] 里。因此把 `(逻辑尺寸, 比例)` 当缓存
/// 键：键不变 ⇒ 一次元组比较，**0 次系统调用**；键变化（真正的 `WM_SIZE` / DPI）才查询。
///
/// 最小化时**不写缓存**：`GetClientRect` 在最小化时是 0×0（会被钳成 1×1），不能当几何；也
/// 不缓存「最小化」这个事实 —— 恢复窗口可能不改尺寸（键仍相同），缓存住就会永远跳过它。
struct GeometryCache {
    entries: Vec<GeometryEntry>,
    /// 累计客户区查询次数（**稳态必须不增长**；只在单测里读，生产构建没有读者）。
    #[cfg(test)]
    queries: u64,
}

struct GeometryEntry {
    window: WindowId,
    key: GeometryKey,
    client: (u32, u32),
}

impl GeometryCache {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            #[cfg(test)]
            queries: 0,
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    /// 取本窗口的客户区：键相同直接命中缓存（`probe` **不会**被调用），否则查询。
    ///
    /// 返回 `None` = 本次探测到最小化（不写缓存，下一轮重试）。
    fn client(
        &mut self,
        window: WindowId,
        key: GeometryKey,
        probe: impl FnOnce() -> ClientProbe,
    ) -> Option<(u32, u32)> {
        if let Some(index) = self.entries.iter().position(|entry| entry.window == window)
            && self.entries[index].key == key
        {
            return Some(self.entries[index].client);
        }
        match probe() {
            ClientProbe::Minimized => None,
            ClientProbe::Measured(client) => {
                #[cfg(test)]
                {
                    self.queries += 1;
                }
                match self.entries.iter_mut().find(|entry| entry.window == window) {
                    Some(entry) => {
                        entry.key = key;
                        entry.client = client;
                    }
                    None => self.entries.push(GeometryEntry {
                        window,
                        key,
                        client,
                    }),
                }
                Some(client)
            }
        }
    }

    /// 丢弃已关闭窗口的缓存项（窗口 churn 不积累条目）。
    fn retain_live(&mut self, live: impl Fn(WindowId) -> bool) {
        self.entries.retain(|entry| live(entry.window));
    }

    /// 累计客户区查询次数（单测的计数判据：稳态必须不增长）。
    #[cfg(test)]
    const fn queries(&self) -> u64 {
        self.queries
    }
}

/// 消费者：`pump`。
/// 消息泵主体（每轮：消息 → 平台事件 → GPU 反馈 → 关闭 → 应用钩子 + 提交帧 → 阻塞等待）。
///
/// **`App` 以 `Rc<RefCell<_>>` 传入**：`DispatchMessageW` 里可能运行系统的 size / move 模态
/// 循环，它同步分派的 `WM_SIZE` 要由 `WndProc` 直接驱动一帧真实帧（`ResizeFrameDriver`）。
/// 因此泵只在**每一步**内短借用 `App`，消息排空期间不持有借用 —— 模态循环里的
/// `try_borrow_mut` 于是总能成功；真遇到帧内重入则跳过那一步（下一条消息会重试），绝不构造
/// 第二个 `&mut App`。
pub(super) fn pump_loop(
    app_cell: &Rc<RefCell<App>>,
    windows: &mut Vec<NativeWindow>,
    dispatcher: &Arc<Dispatcher>,
) -> Result<(), EngineError> {
    let mut message = MSG::default();
    // 合成器节拍线程：只在需要连续帧时存在（空闲时零线程、零定时器）。
    let mut pacer = FramePacer::new();
    // 合成几何的客户区缓存（R07-11）：稳态每轮每窗 0 次系统调用。
    let mut geometry = GeometryCache::new();
    // a11y 补发兜底定时器的宿主窗口（定时器随窗口销毁失效，首窗换人时要重建，P2-4）。
    // 归属判定用 `WindowId` 而不是 HWND 裸值：首窗销毁后系统可能把同一个 HWND 复用给新窗，
    // 裸值比较会「看起来仍匹配」而不重建定时器（复审 P2-8）。
    let mut a11y_timer: Option<WindowId> = None;
    loop {
        // ①′ 新消息批开始：推进 IME 提交抑制队列的批次号（AR3 §1.5 / P2-2）。
        //
        // 队列只在「入队批次 + 紧随其后的一轮」内匹配：系统补发的 `WM_CHAR` 可能落在下一次
        // 消息排空里（`PostMessage`），所以不能在本轮排空结束时清；再晚的批次由
        // `ImeState::consume_char` 的批次判据作废。没有这道边界，「提交后系统不补发
        // `WM_CHAR`」的残留队列会一直留到用户下一次真实键入并吞掉一次按键。
        for native in windows.iter_mut() {
            native.state_mut().end_message_batch();
        }
        // ① 先 drain：`WM_QUIT` 必须显式识别（`PeekMessageW` 会把它取出来）。
        //
        // **这里不持有 `App` 借用**：系统的 size / move 模态循环在 `DispatchMessageW` 里运行，
        // 它同步分派的 `WM_SIZE` 要借 `App` 出一帧真实帧（`ResizeFrameDriver`）。
        loop {
            // SAFETY: `message` 是栈上值；过滤范围 0..0 = 本线程全部消息。
            let has_message = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) };
            if !has_message.as_bool() {
                break;
            }
            if message.message == WM_QUIT {
                return Ok(());
            }
            // SAFETY: `message` 由 `PeekMessageW` 填充；`TranslateMessage` 可能产生 `WM_CHAR`。
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }

        // ② 平台事件 → App。
        {
            let mut app = app_cell.borrow_mut();
            app.sync_clock();
            // ②a 运行时窗口生命周期（P1-2.8）：`App` 里新增/移除的窗口要同步到原生窗口。
            if sync_native_windows(&mut app, app_cell, windows, dispatcher)? {
                retarget_wake(windows, dispatcher);
            }
            // ②b 平台事件 → `App`（几何 / DPI 先落地，再投递事件；时间戳取消息自带的 `MSG.time`）。
            collect_events(&mut app, windows, dispatcher)?;
        }
        // ②a′ 显示 / 电源恢复（R07-14）：显示器拓扑 / 工作区 / 睡眠唤醒之后重新查询显示器
        // 信息，请求 GPU 重建交换链（⇒ 下一帧 `reset_epoch` 整帧重发 + 清遮挡退避），并保证
        // 本轮的 ④ 帧门禁放行一帧全量重绘。放在平台事件之后：同批到达的 `WM_SIZE` /
        // `WM_DPICHANGED` 已经把尺寸与比例写进 `App`，这里查到的值覆盖它们。
        {
            let mut app = app_cell.borrow_mut();
            apply_display_recovery(&mut app, windows, dispatcher);
            // ②b GPU 线程反馈：设备丢失 / 呈现失败是粘性的，在这里冒泡出去（三平台统一）。
            app.poll_renderers()?;
        }
        // ②b″ 模态循环里由 `ResizeFrameDriver` 登记的错误：帧管线失败不可重试，必须冒泡。
        for native in windows.iter_mut() {
            if let Some(error) = native.state_mut().take_pending_error() {
                return Err(error);
            }
        }
        // ②b′ DirectComposition 几何：内容尺寸 = 最近一次**完成 present** 的尺寸，客户区 = 当前
        // 客户区。内容 visual 恒 1:1、**从不拉伸**：系统 size 模态循环里的每一步都由
        // `ResizeFrameDriver` 出真实帧（`WM_SIZE` 分支），本 pass 只把「已上屏的尺寸」与当前
        // 客户区对齐；背景层覆盖「客户区已变、新尺寸的帧还没上屏」的 ≤1 个 vsync 接缝（以及
        // DPI 同步改尺寸 / 首帧 / 设备恢复这些没有帧在飞的瞬间）。
        //
        // R07-11：客户区与最小化状态**不再每轮每窗查询**（旧实现 2 次系统调用/窗/轮）。缓存键
        // = `(逻辑尺寸, 比例)` —— `WM_SIZE` / `WM_DPICHANGED` 已在本轮 ①/② 阶段写进
        // `NativeWindow`，所以键变化精确覆盖「客户区可能变了」的时机；稳态每轮每窗 0 次系统调用。
        let app_shared = app_cell.borrow();
        for native in windows.iter_mut() {
            let id = native.id();
            let key = GeometryKey {
                logical: native.logical_size(),
                scale: native.scale(),
            };
            // 最小化时 `GetClientRect` 是 0×0（会钳成 1×1）：不要拿它当几何。
            let Some(client) = geometry.client(id, key, || {
                if native.is_minimized() {
                    ClientProbe::Minimized
                } else {
                    ClientProbe::Measured(native.client_size())
                }
            }) else {
                continue;
            };
            let presented = app_shared
                .window(id)
                .map(Window::presented_surface)
                .unwrap_or(client);
            // `presented` 是待处理状态本身：每次泵迭代都读它，消息丢了也只是晚一轮（同值 no-op）。
            // `pending` 是 GPU 线程两次唤醒都失败时的兜底标记；正常情况下靠与本窗口上次成功应用
            // 的 `(presented, client)` 比较，避免输入风暴下每轮都进一次互斥锁。
            let pending = app_shared
                .window(id)
                .is_some_and(Window::take_composition_pending);
            if !pending && native.composition_geometry() == Some((presented, client)) {
                continue;
            }
            let target = native.composition();
            let mut target = composition::lock(&target);
            target.set_geometry(presented, client)?;
            native.note_composition_geometry((presented, client));
        }
        drop(app_shared);
        // 已关闭窗口的缓存项跟着丢掉（否则窗口 churn 会让条目无界增长）。
        if geometry.len() > windows.len() {
            geometry.retain_live(|id| windows.iter().any(|native| native.id() == id));
        }

        // ②c 交互反馈：光标 / 指针捕获（P10 §5.4 / §11.3）。
        // 两者都是**每窗口**状态，请求本身携带 `WindowId`（复审 P1-3 / P2-2）：
        // - 光标：`WM_SETCURSOR` 各窗口读自己的 `WndProcState`；广播会把 A 窗口的 I-Beam
        //   写进 B 窗口，而 B 的解析值仍是 Default ⇒ 不会再发变化 ⇒ B 永久显示错光标。
        // - 捕获：`SetCapture` 只对目标 HWND 生效；固定给首窗会导致非首窗拖拽在鼠标移出
        //   窗口后收不到 `WM_LBUTTONUP`，会话永久卡住。
        //
        // **两阶段**（R07-7 纪律 / AR3 §1.2 P1）：`SetCursor` / `SetCapture` / `ReleaseCapture`
        // 都会**同步**派发消息、重入生产 `WndProc`，而重入路径会再构造一个 `&mut WndProcState`。
        // 所以这里只在短借用里做记账（`set_cursor` / `plan_release_capture`），把动作收进
        // `Vec`，离开借用作用域后由 [`apply_interaction_actions`] 统一执行。
        let mut actions: Vec<InteractionAction> = Vec::with_capacity(4);
        {
            let mut app = app_cell.borrow_mut();
            if let Some((target, cursor)) = app.take_cursor_change()
                && let Some(native) = windows.iter_mut().find(|native| native.id() == target)
            {
                native.state_mut().set_cursor(cursor);
                actions.push(InteractionAction::SetCursor(cursor_handle(cursor)));
            }
            for (target, capture) in app.take_capture_requests() {
                let Some(native) = windows.iter_mut().find(|native| native.id() == target) else {
                    continue;
                };
                let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
                if capture {
                    actions.push(InteractionAction::SetCapture(hwnd));
                } else if plan_release_capture(hwnd, native.state_mut()) {
                    // 只有确实持有捕获时才置 suppress 并计划释放（见 `plan_release_capture`）。
                    actions.push(InteractionAction::ReleaseCapture(hwnd));
                }
            }
        }
        apply_interaction_actions(actions);

        // ②d 帧节奏：合成器节拍线程（连续帧 **或** 有被预算推迟的帧时存在）+ a11y 补发兜底定时器。
        let (continuous, deferred, a11y_pending) = {
            let app = app_cell.borrow();
            (
                app.needs_continuous_frames(),
                app.has_deferred_frames(),
                app.a11y_retry_pending(),
            )
        };
        if !windows.is_empty() {
            let interval = {
                let app = app_cell.borrow();
                pace_interval(&app, windows)
            };
            pacer.sync(continuous || deferred, interval, dispatcher);
            let native = windows.first_mut().expect("上面已检查非空");
            let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
            // 定时器随宿主窗口销毁而失效：宿主已不是当前首窗 ⇒ 清掉记录，下面在新首窗重建
            //（否则首窗关闭后其余窗口的 a11y 补发永远不再触发，P2-4）。
            if a11y_timer.is_some_and(|owner| owner != native.id()) {
                a11y_timer = None;
            }
            match (a11y_pending, a11y_timer) {
                (true, None) => {
                    // SAFETY: `hwnd` 是本进程窗口；`KillTimer` 在同一线程（见下面分支）。
                    let id = unsafe { SetTimer(Some(hwnd), A11Y_TIMER_ID, A11Y_RETRY_MS, None) };
                    if id != 0 {
                        a11y_timer = Some(native.id());
                    }
                }
                (false, Some(_)) => {
                    // SAFETY: `A11Y_TIMER_ID` 由本线程在 `hwnd` 上创建；已过期/失败时是 no-op。
                    let _ = unsafe { KillTimer(Some(hwnd), A11Y_TIMER_ID) };
                    a11y_timer = None;
                }
                _ => {}
            }
        } else {
            pacer.sync(false, FALLBACK_PACE, dispatcher);
        }

        // ③ 关闭：关掉 App 里的窗口与原生窗口；一个不剩就退出。
        {
            let mut app = app_cell.borrow_mut();
            if close_requested_windows(&mut app, windows, dispatcher)? {
                return Ok(());
            }
        }

        // ④ 应用钩子（读事件 / 写状态），再提交本帧；两段门禁在 `App::frame` 里，闲时是 `Idle`。
        {
            let mut app = app_cell.borrow_mut();
            app.run_frame_hook();
        }
        // 本轮唤醒是否来自节拍线程的 tick：tick 帧走 force 通道（见 `FramePacer::tick` 的文档）。
        let tick = pacer.take_tick();
        if dispatcher.take_frame_wake() {
            let mut app = app_cell.borrow_mut();
            frame_all(&mut app, windows, tick)?;
        }

        // ④a 帧预算推迟的一次性唤醒（A5）：帧被预算门禁推迟时，泵不能阻塞在 `GetMessageW`
        // 上等下一次无关消息 —— 立即让节拍线程在下一个合成器节拍唤醒泵。节拍线程的生命周期
        // 从「仅连续帧」扩展为「连续帧 **或** 有被推迟的帧」；两者都没有时线程不存在
        // （空闲零唤醒不变式，I3）。
        //
        // RA-5：最小化窗口的推迟标记清掉（不再需要补帧；恢复时可见性路径置全量 damage + 请求
        // 一帧），避免节拍线程为永远不出帧的窗口常驻。
        {
            let mut app = app_cell.borrow_mut();
            for native in windows.iter() {
                if native.is_minimized()
                    && let Some(window) = app.window_mut(native.id())
                {
                    window.clear_frame_budget_deferred();
                }
            }
        }
        let (continuous, deferred) = {
            let app = app_cell.borrow();
            (app.needs_continuous_frames(), app.has_deferred_frames())
        };
        if windows.is_empty() {
            pacer.sync(false, FALLBACK_PACE, dispatcher);
        } else {
            let interval = {
                let app = app_cell.borrow();
                pace_interval(&app, windows)
            };
            pacer.sync(continuous || deferred, interval, dispatcher);
        }

        // ④′ a11y：纯 `A11Y` 变化不会提交帧（`FrameOutcome::Idle`），所以**每轮**都要给 adapter
        // 一次机会；`App::a11y_pending` 是 O(1) 过滤，adapter 不活跃时闭包根本不执行（懒激活）。
        {
            let mut app = app_cell.borrow_mut();
            for native in windows.iter_mut() {
                native.update_a11y(&mut app);
                // IME 启用状态先于 caret hint：刚禁用时不必再写表单，刚启用时要重发 hint
                //（`sync_ime_enabled` 会清掉 `ime_area` 缓存）。
                native.sync_ime_enabled(&app);
                native.sync_ime_cursor_area(&app);
            }
        }

        // ⑤ 跨线程回投的任务。
        for task in dispatcher.drain_tasks() {
            let mut app = app_cell.borrow_mut();
            task.run(&mut app);
        }

        // ⑥ 阻塞等待（I3）：没有消息时停在这里。连续帧的唤醒由节拍线程 `PostMessageW` 送来。
        // SAFETY: 同 ①。
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 == 0 {
            return Ok(());
        }
        if result.0 == -1 {
            // SAFETY: 无参数、无指针的线程局部查询；`GetMessageW == -1` 之后调用才有效。
            let last_error = unsafe { windows::Win32::Foundation::GetLastError() }.0;
            return Err(EngineError::WindowCreate {
                detail: smol_str::format_smolstr!("GetMessageW: GetLastError={last_error}"),
            });
        }
        // SAFETY: `message` 已由 `GetMessageW` 填充。
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// 消费者：`pump_loop` 的 ③ 关闭步骤。
/// 关闭被要求关闭的窗口；返回 `true` = 没有窗口了（泵退出）。
pub(super) fn close_requested_windows(
    app: &mut App,
    windows: &mut Vec<NativeWindow>,
    dispatcher: &Arc<Dispatcher>,
) -> Result<bool, EngineError> {
    let mut closed = false;
    for native in windows.iter_mut() {
        if native.state_mut().take_close_request() {
            let id = native.id();
            // 先问应用：`App::on_close` 返回 `false` 就拦下这次关闭（P2-8）；被拦下时
            // **不**销毁原生窗口（窗口继续存在，下一次点 X 还会再问）。
            if !app.request_close(id) {
                continue;
            }
            native.destroy();
            closed = true;
        }
    }
    if closed {
        windows.retain(|native| !native.is_destroyed());
        retarget_wake(windows, dispatcher);
    }
    Ok(windows.is_empty() || app.windows().next().is_none())
}

/// 消费者：`run` / `pump_loop` / `close_requested_windows`。
/// 唤醒目标 / 剪贴板属主跟随存活的第一个窗口；一个不剩就清空（`wake` 改走线程消息）。
///
/// 目标窗口销毁后 `PostMessageW` 会失败，若不改指，`GetMessageW` 会永久阻塞（P14 审查 #2）。
pub(super) fn retarget_wake(windows: &[NativeWindow], dispatcher: &Arc<Dispatcher>) {
    match windows.first() {
        Some(native) => {
            dispatcher.attach(native.hwnd_raw());
            crate::platform::windows::clipboard::set_owner(native.hwnd_raw());
        }
        None => dispatcher.detach(),
    }
}

/// 消费者：`pump_loop` 的 ④ 帧步骤。
///
/// 逐窗口提交一帧；`Err` 立刻终止（device lost / swapchain 失败都不可重试）。
///
/// `force` = 本轮由节拍线程 tick 驱动（动画 / 帧预算推迟的一次性唤醒）：走 force 通道绕过
/// 「每 refresh 最多一帧」的预算门禁 —— tick 本身就是「这一拍该出帧」的信号；普通 wake 仍走
/// 预算。与 macOS `frame_all` 的 `force` 同义。
pub(super) fn frame_all(
    app: &mut App,
    windows: &[NativeWindow],
    force: bool,
) -> Result<(), EngineError> {
    for native in windows {
        let outcome = if force {
            app.frame_forced(native.id())
        } else {
            app.frame(native.id())
        };
        match outcome {
            None => {}
            Some(Ok(_)) => {}
            Some(Err(error)) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod geometry_cache_tests {
    use super::{ClientProbe, GeometryCache, GeometryKey};
    use crate::{
        math::{Scale, Size},
        window::WindowId,
    };

    fn window(raw: u32) -> WindowId {
        WindowId::from_raw(raw)
    }

    fn key(width: f32) -> GeometryKey {
        GeometryKey {
            logical: Size::new(width, 600.0),
            scale: Scale::ONE,
        }
    }

    fn measured(client: (u32, u32)) -> impl Fn() -> ClientProbe {
        move || ClientProbe::Measured(client)
    }

    /// **R07-11 的计数判据**：稳态（逻辑尺寸 / 比例没变）每轮**0 次**客户区查询。
    ///
    /// 旧实现每轮每窗 2 次系统调用（`IsIconic` + `GetClientRect`）；现在稳态只剩一次元组比较。
    #[test]
    fn a_steady_pump_iteration_queries_the_client_size_zero_times() {
        let mut cache = GeometryCache::new();
        let id = window(1);
        let mut probes = 0u32;
        assert_eq!(
            cache.client(id, key(800.0), || {
                probes += 1;
                ClientProbe::Measured((800, 600))
            }),
            Some((800, 600))
        );
        for _ in 0..1_000 {
            let client = cache.client(id, key(800.0), || {
                probes += 1;
                ClientProbe::Measured((800, 600))
            });
            assert_eq!(client, Some((800, 600)), "缓存命中必须给出上次的客户区");
        }
        assert_eq!(probes, 1, "稳态 1000 轮只允许 1 次查询（实际 {probes} 次）");
        assert_eq!(cache.queries(), 1, "查询计数必须停在 1");
    }

    /// 真正的尺寸 / DPI 变化必须重新查询（否则几何会停在旧客户区上）。
    #[test]
    fn a_resize_or_dpi_change_queries_the_client_size_again() {
        let mut cache = GeometryCache::new();
        let id = window(1);
        let probes = std::cell::Cell::new(0u32);
        let probe = |client: (u32, u32)| {
            probes.set(probes.get() + 1);
            ClientProbe::Measured(client)
        };
        assert_eq!(
            cache.client(id, key(800.0), || probe((800, 600))),
            Some((800, 600))
        );
        assert_eq!(
            cache.client(id, key(900.0), || probe((900, 700))),
            Some((900, 700))
        );
        // 比例变化（DPI）同样是一个新键。
        let mut dpi = key(900.0);
        dpi.scale = Scale::new(1.25).expect("scale");
        assert_eq!(
            cache.client(id, dpi, || probe((1125, 875))),
            Some((1125, 875))
        );
        assert_eq!(
            probes.get(),
            3,
            "最后一次查询必须跟上新键（实际 {} 次）",
            probes.get()
        );
        assert_eq!(cache.queries(), 3);
        // 回到已知键：仍然重新查询（缓存只保留最近一次的客户区）。
        assert_eq!(
            cache.client(id, key(800.0), || probe((800, 600))),
            Some((800, 600))
        );
        assert_eq!(probes.get(), 4);
    }

    /// 最小化时 `GetClientRect` 是 0×0：**不得**写缓存，也不得缓存「最小化」这个事实
    /// （恢复窗口可能不改尺寸 ⇒ 键不变，缓存住就永远跳过几何）。
    #[test]
    fn a_minimized_window_is_never_cached() {
        let mut cache = GeometryCache::new();
        let id = window(1);
        assert_eq!(
            cache.client(id, key(800.0), || ClientProbe::Measured((800, 600))),
            Some((800, 600))
        );
        let mut probes = 0u32;
        for _ in 0..3 {
            assert_eq!(
                cache.client(id, key(900.0), || {
                    probes += 1;
                    ClientProbe::Minimized
                }),
                None,
                "最小化时必须跳过几何应用"
            );
        }
        assert_eq!(probes, 3, "最小化状态不能被缓存（每轮重试）");
        // 恢复：同一新键下探测到客户区 ⇒ 写入缓存（下一轮起 0 系统调用）。
        assert_eq!(
            cache.client(id, key(900.0), measured((900, 700))),
            Some((900, 700))
        );
        let mut restored_probes = 0u32;
        assert_eq!(
            cache.client(id, key(900.0), || {
                restored_probes += 1;
                ClientProbe::Measured((900, 700))
            }),
            Some((900, 700))
        );
        assert_eq!(restored_probes, 0, "恢复后的稳态必须走缓存");
    }

    /// 多个窗口各自独立缓存；窗口关闭后条目被丢弃（窗口 churn 不积累内存）。
    #[test]
    fn closed_windows_do_not_keep_cache_entries() {
        let mut cache = GeometryCache::new();
        for raw in 1..=3u32 {
            let id = window(raw);
            assert_eq!(
                cache.client(id, key(800.0), measured((800, 600))),
                Some((800, 600))
            );
        }
        assert_eq!(cache.len(), 3);
        cache.retain_live(|id| id == window(2));
        assert_eq!(cache.len(), 1, "关闭的窗口必须丢掉缓存项");
        assert_eq!(cache.queries(), 3);
    }
}

#[cfg(test)]
mod pacer_tests {
    use std::time::Duration;

    use super::{FALLBACK_PACE, MAX_PACE_WAIT_MS, pace_interval_from};

    /// A5/A3：UI 侧节拍线程的周期来自窗口所在 output 的刷新率提示。
    /// 未知 ⇒ 60 Hz 兜底（只影响动画 tick，不卡内容帧）；提示异常/过低 ⇒ 上限保护。
    #[test]
    fn the_pacer_interval_follows_the_output_refresh_hint() {
        assert_eq!(pace_interval_from(None), FALLBACK_PACE);
        let hz60 = pace_interval_from(Some(60.0));
        assert!(
            (hz60.as_secs_f64() - 1.0 / 60.0).abs() < 1e-6,
            "60 Hz 提示 ⇒ 16.67 ms 周期：{hz60:?}"
        );
        let hz144 = pace_interval_from(Some(144.0));
        assert!(
            (hz144.as_secs_f64() - 1.0 / 144.0).abs() < 1e-6,
            "144 Hz 提示 ⇒ 6.94 ms 周期：{hz144:?}"
        );
        assert_eq!(
            pace_interval_from(Some(0.5)),
            Duration::from_millis(MAX_PACE_WAIT_MS),
            "异常低的刷新率必须被上限保护（不能睡死）"
        );
    }
}

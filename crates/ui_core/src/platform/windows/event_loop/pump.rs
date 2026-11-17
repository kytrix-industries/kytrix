//! 消息泵：`run`（建窗 → 挂渲染器 → 显示）、每轮固定步骤、唤醒与帧驱动、合成器节拍。
//!
//! ## 帧节奏（R07-10）
//!
//! 连续帧（动画 / HUD）由 [`FramePacer`] 驱动：一条**只在需要时存在**的节拍线程阻塞在
//! `DCompositionWaitForCompositorClock`（Win10 1809+ / MSDN 最低 Win11 build 22000；
//! Chrome Viz 的 `DCLayerTree::WaitForCompositorClock` 同款），每个合成器节拍唤醒一次泵。
//!
//! 为什么不用 `WM_TIMER`（旧实现）：`WM_TIMER` 是**最低优先级**消息，输入密集时被饿
//! （1000 Hz 鼠标下动画反而更抖）；`SetTimer` 的最小粒度受系统定时器分辨率限制
//! （默认 15.6 ms），且不与 DWM 的合成节拍对齐 ⇒ 60 Hz 拍频、120 Hz 显示器用不上。
//! `WM_TIMER` 现在只保留给 a11y 补发兜底（[`A11Y_TIMER_ID`]），空闲时没有定时器。
//!
//! 为什么需要一条独立线程：`DCompositionWaitForCompositorClock` 是阻塞调用，而 UI 线程是
//! 唯一的消息泵 —— 把它放进泵里就等于暂停输入。Chrome 同样是「合成线程等时钟、UI 线程消费」。

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use windows::Win32::{
    Foundation::HWND,
    System::SystemInformation::GetTickCount64,
    UI::{
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
        Input::KeyboardAndMouse::{GetCapture, GetDoubleClickTime, ReleaseCapture, SetCapture},
        WindowsAndMessaging::*,
    },
};

use super::native_window::{collect_events, sync_native_windows};
use crate::{
    app::App,
    error::EngineError,
    platform::{
        dispatcher::Dispatcher,
        windows::{composition, frame_clock::FrameClock, window::NativeWindow},
    },
    render::backend::Renderer,
    window::{Window, WindowId, WindowOptions},
};

/// 消费者：`wndproc` 的 `WM_TIMER` 分支与 `pump_loop` 的定时器管理。
/// a11y 补发兜底定时器 id（**唯一**保留的 `WM_TIMER` 用途；连续帧走合成器节拍）。
pub(super) const A11Y_TIMER_ID: usize = 1;

/// a11y 补发兜底定时器的周期（毫秒）。
///
/// 与 `App` 侧的 `A11Y_THROTTLE`（100 ms）一致：被节流的一次交付最多等一个周期就必须重试，
/// 否则屏幕阅读器会永远收不到最后一笔更新。
const A11Y_RETRY_MS: u32 = 100;

/// 节拍线程单次等待的**下限**（毫秒）：`DCompositionWaitForCompositorClock` 不接受 0 超时，
/// 且过小的超时会让线程退化成忙轮询。
const MIN_PACE_WAIT_MS: u64 = 1;

/// 节拍线程单次等待的**上限**（毫秒）。
///
/// 正常路径靠合成器节拍唤醒（120 Hz 下 8.3 ms），这个上限只在合成器停摆时生效；它同时是
/// 「停止后的最长退出延迟」，所以不能大。
const MAX_PACE_WAIT_MS: u64 = 100;

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

/// 消费者：`run`。
/// 启动时声明 DPI 感知：per-monitor v2（`WM_DPICHANGED` 才带每屏 DPI）。
pub(super) fn install_dpi_awareness() {
    // 失败通常意味着清单/宿主已经设过 awareness（返回 `ERROR_ACCESS_DENIED`）：沿用现有设置。
    // SAFETY: 无参数指针的进程级设置；必须在创建任何窗口之前调用。
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
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

/// 合成器节拍线程（R07-10）。
///
/// 生命周期跟随 `App::needs_continuous_frames()`：需要连续帧时起、不需要时停 —— 空闲时
/// **没有线程、没有定时器、没有周期唤醒**（`DCompositionWaitForCompositorClock` 只用一次）。
pub(super) struct FramePacer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// 启动线程时用的窗口句柄（用于刷新率采样）：窗口换过就重启线程。
    hwnd: isize,
}

impl FramePacer {
    pub(super) fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
            hwnd: 0,
        }
    }

    /// 启动/停止/改指向节拍线程（幂等）。
    pub(super) fn sync(&mut self, active: bool, hwnd: HWND, dispatcher: &Arc<Dispatcher>) {
        let target = hwnd.0 as isize;
        if !active {
            self.shutdown();
            return;
        }
        if self.thread.is_some() && self.hwnd == target {
            return;
        }
        self.shutdown();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_dispatcher = Arc::clone(dispatcher);
        self.thread = Some(std::thread::spawn(move || {
            pace(thread_stop, target, thread_dispatcher);
        }));
        self.stop = stop;
        self.hwnd = target;
    }

    /// 停止并 join（`Drop` 也会调）。
    pub(super) fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // 等待上限是 `CLOCK_WAIT_TIMEOUT_MS`：不能因为线程还在等节拍就跳过 join
            //（跳过会让节拍线程活过 `Dispatcher`）。
            let _ = thread.join();
        }
        self.hwnd = 0;
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
/// 每个合成器节拍唤醒一次泵（`Dispatcher::wake` 是电平信号，天然合并）；时钟不可用
/// （显示器关闭 / 远程桌面）时按采样给出的「距下一次可呈现时刻」退避，避免忙等。
fn pace(stop: Arc<AtomicBool>, hwnd: isize, dispatcher: Arc<Dispatcher>) {
    let mut clock = FrameClock::new(HWND(hwnd as *mut core::ffi::c_void));
    let floor = Duration::from_millis(MIN_PACE_WAIT_MS);
    let ceiling = Duration::from_millis(MAX_PACE_WAIT_MS);
    while !stop.load(Ordering::Acquire) {
        // 采样决定两件事：这次等待的上限（两个帧间隔，120 Hz ⇒ 16.6 ms），以及合成器停摆时
        // 的退避量。采样失败不是错误：`sample` 会退回「整整一帧」。
        let sample = clock.sample();
        let timeout = (clock.frame_interval() * 2).clamp(floor, ceiling);
        if clock.wait_next_frame(timeout) {
            dispatcher.wake();
        } else if !stop.load(Ordering::Acquire) {
            // 没有节拍（屏幕不亮 / 会话断开 / DWM 停摆）：睡到下一个可呈现时刻再试，
            // 绝不忙等，也不永久停摆（屏幕恢复后下一拍就会醒）。
            std::thread::sleep(sample.until_next_present.clamp(floor, ceiling));
        }
    }
}

/// 消费者：`platform::windows` 的 `pub(crate) use event_loop::run`。
/// 引擎自有事件循环：建窗口 → 挂渲染器 → 显示 → 泵 → 关闭。
pub(crate) fn run(mut app: App) -> Result<(), EngineError> {
    install_dpi_awareness();
    install_input_thresholds(&mut app);
    let dispatcher = app.dispatcher();
    // 线程消息兜底的目标：首窗关闭后 `PostMessageW` 会失败，`wake()` 必须还能叫醒 `GetMessageW`。
    // SAFETY: 无参数，返回调用线程的 id（只用于 `PostThreadMessageW`）。
    dispatcher.bind_thread(unsafe { windows::Win32::System::Threading::GetCurrentThreadId() });

    let requests: Vec<(WindowId, WindowOptions)> = app
        .windows()
        .map(|(id, window)| (id, window.options().clone()))
        .collect();
    let mut windows: Vec<NativeWindow> = Vec::with_capacity(requests.len());
    for (id, options) in requests {
        let mut native = NativeWindow::create(id, &options, dispatcher.clone())?;
        let client = native.client_size();
        let renderer = Renderer::new_for_hwnd(native.composition(), client, options.background)?;
        if let Some(window) = app.window_mut(id) {
            window.set_renderer(renderer);
            window.set_scale(native.scale());
            window.set_size(native.logical_size());
            // 渲染目标就是真实客户区：逻辑尺寸取整后的 covering 值可能比它大 1 px。
            window.set_surface(client);
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
    mut app: App,
    mut windows: Vec<NativeWindow>,
    dispatcher: Arc<Dispatcher>,
) -> Result<(), EngineError> {
    let result = pump_loop(&mut app, &mut windows, &dispatcher);
    // 关闭顺序（`design/async-render-pipeline.md` §6.7）：先 drop `App`（`Window` →
    // `Renderer::drop` 里的 GPU 线程 join 握手，期间窗口必须仍然存活），再销毁原生窗口。
    // 反过来会让 GPU 线程在 HWND 已销毁之后还去 `Present`。
    drop(app);
    drop(windows);
    result
}

/// 消费者：`pump`。
/// 消息泵主体（每轮：消息 → 平台事件 → GPU 反馈 → 关闭 → 应用钩子 + 提交帧 → 阻塞等待）。
pub(super) fn pump_loop(
    app: &mut App,
    windows: &mut Vec<NativeWindow>,
    dispatcher: &Arc<Dispatcher>,
) -> Result<(), EngineError> {
    let mut message = MSG::default();
    // 合成器节拍线程：只在需要连续帧时存在（空闲时零线程、零定时器）。
    let mut pacer = FramePacer::new();
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
        app.sync_clock();
        // ②a 运行时窗口生命周期（P1-2.8）：`App` 里新增/移除的窗口要同步到原生窗口。
        if sync_native_windows(app, windows, dispatcher)? {
            retarget_wake(windows, dispatcher);
        }
        collect_events(app, windows, dispatcher)?;

        // ②b GPU 线程反馈：设备丢失 / 呈现失败是粘性的，在这里冒泡出去（三平台统一）。
        app.poll_renderers()?;

        // ②b′ DirectComposition 几何：内容尺寸 = 最近一次**完成 present** 的尺寸，客户区 = 当前
        // 客户区。内容 visual 恒 1:1：拖动中旧内容保持左上角，露出的新区域由背景层填充；静止时
        // 内容 = 客户区，背景层被完全盖住。GPU 线程在每帧完成后 post
        // `WM_APP_COMPOSITION_WAKE`（只唤醒、不请求帧），所以「最后一帧」的几何也会在下一次泵
        // 迭代里落地。这里是几何**唯一**的周期应用点：`WM_SIZE` / DPI 只记录尺寸，不逐条
        // `Commit`（P1-2a）。
        for native in windows.iter_mut() {
            // 最小化时 `GetClientRect` 是 0×0（会钳成 1×1）：不要拿它当几何。
            if native.is_minimized() {
                continue;
            }
            let client = native.client_size();
            let presented = app
                .window(native.id())
                .map(Window::presented_surface)
                .unwrap_or(client);
            // `presented` 是待处理状态本身：每次泵迭代都读它，消息丢了也只是晚一轮（同值 no-op）。
            // `pending` 是 GPU 线程两次唤醒都失败时的兜底标记；正常情况下靠与本窗口上次成功应用
            // 的 `(presented, client)` 比较，避免输入风暴下每轮都进一次互斥锁。
            let pending = app
                .window(native.id())
                .is_some_and(Window::take_composition_pending);
            if !pending && native.composition_geometry() == Some((presented, client)) {
                continue;
            }
            let target = native.composition();
            let mut target = composition::lock(&target);
            target.set_geometry(presented, client)?;
            native.note_composition_geometry((presented, client));
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
        apply_interaction_actions(actions);

        // ②d 帧节奏：合成器节拍线程（只在需要连续帧时存在）+ a11y 补发兜底定时器。
        if let Some(native) = windows.first_mut() {
            let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
            pacer.sync(app.needs_continuous_frames(), hwnd, dispatcher);
            // 定时器随宿主窗口销毁而失效：宿主已不是当前首窗 ⇒ 清掉记录，下面在新首窗重建
            //（否则首窗关闭后其余窗口的 a11y 补发永远不再触发，P2-4）。
            if a11y_timer.is_some_and(|owner| owner != native.id()) {
                a11y_timer = None;
            }
            match (app.a11y_retry_pending(), a11y_timer) {
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
            pacer.sync(false, HWND::default(), dispatcher);
        }

        // ③ 关闭：关掉 App 里的窗口与原生窗口；一个不剩就退出。
        if close_requested_windows(app, windows, dispatcher)? {
            return Ok(());
        }

        // ④ 应用钩子（读事件 / 写状态），再提交本帧；两段门禁在 `App::frame` 里，闲时是 `Idle`。
        app.run_frame_hook();
        if dispatcher.take_frame_wake() {
            frame_all(app)?;
        }

        // ④′ a11y：纯 `A11Y` 变化不会提交帧（`FrameOutcome::Idle`），所以**每轮**都要给 adapter
        // 一次机会；`App::a11y_pending` 是 O(1) 过滤，adapter 不活跃时闭包根本不执行（懒激活）。
        for native in windows.iter_mut() {
            native.update_a11y(app);
            // IME 启用状态先于 caret hint：刚禁用时不必再写表单，刚启用时要重发 hint
            //（`sync_ime_enabled` 会清掉 `ime_area` 缓存）。
            native.sync_ime_enabled(app);
            native.sync_ime_cursor_area(app);
        }

        // ⑤ 跨线程回投的任务。
        for task in dispatcher.drain_tasks() {
            task.run(app);
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
/// 逐窗口提交一帧；`Err` 立刻终止（device lost / swapchain 失败都不可重试）。
pub(super) fn frame_all(app: &mut App) -> Result<(), EngineError> {
    let ids: Vec<WindowId> = app.windows().map(|(id, _)| id).collect();
    for id in ids {
        match app.frame(id) {
            None | Some(Ok(_)) => {}
            Some(Err(error)) => return Err(error),
        }
    }
    Ok(())
}

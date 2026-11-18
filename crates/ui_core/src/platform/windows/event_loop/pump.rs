//! 消息泵：`run`（建窗 → 挂渲染器 → 显示）、每轮固定步骤、唤醒与帧驱动、合成器节拍。
//!
//! ## 帧节奏（R07-10）
//!
//! 连续帧（**动画**；HUD 自 P-C 起是纯观察者，不再参与 `needs_continuous_frames`）由
//! [`FramePacer`] 驱动：一条**只在需要时存在**的节拍线程阻塞在
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
        windows::{composition, dpi_awareness, frame_clock::FrameClock, window::NativeWindow},
    },
    render::backend::Renderer,
    window::{FrameOutcome, Window, WindowId, WindowOptions},
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
    // 关闭顺序：先 drop `App`（`Window` →
    // `Renderer::drop` 里的 GPU 线程 join 握手，期间窗口必须仍然存活），再销毁原生窗口。
    // 反过来会让 GPU 线程在 HWND 已销毁之后还去 `Present`。
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
pub(super) fn pump_loop(
    app: &mut App,
    windows: &mut Vec<NativeWindow>,
    dispatcher: &Arc<Dispatcher>,
) -> Result<(), EngineError> {
    let mut message = MSG::default();
    // 合成器节拍线程：只在需要连续帧时存在（空闲时零线程、零定时器）。
    let mut pacer = FramePacer::new();
    // 合成几何的客户区缓存（R07-11）：稳态每轮每窗 0 次系统调用。
    let mut geometry = GeometryCache::new();
    // 拖动期出帧背压（P-A）：自持缩放会话中「上一帧未上屏则跳过」，不用定时器合并鼠标。
    let mut resize_pacing = ResizePacing::new();
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
        // 自持缩放（P-A）：把会话累积的目标外框 `SetWindowPos` 一次（同一轮的 ④ 就按新尺寸出
        // 真实帧）。它同步派发 `WM_SIZE`，紧随的 `collect_events` 用现有链路写 `set_size` /
        // `set_surface`；放在事件之前 ⇒ 拖动期间不存在「只更新了客户区、没重绘」的持久帧。
        // **这条接线本身没有单测**（`apply_pending_resizes` 的函数级行为有单测；删掉这一行
        // 单测仍全绿）—— 由真机拖动探针 `verify_pa_pc.ps1` 验收，见该函数文档。
        apply_pending_resizes(windows);
        collect_events(app, windows, dispatcher)?;

        // ②a′ 显示 / 电源恢复（R07-14）：显示器拓扑 / 工作区 / 睡眠唤醒之后重新查询显示器
        // 信息，请求 GPU 重建交换链（⇒ 下一帧 `reset_epoch` 整帧重发 + 清遮挡退避），并保证
        // 本轮的 ④ 帧门禁放行一帧全量重绘。放在平台事件之后：同批到达的 `WM_SIZE` /
        // `WM_DPICHANGED` 已经把尺寸与比例写进 `App`，这里查到的值覆盖它们。
        apply_display_recovery(app, windows, dispatcher);

        // ②b GPU 线程反馈：设备丢失 / 呈现失败是粘性的，在这里冒泡出去（三平台统一）。
        app.poll_renderers()?;

        // ②b′ DirectComposition 几何：内容尺寸 = 最近一次**完成 present** 的尺寸，客户区 = 当前
        // 客户区。内容 visual 恒 1:1、**从不拉伸**：自持缩放（P-A）的每一步都由本轮 ④ 出一帧
        // 真实帧，所以背景层只覆盖「客户区已变、新尺寸的帧还没上屏」的 ≤1 个 vsync 接缝（以及
        // DPI 同步改尺寸 / 首帧 / 设备恢复这些没有帧在飞的瞬间），不是拖动期的画面来源。
        // GPU 线程在每帧完成后 post `WM_APP_COMPOSITION_WAKE`（平时只唤醒、不请求帧；缩放
        // 会话中额外请求一次出帧尝试，见 `ResizePacing`），所以「最后一帧」的几何也会在
        // 下一次泵迭代里落地。这里是几何**唯一**的周期应用点：`WM_SIZE` / DPI 只记录尺寸，
        // 不逐条 `Commit`（P1-2a）。
        //
        // R07-11：客户区与最小化状态**不再每轮每窗查询**（旧实现 2 次系统调用/窗/轮）。缓存键
        // = `(逻辑尺寸, 比例)` —— `WM_SIZE` / `WM_DPICHANGED` 已在本轮 ①/② 阶段写进
        // `NativeWindow`，所以键变化精确覆盖「客户区可能变了」的时机；稳态每轮每窗 0 次系统调用。
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
            let presented = app
                .window(id)
                .map(Window::presented_surface)
                .unwrap_or(client);
            // `presented` 是待处理状态本身：每次泵迭代都读它，消息丢了也只是晚一轮（同值 no-op）。
            // `pending` 是 GPU 线程两次唤醒都失败时的兜底标记；正常情况下靠与本窗口上次成功应用
            // 的 `(presented, client)` 比较，避免输入风暴下每轮都进一次互斥锁。
            let pending = app.window(id).is_some_and(Window::take_composition_pending);
            if !pending && native.composition_geometry() == Some((presented, client)) {
                continue;
            }
            let target = native.composition();
            let mut target = composition::lock(&target);
            target.set_geometry(presented, client)?;
            native.note_composition_geometry((presented, client));
        }
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
        // 自持缩放会话中的窗口按 `ResizePacing` 背压跳过（上一帧未上屏就不重排/重绘）。
        app.run_frame_hook();
        if dispatcher.take_frame_wake() {
            frame_all(app, windows, &mut resize_pacing)?;
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
///
/// 逐窗口提交一帧；`Err` 立刻终止（device lost / swapchain 失败都不可重试）。
/// 自持缩放会话中的窗口按 [`ResizePacing`] 的背压跳过（合并鼠标上报，见该类型文档）。
pub(super) fn frame_all(
    app: &mut App,
    windows: &mut [NativeWindow],
    pacing: &mut ResizePacing,
) -> Result<(), EngineError> {
    // 本帧放行的窗口；`Some(surface)` = 会话中（出帧后记进背压账目），`None` = 无常驻账目。
    let mut ready: Vec<(WindowId, Option<(u32, u32)>)> = Vec::with_capacity(windows.len());
    for native in windows.iter_mut() {
        let id = native.id();
        if !native.state_mut().resize_active() {
            ready.push((id, None));
            continue;
        }
        // 只有会话中的窗口才查客户区：它就是本帧要发布的渲染目标（`WM_SIZE` 的 lParam 与
        // `GetClientRect` 同源），而稳态非会话窗口仍然是每轮 0 次系统调用（R07-11）。
        let surface = native.client_size();
        let presented = app
            .window(id)
            .map(Window::presented_surface)
            .unwrap_or_default();
        if pacing.should_skip(id, true, presented) {
            continue;
        }
        ready.push((id, Some(surface)));
    }
    for (id, surface) in ready {
        match app.frame(id) {
            None => {}
            Some(Ok(outcome)) => {
                if matches!(outcome, FrameOutcome::Committed(_))
                    && let Some(surface) = surface
                {
                    pacing.note_published(id, surface);
                }
            }
            Some(Err(error)) => return Err(error),
        }
    }
    // 会话结束 / 窗口关闭：账目跟着清掉，否则残留项会挡住该窗口以后的帧。
    pacing.retain_live(|id| {
        windows
            .iter_mut()
            .any(|native| native.id() == id && native.state_mut().resize_active())
    });
    Ok(())
}

/// 拖动期出帧背压（P-A）：每窗口「最近一次已发布的 surface」。
///
/// 自持缩放会话里鼠标可以上报到数百 Hz，而一次真实帧要重排 + 按新尺寸重绘。合并**不能**用
/// 定时器（产品前提：不用定时器驱动刷新），所以改成背压：会话中的窗口只有在上一次发布的帧
/// **已经上屏**（`presented == published`）时才出下一帧，否则跳过本帧、把 damage 留给下一轮。
///
/// 唤醒来源是 present 完成后的 `WM_APP_COMPOSITION_WAKE`：缩放期间每帧尺寸都在变，
/// `publish_presented` 的「尺寸变化才通知」因此每帧都会触发，正好当「上一帧上屏了」的信号。
/// 会话结束（松手 / Esc / 取消）后不再跳过，最终尺寸一定落在一帧真实帧上。
pub(crate) struct ResizePacing {
    entries: Vec<ResizePacingEntry>,
}

struct ResizePacingEntry {
    window: WindowId,
    /// 最近一次**已发布**（`App::frame` 提交成功）的 surface 尺寸。
    published: (u32, u32),
}

impl ResizePacing {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// 该窗口本帧是否跳过：会话中且「已发布的帧还没上屏」。
    ///
    /// 第一次看到某窗口的会话（还没有账目）时按 `presented` 初始化并**放行**本帧：会话开始时
    /// 没有已知的在飞帧，缩放的第一个真实帧应当先出去。
    fn should_skip(&mut self, window: WindowId, active: bool, presented: (u32, u32)) -> bool {
        if !active {
            return false;
        }
        match self.entries.iter_mut().find(|entry| entry.window == window) {
            Some(entry) => entry.published != presented,
            None => {
                self.entries.push(ResizePacingEntry {
                    window,
                    published: presented,
                });
                false
            }
        }
    }

    /// 出帧成功后记账：本窗口最近一次发布的 surface。
    fn note_published(&mut self, window: WindowId, surface: (u32, u32)) {
        match self.entries.iter_mut().find(|entry| entry.window == window) {
            Some(entry) => entry.published = surface,
            None => self.entries.push(ResizePacingEntry {
                window,
                published: surface,
            }),
        }
    }

    /// 丢掉「会话已结束 / 窗口已关闭」的账目。
    fn retain_live(&mut self, mut live: impl FnMut(WindowId) -> bool) {
        self.entries.retain(|entry| live(entry.window));
    }

    /// 账目条数（测试判据：会话结束后必须归零，不能随窗口 churn 无界增长）。
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// 消费者：`pump_loop` 的 ② 步骤（`collect_events` **之前**）。
///
/// 应用自持缩放会话累积的目标外框：每轮每窗**最多一次**（`pending_resize` 最新赢）。
///
/// `SetWindowPos` 会**同步**派发 `WM_SIZE`（重入 `WndProc`），所以必须在短借用之外执行，并且
/// 放在 `collect_events` 之前：那条 `WM_SIZE` 产生的 `Resize` 事件会在同一轮被 `apply_event`
/// 写进 `set_size`（标 `LAYOUT`）与 `set_surface`（标全量 damage），紧接着本轮 ④ 出一帧。
///
/// **测试边界**：函数级行为（取走一次 → `SetWindowPos` → 再次调用 no-op）有单测
/// （`event_loop/tests/pump_resize_tests.rs`）；**泵里的调用点接线**（这一轮真的在事件之前
/// 调用它）没有单测覆盖 —— 删掉下面 `pump_loop` 里的调用，既有单测仍会全绿。那条接线由真机
/// 拖动探针 `verify_pa_pc.ps1` 验收（AR7 已把它登记为已知测试缺口）。
pub(super) fn apply_pending_resizes(windows: &mut [NativeWindow]) {
    for native in windows.iter_mut() {
        // 短借用只做取走：在借用内调 `SetWindowPos` 会重入 `WndProc` 再取一次 `&mut`。
        let Some(rect) = native.state_mut().take_pending_resize() else {
            continue;
        };
        let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
        let (x, y, width, height) = rect.as_window_rect();
        // SAFETY: `hwnd` 是本进程窗口；只改位置/尺寸，不激活、不改 Z 序。同步派发的
        // `WM_SIZE` / `WM_WINDOWPOSCHANGED` 由生产 `WndProc` 处理（它只记账，不调用 `App`）。
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                x,
                y,
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
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
mod resize_pacing_tests {
    use super::ResizePacing;
    use crate::window::WindowId;

    fn window(raw: u32) -> WindowId {
        WindowId::from_raw(raw)
    }

    /// 会话中「上一帧已发布但还没上屏」必须跳过；presented 追平后立刻放行。
    ///
    /// 这就是拖动期的合并机制（替代定时器）：数百 Hz 鼠标上报只会在每个上屏周期里放行一帧。
    #[test]
    fn a_published_but_unpresented_frame_skips_until_presented_catches_up() {
        let mut pacing = ResizePacing::new();
        let id = window(1);
        assert!(
            !pacing.should_skip(id, true, (800, 600)),
            "会话的第一帧必须先出去（此时没有已知在飞帧）"
        );
        pacing.note_published(id, (900, 700));
        assert!(
            pacing.should_skip(id, true, (800, 600)),
            "已发布 (900,700) 还没上屏：本帧必须跳过"
        );
        assert!(
            pacing.should_skip(id, true, (800, 600)),
            "presented 不变就继续跳过（不进入帧 ⇒ damage 保留）"
        );
        assert!(
            !pacing.should_skip(id, true, (900, 700)),
            "presented 追上已发布尺寸后必须放行"
        );
    }

    /// 非会话窗口永远不跳过，也不产生账目（稳态零成本）。
    #[test]
    fn an_inactive_session_never_skips_and_keeps_no_ledger() {
        let mut pacing = ResizePacing::new();
        let id = window(1);
        for presented in [(0, 0), (800, 600), (900, 700)] {
            assert!(
                !pacing.should_skip(id, false, presented),
                "没有缩放会话的窗口不受背压影响"
            );
        }
        assert_eq!(pacing.len(), 0, "非会话窗口不得留下账目");
    }

    /// 会话结束 / 窗口关闭后账目必须被清掉（否则残留项会挡住该窗口以后的帧）。
    #[test]
    fn the_ledger_drops_windows_whose_session_ended() {
        let mut pacing = ResizePacing::new();
        pacing.note_published(window(1), (100, 100));
        pacing.note_published(window(2), (200, 200));
        assert_eq!(pacing.len(), 2);
        pacing.retain_live(|id| id == window(2));
        assert_eq!(pacing.len(), 1, "会话结束 / 窗口关闭的账目必须清掉");
        assert!(
            !pacing.should_skip(window(2), true, (200, 200)),
            "保留的窗口在 presented 追平后正常出帧"
        );
    }

    /// 多窗口互不影响：一个窗口在飞不会让另一个窗口跳过。
    #[test]
    fn windows_are_paced_independently() {
        let mut pacing = ResizePacing::new();
        let (one, two) = (window(1), window(2));
        pacing.should_skip(one, true, (100, 100));
        pacing.note_published(one, (300, 300));
        pacing.should_skip(two, true, (200, 200));
        assert!(pacing.should_skip(one, true, (100, 100)), "窗口 1 在飞");
        assert!(
            !pacing.should_skip(two, true, (200, 200)),
            "窗口 2 自己的账目是同步的，不受窗口 1 影响"
        );
    }
}

//! 消息泵：`run`（建窗 → 挂渲染器 → 显示）、每轮固定步骤、唤醒与帧驱动、连续帧定时器。
//! 自由函数由 `event_loop.rs` 的 `run` 出口与 `wndproc` 调用（零行为变更的结构拆分）。

use std::sync::Arc;

use windows::Win32::{
    Foundation::HWND,
    System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
    UI::{
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
        Input::KeyboardAndMouse::{GetDoubleClickTime, SetCapture},
        WindowsAndMessaging::*,
    },
};

use super::{
    caption::release_capture,
    native_window::{collect_events, sync_native_windows},
};
use crate::{
    app::App,
    error::EngineError,
    platform::{
        dispatcher::Dispatcher,
        windows::{composition, window::NativeWindow},
    },
    render::backend::Renderer,
    window::{Window, WindowId, WindowOptions},
};

/// 消费者：`pump_loop` 的定时器管理与 `wndproc` 的 `WM_TIMER` 分支。
/// 连续帧定时器 id（动画 / P17 HUD 共用；`wndproc` 用它唤醒；P09 probe 实测 1 ms → 16.0 ms）。
pub(super) const FRAME_TIMER_ID: usize = 1;

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

/// 当前消息的时间（`GetMessageTime()`，毫秒，与 `GetTickCount` 同源）。
///
/// **必须**在处理消息期间调用（系统记录的是「最近一次 `GetMessage`/`PeekMessage` 取到的
/// 消息」的时间），正是 `WndProc` 与合成事件所处的时机。
pub(super) fn message_ticks() -> u64 {
    // SAFETY: 无参数、无线程副作用的查询。
    u64::from(unsafe { GetMessageTime() } as u32)
}

/// 单调毫秒（`QueryPerformanceCounter` / `QueryPerformanceFrequency`）。
///
/// 用于把 `MSG.time` 换算成 `Instant`（R02-14）：两者都以系统启动为原点，
/// 因此同一个事件可以按「现在 −（现在 tick − 消息 tick）」回推。
pub(super) fn monotonic_millis() -> u64 {
    let mut frequency = 0i64;
    let mut counter = 0i64;
    // SAFETY: 两个都是无副作用的计时器查询。
    let ok = unsafe {
        (
            QueryPerformanceFrequency(&raw mut frequency),
            QueryPerformanceCounter(&raw mut counter),
        )
    };
    if ok.0.is_err() || ok.1.is_err() || frequency <= 0 || counter < 0 {
        return 0;
    }
    (u128::from(counter as u64) * 1_000 / u128::from(frequency as u64)) as u64
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
    // 定时器随宿主窗口销毁而失效：宿主窗口关闭后必须在新首窗上重建，否则其余窗口的
    // 动画/连续帧永远不再推进（P2-4）。归属判定用 `WindowId` 而不是 HWND 裸值：
    // 首窗销毁后系统可能把同一个 HWND 复用给新窗，裸值比较会「看起来仍匹配」而不重建
    // 定时器（复审 P2-8）。
    let mut frame_timer: Option<(crate::window::WindowId, isize, usize)> = None;
    loop {
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
        // 旧实现只在 `run` 启动时枚举一次，运行时 `App::create_window` 得到的窗口永远没有
        // 原生窗口与渲染器（`App::frame` 对着无 renderer 的 Window 空转）；应用自己调
        // `App::close_window` 也只删 App 状态，留下幽灵原生窗口并继续收事件。
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

        // ②c 交互反馈：光标 / 指针捕获 / 连续帧定时器（P10 §5.4 / §11.3、P17 HUD）。
        // 两者都是**每窗口**状态，请求本身携带 `WindowId`（复审 P1-3 / P2-2）：
        // - 光标：`WM_SETCURSOR` 各窗口读自己的 `WndProcState`；广播会把 A 窗口的 I-Beam
        //   写进 B 窗口，而 B 的解析值仍是 Default ⇒ 不会再发变化 ⇒ B 永久显示错光标。
        // - 捕获：`SetCapture` 只对目标 HWND 生效；固定给首窗会导致非首窗拖拽在鼠标移出
        //   窗口后收不到 `WM_LBUTTONUP`，会话永久卡住。
        if let Some((target, cursor)) = app.take_cursor_change()
            && let Some(native) = windows.iter_mut().find(|native| native.id() == target)
        {
            native.state_mut().set_cursor(cursor);
            // SAFETY: 系统光标句柄由 `LoadCursorW` 借用，进程不拥有；`SetCursor` 只影响本线程。
            unsafe { SetCursor(Some(cursor_handle(cursor))) };
        }
        for (target, capture) in app.take_capture_requests() {
            let Some(native) = windows.iter_mut().find(|native| native.id() == target) else {
                continue;
            };
            let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
            if capture {
                // SAFETY: `hwnd` 是本进程创建的窗口；捕获在拖拽结束/释放/失焦时释放。
                unsafe { SetCapture(hwnd) };
            } else {
                // 只有确实持有捕获时才置 suppress 并释放（见 `release_capture`）。
                release_capture(hwnd, native.state_mut());
            }
        }
        if let Some(native) = windows.first_mut() {
            let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
            // 定时器随宿主窗口销毁而失效：宿主已不是当前首窗 ⇒ 清掉记录，下面在新首窗重建
            // （否则首窗关闭后其余窗口的动画/连续帧永远不再推进，P2-4）。
            if frame_timer.is_some_and(|(owner, _, _)| owner != native.id()) {
                frame_timer = None;
            }
            // a11y 到期补发也借用这个定时器：`a11y_retry_pending` 最多持续 A11Y_THROTTLE
            // （100 ms），到期交付后自动停表。
            let needs_tick = app.needs_continuous_frames() || app.a11y_retry_pending();
            match (needs_tick, frame_timer) {
                (true, None) => {
                    // SAFETY: 1 ms 定时器（probe：实测 16.0 ms）；`KillTimer` 在同一线程。
                    let id = unsafe { SetTimer(Some(hwnd), FRAME_TIMER_ID, 1, None) };
                    if id != 0 {
                        frame_timer = Some((native.id(), native.hwnd_raw(), id));
                    }
                }
                (false, Some((_, owner, id))) => {
                    // SAFETY: `id` 由本线程在 `owner` 上 `SetTimer` 产生。
                    let owner = HWND(owner as *mut core::ffi::c_void);
                    let _ = unsafe { KillTimer(Some(owner), id) };
                    frame_timer = None;
                }
                _ => {}
            }
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

        // ⑥ 阻塞等待（I3）：没有消息时停在这里。
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

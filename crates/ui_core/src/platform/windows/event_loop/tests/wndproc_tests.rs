//! `WndProc` 消息分派与 `WndProcState` 的集成单测（真实隐藏窗口）。
//!
//! 非客户区全部交系统（R07-2）：
//! - `WM_NCCALCSIZE` / `WM_NCHITTEST` / `WM_NCLBUTTONDOWN` 不改写，
//!   `nonclient_hit_table_returns_system_hit_codes` 钉死「命中码必须由系统给」这个不变量
//!   （改写了它，系统的 move / size 模态循环就拿不到 `HTCAPTION` / `HTLEFT`，Aero Snap /
//!   双击最大化 / 原生 resize 全部失效）；
//! - size 模态循环里的重排 + 重绘由 `WM_SIZE` 直接驱动（`size_move_*` 用例）。
//!
//! 停在 `WM_ENTERSIZEMOVE` + `WM_SIZE` 这一层：系统模态循环本身无法在单测里进入
//! （`SendMessage(WM_NCLBUTTONDOWN)` 会阻塞到用户松手），那条接线由真机拖动探针验收。

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use windows::Win32::{
    Foundation::{HWND, LPARAM, POINT, RECT, WPARAM},
    Graphics::Gdi::ClientToScreen,
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        HiDpi::GetSystemMetricsForDpi,
        Input::{
            Ime::{GCS_RESULTSTR, ImmAssociateContext, ImmGetContext, ImmReleaseContext},
            KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture},
        },
        WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, GWLP_USERDATA, GetWindowRect, HTBOTTOMRIGHT, HTCAPTION,
            HTCLIENT, HTCLOSE, HTLEFT, HTMAXBUTTON, HTMINBUTTON, HTSYSMENU, HTTOPLEFT, MINMAXINFO,
            MSG, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PM_REMOVE, PeekMessageW, RegisterClassW,
            SM_CXPADDEDBORDER, SM_CXSIZEFRAME, SPI_SETANIMATION, SPI_SETWORKAREA, SendMessageW,
            SetWindowLongPtrW, WINDOW_EX_STYLE, WM_APP, WM_CAPTURECHANGED, WM_DISPLAYCHANGE,
            WM_DPICHANGED, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_GETMINMAXINFO, WM_IME_CHAR,
            WM_IME_COMPOSITION, WM_IME_ENDCOMPOSITION, WM_IME_SETCONTEXT, WM_IME_STARTCOMPOSITION,
            WM_INPUTLANGCHANGE, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_MOVE, WM_NCHITTEST,
            WM_POWERBROADCAST, WM_SETTINGCHANGE, WM_SIZE, WM_TIMER, WNDCLASSW, WS_OVERLAPPEDWINDOW,
        },
    },
};

use super::super::{WndProcState, pump::A11Y_TIMER_ID, wndproc};
use crate::{
    app::App,
    event::ImeEvent,
    math::{Rect, Scale, Size},
    platform::{
        dispatcher::{Dispatcher, UserTask, WM_APP_COMPOSITION_WAKE, WM_APP_FRAME_WAKE},
        event::PlatformEvent,
        windows::{
            dark_mode, dpi,
            ime::{self, ImeApplied, ImeCommand},
            placement,
            window::NativeWindow,
        },
    },
    window::{WindowId, WindowOptions},
};

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 建一个**隐藏**窗口并挂上生产 `WndProc` + `WndProcState`（`GWLP_USERDATA` 指向它）。
fn hidden_window(tag: &str, dispatcher: Arc<Dispatcher>) -> (HWND, *mut WndProcState, Vec<u16>) {
    hidden_window_sized(tag, dispatcher, 320, 240)
}

/// 与 [`hidden_window`] 相同，但窗口外框尺寸可指定。
///
/// 模态循环用例要一个足够大的窗口（`WM_GETMINMAXINFO` 的 `ptMinTrackSize` 随 DPI 放大），
/// 但不需要真的拖动 —— 尺寸只影响窗口本身的创建参数。
fn hidden_window_sized(
    tag: &str,
    dispatcher: Arc<Dispatcher>,
    width: i32,
    height: i32,
) -> (HWND, *mut WndProcState, Vec<u16>) {
    hidden_window_with_limits(
        tag,
        dispatcher,
        placement::ClientLimits::default(),
        width,
        height,
    )
}

/// 与 [`hidden_window`] 相同，但 `WndProcState` 的客户区尺寸约束可指定（B3 的
/// `WM_GETMINMAXINFO` 用例：约束来源必须是建窗时解析出的 `WindowOptions`）。
fn hidden_window_with_limits(
    tag: &str,
    dispatcher: Arc<Dispatcher>,
    limits: placement::ClientLimits,
    width: i32,
    height: i32,
) -> (HWND, *mut WndProcState, Vec<u16>) {
    let class_name = wide(tag);
    // SAFETY: 进程级模块句柄；类名是 NUL 结尾的宽字符串，`RegisterClassW` 同步拷贝。
    let instance = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
    let class = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    // SAFETY: 同上；每个测试用唯一的类名，只注册一次。
    let atom = unsafe { RegisterClassW(&class) };
    assert_ne!(atom, 0, "RegisterClassW");
    // SAFETY: 类已注册；`WS_OVERLAPPEDWINDOW` 不含 `WS_VISIBLE` ⇒ 窗口保持隐藏。
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            windows::core::PCWSTR(class_name.as_ptr()),
            windows::core::PCWSTR(class_name.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            width,
            height,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
    .expect("CreateWindowExW");
    let state = match WndProcState::new(dispatcher, hwnd, WindowId::for_test(1), limits, None) {
        Ok(state) => Box::into_raw(Box::new(state)),
        Err(error) => {
            // SAFETY: 窗口由本函数创建、尚未交给其它所有者。
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            panic!("WndProcState::new: {error}");
        }
    };
    // SAFETY: `hwnd` 由本函数创建；`state` 的所有权由测试通过 `destroy_hidden_window` 回收。
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) };
    (hwnd, state, class_name)
}

/// 先销毁窗口（此后不会再有消息），再回收 `WndProcState`（与 `NativeWindow::drop` 同序）。
fn destroy_hidden_window(hwnd: HWND, state: *mut WndProcState) {
    // SAFETY: 窗口与状态都由本测试独占，且只回收一次。
    unsafe {
        let _ = DestroyWindow(hwnd);
        drop(Box::from_raw(state));
    }
}

/// `WM_SIZE` 的 `lParam`：低 16 位 = 客户区物理宽，高 16 位 = 客户区物理高。
fn encoded_client(width: u16, height: u16) -> isize {
    (((height as u32) << 16) | width as u32) as isize
}

/// `WM_MOVE` / `WM_NCHITTEST` 的 `lParam`：低 16 位 = x、高 16 位 = y（两段有符号）。
fn encoded_point(x: i32, y: i32) -> isize {
    ((((y as u32) << 16) | (x as u32 & 0xFFFF)) as i32) as isize
}

/// 用一个**逻辑客户区坐标**向窗口发 `WM_NCHITTEST`（lParam 是屏幕物理坐标），返回命中码。
fn hit_test(hwnd: HWND, scale: Scale, client_origin: POINT, x: f32, y: f32) -> u32 {
    let point = POINT {
        x: client_origin.x + (x * scale.factor()).round() as i32,
        y: client_origin.y + (y * scale.factor()).round() as i32,
    };
    // SAFETY: 窗口由调用方保证存活；`WM_NCHITTEST` 的 lParam 是两个 16 位屏幕坐标。
    let result = unsafe {
        SendMessageW(
            hwnd,
            WM_NCHITTEST,
            None,
            Some(LPARAM(encoded_point(point.x, point.y))),
        )
    };
    result.0 as u32
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

/// 两阶段驱动一次 IME 命令（与生产 `NativeWindow::sync_ime_enabled` 同序）。
fn drive_ime(raw: &mut WndProcState, hwnd: HWND, command: Option<ImeCommand>) {
    if let Some(command) = command {
        let applied: ImeApplied = ime::execute(hwnd, command);
        raw.note_ime_applied(applied);
    }
}

/// 等价的 `App::set_ime_enabled` 平台侧动作（计划 → 执行 → 写回）。
fn set_ime_enabled(raw: &mut WndProcState, hwnd: HWND, enabled: bool) {
    let command = raw.plan_ime_enabled(enabled);
    drive_ime(raw, hwnd, command);
}

/// 复审 F2：不持有捕获时引擎的 `plan_release_capture` 不得留下 suppress 残留
/// （否则下一次真实会话丢失会被误当成「自己释放」而吞掉取消）。
#[test]
fn release_capture_without_ownership_leaves_no_suppress() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, class_name) = hidden_window("KytrixReleaseCaptureTest", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配、生命周期到 `destroy_hidden_window`。
    let state = unsafe { &mut *state };
    // SAFETY: 只读查询当前线程的捕获窗口。
    assert!(unsafe { GetCapture() } != hwnd, "前置：本窗口不持有捕获");
    assert!(
        !super::super::pump::plan_release_capture(hwnd, state),
        "没有捕获时不得计划释放（也不得置 suppress：会吞掉下一次真实取消）"
    );
    assert!(!state.suppress_capture_cancel);
    destroy_hidden_window(hwnd, state as *mut WndProcState);
    // 占位：保证 `class_name` 活到窗口销毁之后。
    let _ = class_name;
}

/// AR3 §1.2 P1：`ReleaseCapture` 必须**两阶段**执行 —— 计划阶段只置 suppress（不持有借用时
/// 才真正调用 API）。
///
/// 失败面：在 `&mut WndProcState` 借用里直接调 `ReleaseCapture`，它会**同步**派发
/// `WM_CAPTURECHANGED` 并重入生产 `WndProc`，后者从 `GWLP_USERDATA` 再构造一个
/// `&mut WndProcState` ⇒ 同一对象两个活跃 `&mut`（别名 UB，与 R07-7 的 IMM32 同一条纪律）。
/// 本用例钉住「第一阶段不释放、suppress 先到位；第二阶段才释放、且 suppress 被消费」。
#[test]
fn releasing_the_pointer_capture_is_two_phase() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, class_name) = hidden_window("KytrixReleaseCaptureTwoPhase", dispatcher);
    // SAFETY: 只在当前线程设置/查询捕获；`hwnd` 由本用例独占。
    unsafe { SetCapture(hwnd) };
    // SAFETY: 只读查询；上一步已把捕获交给本窗口。
    assert!(unsafe { GetCapture() } == hwnd, "前置：本窗口持有捕获");
    let raw = unsafe { &mut *state };
    assert!(
        super::super::pump::plan_release_capture(hwnd, raw),
        "持有捕获时必须计划释放"
    );
    // SAFETY: 只读查询 —— 计划阶段不能已经调用过 `ReleaseCapture`。
    assert!(
        unsafe { GetCapture() } == hwnd,
        "第一阶段不得调用 ReleaseCapture（会在持有 &mut WndProcState 时重入 WndProc）"
    );
    assert!(
        raw.suppress_capture_cancel,
        "第一阶段必须先置 suppress：重入的 WndProc 才能吞掉自己释放产生的 WM_CAPTURECHANGED"
    );
    // 第二阶段：不持有任何借用，执行 ReleaseCapture（同步派发 WM_CAPTURECHANGED → 生产 WndProc）。
    // SAFETY: 本线程持有捕获；调用只影响本线程，同步派发的消息由已注册的生产 WndProc 处理。
    let _ = unsafe { ReleaseCapture() };
    // SAFETY: 只读查询。
    assert!(unsafe { GetCapture() } != hwnd, "第二阶段真正释放");
    assert!(
        !raw.suppress_capture_cancel,
        "重入的 WndProc 必须消费掉 suppress（否则下一次真实会话丢失会被吞）"
    );
    assert!(
        !raw.events
            .iter()
            .any(|event| matches!(event.event, PlatformEvent::PointerCancel)),
        "自己释放产生的捕获变化不得派发 PointerCancel"
    );
    destroy_hidden_window(hwnd, state);
    let _ = class_name;
}

/// 复审 F1/F2：suppress 只吞**同一次**自己释放产生的 `WM_CAPTURECHANGED`。
#[test]
fn suppress_consumes_one_capture_change_and_clears() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, class_name) = hidden_window("KytrixSuppressTest", dispatcher);
    // SAFETY: 同上。
    let raw = unsafe { &mut *state };
    raw.suppress_capture_cancel = true;
    // SAFETY: 隐藏窗口存活；消息同步派发到生产 `wndproc`。
    unsafe {
        let _ = SendMessageW(hwnd, WM_CAPTURECHANGED, None, None);
    }
    assert!(!raw.suppress_capture_cancel, "suppress 必须被消费并清零");
    assert!(
        !raw.events.iter().any(|event| matches!(
            event.event,
            crate::platform::event::PlatformEvent::PointerCancel
        )),
        "自己释放产生的捕获变化不得派发 PointerCancel"
    );
    destroy_hidden_window(hwnd, state);
    let _ = class_name;
}

/// R07-2 的连带语义：没有按键按下时的 `WM_CAPTURECHANGED` 不注入 `PointerCancel`。
///
/// 非客户区交系统之后，原生 move/size 模态循环会自己 `SetCapture`/`ReleaseCapture`，于是每次
/// 拖动/缩放窗口结束都会收到一条 `WM_CAPTURECHANGED`（此时按键已抬起）。不区分就会给应用打
/// 一个幽灵「拖拽被取消」事件（清 hover、误触发 `DragPhase::Cancel`）。
#[test]
fn a_capture_change_without_buttons_down_is_not_a_cancel() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, class_name) = hidden_window("KytrixCaptureChangeIdle", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配、生命周期到 `destroy_hidden_window`。
    let raw = unsafe { &mut *state };
    let _ = raw.take_events();
    // 前提：本用例运行时没有物理按键按下（CI 恒成立；人工跑测试也不会按住鼠标）。
    assert!(!any_mouse_button_down(), "本用例要求没有物理按键按下");

    // SAFETY: 隐藏窗口存活；消息同步派发到生产 `wndproc`。
    unsafe {
        let _ = SendMessageW(hwnd, WM_CAPTURECHANGED, None, None);
    }

    assert!(
        !raw.take_events()
            .iter()
            .any(|event| matches!(event.event, PlatformEvent::PointerCancel)),
        "按键已抬起时的捕获变化只是系统收尾，不得注入 PointerCancel"
    );
    destroy_hidden_window(hwnd, state);
    let _ = class_name;
}

/// 有没有鼠标键处于按下状态（`GetKeyState` 高位）。
fn any_mouse_button_down() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
    };
    [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2]
        .into_iter()
        .any(|key| {
            // SAFETY: 无副作用的线程局部查询。
            (unsafe { GetKeyState(i32::from(key.0)) }) < 0
        })
}

/// P1-1 回归：`WM_APP_COMPOSITION_WAKE` 只唤醒泵，**不**置帧唤醒位。
#[test]
fn the_composition_wake_does_not_request_a_frame() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) =
        hidden_window("KytrixWndProcCompositionWake", Arc::clone(&dispatcher));
    // 窗口创建 / a11y 构造不应留下待处理帧；清一次让断言只测被发送的消息。
    let _ = dispatcher.take_frame_wake();

    // SAFETY: 窗口由本测试创建且仍存活；`SendMessageW` 同步进入 `wndproc`。
    unsafe {
        SendMessageW(hwnd, WM_APP_COMPOSITION_WAKE, None, None);
    }

    assert!(
        !dispatcher.take_frame_wake(),
        "合成通知必须只唤醒 GetMessageW，不得请求 App::frame"
    );
    destroy_hidden_window(hwnd, state);
}

/// R07-10 回归：**只有** a11y 补发定时器允许经 `WM_TIMER` 唤醒帧。
///
/// 连续帧已改由合成器节拍线程驱动；一旦有人把 `WM_TIMER` 又接回帧生产，这条断言会抓住
/// 「定时器驱动帧」的回归（旧实现的 1 ms 定时器正是 R07-10 的缺陷本体）。
#[test]
fn only_the_a11y_timer_wakes_a_frame() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcTimer", Arc::clone(&dispatcher));
    let _ = dispatcher.take_frame_wake();

    // SAFETY: 窗口由本测试创建且仍存活；`SendMessageW` 同步进入 `wndproc`。
    unsafe {
        SendMessageW(hwnd, WM_TIMER, Some(WPARAM(A11Y_TIMER_ID)), None);
    }
    assert!(
        dispatcher.take_frame_wake(),
        "a11y 补发定时器必须能唤醒泵（否则被节流的交付永远不重试）"
    );

    // SAFETY: 同上；任意其它 id 的定时器不是帧源。
    unsafe {
        SendMessageW(hwnd, WM_TIMER, Some(WPARAM(A11Y_TIMER_ID + 1)), None);
    }
    assert!(!dispatcher.take_frame_wake(), "非 a11y 定时器不得唤醒帧");
    destroy_hidden_window(hwnd, state);
}

/// R07-14 回归：显示 / 电源消息必须登记一次恢复并唤醒泵，且**不**翻译成平台事件
/// （恢复是平台层内部动作，应用层不该看到假输入）；准备挂起与无关设置项不触发恢复。
#[test]
fn display_and_power_messages_mark_one_pending_display_recovery() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) =
        hidden_window("KytrixWndProcDisplayRecovery", Arc::clone(&dispatcher));
    let _ = dispatcher.take_frame_wake();
    // SAFETY: `state` 由 `hidden_window` 分配；窗口存活到 `destroy_hidden_window`。
    let raw = unsafe { &mut *state };
    let _ = raw.take_events();

    // 显示器拓扑变化：登记恢复 + 唤醒帧；不交给默认过程（回 0）。
    // SAFETY: 窗口由本测试创建且仍存活；`SendMessageW` 同步进入生产 `wndproc`。
    let result = unsafe { SendMessageW(hwnd, WM_DISPLAYCHANGE, None, None) };
    assert_eq!(result.0, 0, "WM_DISPLAYCHANGE 必须返回 0");
    assert!(raw.take_display_recovery(), "拓扑变化必须登记恢复");
    assert!(dispatcher.take_frame_wake(), "拓扑变化必须唤醒一帧");

    // 睡眠唤醒：电源广播的约定是返回 `TRUE`。
    // SAFETY: 同上。
    let result = unsafe {
        SendMessageW(
            hwnd,
            WM_POWERBROADCAST,
            Some(WPARAM(PBT_APMRESUMEAUTOMATIC as usize)),
            None,
        )
    };
    assert_eq!(result.0, 1, "恢复广播必须应答 TRUE");
    assert!(raw.take_display_recovery(), "睡眠唤醒必须登记恢复");
    assert!(dispatcher.take_frame_wake(), "睡眠唤醒必须唤醒一帧");

    // 准备挂起不是恢复：不登记。
    // SAFETY: 同上。
    unsafe {
        SendMessageW(
            hwnd,
            WM_POWERBROADCAST,
            Some(WPARAM(PBT_APMSUSPEND as usize)),
            None,
        );
    }
    assert!(!raw.take_display_recovery(), "PBT_APMSUSPEND 不得登记恢复");

    // 工作区变化（任务栏 / 停靠栏）：登记恢复。
    // SAFETY: 同上。
    unsafe {
        SendMessageW(
            hwnd,
            WM_SETTINGCHANGE,
            Some(WPARAM(SPI_SETWORKAREA.0 as usize)),
            None,
        );
    }
    assert!(raw.take_display_recovery(), "工作区变化必须登记恢复");

    // 无关设置项（动画开关）：不登记 —— 主题变化不值得重建交换链。
    // SAFETY: 同上。
    unsafe {
        SendMessageW(
            hwnd,
            WM_SETTINGCHANGE,
            Some(WPARAM(SPI_SETANIMATION.0 as usize)),
            None,
        );
    }
    assert!(!raw.take_display_recovery(), "无关设置项不得登记恢复");
    assert!(
        raw.take_events().is_empty(),
        "恢复消息不得产生平台事件（应用层看不到显示恢复）"
    );
    destroy_hidden_window(hwnd, state);
}

/// B2 回归：系统主题变化（`WM_SETTINGCHANGE` + `ImmersiveColorSet`）必须把标题栏切到注册表
/// 里的最新主题值 —— `WndProcState` 的 test-only 观察点 + DWM 真值回读双重断言。
///
/// 前置：先把 DWM 属性设成与系统主题**相反**的值，因此「调用被删掉 / 没真正执行」时回读
/// 停在反值上，本条在任何系统主题的宿主下都会变红。
/// 变异守卫：删掉 `WM_SETTINGCHANGE` 分支里的 `dark_mode::set_titlebar` 调用（或把它的结果
/// 替换成常量）⇒ 观察点恒 `None`、回读停在反值，本条变红；应用侧仍必须收到
/// `AppearanceChanged`（两条线不能互相替代）。
#[test]
fn a_theme_change_applies_the_dark_titlebar() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, class_name) = hidden_window("KytrixWndProcDarkTitlebar", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配、生命周期到 `destroy_hidden_window`。
    let raw = unsafe { &mut *state };
    let expected = dark_mode::current()
        .expect("本机 HKCU\\…\\Personalize\\AppsUseLightTheme 必须可读（Win11 宿主）");
    assert_eq!(
        raw.last_dark_titlebar, None,
        "未收到主题变化前不得有标题栏观察值"
    );
    // 前置：DWM 属性停在反值，主题变化的同步必须把它翻回 `expected`。
    assert!(
        dark_mode::set_titlebar(hwnd, !expected),
        "前置：把标题栏设成与系统主题相反的值"
    );
    assert_eq!(read_titlebar(hwnd), !expected, "前置：DWM 回读为反值");

    let changed = wide("ImmersiveColorSet");
    // SAFETY: 窗口由本测试创建且仍存活；`lParam` 是本函数栈上的 NUL 结尾宽字符串，只在本次
    // 消息处理期使用；`SendMessageW` 同步进入生产 `wndproc`。
    unsafe {
        SendMessageW(
            hwnd,
            WM_SETTINGCHANGE,
            None,
            Some(LPARAM(changed.as_ptr() as isize)),
        );
    }

    assert_eq!(
        raw.last_dark_titlebar,
        Some(expected),
        "系统主题变化必须把 DWM 标题栏切到注册表里的主题值"
    );
    assert_eq!(
        read_titlebar(hwnd),
        expected,
        "DWM 真值必须跟随系统主题（不能只记观察点）"
    );
    let appearance = raw
        .take_events()
        .into_iter()
        .find_map(|event| match event.event {
            PlatformEvent::Appearance(dark) => Some(dark),
            _ => None,
        });
    assert_eq!(
        appearance,
        Some(expected),
        "应用仍必须收到 AppearanceChanged（平台同步不能替代事件）"
    );
    destroy_hidden_window(hwnd, state);
    let _ = class_name;
}

/// 从 DWM 回读标题栏明暗（B2 用例的「真值」断言用）。
fn read_titlebar(hwnd: HWND) -> bool {
    let mut value = windows::core::BOOL::default();
    // SAFETY: `hwnd` 由调用方保证存活；输出是指向栈上 `BOOL` 的可写指针，长度 = 4 字节。
    unsafe {
        windows::Win32::Graphics::Dwm::DwmGetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&raw mut value).cast(),
            std::mem::size_of::<windows::core::BOOL>() as u32,
        )
    }
    .expect("DwmGetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)");
    value.as_bool()
}

/// R07-13 回归：同一批内的 `WM_MOUSEMOVE` 合并成一条（保留最后一条的位置）。
///
/// 系统以最高 1000 Hz 投递鼠标移动；旧实现每条都入队 + 唤醒 + 应用级派发（每条都做命中
/// 测试与 hover 迁移），一批里 16 条只有最后一条对渲染有意义。
#[test]
fn mouse_moves_in_one_batch_coalesce_into_the_latest_position() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcMouseCoalesce", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配。
    let raw = unsafe { &mut *state };
    let _ = raw.take_events();

    let point =
        |x: i32, y: i32| LPARAM(((((y as u32) << 16) | (x as u32 & 0xFFFF)) as i32) as isize);
    // SAFETY: 窗口由本测试创建且仍存活；`SendMessageW` 同步进入生产 `wndproc`。
    unsafe {
        // 预热：第一条移动会注册 `TrackMouseEvent` 并补一条 `PointerEnter`（R02-4），
        // 它不属于本用例的观测范围。
        SendMessageW(hwnd, WM_MOUSEMOVE, None, Some(point(0, 0)));
    }
    let _ = raw.take_events();

    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_MOUSEMOVE, None, Some(point(10, 20)));
        SendMessageW(hwnd, WM_MOUSEMOVE, None, Some(point(30, 40)));
        SendMessageW(hwnd, WM_MOUSEMOVE, None, Some(point(50, 60)));
    }

    let events = raw.take_events();
    let moves: Vec<_> = events
        .iter()
        .filter_map(|event| match event.event {
            PlatformEvent::Pointer {
                x,
                y,
                kind: crate::platform::event::PointerKind::Move,
            } => Some((x, y)),
            _ => None,
        })
        .collect();
    assert_eq!(
        moves,
        vec![(50, 60)],
        "同批次的三条移动必须合并成一条（最后位置）"
    );
    destroy_hidden_window(hwnd, state);
}

/// 合并只在**连续**的移动之间生效：按钮事件必须切断合并链（顺序语义不能变）。
#[test]
fn a_button_event_breaks_the_mouse_move_coalescing() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcMouseBreak", dispatcher);
    // SAFETY: 同上。
    let raw = unsafe { &mut *state };
    let _ = raw.take_events();

    let point =
        |x: i32, y: i32| LPARAM(((((y as u32) << 16) | (x as u32 & 0xFFFF)) as i32) as isize);
    // SAFETY: 同上。
    unsafe {
        // 预热：第一条移动会补一条 `PointerEnter`，它不属于本用例的观测范围。
        SendMessageW(hwnd, WM_MOUSEMOVE, None, Some(point(0, 0)));
    }
    let _ = raw.take_events();

    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_MOUSEMOVE, None, Some(point(10, 20)));
        SendMessageW(hwnd, WM_LBUTTONDOWN, None, Some(point(30, 40)));
        SendMessageW(hwnd, WM_MOUSEMOVE, None, Some(point(50, 60)));
    }

    let events = raw.take_events();
    assert_eq!(
        events.len(),
        3,
        "移动 / 按下 / 移动 必须各自成条（合并链被按钮切断）"
    );
    assert!(matches!(
        events[1].event,
        PlatformEvent::Pointer {
            kind: crate::platform::event::PointerKind::Down(_),
            ..
        }
    ));
    destroy_hidden_window(hwnd, state);
}

/// R07-12 回归：`WM_DPICHANGED` 在 `WndProc` 内**同步**生效。
///
/// 断言三件事：
/// 1. 窗口按系统建议矩形落位（不等下一轮泵）；
/// 2. `WndProcState` 的 scale 立刻是新值（`WM_GETMINMAXINFO` 等读到新值）；
/// 3. **同批次后续**输入事件的 `StampedEvent::scale` 是新 scale（旧实现要等整批派发完
///    才应用 DPI，同批鼠标位移会用旧 scale 换算 ⇒ 跨屏拖动时指针跳变）。
#[test]
fn a_dpi_change_applies_immediately_and_stamps_later_events() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcDpi", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配。
    let raw = unsafe { &mut *state };
    let _ = raw.take_events();
    // 预热鼠标跟踪：第一条 `WM_MOUSEMOVE` 会注册 `TrackMouseEvent` 并补一条 `PointerEnter`，
    // 后者不属于本用例的观测范围（否则「同批后续移动」会多出一条 enter）。
    // SAFETY: 窗口由本测试创建且仍存活；`SendMessageW` 同步进入生产 `wndproc`。
    unsafe {
        SendMessageW(hwnd, WM_MOUSEMOVE, None, Some(LPARAM(0)));
    }
    let _ = raw.take_events();
    let suggested = RECT {
        left: 40,
        top: 50,
        right: 840,
        bottom: 650,
    };

    // SAFETY: 窗口由本测试创建且仍存活；`WM_DPICHANGED` 的 lParam 在消息处理期有效。
    unsafe {
        SendMessageW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::WM_DPICHANGED,
            Some(WPARAM(192)),
            Some(LPARAM((&raw const suggested) as isize)),
        );
    }

    let events = raw.take_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.event, PlatformEvent::Dpi { dpi: 192 })),
        "必须派发 Dpi{{192}}，实际 {events:?}"
    );
    assert_eq!(
        raw.scale().factor(),
        2.0,
        "WndProcState 必须立刻换到新 scale"
    );

    let mut placed = RECT::default();
    // SAFETY: 窗口存活；`placed` 是栈上输出。
    unsafe {
        GetWindowRect(hwnd, &mut placed).expect("GetWindowRect");
    }
    // 建议矩形（800×600）比 192 dpi 下的最小跟踪尺寸大，系统不会钳制 ⇒ 位置与尺寸逐字落地。
    assert!(
        raw.min_track_size.0 <= 800 && raw.min_track_size.1 <= 600,
        "前置：建议矩形必须大于最小跟踪尺寸（否则会被系统钳制），min_track_size={:?}",
        raw.min_track_size
    );
    assert_eq!(
        (
            placed.left,
            placed.top,
            placed.right - placed.left,
            placed.bottom - placed.top
        ),
        (40, 50, 800, 600),
        "必须同步套用系统建议矩形（不等下一轮泵）"
    );

    // 同批后续移动：`StampedEvent::scale` 必须是新 scale。
    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_MOUSEMOVE, None, Some(LPARAM(0x000A_0014)));
    }
    let later = raw.take_events();
    assert_eq!(later.len(), 1, "一条移动事件，实际 {later:?}");
    assert_eq!(
        later[0].scale.factor(),
        2.0,
        "同批后续输入必须用新 scale（R07-12）"
    );
    destroy_hidden_window(hwnd, state);
}

/// B3：`WM_GETMINMAXINFO` 的 `ptMinTrackSize` / `ptMaxTrackSize` 必须来自建窗时解析的
/// `WindowOptions::min_size` / `max_size`（客户区逻辑尺寸 → **窗口外框**物理像素，含 DPI
/// 换算）；`min > max` 时按 `max` 钳制（公共契约 min = max）；未配置 `max_size` 时必须保留
/// 系统默认值（`DefWindowProcW` 预填的屏幕上限），不能被引擎写成 0。
///
/// 变异守卫：删掉 `ptMinTrackSize` 赋值 / 删掉 `max_track_size` 的覆盖判断 ⇒ 对应断言变红。
#[test]
fn wm_getminmaxinfo_applies_the_configured_client_limits() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let options = WindowOptions {
        min_size: Some(Size::new(500.0, 300.0)),
        max_size: Some(Size::new(1200.0, 900.0)),
        ..WindowOptions::default()
    };
    let limits = placement::ClientLimits::from_options(&options);
    let (hwnd, state, class_name) =
        hidden_window_with_limits("KytrixWndProcMinMax", dispatcher, limits, 640, 480);
    let dpi = dpi::window_dpi(hwnd);
    let mut info = MINMAXINFO::default();

    // SAFETY: 窗口由本测试创建且仍存活；`WM_GETMINMAXINFO` 处理期只写 `lParam` 指向的栈上结构。
    unsafe {
        SendMessageW(
            hwnd,
            WM_GETMINMAXINFO,
            None,
            Some(LPARAM((&raw mut info) as isize)),
        );
    }

    let min = dpi::track_size(Size::new(500.0, 300.0), dpi);
    let max = dpi::track_size(Size::new(1200.0, 900.0), dpi);
    assert_eq!(
        (info.ptMinTrackSize.x, info.ptMinTrackSize.y),
        (min.0 as i32, min.1 as i32),
        "ptMinTrackSize 必须是配置值的窗口外框尺寸（含 DPI 换算，dpi={dpi}）"
    );
    assert_eq!(
        (info.ptMaxTrackSize.x, info.ptMaxTrackSize.y),
        (max.0 as i32, max.1 as i32),
        "ptMaxTrackSize 必须是配置值的窗口外框尺寸（含 DPI 换算，dpi={dpi}）"
    );
    destroy_hidden_window(hwnd, state);
    let _ = class_name;

    // `min > max` ⇒ 按 max 钳制：两个跟踪尺寸相等。`ptMaxTrackSize` 的系统默认值必须仍被
    // 覆盖成钳制后的值（否则用户可以拖到 max 以外）。
    let dispatcher = Dispatcher::new();
    let options = WindowOptions {
        min_size: Some(Size::new(900.0, 700.0)),
        max_size: Some(Size::new(600.0, 500.0)),
        ..WindowOptions::default()
    };
    let limits = placement::ClientLimits::from_options(&options);
    let (hwnd, state, class_name) =
        hidden_window_with_limits("KytrixWndProcMinMaxClamped", dispatcher, limits, 640, 480);
    // SAFETY: 同上的所有权约定。
    let _raw = unsafe { &mut *state };
    let dpi = dpi::window_dpi(hwnd);
    let mut info = MINMAXINFO::default();
    // SAFETY: 同上。
    unsafe {
        SendMessageW(
            hwnd,
            WM_GETMINMAXINFO,
            None,
            Some(LPARAM((&raw mut info) as isize)),
        );
    }
    let clamped = dpi::track_size(Size::new(600.0, 500.0), dpi);
    assert_eq!(
        (info.ptMinTrackSize.x, info.ptMinTrackSize.y),
        (clamped.0 as i32, clamped.1 as i32),
        "min > max 时最小跟踪尺寸按 max 钳制"
    );
    assert_eq!(
        (info.ptMaxTrackSize.x, info.ptMaxTrackSize.y),
        (clamped.0 as i32, clamped.1 as i32),
        "钳制后 min == max"
    );
    destroy_hidden_window(hwnd, state);
    let _ = class_name;
}

/// B3：`WM_MOVE` 必须翻译成 `PlatformEvent::Moved`（**屏幕物理坐标**）；`WM_DPICHANGED`
/// 建议矩形重排期间系统同步派发的 `WM_MOVE` 被过滤（公共契约：DPI 路径只产生 `DpiChanged`），
/// 且过滤只覆盖那一次重排 —— 之后的普通移动照常派发。
///
/// 变异守卫：删掉 `translate` 的 `WM_MOVE` 分支 ⇒ 第一条断言变红；删掉 `suppress_move`
/// 判定 ⇒ DPI 重排期间多出一条 `Moved`，第二条断言变红。
#[test]
fn wm_move_is_translated_and_dpi_repositioning_moves_are_filtered() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, class_name) = hidden_window("KytrixWndProcMove", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配、生命周期到 `destroy_hidden_window`。
    let raw = unsafe { &mut *state };
    let _ = raw.take_events();

    // 普通移动（负屏幕坐标也是合法的多屏位置）：一条 `Moved`，坐标不换算。
    // SAFETY: 窗口由本测试创建且仍存活；`SendMessageW` 同步进入生产 `wndproc`。
    unsafe {
        SendMessageW(hwnd, WM_MOVE, None, Some(LPARAM(encoded_point(-1920, 120))));
    }
    let events = raw.take_events();
    assert_eq!(events.len(), 1, "一条移动事件，实际 {events:?}");
    assert_eq!(events[0].event, PlatformEvent::Moved { x: -1920, y: 120 });
    assert_eq!(events[0].scale, raw.scale(), "Moved 必须带消息时刻的 scale");

    // DPI 建议矩形重排：期间同步派发的 `WM_MOVE` 被过滤，DPI 事件仍在。
    let suggested = RECT {
        left: 40,
        top: 50,
        right: 840,
        bottom: 650,
    };
    // SAFETY: 窗口存活；`WM_DPICHANGED` 的 lParam 在消息处理期有效。
    unsafe {
        SendMessageW(
            hwnd,
            WM_DPICHANGED,
            Some(WPARAM(192)),
            Some(LPARAM((&raw const suggested) as isize)),
        );
    }
    assert!(!raw.suppress_move, "过滤标记必须在消息处理结束时复位");
    let events = raw.take_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.event, PlatformEvent::Dpi { dpi: 192 })),
        "DPI 事件仍必须派发，实际 {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.event, PlatformEvent::Moved { .. })),
        "DPI 重排期间产生的 WM_MOVE 不得派发 Event::Moved（公共契约），实际 {events:?}"
    );

    // 过滤不是永久的：之后的普通移动照常派发。
    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_MOVE, None, Some(LPARAM(encoded_point(100, 200))));
    }
    assert!(
        raw.take_events()
            .iter()
            .any(|event| matches!(event.event, PlatformEvent::Moved { x: 100, y: 200 })),
        "DPI 重排结束后 WM_MOVE 必须恢复派发"
    );
    destroy_hidden_window(hwnd, state);
    let _ = class_name;
}

/// X2：`WM_NCHITTEST` 命中应用声明的**自绘标题栏拖动区**时返回 `HTCAPTION`（系统 move
/// 模态循环的入场券：双击最大化/还原、Aero Snap、Snap Layouts 全部照常）；区域之外仍是系统的
/// `HTCLIENT`；清空区域后恢复系统命中码。
///
/// 变异守卫：删掉 `WM_NCHITTEST` 分支里的拖动区判定 ⇒ 第一条断言变红（返回 `HTCLIENT`）。
#[test]
fn drag_regions_hit_test_as_caption_and_clear_back_to_the_system() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, class_name) = hidden_window("KytrixWndProcDragRegion", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配、生命周期到 `destroy_hidden_window`。
    let raw = unsafe { &mut *state };
    let scale = raw.scale();
    let mut client_origin = POINT::default();
    // SAFETY: 窗口由本测试创建且仍存活；只做坐标换算。
    assert!(
        unsafe { ClientToScreen(hwnd, &mut client_origin) }.as_bool(),
        "ClientToScreen"
    );
    raw.set_drag_regions(&[Rect::new(0.0, 0.0, 200.0, 24.0)]);

    assert_eq!(
        hit_test(hwnd, scale, client_origin, 10.0, 10.0),
        HTCAPTION,
        "拖动区内必须返回 HTCAPTION（系统 move 模态循环只认它）"
    );
    assert_eq!(
        hit_test(hwnd, scale, client_origin, 10.0, 100.0),
        HTCLIENT,
        "拖动区外仍是客户区（非客户区语义不变）"
    );
    assert_eq!(
        hit_test(hwnd, scale, client_origin, 150.0, 30.0),
        HTCLIENT,
        "拖动区下沿之外仍是客户区"
    );

    raw.set_drag_regions(&[]);
    assert_eq!(
        hit_test(hwnd, scale, client_origin, 10.0, 10.0),
        HTCLIENT,
        "清空拖动区后必须恢复系统命中码"
    );
    destroy_hidden_window(hwnd, state);
    let _ = class_name;
}

/// X2：泵的每轮对账把 `App::window(id).drag_regions()` 同步进 `WndProcState` —— 运行时设置 /
/// 更新 / 清空立即反映到命中测试（应用在布局变化后重新声明区域的路径）。
#[test]
fn the_pump_sync_copies_drag_regions_into_the_wndproc_state() {
    let runtime = runtime();
    let mut app = App::new(runtime.handle().clone());
    let options = WindowOptions::default();
    let id = app.create_window(options.clone());
    let mut native = NativeWindow::create(id, &options, None, Dispatcher::new(), None)
        .expect("NativeWindow::create");
    let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
    let scale = native.scale();
    let mut client_origin = POINT::default();
    // SAFETY: 窗口由 `native` 持有且仍存活；只做坐标换算。
    assert!(
        unsafe { ClientToScreen(hwnd, &mut client_origin) }.as_bool(),
        "ClientToScreen"
    );

    // 首次声明：区域同步到 WndProcState 并生效。
    let first = [Rect::new(0.0, 0.0, 320.0, 32.0)];
    app.set_drag_regions(id, &first);
    native.sync_drag_regions(&app);
    assert_eq!(native.state_mut().drag_regions(), &first);
    assert_eq!(
        hit_test(hwnd, scale, client_origin, 10.0, 10.0),
        HTCAPTION,
        "同步后的区域必须命中 HTCAPTION"
    );

    // 运行时更新：新区域覆盖旧区域（旧区域的剩余部分回到系统命中码）。
    let second = [Rect::new(0.0, 0.0, 120.0, 16.0)];
    app.set_drag_regions(id, &second);
    native.sync_drag_regions(&app);
    assert_eq!(native.state_mut().drag_regions(), &second);
    assert_eq!(
        hit_test(hwnd, scale, client_origin, 200.0, 10.0),
        HTCLIENT,
        "更新后旧区域之外的命中必须回到系统命中码"
    );

    // 清空：同步为空表，区域不再生效。
    app.set_drag_regions(id, &[]);
    native.sync_drag_regions(&app);
    assert!(native.state_mut().drag_regions().is_empty());
    assert_eq!(hit_test(hwnd, scale, client_origin, 10.0, 10.0), HTCLIENT);
}

/// R07-2 回归：`WM_NCHITTEST` 必须返回**系统命中码**。
///
/// `HTCAPTION` / `HT*` 是系统 move/size 模态循环的入场券：`DefWindowProcW` 只有在按下
/// `HTCAPTION` 时才进入原生拖动循环（Aero Snap / Snap Layouts / 双击最大化 / 右键系统菜单
/// 都在里面）。引擎接管过这些区域 ⇒ 全部系统手势消失。这张命中表把「系统拥有非客户区」
/// 钉死在测试里。
#[test]
fn nonclient_hit_table_returns_system_hit_codes() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcHitTest", dispatcher);

    let mut window = RECT::default();
    let mut client = RECT {
        right: 320,
        bottom: 240,
        ..RECT::default()
    };
    // SAFETY: 窗口存活；两个输出都是栈上值。
    unsafe {
        GetWindowRect(hwnd, &mut window).expect("GetWindowRect");
        let mut origin = POINT::default();
        let _ = ClientToScreen(hwnd, &mut origin);
        client.left = origin.x;
        client.top = origin.y;
        client.right += origin.x;
        client.bottom += origin.y;
    }
    let middle_x = (client.left + client.right) / 2;
    let middle_y = (client.top + client.bottom) / 2;

    let hit = |point: POINT| -> u32 {
        // SAFETY: 窗口存活；`WM_NCHITTEST` 的 lParam 是两个 16 位屏幕坐标。
        let encoded = ((((point.y as u32) << 16) | (point.x as u32 & 0xFFFF)) as i32) as isize;
        let result = unsafe { SendMessageW(hwnd, WM_NCHITTEST, None, Some(LPARAM(encoded))) };
        result.0 as u32
    };

    assert_eq!(
        hit(POINT {
            x: middle_x,
            y: middle_y
        }),
        HTCLIENT,
        "客户区必须是 HTCLIENT（引擎自绘内容区）"
    );
    let caption_y = (window.top + client.top) / 2;
    assert_eq!(
        hit(POINT {
            x: middle_x,
            y: caption_y
        }),
        HTCAPTION,
        "标题栏必须是 HTCAPTION：系统的 move 模态循环（Snap / 双击最大化）只认它"
    );
    assert_eq!(
        hit(POINT {
            x: window.left + 1,
            y: middle_y
        }),
        HTLEFT,
        "左边框必须是 HTLEFT：size 模态循环只认系统给的命中码"
    );
    assert_eq!(
        hit(POINT {
            x: window.left + 1,
            y: window.top + 1
        }),
        HTTOPLEFT,
        "左上角必须是 HTTOPLEFT"
    );
    assert_eq!(
        hit(POINT {
            x: window.right - 1,
            y: window.bottom - 1
        }),
        HTBOTTOMRIGHT,
        "窗口右下角必须是 HTBOTTOMRIGHT（系统靠它进入 size 模态循环）"
    );
    // 系统菜单图标：标题栏最左侧（边框之后）。允许命中 `HTSYSMENU`（双击关闭 / 单击出菜单
    // 都是系统行为）。
    let dpi = dpi::window_dpi(hwnd);
    // SAFETY: 无参数、无副作用的系统度量查询。
    let frame = unsafe {
        GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
    }
    .max(1);
    assert_eq!(
        hit(POINT {
            x: window.left + frame + 2,
            y: caption_y
        }),
        HTSYSMENU,
        "标题栏图标区必须是 HTSYSMENU（系统菜单 / 双击关闭）"
    );
    // 右侧按钮：具体是哪个取决于标题栏宽度，但必须仍在系统的按钮集合里（引擎不接管）。
    let button = hit(POINT {
        x: window.right - frame - 4,
        y: caption_y,
    });
    assert!(
        matches!(button, h if h == HTCLOSE || h == HTMAXBUTTON || h == HTMINBUTTON),
        "标题栏右侧按钮必须由系统命中（HTMINBUTTON/HTMAXBUTTON/HTCLOSE 之一），实际 {button}"
    );

    destroy_hidden_window(hwnd, state);
}

/// P2-7 回归：应用禁用 IME 后窗口过程吞掉全部 `WM_IME_*`（不派发 `ImeEvent`），
/// 重新启用后恢复派发。
///
/// 只验证窗口过程的状态机（不真正给测试窗口绑 IME）。注意：实测隐藏窗口创建时**已有**
/// 默认输入上下文，因此禁用走「保存旧 HIMC」分支、启用走「恢复保存值」分支；
/// `saved_context == None → IACE_DEFAULT` 分支需要在真机 IME 环境下覆盖。
#[test]
fn disabled_ime_swallows_composition_events_until_reenabled() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcImeGate", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配、生命周期到 `destroy_hidden_window`。
    let raw = unsafe { &mut *state };

    // 基线：启用态的组合开始必须派发 `ImeEvent::Start`。
    // SAFETY: 窗口由本测试创建且仍存活；`SendMessageW` 同步进入生产 `wndproc`。
    unsafe {
        SendMessageW(hwnd, WM_IME_STARTCOMPOSITION, None, None);
    }
    assert!(
        raw.take_events()
            .iter()
            .any(|event| matches!(event.event, PlatformEvent::Ime(ImeEvent::Start))),
        "启用态必须派发 ImeEvent::Start"
    );

    set_ime_enabled(raw, hwnd, false);
    assert!(!raw.ime_enabled(), "禁用状态必须被记账");
    // 组合开始 / 提交 / IME 字符三条路径都要被吞掉。
    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_IME_STARTCOMPOSITION, None, None);
        SendMessageW(
            hwnd,
            WM_IME_COMPOSITION,
            None,
            Some(LPARAM(GCS_RESULTSTR.0 as isize)),
        );
        SendMessageW(hwnd, WM_IME_CHAR, Some(WPARAM(b'a' as usize)), None);
    }
    assert!(
        raw.take_events().is_empty(),
        "禁用期间不得派发任何 Ime / Text 事件"
    );

    set_ime_enabled(raw, hwnd, true);
    assert!(raw.ime_enabled(), "重新启用状态必须被记账");
    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_IME_STARTCOMPOSITION, None, None);
    }
    assert!(
        raw.take_events()
            .iter()
            .any(|event| matches!(event.event, PlatformEvent::Ime(ImeEvent::Start))),
        "重新启用后必须恢复派发"
    );

    destroy_hidden_window(hwnd, state);
}

/// R07-3：启用态下 `WM_IME_SETCONTEXT` 只做掩码后交默认过程，**不得**派发应用事件、
/// 也不得摘掉输入上下文（掩码位本身的断言在 `ime::tests` 的纯函数用例里）。
#[test]
fn an_enabled_setcontext_is_forwarded_without_events_or_detaching() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcImeSetContext", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配。
    let raw = unsafe { &mut *state };
    let _ = raw.take_events();

    // SAFETY: 窗口由本测试创建且仍存活。
    let baseline = unsafe { ImmGetContext(hwnd) };
    if baseline.is_invalid() {
        eprintln!("skip: hidden window has no IME context");
        destroy_hidden_window(hwnd, state);
        return;
    }
    // SAFETY: 句柄成对释放。
    unsafe {
        let _ = ImmReleaseContext(hwnd, baseline);
    }

    // SAFETY: 同上；wParam = TRUE（上下文激活），lParam 带组合窗标志。
    unsafe {
        SendMessageW(
            hwnd,
            WM_IME_SETCONTEXT,
            Some(WPARAM(1)),
            Some(LPARAM(
                windows::Win32::UI::Input::Ime::ISC_SHOWUICOMPOSITIONWINDOW as isize,
            )),
        );
    }

    assert!(
        raw.take_events().is_empty(),
        "WM_IME_SETCONTEXT 不是应用可见事件"
    );
    // SAFETY: 只读查询上下文，句柄成对释放。
    let still_attached = unsafe { ImmGetContext(hwnd) };
    assert!(
        !still_attached.is_invalid(),
        "启用态不得因为 SETCONTEXT 摘掉输入上下文"
    );
    // SAFETY: 同上。
    unsafe {
        let _ = ImmReleaseContext(hwnd, still_attached);
    }
    destroy_hidden_window(hwnd, state);
}

/// win-1 P3（任务 1）：禁用态下 IMM32 在焦点 / 输入法切换后可能把上下文**重新关联**回
/// 窗口；`WM_IME_SETCONTEXT` / `WM_INPUTLANGCHANGE` 必须幂等再摘除，且不覆盖最初保存的
/// 上下文（重新启用时恢复的仍是它）。
#[test]
fn disabled_ime_detaches_a_reassociated_context() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcImeReattach", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配、生命周期到 `destroy_hidden_window`。
    let raw = unsafe { &mut *state };

    // 基线上下文：禁用前窗口关联的 HIMC（实测隐藏窗口创建时系统就给默认上下文）。
    // SAFETY: 窗口由本测试创建且仍存活；`ImmGetContext` 的句柄成对释放。
    let baseline = unsafe { ImmGetContext(hwnd) };
    if baseline.is_invalid() {
        eprintln!("skip: hidden window has no IME context");
        destroy_hidden_window(hwnd, state);
        return;
    }
    // SAFETY: 同上。
    unsafe {
        let _ = ImmReleaseContext(hwnd, baseline);
    }

    set_ime_enabled(raw, hwnd, false);
    // SAFETY: 窗口存活。
    assert!(
        unsafe { ImmGetContext(hwnd) }.is_invalid(),
        "禁用后窗口不得再关联上下文"
    );

    // 模拟系统的重新关联（焦点 / 输入法切换）。若 `ImmAssociateContext` 本身同步派发了
    // `WM_IME_SETCONTEXT`，wndproc 会立刻再摘除；这里再显式发一次，覆盖「消息到了但没有
    // 摘除」的缺陷路径（两种时序都要通过）。
    // SAFETY: 窗口由本测试创建且仍存活；消息同步派发到生产 `wndproc`。
    unsafe {
        let _ = ImmAssociateContext(hwnd, baseline);
        SendMessageW(hwnd, WM_IME_SETCONTEXT, Some(WPARAM(1)), Some(LPARAM(0)));
    }
    // SAFETY: 窗口存活。
    assert!(
        unsafe { ImmGetContext(hwnd) }.is_invalid(),
        "WM_IME_SETCONTEXT 必须把重新关联的上下文再摘掉"
    );

    // 再关联 + 输入法切换通知：同样幂等。
    // SAFETY: 同上。
    unsafe {
        let _ = ImmAssociateContext(hwnd, baseline);
        SendMessageW(hwnd, WM_INPUTLANGCHANGE, Some(WPARAM(0)), Some(LPARAM(0)));
    }
    // SAFETY: 窗口存活。
    assert!(
        unsafe { ImmGetContext(hwnd) }.is_invalid(),
        "WM_INPUTLANGCHANGE 必须把重新关联的上下文再摘掉"
    );

    // 重新启用：恢复的必须是最初保存的那个上下文（再摘除没有把它覆盖掉）。
    set_ime_enabled(raw, hwnd, true);
    // SAFETY: 窗口存活。
    let restored = unsafe { ImmGetContext(hwnd) };
    assert_eq!(restored, baseline, "重新启用必须恢复最初保存的上下文");
    // SAFETY: 句柄成对释放。
    unsafe {
        let _ = ImmReleaseContext(hwnd, restored);
    }
    destroy_hidden_window(hwnd, state);
}

/// `WM_SIZE` 的 `Resized` 事件必须用**该消息的 scale** 换算逻辑尺寸（R07-12 的另一半）：
/// `WM_SIZE` 与 `WM_DPICHANGED` 同批到达时，用窗口侧旧 scale 会把逻辑尺寸算错。
#[test]
fn a_resize_event_is_stamped_with_the_message_scale() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcResizeStamp", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配。
    let raw = unsafe { &mut *state };
    let _ = raw.take_events();

    // SAFETY: 窗口由本测试创建且仍存活。
    unsafe {
        SendMessageW(
            hwnd,
            WM_SIZE,
            Some(WPARAM(
                windows::Win32::UI::WindowsAndMessaging::SIZE_RESTORED as usize,
            )),
            Some(LPARAM(640 | (480 << 16))),
        );
    }

    let events = raw.take_events();
    assert_eq!(events.len(), 1, "一条 Resize 事件，实际 {events:?}");
    assert_eq!(events[0].scale, raw.scale());
    assert!(matches!(
        events[0].event,
        PlatformEvent::Resize {
            width: 640,
            height: 480
        }
    ));
    destroy_hidden_window(hwnd, state);
}

/// 任意未识别的消息不得被引擎吞掉：必须落到 `DefWindowProcW`（`WM_APP` 的默认处理是
/// 「无动作」，但只要我们把返回值改写就会破坏系统语义）。
#[test]
fn unhandled_messages_fall_through_to_the_default_procedure() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcFallThrough", dispatcher);
    // SAFETY: 窗口由本测试创建且仍存活；`SendMessageW` 同步进入生产 `wndproc`。
    let result = unsafe { SendMessageW(hwnd, WM_APP + 0x42, None, None) };
    assert_eq!(result.0, 0, "未识别消息必须按默认过程返回");

    // 未识别消息也不得产生应用事件。
    // SAFETY: `state` 由 `hidden_window` 分配。
    let raw = unsafe { &mut *state };
    assert!(raw.take_events().is_empty());
    destroy_hidden_window(hwnd, state);
}

// ── size 模态循环里的真实帧（系统拥有交互，`WM_SIZE` 驱动重排 + 重绘）──────────────

/// `WM_ENTERSIZEMOVE` 之后每一条 `WM_SIZE` 都必须**立刻**出一帧真实帧。
///
/// 这是「resize 时真实重排 + 重绘」的唯一接线点：系统 size 模态循环期间泵的 ④ 不运行，
/// 重排 / 重绘只能由系统分派给 `WndProc` 的 `WM_SIZE` 直接驱动（Qt / Chrome 同构）。
/// 断言两件事：窗口的客户区尺寸按消息更新、`FrameIndex` 前进（真的提交了一帧）。
#[test]
fn a_size_move_resize_drives_a_real_frame() {
    let runtime = runtime();
    let mut app = App::new(runtime.handle().clone());
    let options = WindowOptions::default();
    let id = app.create_window(options.clone());
    let app = std::rc::Rc::new(std::cell::RefCell::new(app));
    let dispatcher = Dispatcher::new();
    let native = NativeWindow::create(id, &options, None, Arc::clone(&dispatcher), Some(&app))
        .expect("NativeWindow");
    let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
    let before = app.borrow().window(id).expect("window").frame_index();

    // 系统进入 size 模态循环。
    // SAFETY: 窗口由 `native` 持有且仍存活；`SendMessageW` 同步进入生产 `wndproc`。
    unsafe {
        SendMessageW(hwnd, WM_ENTERSIZEMOVE, None, None);
        SendMessageW(hwnd, WM_SIZE, None, Some(LPARAM(encoded_client(800, 600))));
    }

    let window = app.borrow();
    let window = window.window(id).expect("window");
    assert_eq!(
        window.size(),
        crate::math::Size::new(
            window.scale().to_logical(800.0),
            window.scale().to_logical(600.0)
        ),
        "WM_SIZE 必须把客户区写进窗口（逻辑尺寸 = 物理客户区 / scale）"
    );
    assert!(
        window.frame_index() > before,
        "模态循环里的 WM_SIZE 必须提交一帧真实帧（泵的 ④ 这段时间不运行）"
    );
}

/// `WM_EXITSIZEMOVE` 之后回到泵的常规路径：`WM_SIZE` 只记账（事件交给泵），不再直接出帧。
#[test]
fn a_size_after_the_modal_loop_is_only_recorded_for_the_pump() {
    let runtime = runtime();
    let mut app = App::new(runtime.handle().clone());
    let options = WindowOptions::default();
    let id = app.create_window(options.clone());
    let app = std::rc::Rc::new(std::cell::RefCell::new(app));
    let dispatcher = Dispatcher::new();
    let mut native = NativeWindow::create(id, &options, None, Arc::clone(&dispatcher), Some(&app))
        .expect("NativeWindow");
    let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
    // 先走完一次模态循环（会话打开又关闭）。
    // SAFETY: 同 `a_size_move_resize_drives_a_real_frame`。
    unsafe {
        SendMessageW(hwnd, WM_ENTERSIZEMOVE, None, None);
        SendMessageW(hwnd, WM_SIZE, None, Some(LPARAM(encoded_client(800, 600))));
        SendMessageW(hwnd, WM_EXITSIZEMOVE, None, None);
    }
    // SAFETY: `state` 由 `hidden_window*` 同款生命周期（`native` 持有）持有；这里只读事件队列。
    let events_before = native.state_mut().take_events();
    let before = app.borrow().window(id).expect("window").frame_index();

    // SAFETY: 同 `a_size_move_resize_drives_a_real_frame`。
    unsafe {
        SendMessageW(hwnd, WM_SIZE, None, Some(LPARAM(encoded_client(700, 500))));
    }
    assert_eq!(
        app.borrow().window(id).expect("window").frame_index(),
        before,
        "模态循环结束后 WM_SIZE 不得直接出帧（由泵的 ④ 统一决定）"
    );
    assert!(
        native
            .state_mut()
            .take_events()
            .iter()
            .any(|event| matches!(event.event, PlatformEvent::Resize { .. })),
        "事件仍必须入队交给泵（否则模态循环外的 resize 永远不重排）：{events_before:?}"
    );
}

// ── B1：系统 move 模态循环里的出帧（`WM_APP_FRAME_WAKE` → `ModalTick`）──────────────────
//
// 拖动标题栏走系统 move 模态循环：它不产生 `WM_SIZE`，而唯一的帧请求入口
// `WM_APP_FRAME_WAKE` 曾经被直接吞掉 ⇒ tokio 回投任务 / `on_frame` 钩子 / `App::frame` 全部
// 停摆（报告 §1 VALID）。下面的用例钉住三件事：
// (a) 模态循环内一条 `WM_APP_FRAME_WAKE` 执行任务 + 钩子 + 提交一帧；
// (b) 模态循环外同序列**不得**直接出帧（守住泵的 ④ 路径，不回归成双帧）；
// (c) 模态 tick 里任务关掉全部窗口：不 panic、不重入、不泄漏唤醒位。

/// B1 用例夹具：真实 `App` + 隐藏 `NativeWindow`（挂上生产 `WndProc` 与模态 tick 驱动）。
///
/// `native` 必须活到用例结束（`WndProcState` 的所有者）；`hook_runs` 是 `App::on_frame` 的
/// 调用计数（模态 tick 与泵的 ④ 都会让它 +1）。`runtime` 由调用方持有（`App` 的 tokio 句柄）。
struct ModalFixture {
    app: Rc<RefCell<App>>,
    dispatcher: Arc<Dispatcher>,
    native: NativeWindow,
    hwnd: HWND,
    id: WindowId,
    hook_runs: Rc<Cell<u32>>,
}

fn modal_tick_fixture(runtime: &tokio::runtime::Runtime) -> ModalFixture {
    let mut app = App::new(runtime.handle().clone());
    let options = WindowOptions::default();
    let id = app.create_window(options.clone());
    let app = Rc::new(RefCell::new(app));
    let dispatcher = Dispatcher::new();
    let native = NativeWindow::create(id, &options, None, Arc::clone(&dispatcher), Some(&app))
        .expect("NativeWindow::create");
    let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
    let hook_runs = Rc::new(Cell::new(0u32));
    let counter = Rc::clone(&hook_runs);
    app.borrow_mut().on_frame(move |_app| {
        counter.set(counter.get() + 1);
    });
    // 建窗 / a11y 构造不应留下待处理帧：清一次让断言只测被发送的消息。
    let _ = dispatcher.take_frame_wake();
    ModalFixture {
        app,
        dispatcher,
        native,
        hwnd,
        id,
        hook_runs,
    }
}

/// B1 (a)：`WM_ENTERSIZEMOVE` 之后，一条 `WM_APP_FRAME_WAKE` 必须在 `WndProc` 内直接
/// 执行回投任务、`on_frame` 钩子并提交一帧（泵的 ④ 在系统模态循环里不运行）。
///
/// 变异守卫：删掉 `WM_APP_FRAME_WAKE` 分支里的 `in_modal_loop()` / `modal_tick_step()`
/// 判定 ⇒ 任务与钩子都不执行、`FrameIndex` 不前进，本条变红；删掉 `modal_tick` 里的
/// `drain_tasks` ⇒ 任务断言红；删掉 `run_frame_hook` ⇒ 钩子计数断言红。
#[test]
fn a_modal_loop_wake_runs_tasks_and_frames_inside_the_wndproc() {
    let runtime = runtime();
    let ModalFixture {
        app,
        dispatcher,
        native: _native,
        hwnd,
        id,
        hook_runs,
    } = modal_tick_fixture(&runtime);
    let task_ran = Arc::new(AtomicBool::new(false));
    let task_flag = Arc::clone(&task_ran);
    // 回投任务：写状态并请求重绘（行情 / HUD 更新的典型形态）。
    dispatcher.push(UserTask::new(move |app| {
        app.window_mut(id).expect("窗口存在").request_repaint();
        task_flag.store(true, Ordering::Release);
    }));
    let before = app.borrow().window(id).expect("窗口存在").frame_index();

    // 系统 move 模态循环开始（被拖窗口 = 本窗口）。
    // SAFETY: 窗口由 `native` 持有且仍存活；`SendMessageW` 同步进入生产 `wndproc`。
    unsafe {
        SendMessageW(hwnd, WM_ENTERSIZEMOVE, None, None);
    }
    assert!(
        dispatcher.in_modal_loop(),
        "WM_ENTERSIZEMOVE 必须进入全局模态计数（唤醒目标可能是另一个窗口）"
    );

    // SAFETY: 同 `a_size_move_resize_drives_a_real_frame`；消息同步进入生产 `wndproc`。
    unsafe {
        SendMessageW(hwnd, WM_APP_FRAME_WAKE, None, None);
    }

    assert!(
        task_ran.load(Ordering::Acquire),
        "模态循环里的回投任务必须被执行（否则任务堆到松手后集中爆发）"
    );
    assert_eq!(
        hook_runs.get(),
        1,
        "on_frame 钩子必须在模态循环里每拍执行一次"
    );
    let after = app.borrow().window(id).expect("窗口存在").frame_index();
    assert!(
        after > before,
        "模态 tick 必须提交一帧（frame_index {before:?} → {after:?}；泵的 ④ 这段时间不运行）"
    );
    assert!(
        !dispatcher.take_frame_wake(),
        "唤醒位必须被这一拍消费（不能留在泵的一轮里造成双帧）"
    );
    assert!(
        dispatcher.drain_tasks().is_empty(),
        "任务队列必须已排空（否则下一条消息会重复执行）"
    );

    // 退出模态循环：计数归零，唤醒回到泵路径。
    // SAFETY: 窗口仍存活。
    unsafe {
        SendMessageW(hwnd, WM_EXITSIZEMOVE, None, None);
    }
    assert!(
        !dispatcher.in_modal_loop(),
        "WM_EXITSIZEMOVE 必须退出全局模态计数"
    );
}

/// B1 (b)：模态循环**外**同一序列不得直接出帧 —— `WM_APP_FRAME_WAKE` 仍只把唤醒位留给泵的
/// ④（否则每次唤醒都被 `WndProc` 抢跑，泵一侧再出一帧，形成系统性双帧）。
#[test]
fn a_frame_wake_outside_the_modal_loop_stays_for_the_pump() {
    let runtime = runtime();
    let ModalFixture {
        app,
        dispatcher,
        native: _native,
        hwnd,
        id,
        hook_runs,
    } = modal_tick_fixture(&runtime);
    let task_ran = Arc::new(AtomicBool::new(false));
    let task_flag = Arc::clone(&task_ran);
    dispatcher.push(UserTask::new(move |app| {
        app.window_mut(id).expect("窗口存在").request_repaint();
        task_flag.store(true, Ordering::Release);
    }));
    let before = app.borrow().window(id).expect("窗口存在").frame_index();
    assert!(!dispatcher.in_modal_loop(), "前置：不在模态循环里");

    // SAFETY: 窗口由 `native` 持有且仍存活；消息同步进入生产 `wndproc`。
    unsafe {
        SendMessageW(hwnd, WM_APP_FRAME_WAKE, None, None);
    }

    assert_eq!(hook_runs.get(), 0, "模态循环外不得跑钩子（归泵的 ④）");
    assert!(
        !task_ran.load(Ordering::Acquire),
        "模态循环外不得执行回投任务"
    );
    assert_eq!(
        app.borrow().window(id).expect("窗口存在").frame_index(),
        before,
        "模态循环外 WM_APP_FRAME_WAKE 不得直接出帧"
    );
    assert!(
        dispatcher.take_frame_wake(),
        "唤醒位必须留给泵的 ④（这里抢走就等于丢帧）"
    );
    let _ = dispatcher.drain_tasks();
}

/// B1 (c) 边界：模态 tick 里任务关闭**全部**窗口 —— 不 panic、帧内不重入（`try_borrow_mut`
/// 失败即放弃本拍）、唤醒位不泄漏（否则泵退出前还会空转一轮）。
#[test]
fn a_modal_tick_that_closes_every_window_neither_panics_nor_leaks_the_wake() {
    let runtime = runtime();
    let ModalFixture {
        app,
        dispatcher,
        mut native,
        hwnd,
        id,
        hook_runs,
    } = modal_tick_fixture(&runtime);
    let ids: Vec<WindowId> = app.borrow().windows().map(|(window, _)| window).collect();
    assert_eq!(ids, vec![id], "前置：App 里只有一个窗口");
    dispatcher.push(UserTask::new(move |app| {
        for window in ids {
            assert!(app.close_window(window), "任务必须关闭窗口");
        }
    }));

    // SAFETY: 窗口由 `native` 持有且仍存活；消息同步进入生产 `wndproc`。
    unsafe {
        SendMessageW(hwnd, WM_ENTERSIZEMOVE, None, None);
        SendMessageW(hwnd, WM_APP_FRAME_WAKE, None, None);
    }

    assert!(
        app.borrow().windows().next().is_none(),
        "任务必须把 App 里的窗口全部关掉"
    );
    assert_eq!(hook_runs.get(), 1, "钩子仍按拍执行（关窗之后才出帧）");
    assert!(
        !dispatcher.take_frame_wake(),
        "关掉全部窗口之后唤醒位不得泄漏（否则泵退出路径多一轮空转）"
    );
    assert!(dispatcher.drain_tasks().is_empty());
    assert!(
        native.state_mut().take_pending_error().is_none(),
        "关窗不是帧管线错误，不得登记粘性错误"
    );

    // SAFETY: 隐藏窗口仍由 `NativeWindow` 持有，直到用例结束。
    unsafe {
        SendMessageW(hwnd, WM_EXITSIZEMOVE, None, None);
    }
    assert!(!dispatcher.in_modal_loop());
}

/// B1 (d) 空唤醒守卫：模态循环里一条**没有唤醒位**的 `WM_APP_FRAME_WAKE` 不得出帧 ——
/// 「唤醒位 + 一条消息」是同一电平信号，若先跑任务 / 钩子再检查唤醒位，重复/迟到的消息会把
/// 模态循环变成忙循环（持续跑钩子 + 空帧）。`take_frame_wake` 必须是本函数的第一件事。
#[test]
fn a_spurious_modal_wake_without_a_pending_request_does_not_tick() {
    let runtime = runtime();
    let ModalFixture {
        app: _app,
        dispatcher,
        native: _native,
        hwnd,
        hook_runs,
        ..
    } = modal_tick_fixture(&runtime);
    // 前置：夹具已清掉建窗 / a11y 构造留下的唤醒位。
    assert!(!dispatcher.take_frame_wake(), "前置：没有待消费的帧唤醒");

    // SAFETY: 窗口由 `native` 持有且仍存活；消息同步进入生产 `wndproc`。
    unsafe {
        SendMessageW(hwnd, WM_ENTERSIZEMOVE, None, None);
        SendMessageW(hwnd, WM_APP_FRAME_WAKE, None, None);
    }
    assert_eq!(hook_runs.get(), 0, "没有唤醒位时不得跑任务 / 钩子");

    // 守卫不能把模态 tick 关死：之后的真实唤醒仍必须驱动一拍（并消费唤醒位）。
    dispatcher.wake();
    // SAFETY: 窗口仍存活。
    unsafe {
        SendMessageW(hwnd, WM_APP_FRAME_WAKE, None, None);
    }
    assert_eq!(hook_runs.get(), 1, "真实唤醒必须执行一拍钩子");
    assert!(
        !dispatcher.take_frame_wake(),
        "真实唤醒的唤醒位必须被这一拍消费"
    );

    // SAFETY: 窗口仍存活。
    unsafe {
        SendMessageW(hwnd, WM_EXITSIZEMOVE, None, None);
    }
    assert!(!dispatcher.in_modal_loop());
}

/// B1 (e) 异常路径对称性：窗口在系统模态循环里被销毁（`WM_EXITSIZEMOVE` 永远不会到达本
/// 窗口过程）时，`WndProcState::drop` 必须把本窗口记过的模态层数退回全局计数。
///
/// 泄漏的后果不是「少出帧」而是「多出帧」：模态循环外每条 `WM_APP_FRAME_WAKE` 都会在
/// `WndProc` 里抢跑泵的 ④（钩子重复执行、绕过泵的 a11y / 几何同步）。
#[test]
fn a_window_destroyed_inside_the_modal_loop_returns_its_global_entry() {
    let runtime = runtime();
    let ModalFixture {
        app: _app,
        dispatcher,
        native,
        hwnd,
        ..
    } = modal_tick_fixture(&runtime);
    // SAFETY: 窗口由 `native` 持有且仍存活。
    unsafe {
        SendMessageW(hwnd, WM_ENTERSIZEMOVE, None, None);
    }
    assert!(dispatcher.in_modal_loop(), "前置：全局计数已进入一层");

    // 模拟「窗口在 move / size 循环里被销毁」：销毁路径没有配对的 `WM_EXITSIZEMOVE`。
    drop(native);
    assert!(
        !dispatcher.in_modal_loop(),
        "销毁窗口必须退回尚未退出的模态层数（否则唤醒会在模态循环外抢跑泵的 ④）"
    );
}

/// 取走本窗口消息队列里的下一条 `WM_APP_FRAME_WAKE`（其它消息丢弃）。
fn take_posted_frame_wake(hwnd: HWND) -> bool {
    let mut message = MSG::default();
    loop {
        // SAFETY: `message` 是栈上输出；只取本窗口的消息（测试线程就是窗口线程）。
        let has_message =
            unsafe { PeekMessageW(&mut message, Some(hwnd), 0, 0, PM_REMOVE) }.as_bool();
        if !has_message {
            return false;
        }
        if message.message == WM_APP_FRAME_WAKE {
            return true;
        }
    }
}

/// B1 (f) 模态循环入口的唤醒位重投：钉住真机帧冻结的竞态。
///
/// 复现（真机实测偶发、拖动 3 s 一帧不出）：
/// ① 数据泵 `wake()` 置位并 post 一条 `WM_APP_FRAME_WAKE`；
/// ② 泵的 ① drain 把它派发进 `WndProc`（模态循环外 ⇒ 只返回、**不**消费位，消费归泵的 ④）；
/// ③ 同一批消息里紧随的鼠标按下让 `DefWindowProc` 在泵的 ④ 之前进入模态循环。
/// 此时唤醒位仍置位、队列里却没有唤醒消息：生产者看到「已置位」不再 post ⇒ 循环内零 tick。
/// `begin_size_move` 必须在进入循环时消费并重投一条唤醒消息。
#[test]
fn entering_the_modal_loop_reposts_a_wake_that_was_dispatched_but_not_consumed() {
    let runtime = runtime();
    let ModalFixture {
        app: _app,
        dispatcher,
        native: _native,
        hwnd,
        hook_runs,
        ..
    } = modal_tick_fixture(&runtime);
    // `wake()` 要在生产投递路径上真的 post 到这个窗口：attach 之后走 `PostMessageW`。
    dispatcher.attach(hwnd.0 as isize);
    // ① 数据泵唤醒：置位 + post 一条消息。
    dispatcher.wake();
    // ② 泵的 ① drain 取走并派发这条消息（模态循环外分支：只返回、不消费位）。
    assert!(
        take_posted_frame_wake(hwnd),
        "前置：wake() 必须真的 post 一条 WM_APP_FRAME_WAKE"
    );
    // SAFETY: 窗口仍存活；消息同步进入生产 `wndproc`（模态循环外分支）。
    unsafe {
        SendMessageW(hwnd, WM_APP_FRAME_WAKE, None, None);
    }
    // 现在唤醒位仍置位、队列里没有消息 —— 再 wake() 必须被 coalesce（不再 post 新消息），
    // 这就是真机上「泵 ④ 没来得及消费就进入模态循环」的竞态状态。
    dispatcher.wake();
    assert!(
        !take_posted_frame_wake(hwnd),
        "前置：竞态状态必须是「位置位、队列里没有唤醒消息」"
    );
    assert_eq!(hook_runs.get(), 0, "前置：模态循环外派发不得出帧");

    // ③ 进入系统模态循环：必须消费并重投一条唤醒消息。
    // SAFETY: 窗口仍存活；消息同步进入生产 `wndproc`。
    unsafe {
        SendMessageW(hwnd, WM_ENTERSIZEMOVE, None, None);
    }
    assert!(
        take_posted_frame_wake(hwnd),
        "进入模态循环时必须重投一条唤醒消息（否则拖动全程零帧，松手后才爆发）"
    );

    // 重投的那条消息派发后必须**真的**跑一拍（钩子 / 帧），而不是空转。
    // SAFETY: 窗口仍存活；同步派发走同一条 `WndProc` 路径。
    unsafe {
        SendMessageW(hwnd, WM_APP_FRAME_WAKE, None, None);
    }
    assert_eq!(hook_runs.get(), 1, "重投的唤醒必须在模态循环里跑一拍钩子");
    assert!(
        !dispatcher.take_frame_wake(),
        "这一拍必须消费唤醒位（否则下一次生产者仍然 coalesce）"
    );

    // SAFETY: 窗口仍存活。
    unsafe {
        SendMessageW(hwnd, WM_EXITSIZEMOVE, None, None);
    }
    assert!(!dispatcher.in_modal_loop());
}

// ── B6/D6：Windows IME 失焦对齐（组合中失焦恰好一条 `ImeEvent::End`）──────────────────

/// 组合中 `WM_KILLFOCUS`：必须补**恰好一条** `ImeEvent::End`（Win32 不保证失焦会先派发
/// `WM_IME_ENDCOMPOSITION`），系统随后迟到的 `WM_IME_ENDCOMPOSITION` 不得重复投递；
/// `Focus(false)` 事件仍在，且 `End` 先于 `Focus(false)`（先结束组合、再通知失焦）。
#[test]
fn losing_keyboard_focus_during_composition_emits_exactly_one_ime_end() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcImeFocusLoss", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配、生命周期到测试结束。
    let raw = unsafe { &mut *state };
    // SAFETY: 窗口由本测试创建且仍存活；`SendMessageW` 同步进入生产 `wndproc`。
    unsafe {
        SendMessageW(hwnd, WM_IME_STARTCOMPOSITION, None, None);
    }
    assert!(
        raw.take_events()
            .iter()
            .any(|event| matches!(event.event, PlatformEvent::Ime(ImeEvent::Start))),
        "前置：启用态组合开始必须派发 Start"
    );

    // SAFETY: 同上。第二次 KILLFOCUS 用于验证组合态清零后不会重复补 End。
    unsafe {
        SendMessageW(hwnd, WM_KILLFOCUS, None, None);
        SendMessageW(hwnd, WM_KILLFOCUS, None, None);
        SendMessageW(hwnd, WM_IME_ENDCOMPOSITION, None, None);
    }
    let events = raw.take_events();
    let ends: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event.event, PlatformEvent::Ime(ImeEvent::End)))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        ends.len(),
        1,
        "组合中失焦必须补恰好一条 ImeEvent::End（实际 {:?}）",
        events.iter().map(|event| &event.event).collect::<Vec<_>>()
    );
    let focus_lost = events
        .iter()
        .position(|event| matches!(event.event, PlatformEvent::Focus(false)))
        .expect("失焦必须派发 Focus(false)");
    assert!(
        ends[0] < focus_lost,
        "End 必须先于 Focus(false)（先结束组合、再通知失焦）"
    );
}

/// 不在组合中失焦：不得凭空补 `ImeEvent::End`；后续孤立 `WM_IME_ENDCOMPOSITION` 也不得投递。
#[test]
fn losing_keyboard_focus_outside_composition_emits_no_ime_end() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcImeNoComposition", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配、生命周期到测试结束。
    let raw = unsafe { &mut *state };
    // SAFETY: 窗口由本测试创建且仍存活；`SendMessageW` 同步进入生产 `wndproc`。
    unsafe {
        SendMessageW(hwnd, WM_KILLFOCUS, None, None);
        SendMessageW(hwnd, WM_IME_ENDCOMPOSITION, None, None);
    }
    let events = raw.take_events();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.event, PlatformEvent::Ime(ImeEvent::End))),
        "没有组合时失焦不得补 End"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event.event, PlatformEvent::Focus(false))),
        "失焦事件本身仍必须派发"
    );
}

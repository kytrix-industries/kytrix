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
            CreateWindowExW, DestroyWindow, GWLP_USERDATA, GetClientRect, GetWindowRect, HTBOTTOM,
            HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTCLIENT, HTCLOSE, HTLEFT, HTMAXBUTTON,
            HTMINBUTTON, HTRIGHT, HTSYSMENU, HTTOP, HTTOPLEFT, HTTOPRIGHT, MINMAXINFO, MSG,
            PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PM_REMOVE, PeekMessageW, RegisterClassW,
            SM_CXPADDEDBORDER, SM_CXSIZEFRAME, SPI_SETANIMATION, SPI_SETWORKAREA, SendMessageW,
            SetWindowLongPtrW, WINDOW_EX_STYLE, WM_APP, WM_CAPTURECHANGED, WM_DISPLAYCHANGE,
            WM_DPICHANGED, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_GETMINMAXINFO, WM_IME_CHAR,
            WM_IME_COMPOSITION, WM_IME_ENDCOMPOSITION, WM_IME_SETCONTEXT, WM_IME_STARTCOMPOSITION,
            WM_INPUTLANGCHANGE, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_MOVE, WM_NCHITTEST,
            WM_PAINT, WM_POWERBROADCAST, WM_SETTINGCHANGE, WM_SIZE, WM_TIMER, WNDCLASSW,
            WS_OVERLAPPEDWINDOW,
        },
    },
};

use super::super::{WndProcState, pump::A11Y_TIMER_ID, wndproc};
use crate::{
    app::App,
    event::ImeEvent,
    math::{DevicePixelRatio, DeviceSize, LogicalRect, LogicalSize, dp, px},
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
    window::{Decorations, WindowId, WindowOptions},
};

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn hidden_window(tag: &str, dispatcher: Arc<Dispatcher>) -> (HWND, *mut WndProcState, Vec<u16>) {
    hidden_window_sized(tag, dispatcher, 320, 240)
}

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
        Decorations::Native,
        width,
        height,
    )
}

fn frameless_hidden_window(
    tag: &str,
    dispatcher: Arc<Dispatcher>,
    width: i32,
    height: i32,
) -> (HWND, *mut WndProcState, Vec<u16>) {
    hidden_window_with_limits(
        tag,
        dispatcher,
        placement::ClientLimits::default(),
        Decorations::None,
        width,
        height,
    )
}

fn hidden_window_with_limits(
    tag: &str,
    dispatcher: Arc<Dispatcher>,
    limits: placement::ClientLimits,
    decorations: Decorations,
    width: i32,
    height: i32,
) -> (HWND, *mut WndProcState, Vec<u16>) {
    let class_name = wide(tag);
    let instance = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
    let class = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    let atom = unsafe { RegisterClassW(&class) };
    assert_ne!(atom, 0, "RegisterClassW");
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
    let state = match WndProcState::new(
        dispatcher,
        hwnd,
        WindowId::for_test(1),
        limits,
        decorations,
        None,
        crate::a11y::adapter::A11yBridge::new().consumer_epoch(),
    ) {
        Ok(state) => Box::into_raw(Box::new(state)),
        Err(error) => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            panic!("WndProcState::new: {error}");
        }
    };
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) };
    (hwnd, state, class_name)
}

fn destroy_hidden_window(hwnd: HWND, state: *mut WndProcState) {
    unsafe {
        let _ = DestroyWindow(hwnd);
        drop(Box::from_raw(state));
    }
}

fn encoded_client(width: u16, height: u16) -> isize {
    (((height as u32) << 16) | width as u32) as isize
}

fn encoded_point(x: i32, y: i32) -> isize {
    ((((y as u32) << 16) | (x as u32 & 0xFFFF)) as i32) as isize
}

fn hit_test(hwnd: HWND, scale: DevicePixelRatio, client_origin: POINT, x: f32, y: f32) -> u32 {
    let point = POINT {
        x: client_origin.x + (x * scale.factor()).round() as i32,
        y: client_origin.y + (y * scale.factor()).round() as i32,
    };
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

fn drive_ime(raw: &mut WndProcState, hwnd: HWND, command: Option<ImeCommand>) {
    if let Some(command) = command {
        let applied: ImeApplied = ime::execute(hwnd, command);
        raw.note_ime_applied(applied);
    }
}

fn set_ime_enabled(raw: &mut WndProcState, hwnd: HWND, enabled: bool) {
    let command = raw.plan_ime_enabled(enabled);
    drive_ime(raw, hwnd, command);
}

#[test]
fn release_capture_without_ownership_leaves_no_suppress() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, class_name) = hidden_window("KytrixReleaseCaptureTest", dispatcher);
    let state = unsafe { &mut *state };
    assert!(unsafe { GetCapture() } != hwnd, "前置：本窗口不持有捕获");
    assert!(
        !super::super::pump::plan_release_capture(hwnd, state),
        "没有捕获时不得计划释放（也不得置 suppress：会吞掉下一次真实取消）"
    );
    assert!(!state.suppress_capture_cancel);
    destroy_hidden_window(hwnd, state as *mut WndProcState);
    let _ = class_name;
}

#[test]
fn wm_paint_advances_the_composition_check_epoch() {
    use crate::platform::windows::composition;

    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, class_name) = hidden_window("KytrixCompositionCheckEpoch", dispatcher);
    let before = composition::device_check_epoch();
    unsafe {
        SendMessageW(hwnd, WM_PAINT, Some(WPARAM(0)), Some(LPARAM(0)));
    }
    let after = composition::device_check_epoch();
    assert!(
        after > before,
        "WM_PAINT 必须推进 DComp 健康检查代际（before={before} after={after}）"
    );
    destroy_hidden_window(hwnd, state);
    let _ = class_name;
}

#[test]
fn releasing_the_pointer_capture_is_two_phase() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, class_name) = hidden_window("KytrixReleaseCaptureTwoPhase", dispatcher);
    unsafe { SetCapture(hwnd) };
    assert!(unsafe { GetCapture() } == hwnd, "前置：本窗口持有捕获");
    let raw = unsafe { &mut *state };
    assert!(
        super::super::pump::plan_release_capture(hwnd, raw),
        "持有捕获时必须计划释放"
    );
    assert!(
        unsafe { GetCapture() } == hwnd,
        "第一阶段不得调用 ReleaseCapture（会在持有 &mut WndProcState 时重入 WndProc）"
    );
    assert!(
        raw.suppress_capture_cancel,
        "第一阶段必须先置 suppress：重入的 WndProc 才能吞掉自己释放产生的 WM_CAPTURECHANGED"
    );
    let _ = unsafe { ReleaseCapture() };
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

#[test]
fn suppress_consumes_one_capture_change_and_clears() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, class_name) = hidden_window("KytrixSuppressTest", dispatcher);
    let raw = unsafe { &mut *state };
    raw.suppress_capture_cancel = true;
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

#[test]
fn a_capture_change_without_buttons_down_is_not_a_cancel() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, class_name) = hidden_window("KytrixCaptureChangeIdle", dispatcher);
    let raw = unsafe { &mut *state };
    let _ = raw.take_events();
    assert!(!any_mouse_button_down(), "本用例要求没有物理按键按下");

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

fn any_mouse_button_down() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
    };
    [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2]
        .into_iter()
        .any(|key| (unsafe { GetKeyState(i32::from(key.0)) }) < 0)
}

#[test]
fn the_composition_wake_does_not_request_a_frame() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) =
        hidden_window("KytrixWndProcCompositionWake", Arc::clone(&dispatcher));
    let _ = dispatcher.take_frame_wake();

    unsafe {
        SendMessageW(hwnd, WM_APP_COMPOSITION_WAKE, None, None);
    }

    assert!(
        !dispatcher.take_frame_wake(),
        "合成通知必须只唤醒 GetMessageW，不得请求 App::frame"
    );
    destroy_hidden_window(hwnd, state);
}

#[test]
fn only_the_a11y_timer_wakes_a_frame() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcTimer", Arc::clone(&dispatcher));
    let _ = dispatcher.take_frame_wake();

    unsafe {
        SendMessageW(hwnd, WM_TIMER, Some(WPARAM(A11Y_TIMER_ID)), None);
    }
    assert!(
        dispatcher.take_frame_wake(),
        "a11y 补发定时器必须能唤醒泵（否则被节流的交付永远不重试）"
    );

    unsafe {
        SendMessageW(hwnd, WM_TIMER, Some(WPARAM(A11Y_TIMER_ID + 1)), None);
    }
    assert!(!dispatcher.take_frame_wake(), "非 a11y 定时器不得唤醒帧");
    destroy_hidden_window(hwnd, state);
}

#[test]
fn display_and_power_messages_mark_one_pending_display_recovery() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) =
        hidden_window("KytrixWndProcDisplayRecovery", Arc::clone(&dispatcher));
    let _ = dispatcher.take_frame_wake();
    let raw = unsafe { &mut *state };
    let _ = raw.take_events();

    let result = unsafe { SendMessageW(hwnd, WM_DISPLAYCHANGE, None, None) };
    assert_eq!(result.0, 0, "WM_DISPLAYCHANGE 必须返回 0");
    assert!(raw.take_display_recovery(), "拓扑变化必须登记恢复");
    assert!(dispatcher.take_frame_wake(), "拓扑变化必须唤醒一帧");

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

    unsafe {
        SendMessageW(
            hwnd,
            WM_POWERBROADCAST,
            Some(WPARAM(PBT_APMSUSPEND as usize)),
            None,
        );
    }
    assert!(!raw.take_display_recovery(), "PBT_APMSUSPEND 不得登记恢复");

    unsafe {
        SendMessageW(
            hwnd,
            WM_SETTINGCHANGE,
            Some(WPARAM(SPI_SETWORKAREA.0 as usize)),
            None,
        );
    }
    assert!(raw.take_display_recovery(), "工作区变化必须登记恢复");

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

#[test]
fn a_theme_change_applies_the_dark_titlebar() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, class_name) = hidden_window("KytrixWndProcDarkTitlebar", dispatcher);
    let raw = unsafe { &mut *state };
    let expected = dark_mode::current()
        .expect("本机 HKCU\\…\\Personalize\\AppsUseLightTheme 必须可读（Win11 宿主）");
    assert_eq!(
        raw.last_dark_titlebar, None,
        "未收到主题变化前不得有标题栏观察值"
    );
    assert!(
        dark_mode::set_titlebar(hwnd, !expected),
        "前置：把标题栏设成与系统主题相反的值"
    );
    assert_eq!(read_titlebar(hwnd), !expected, "前置：DWM 回读为反值");

    let changed = wide("ImmersiveColorSet");
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

fn read_titlebar(hwnd: HWND) -> bool {
    let mut value = windows::core::BOOL::default();
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

#[test]
fn mouse_moves_in_one_batch_coalesce_into_the_latest_position() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcMouseCoalesce", dispatcher);
    let raw = unsafe { &mut *state };
    let _ = raw.take_events();

    let point =
        |x: i32, y: i32| LPARAM(((((y as u32) << 16) | (x as u32 & 0xFFFF)) as i32) as isize);
    unsafe {
        SendMessageW(hwnd, WM_MOUSEMOVE, None, Some(point(0, 0)));
    }
    let _ = raw.take_events();

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

#[test]
fn a_button_event_breaks_the_mouse_move_coalescing() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcMouseBreak", dispatcher);
    let raw = unsafe { &mut *state };
    let _ = raw.take_events();

    let point =
        |x: i32, y: i32| LPARAM(((((y as u32) << 16) | (x as u32 & 0xFFFF)) as i32) as isize);
    unsafe {
        SendMessageW(hwnd, WM_MOUSEMOVE, None, Some(point(0, 0)));
    }
    let _ = raw.take_events();

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

#[test]
fn a_dpi_change_applies_immediately_and_stamps_later_events() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcDpi", dispatcher);
    let raw = unsafe { &mut *state };
    let _ = raw.take_events();
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
    let dpr = raw.scale();
    assert_eq!(dpr.factor(), 2.0, "WndProcState 必须立刻换到新 scale");

    let mut placed = RECT::default();
    unsafe {
        GetWindowRect(hwnd, &mut placed).expect("GetWindowRect");
    }
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

#[test]
fn wm_getminmaxinfo_applies_the_configured_client_limits() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let options = WindowOptions {
        min_size: Some(LogicalSize::new(px(500.0), px(300.0))),
        max_size: Some(LogicalSize::new(px(1200.0), px(900.0))),
        ..WindowOptions::default()
    };
    let limits = placement::ClientLimits::from_options(&options);
    let (hwnd, state, class_name) = hidden_window_with_limits(
        "KytrixWndProcMinMax",
        dispatcher,
        limits,
        Decorations::Native,
        640,
        480,
    );
    let dpi = dpi::window_dpi(hwnd);
    let mut info = MINMAXINFO::default();

    unsafe {
        SendMessageW(
            hwnd,
            WM_GETMINMAXINFO,
            None,
            Some(LPARAM((&raw mut info) as isize)),
        );
    }

    let min = dpi::track_size(
        LogicalSize::new(px(500.0), px(300.0)),
        dpi,
        Decorations::Native,
    );
    let max = dpi::track_size(
        LogicalSize::new(px(1200.0), px(900.0)),
        dpi,
        Decorations::Native,
    );
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

    let dispatcher = Dispatcher::new();
    let options = WindowOptions {
        min_size: Some(LogicalSize::new(px(900.0), px(700.0))),
        max_size: Some(LogicalSize::new(px(600.0), px(500.0))),
        ..WindowOptions::default()
    };
    let limits = placement::ClientLimits::from_options(&options);
    let (hwnd, state, class_name) = hidden_window_with_limits(
        "KytrixWndProcMinMaxClamped",
        dispatcher,
        limits,
        Decorations::Native,
        640,
        480,
    );
    let _raw = unsafe { &mut *state };
    let dpi = dpi::window_dpi(hwnd);
    let mut info = MINMAXINFO::default();
    unsafe {
        SendMessageW(
            hwnd,
            WM_GETMINMAXINFO,
            None,
            Some(LPARAM((&raw mut info) as isize)),
        );
    }
    let clamped = dpi::track_size(
        LogicalSize::new(px(600.0), px(500.0)),
        dpi,
        Decorations::Native,
    );
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

#[test]
fn wm_move_is_translated_and_dpi_repositioning_moves_are_filtered() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, class_name) = hidden_window("KytrixWndProcMove", dispatcher);
    let raw = unsafe { &mut *state };
    let _ = raw.take_events();

    unsafe {
        SendMessageW(hwnd, WM_MOVE, None, Some(LPARAM(encoded_point(-1920, 120))));
    }
    let events = raw.take_events();
    assert_eq!(events.len(), 1, "一条移动事件，实际 {events:?}");
    assert_eq!(events[0].event, PlatformEvent::Moved { x: -1920, y: 120 });
    assert_eq!(events[0].scale, raw.scale(), "Moved 必须带消息时刻的 scale");

    let suggested = RECT {
        left: 40,
        top: 50,
        right: 840,
        bottom: 650,
    };
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

#[test]
fn drag_regions_hit_test_as_caption_and_clear_back_to_the_system() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, class_name) = hidden_window("KytrixWndProcDragRegion", dispatcher);
    let raw = unsafe { &mut *state };
    let scale = raw.scale();
    let mut client_origin = POINT::default();
    assert!(
        unsafe { ClientToScreen(hwnd, &mut client_origin) }.as_bool(),
        "ClientToScreen"
    );
    raw.set_drag_regions(&[LogicalRect::new(px(0.0), px(0.0), px(200.0), px(24.0))]);

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

#[test]
fn the_pump_sync_copies_drag_regions_into_the_wndproc_state() {
    let runtime = runtime();
    let mut app = App::new(runtime.handle().clone());
    let options = WindowOptions::default();
    let id = app.create_window(options.clone());
    let mut native = NativeWindow::create(
        id,
        &options,
        None,
        Dispatcher::new(),
        None,
        crate::platform::windows::window::test_a11y_epoch(),
    )
    .expect("NativeWindow::create");
    let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
    let scale = native.scale();
    let mut client_origin = POINT::default();
    assert!(
        unsafe { ClientToScreen(hwnd, &mut client_origin) }.as_bool(),
        "ClientToScreen"
    );

    let first = [LogicalRect::new(px(0.0), px(0.0), px(320.0), px(32.0))];
    app.set_drag_regions(id, &first);
    native.sync_drag_regions(&app);
    assert_eq!(native.state_mut().drag_regions(), &first);
    assert_eq!(
        hit_test(hwnd, scale, client_origin, 10.0, 10.0),
        HTCAPTION,
        "同步后的区域必须命中 HTCAPTION"
    );

    let second = [LogicalRect::new(px(0.0), px(0.0), px(120.0), px(16.0))];
    app.set_drag_regions(id, &second);
    native.sync_drag_regions(&app);
    assert_eq!(native.state_mut().drag_regions(), &second);
    assert_eq!(
        hit_test(hwnd, scale, client_origin, 200.0, 10.0),
        HTCLIENT,
        "更新后旧区域之外的命中必须回到系统命中码"
    );

    app.set_drag_regions(id, &[]);
    native.sync_drag_regions(&app);
    assert!(native.state_mut().drag_regions().is_empty());
    assert_eq!(hit_test(hwnd, scale, client_origin, 10.0, 10.0), HTCLIENT);
}

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
    let dpi = dpi::window_dpi(hwnd);
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

#[test]
fn disabled_ime_swallows_composition_events_until_reenabled() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcImeGate", dispatcher);
    let raw = unsafe { &mut *state };

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

#[test]
fn an_enabled_setcontext_is_forwarded_without_events_or_detaching() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcImeSetContext", dispatcher);
    let raw = unsafe { &mut *state };
    let _ = raw.take_events();

    let baseline = unsafe { ImmGetContext(hwnd) };
    if baseline.is_invalid() {
        eprintln!("skip: hidden window has no IME context");
        destroy_hidden_window(hwnd, state);
        return;
    }
    unsafe {
        let _ = ImmReleaseContext(hwnd, baseline);
    }

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
    let still_attached = unsafe { ImmGetContext(hwnd) };
    assert!(
        !still_attached.is_invalid(),
        "启用态不得因为 SETCONTEXT 摘掉输入上下文"
    );
    unsafe {
        let _ = ImmReleaseContext(hwnd, still_attached);
    }
    destroy_hidden_window(hwnd, state);
}

#[test]
fn disabled_ime_detaches_a_reassociated_context() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcImeReattach", dispatcher);
    let raw = unsafe { &mut *state };

    let baseline = unsafe { ImmGetContext(hwnd) };
    if baseline.is_invalid() {
        eprintln!("skip: hidden window has no IME context");
        destroy_hidden_window(hwnd, state);
        return;
    }
    unsafe {
        let _ = ImmReleaseContext(hwnd, baseline);
    }

    set_ime_enabled(raw, hwnd, false);
    assert!(
        unsafe { ImmGetContext(hwnd) }.is_invalid(),
        "禁用后窗口不得再关联上下文"
    );

    unsafe {
        let _ = ImmAssociateContext(hwnd, baseline);
        SendMessageW(hwnd, WM_IME_SETCONTEXT, Some(WPARAM(1)), Some(LPARAM(0)));
    }
    assert!(
        unsafe { ImmGetContext(hwnd) }.is_invalid(),
        "WM_IME_SETCONTEXT 必须把重新关联的上下文再摘掉"
    );

    unsafe {
        let _ = ImmAssociateContext(hwnd, baseline);
        SendMessageW(hwnd, WM_INPUTLANGCHANGE, Some(WPARAM(0)), Some(LPARAM(0)));
    }
    assert!(
        unsafe { ImmGetContext(hwnd) }.is_invalid(),
        "WM_INPUTLANGCHANGE 必须把重新关联的上下文再摘掉"
    );

    set_ime_enabled(raw, hwnd, true);
    let restored = unsafe { ImmGetContext(hwnd) };
    assert_eq!(restored, baseline, "重新启用必须恢复最初保存的上下文");
    unsafe {
        let _ = ImmReleaseContext(hwnd, restored);
    }
    destroy_hidden_window(hwnd, state);
}

#[test]
fn a_resize_event_is_stamped_with_the_message_scale() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcResizeStamp", dispatcher);
    let raw = unsafe { &mut *state };
    let _ = raw.take_events();

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

#[test]
fn unhandled_messages_fall_through_to_the_default_procedure() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcFallThrough", dispatcher);
    let result = unsafe { SendMessageW(hwnd, WM_APP + 0x42, None, None) };
    assert_eq!(result.0, 0, "未识别消息必须按默认过程返回");

    let raw = unsafe { &mut *state };
    assert!(raw.take_events().is_empty());
    destroy_hidden_window(hwnd, state);
}

#[test]
fn a_size_move_resize_drives_a_real_frame() {
    let runtime = runtime();
    let mut app = App::new(runtime.handle().clone());
    let options = WindowOptions::default();
    let id = app.create_window(options.clone());
    let app = std::rc::Rc::new(std::cell::RefCell::new(app));
    let dispatcher = Dispatcher::new();
    let a11y_epoch = app.borrow().window_a11y_epoch(id).expect("window exists");
    let native = NativeWindow::create(
        id,
        &options,
        None,
        Arc::clone(&dispatcher),
        Some(&app),
        a11y_epoch,
    )
    .expect("NativeWindow");
    let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
    let before = app.borrow().window(id).expect("window").frame_index();

    unsafe {
        SendMessageW(hwnd, WM_ENTERSIZEMOVE, None, None);
        SendMessageW(hwnd, WM_SIZE, None, Some(LPARAM(encoded_client(800, 600))));
    }

    let window = app.borrow();
    let window = window.window(id).expect("window");
    assert_eq!(
        window.size(),
        window
            .device_pixel_ratio()
            .to_logical_size(DeviceSize::new(dp(800), dp(600))),
        "WM_SIZE 必须把客户区写进窗口（逻辑尺寸 = 物理客户区 / scale）"
    );
    assert!(
        window.frame_index() > before,
        "模态循环里的 WM_SIZE 必须提交一帧真实帧（泵的 ④ 这段时间不运行）"
    );
}

#[test]
fn a_size_after_the_modal_loop_is_only_recorded_for_the_pump() {
    let runtime = runtime();
    let mut app = App::new(runtime.handle().clone());
    let options = WindowOptions::default();
    let id = app.create_window(options.clone());
    let app = std::rc::Rc::new(std::cell::RefCell::new(app));
    let dispatcher = Dispatcher::new();
    let a11y_epoch = app.borrow().window_a11y_epoch(id).expect("window exists");
    let mut native = NativeWindow::create(
        id,
        &options,
        None,
        Arc::clone(&dispatcher),
        Some(&app),
        a11y_epoch,
    )
    .expect("NativeWindow");
    let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
    unsafe {
        SendMessageW(hwnd, WM_ENTERSIZEMOVE, None, None);
        SendMessageW(hwnd, WM_SIZE, None, Some(LPARAM(encoded_client(800, 600))));
        SendMessageW(hwnd, WM_EXITSIZEMOVE, None, None);
    }
    let events_before = native.state_mut().take_events();
    let before = app.borrow().window(id).expect("window").frame_index();

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
    let native = NativeWindow::create(
        id,
        &options,
        None,
        Arc::clone(&dispatcher),
        Some(&app),
        crate::platform::windows::window::test_a11y_epoch(),
    )
    .expect("NativeWindow::create");
    let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
    let hook_runs = Rc::new(Cell::new(0u32));
    let counter = Rc::clone(&hook_runs);
    app.borrow_mut().on_frame(move |_app| {
        counter.set(counter.get() + 1);
    });
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
    dispatcher.push(UserTask::new(move |app| {
        app.window_mut(id).expect("窗口存在").request_repaint();
        task_flag.store(true, Ordering::Release);
    }));
    let before = app.borrow().window(id).expect("窗口存在").frame_index();

    unsafe {
        SendMessageW(hwnd, WM_ENTERSIZEMOVE, None, None);
    }
    assert!(
        dispatcher.in_modal_loop(),
        "WM_ENTERSIZEMOVE 必须进入全局模态计数（唤醒目标可能是另一个窗口）"
    );

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

    unsafe {
        SendMessageW(hwnd, WM_EXITSIZEMOVE, None, None);
    }
    assert!(
        !dispatcher.in_modal_loop(),
        "WM_EXITSIZEMOVE 必须退出全局模态计数"
    );
}

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

    unsafe {
        SendMessageW(hwnd, WM_EXITSIZEMOVE, None, None);
    }
    assert!(!dispatcher.in_modal_loop());
}

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
    assert!(!dispatcher.take_frame_wake(), "前置：没有待消费的帧唤醒");

    unsafe {
        SendMessageW(hwnd, WM_ENTERSIZEMOVE, None, None);
        SendMessageW(hwnd, WM_APP_FRAME_WAKE, None, None);
    }
    assert_eq!(hook_runs.get(), 0, "没有唤醒位时不得跑任务 / 钩子");

    dispatcher.wake();
    unsafe {
        SendMessageW(hwnd, WM_APP_FRAME_WAKE, None, None);
    }
    assert_eq!(hook_runs.get(), 1, "真实唤醒必须执行一拍钩子");
    assert!(
        !dispatcher.take_frame_wake(),
        "真实唤醒的唤醒位必须被这一拍消费"
    );

    unsafe {
        SendMessageW(hwnd, WM_EXITSIZEMOVE, None, None);
    }
    assert!(!dispatcher.in_modal_loop());
}

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
    unsafe {
        SendMessageW(hwnd, WM_ENTERSIZEMOVE, None, None);
    }
    assert!(dispatcher.in_modal_loop(), "前置：全局计数已进入一层");

    drop(native);
    assert!(
        !dispatcher.in_modal_loop(),
        "销毁窗口必须退回尚未退出的模态层数（否则唤醒会在模态循环外抢跑泵的 ④）"
    );
}

fn take_posted_frame_wake(hwnd: HWND) -> bool {
    let mut message = MSG::default();
    loop {
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
    dispatcher.attach(hwnd.0 as isize);
    dispatcher.wake();
    assert!(
        take_posted_frame_wake(hwnd),
        "前置：wake() 必须真的 post 一条 WM_APP_FRAME_WAKE"
    );
    unsafe {
        SendMessageW(hwnd, WM_APP_FRAME_WAKE, None, None);
    }
    dispatcher.wake();
    assert!(
        !take_posted_frame_wake(hwnd),
        "前置：竞态状态必须是「位置位、队列里没有唤醒消息」"
    );
    assert_eq!(hook_runs.get(), 0, "前置：模态循环外派发不得出帧");

    unsafe {
        SendMessageW(hwnd, WM_ENTERSIZEMOVE, None, None);
    }
    assert!(
        take_posted_frame_wake(hwnd),
        "进入模态循环时必须重投一条唤醒消息（否则拖动全程零帧，松手后才爆发）"
    );

    unsafe {
        SendMessageW(hwnd, WM_APP_FRAME_WAKE, None, None);
    }
    assert_eq!(hook_runs.get(), 1, "重投的唤醒必须在模态循环里跑一拍钩子");
    assert!(
        !dispatcher.take_frame_wake(),
        "这一拍必须消费唤醒位（否则下一次生产者仍然 coalesce）"
    );

    unsafe {
        SendMessageW(hwnd, WM_EXITSIZEMOVE, None, None);
    }
    assert!(!dispatcher.in_modal_loop());
}

#[test]
fn losing_keyboard_focus_during_composition_emits_exactly_one_ime_end() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcImeFocusLoss", dispatcher);
    let raw = unsafe { &mut *state };
    unsafe {
        SendMessageW(hwnd, WM_IME_STARTCOMPOSITION, None, None);
    }
    assert!(
        raw.take_events()
            .iter()
            .any(|event| matches!(event.event, PlatformEvent::Ime(ImeEvent::Start))),
        "前置：启用态组合开始必须派发 Start"
    );

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

#[test]
fn losing_keyboard_focus_outside_composition_emits_no_ime_end() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcImeNoComposition", dispatcher);
    let raw = unsafe { &mut *state };
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

#[test]
fn frameless_hit_test_returns_resize_codes_caption_and_client() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, class_name) =
        frameless_hidden_window("KytrixFramelessHitTest", dispatcher, 640, 480);
    let mut client = RECT::default();
    unsafe { GetClientRect(hwnd, &mut client) }.expect("GetClientRect");
    let width = client.right - client.left;
    let height = client.bottom - client.top;
    assert!(width > 32 && height > 32, "前置：客户区必须足够大");
    let mut origin = POINT { x: 0, y: 0 };
    unsafe {
        let _ = ClientToScreen(hwnd, &mut origin);
    }
    let center_x = f32::from(i16::try_from(width / 2).expect("width"));
    let center_y = f32::from(i16::try_from(height / 2).expect("height"));
    let right_x = f32::from(i16::try_from(width - 2).expect("width"));
    let bottom_y = f32::from(i16::try_from(height - 2).expect("height"));

    assert_eq!(
        hit_test(hwnd, DevicePixelRatio::ONE, origin, 1.0, 1.0),
        HTTOPLEFT,
        "左上角必须是 HTTOPLEFT"
    );
    assert_eq!(
        hit_test(hwnd, DevicePixelRatio::ONE, origin, right_x, 1.0),
        HTTOPRIGHT,
        "右上角必须是 HTTOPRIGHT"
    );
    assert_eq!(
        hit_test(hwnd, DevicePixelRatio::ONE, origin, 1.0, bottom_y),
        HTBOTTOMLEFT,
        "左下角必须是 HTBOTTOMLEFT"
    );
    assert_eq!(
        hit_test(hwnd, DevicePixelRatio::ONE, origin, right_x, bottom_y),
        HTBOTTOMRIGHT,
        "右下角必须是 HTBOTTOMRIGHT"
    );
    assert_eq!(
        hit_test(hwnd, DevicePixelRatio::ONE, origin, 1.0, center_y),
        HTLEFT,
        "左边必须是 HTLEFT"
    );
    assert_eq!(
        hit_test(hwnd, DevicePixelRatio::ONE, origin, right_x, center_y),
        HTRIGHT,
        "右边必须是 HTRIGHT"
    );
    assert_eq!(
        hit_test(hwnd, DevicePixelRatio::ONE, origin, center_x, 1.0),
        HTTOP,
        "上边必须是 HTTOP"
    );
    assert_eq!(
        hit_test(hwnd, DevicePixelRatio::ONE, origin, center_x, bottom_y),
        HTBOTTOM,
        "下边必须是 HTBOTTOM"
    );
    assert_eq!(
        hit_test(hwnd, DevicePixelRatio::ONE, origin, center_x, center_y),
        HTCLIENT,
        "客户区内部必须是 HTCLIENT"
    );

    unsafe {
        (*state).set_drag_regions(&[LogicalRect::new(px(0.0), px(0.0), px(200.0), px(24.0))]);
    }
    assert_eq!(
        hit_test(hwnd, DevicePixelRatio::ONE, origin, 100.0, 12.0),
        HTCAPTION,
        "拖动区命中必须返回 HTCAPTION"
    );
    assert_eq!(
        hit_test(hwnd, DevicePixelRatio::ONE, origin, 100.0, 100.0),
        HTCLIENT,
        "拖动区之外仍是 HTCLIENT"
    );
    assert_eq!(
        hit_test(hwnd, DevicePixelRatio::ONE, origin, 2.0, 2.0),
        HTTOPLEFT,
        "resize 带优先于拖动区"
    );
    destroy_hidden_window(hwnd, state);
    let _ = class_name;
}

#[test]
fn frameless_track_size_is_the_client_size_without_the_non_client_area() {
    let _runtime = runtime();
    let limits = placement::ClientLimits::default();
    let dpi = crate::platform::windows::dpi::system_dpi();
    let frameless = dpi::track_size(
        LogicalSize::new(px(400.0), px(300.0)),
        dpi,
        Decorations::None,
    );
    let native = dpi::track_size(
        LogicalSize::new(px(400.0), px(300.0)),
        dpi,
        Decorations::Native,
    );
    assert!(
        native.0 > frameless.0 && native.1 > frameless.1,
        "Native 的跟踪尺寸必须多一圈非客户区：frameless={frameless:?} native={native:?}"
    );

    let dispatcher = Dispatcher::new();
    let (hwnd, state, class_name) = hidden_window_with_limits(
        "KytrixFramelessMinMax",
        dispatcher,
        limits,
        Decorations::None,
        640,
        480,
    );
    let mut info = MINMAXINFO::default();
    unsafe {
        SendMessageW(
            hwnd,
            WM_GETMINMAXINFO,
            None,
            Some(LPARAM((&raw mut info) as isize)),
        );
    }
    let dpi = dpi::window_dpi(hwnd);
    let expected = dpi::track_size(dpi::MIN_SIZE_LOGICAL, dpi, Decorations::None);
    assert_eq!(
        (info.ptMinTrackSize.x, info.ptMinTrackSize.y),
        (expected.0 as i32, expected.1 as i32),
        "frameless 的 ptMinTrackSize 必须是客户区物理尺寸（dpi={dpi}）"
    );
    destroy_hidden_window(hwnd, state);
    let _ = class_name;
}

#[test]
fn wm_size_marks_chrome_dirty_for_the_next_sync() {
    let dispatcher = Dispatcher::new();
    let (hwnd, state, class_name) =
        frameless_hidden_window("KytrixChromeDirty", Arc::clone(&dispatcher), 320, 240);
    assert!(
        unsafe { (*state).take_chrome_dirty() },
        "建窗初值必须待对账（首轮泵据此校正 WindowState）"
    );
    unsafe {
        SendMessageW(
            hwnd,
            WM_SIZE,
            Some(WPARAM(0)),
            Some(LPARAM(encoded_client(640, 480))),
        );
    }
    assert!(
        unsafe { (*state).take_chrome_dirty() },
        "WM_SIZE 必须置 chrome_dirty（否则最大化 / 还原的 WindowState 永不刷新）"
    );
    assert!(
        !unsafe { (*state).take_chrome_dirty() },
        "取走即清：同一条 WM_SIZE 只允许触发一次对账"
    );
    destroy_hidden_window(hwnd, state);
    let _ = class_name;
}

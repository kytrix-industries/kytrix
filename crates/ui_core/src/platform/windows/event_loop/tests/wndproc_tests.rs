//! `WndProc` 消息分派与 `WndProcState` 的集成单测（真实隐藏窗口）。

use std::sync::Arc;

use windows::Win32::{
    Foundation::{HWND, LPARAM, POINT, RECT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Input::{
            Ime::{GCS_RESULTSTR, ImmAssociateContext, ImmGetContext, ImmReleaseContext},
            KeyboardAndMouse::{GetCapture, VK_ESCAPE},
        },
        WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, GWLP_USERDATA, GetCursorPos, GetWindowRect, HTCAPTION,
            HTRIGHT, IsZoomed, RegisterClassW, SendMessageW, SetWindowLongPtrW, WINDOW_EX_STYLE,
            WM_CANCELMODE, WM_CAPTURECHANGED, WM_IME_CHAR, WM_IME_COMPOSITION, WM_IME_SETCONTEXT,
            WM_IME_STARTCOMPOSITION, WM_INPUTLANGCHANGE, WM_KEYDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
            WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCLBUTTONDBLCLK,
            WM_NCLBUTTONDOWN, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
            WS_OVERLAPPEDWINDOW,
        },
    },
};

use super::super::{WndProcState, caption::CAPTION_TIMER_ID, wndproc};
use crate::{
    event::ImeEvent,
    platform::{
        dispatcher::{Dispatcher, WM_APP_COMPOSITION_WAKE},
        event::PlatformEvent,
    },
    window::WindowId,
};

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 建一个**隐藏**窗口并挂上生产 `WndProc` + `WndProcState`（`GWLP_USERDATA` 指向它）。
fn hidden_window(tag: &str, dispatcher: Arc<Dispatcher>) -> (HWND, *mut WndProcState, Vec<u16>) {
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
            320,
            240,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
    .expect("CreateWindowExW");
    let state = match WndProcState::new(dispatcher, hwnd, WindowId::for_test(1)) {
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

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

/// 拖动测试装配：读出真实光标与当前窗口矩形，并把会话的「上一次应用位置」伪造到真实
/// 光标之前 `delta` —— 下一次 apply 会产生确定的位移（测试不能移动真实光标）。
fn stage_caption_delta(hwnd: HWND, raw: &mut WndProcState, delta: (i32, i32)) -> (POINT, RECT) {
    let mut cursor = POINT::default();
    let mut window = RECT::default();
    // SAFETY: `hwnd` 由调用方的隐藏窗口保证存活；两个输出都是栈上值。
    unsafe {
        GetCursorPos(&mut cursor).expect("GetCursorPos");
        GetWindowRect(hwnd, &mut window).expect("GetWindowRect");
    }
    raw.caption_drag.as_mut().expect("drag session").cursor = POINT {
        x: cursor.x - delta.0,
        y: cursor.y - delta.1,
    };
    (cursor, window)
}

/// 拖动测试的期望位置：与生产 `apply_caption_drag` 同一个纯函数 + 同一个工作区/DPI。
fn expected_caption_position(
    hwnd: HWND,
    cursor: POINT,
    window: RECT,
    delta: (i32, i32),
) -> (i32, i32) {
    let work = super::super::caption::cursor_work_area(cursor).expect("光标所在显示器工作区");
    super::super::caption::caption_position(
        window,
        delta,
        work,
        super::super::caption::window_scale(hwnd),
    )
}

/// 复审 F2：不持有捕获时引擎的 `release_capture` 不得留下 suppress 残留
/// （否则下一次真实会话丢失会被误当成「自己释放」而吞掉取消）。
#[test]
fn release_capture_without_ownership_leaves_no_suppress() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, class_name) = hidden_window("KytrixReleaseCaptureTest", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配、生命周期到 `destroy_hidden_window`。
    let state = unsafe { &mut *state };
    super::super::caption::release_capture(hwnd, state);
    assert!(
        !state.suppress_capture_cancel,
        "没有捕获时不得置 suppress（会吞掉下一次真实取消）"
    );
    destroy_hidden_window(hwnd, state as *mut WndProcState);
    // 占位：保证 `class_name` 活到窗口销毁之后。
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
        !raw.events
            .iter()
            .any(|event| matches!(event, crate::platform::event::PlatformEvent::PointerCancel)),
        "自己释放产生的捕获变化不得派发 PointerCancel"
    );
    destroy_hidden_window(hwnd, state);
    let _ = class_name;
}

/// P1-1 回归：`WM_APP_COMPOSITION_WAKE` 只唤醒泵，**不**置帧唤醒位。
///
/// 旧实现调用 `state.wake()`：每次帧完成都会让泵多跑一次 `App::frame`（HUD 下是真实的
/// paint + present），完成通知因此变成了额外的帧源。
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

/// P1-2b 回归：缩放期间的 `WM_MOUSEMOVE` 只记录光标，不移动窗口（`SetWindowPos` 在定时器
/// tick / 抬起时合并）。旧实现每个鼠标消息都 `SetWindowPos` ⇒ 348 Hz 上报时 FPS 193.8。
#[test]
fn mouse_move_during_a_resize_does_not_move_the_window() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcResizePacing", Arc::clone(&dispatcher));

    let mut before = RECT::default();
    // SAFETY: 窗口由本测试创建且仍存活；`SendMessageW` 同步进入 `wndproc`。
    unsafe {
        // 按下右边框：进入自绘缩放（定时器启动），不交给 DefWindowProcW 的模态循环。
        SendMessageW(hwnd, WM_NCLBUTTONDOWN, Some(WPARAM(HTRIGHT as usize)), None);
        GetWindowRect(hwnd, &mut before).expect("GetWindowRect");
        SendMessageW(hwnd, WM_MOUSEMOVE, None, None);
    }
    let mut after = RECT::default();
    // SAFETY: 同上。
    unsafe {
        GetWindowRect(hwnd, &mut after).expect("GetWindowRect");
    }
    assert_eq!(
        (after.left, after.top, after.right, after.bottom),
        (before.left, before.top, before.right, before.bottom),
        "WM_MOUSEMOVE 不得直接 SetWindowPos"
    );

    // 抬起：最终 apply（可能按真实光标位置移动窗口）+ 杀定时器 + 释放捕获。
    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_LBUTTONUP, None, None);
    }
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
            .any(|event| matches!(event, PlatformEvent::Ime(ImeEvent::Start))),
        "启用态必须派发 ImeEvent::Start"
    );

    raw.set_ime_enabled(hwnd, false);
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

    raw.set_ime_enabled(hwnd, true);
    assert!(raw.ime_enabled(), "重新启用状态必须被记账");
    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_IME_STARTCOMPOSITION, None, None);
    }
    assert!(
        raw.take_events()
            .iter()
            .any(|event| matches!(event, PlatformEvent::Ime(ImeEvent::Start))),
        "重新启用后必须恢复派发"
    );

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
    unsafe {
        let _ = ImmReleaseContext(hwnd, baseline);
    }

    raw.set_ime_enabled(hwnd, false);
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
    assert!(
        unsafe { ImmGetContext(hwnd) }.is_invalid(),
        "WM_INPUTLANGCHANGE 必须把重新关联的上下文再摘掉"
    );

    // 重新启用：恢复的必须是最初保存的那个上下文（再摘除没有把它覆盖掉）。
    raw.set_ime_enabled(hwnd, true);
    let restored = unsafe { ImmGetContext(hwnd) };
    assert_eq!(restored, baseline, "重新启用必须恢复最初保存的上下文");
    // SAFETY: 同上。
    unsafe {
        let _ = ImmReleaseContext(hwnd, restored);
    }
    destroy_hidden_window(hwnd, state);
}

/// P2-9 回归：按下标题栏进入自绘拖动（不交给 `DefWindowProcW` 的 move 模态循环），
/// 拖动期间的 `WM_MOUSEMOVE` 不直接移动窗口（由 1 ms 定时器合并），抬起收尾并释放捕获。
#[test]
fn caption_press_starts_and_ends_the_engine_drag() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcCaptionDrag", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配、生命周期到 `destroy_hidden_window`。
    let raw = unsafe { &mut *state };
    let mut before = RECT::default();
    let mut after = RECT::default();
    // SAFETY: 窗口由本测试创建且仍存活；`SendMessageW` 同步进入生产 `wndproc`。
    unsafe {
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDOWN,
            Some(WPARAM(HTCAPTION as usize)),
            None,
        );
        assert!(raw.caption_drag.is_some(), "标题栏按下必须进入自绘拖动");
        GetWindowRect(hwnd, &mut before).expect("GetWindowRect");
        SendMessageW(hwnd, WM_MOUSEMOVE, None, None);
        GetWindowRect(hwnd, &mut after).expect("GetWindowRect");
    }
    assert_eq!(
        (after.left, after.top, after.right, after.bottom),
        (before.left, before.top, before.right, before.bottom),
        "拖动期间的 WM_MOUSEMOVE 不得直接 SetWindowPos（合并到定时器）"
    );
    // SAFETY: `GetCapture` 只读查询。
    assert_eq!(
        unsafe { GetCapture() },
        hwnd,
        "拖动期间窗口必须持有鼠标捕获"
    );

    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_LBUTTONUP, None, None);
    }
    assert!(raw.caption_drag.is_none(), "抬起必须结束拖动");
    // SAFETY: `GetCapture` 只读查询。
    assert_ne!(unsafe { GetCapture() }, hwnd, "抬起后必须释放鼠标捕获");
    assert!(
        !raw.suppress_capture_cancel,
        "引擎自己释放捕获产生的 WM_CAPTURECHANGED 必须被消费"
    );
    destroy_hidden_window(hwnd, state);
}

/// win-2 P3（任务 6）：拖动中的 `WM_TIMER` tick 按光标增量移动窗口；会话不结束、捕获保持。
#[test]
fn caption_timer_tick_moves_the_window_and_keeps_the_session() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcCaptionTick", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配、生命周期到 `destroy_hidden_window`。
    let raw = unsafe { &mut *state };
    // SAFETY: 窗口由本测试创建且仍存活；消息同步派发到生产 `wndproc`。
    unsafe {
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDOWN,
            Some(WPARAM(HTCAPTION as usize)),
            None,
        );
    }
    assert!(raw.caption_drag.is_some(), "前置：拖动会话已开始");
    let (cursor, before) = stage_caption_delta(hwnd, raw, (40, 30));

    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_TIMER, Some(WPARAM(CAPTION_TIMER_ID)), None);
    }
    let mut after = RECT::default();
    // SAFETY: 同上。
    unsafe {
        GetWindowRect(hwnd, &mut after).expect("GetWindowRect");
    }
    let expected = expected_caption_position(hwnd, cursor, before, (40, 30));
    assert_eq!(
        (after.left, after.top),
        expected,
        "tick 必须按光标增量平移窗口"
    );
    assert!(raw.caption_drag.is_some(), "tick 不得结束拖动会话");
    // SAFETY: `GetCapture` 只读查询。
    assert_eq!(unsafe { GetCapture() }, hwnd, "拖动期间必须持有鼠标捕获");

    // 收尾：抬起结束会话并释放捕获。
    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_LBUTTONUP, None, None);
    }
    destroy_hidden_window(hwnd, state);
}

/// win-2 P3（任务 6）：`WM_CAPTURECHANGED`（捕获被系统收回）用最新光标位置收尾，
/// 会话归零并释放捕获。
#[test]
fn capture_change_ends_the_caption_drag_at_the_latest_cursor() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcCaptionCapture", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配、生命周期到 `destroy_hidden_window`。
    let raw = unsafe { &mut *state };
    // SAFETY: 窗口由本测试创建且仍存活；消息同步派发到生产 `wndproc`。
    unsafe {
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDOWN,
            Some(WPARAM(HTCAPTION as usize)),
            None,
        );
    }
    let (cursor, before) = stage_caption_delta(hwnd, raw, (35, 25));

    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_CAPTURECHANGED, None, None);
    }
    let mut after = RECT::default();
    // SAFETY: 同上。
    unsafe {
        GetWindowRect(hwnd, &mut after).expect("GetWindowRect");
    }
    let expected = expected_caption_position(hwnd, cursor, before, (35, 25));
    assert_eq!(
        (after.left, after.top),
        expected,
        "失去捕获时必须按最新光标位置收尾"
    );
    assert!(raw.caption_drag.is_none(), "失去捕获必须结束拖动会话");
    // SAFETY: `GetCapture` 只读查询。
    assert_ne!(unsafe { GetCapture() }, hwnd, "收尾后不得继续持有捕获");
    assert!(
        !raw.suppress_capture_cancel,
        "引擎自己释放捕获产生的 WM_CAPTURECHANGED 必须被消费"
    );
    destroy_hidden_window(hwnd, state);
}

/// win-2 P3（任务 6）：`WM_CANCELMODE`（系统取消模态交互）同样按最新光标收尾。
#[test]
fn cancel_mode_ends_the_caption_drag_at_the_latest_cursor() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcCaptionCancel", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配、生命周期到 `destroy_hidden_window`。
    let raw = unsafe { &mut *state };
    // SAFETY: 窗口由本测试创建且仍存活；消息同步派发到生产 `wndproc`。
    unsafe {
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDOWN,
            Some(WPARAM(HTCAPTION as usize)),
            None,
        );
    }
    let (cursor, before) = stage_caption_delta(hwnd, raw, (-30, 20));

    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_CANCELMODE, None, None);
    }
    let mut after = RECT::default();
    // SAFETY: 同上。
    unsafe {
        GetWindowRect(hwnd, &mut after).expect("GetWindowRect");
    }
    let expected = expected_caption_position(hwnd, cursor, before, (-30, 20));
    assert_eq!(
        (after.left, after.top),
        expected,
        "WM_CANCELMODE 必须按最新光标位置收尾"
    );
    assert!(raw.caption_drag.is_none(), "WM_CANCELMODE 必须结束拖动会话");
    // SAFETY: `GetCapture` 只读查询。
    assert_ne!(unsafe { GetCapture() }, hwnd, "收尾后不得继续持有捕获");
    destroy_hidden_window(hwnd, state);
}

/// win-2 P3（任务 3）：拖动中按 `Esc` 还原按下时的窗口矩形、结束会话、释放捕获，
/// 且按键不投递给应用（参考原生 move 循环的取消语义）。
#[test]
fn escape_cancels_the_caption_drag_and_restores_the_origin() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcCaptionEsc", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配、生命周期到 `destroy_hidden_window`。
    let raw = unsafe { &mut *state };
    // SAFETY: 窗口由本测试创建且仍存活；消息同步派发到生产 `wndproc`。
    unsafe {
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDOWN,
            Some(WPARAM(HTCAPTION as usize)),
            None,
        );
    }
    let origin = raw.caption_drag.expect("drag session").origin;

    // 先真实移动一次窗口：证明 Esc 会把窗口放回起始矩形，而不只是结束会话。
    stage_caption_delta(hwnd, raw, (60, 40));
    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_TIMER, Some(WPARAM(CAPTION_TIMER_ID)), None);
    }
    let mut moved = RECT::default();
    // SAFETY: 同上。
    unsafe {
        GetWindowRect(hwnd, &mut moved).expect("GetWindowRect");
    }
    assert_ne!(
        (moved.left, moved.top),
        (origin.left, origin.top),
        "前置：窗口已被拖动"
    );

    // SAFETY: 同上；`WM_KEYDOWN` 的 wParam 是虚拟键码。
    unsafe {
        SendMessageW(hwnd, WM_KEYDOWN, Some(WPARAM(VK_ESCAPE.0 as usize)), None);
    }
    let mut restored = RECT::default();
    // SAFETY: 同上。
    unsafe {
        GetWindowRect(hwnd, &mut restored).expect("GetWindowRect");
    }
    assert_eq!(
        (restored.left, restored.top, restored.right, restored.bottom),
        (origin.left, origin.top, origin.right, origin.bottom),
        "Esc 必须还原按下时的窗口矩形"
    );
    assert!(raw.caption_drag.is_none(), "Esc 必须结束拖动会话");
    // SAFETY: `GetCapture` 只读查询。
    assert_ne!(unsafe { GetCapture() }, hwnd, "Esc 必须释放鼠标捕获");
    assert!(raw.take_events().is_empty(), "Esc 不得投递给应用");
    destroy_hidden_window(hwnd, state);
}

/// win-2 P3（任务 3，最大化分支）：按下最大化窗口的标题栏并拖动到已还原之后，`Esc`
/// 必须回到**最大化**状态（只还原矩形而不重新最大化会留下一个「还原态但占满屏幕」的窗口）。
#[test]
fn escape_cancels_a_restored_drag_back_to_maximized() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcCaptionEscMax", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配、生命周期到 `destroy_hidden_window`。
    let raw = unsafe { &mut *state };
    // SAFETY: 窗口由本测试创建且仍存活；消息同步派发到生产 `wndproc`。
    unsafe {
        // 先最大化（双击标题栏），再按住标题栏拖动。
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDBLCLK,
            Some(WPARAM(HTCAPTION as usize)),
            None,
        );
        assert!(IsZoomed(hwnd).as_bool(), "前置：窗口已最大化");
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDOWN,
            Some(WPARAM(HTCAPTION as usize)),
            None,
        );
    }
    assert!(
        raw.caption_drag.expect("drag session").restore,
        "前置：按下时等待还原（仍是最大化）"
    );

    // 位移超过系统拖拽阈值 ⇒ 还原并按光标摆位。
    stage_caption_delta(hwnd, raw, (120, 60));
    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_TIMER, Some(WPARAM(CAPTION_TIMER_ID)), None);
    }
    // SAFETY: 同上。
    assert!(
        !unsafe { IsZoomed(hwnd) }.as_bool(),
        "前置：拖动已经触发还原"
    );

    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_KEYDOWN, Some(WPARAM(VK_ESCAPE.0 as usize)), None);
    }
    // SAFETY: 同上。
    assert!(
        unsafe { IsZoomed(hwnd) }.as_bool(),
        "Esc 必须把通过拖动还原的最大化窗口重新最大化"
    );
    assert!(raw.caption_drag.is_none(), "Esc 必须结束拖动会话");
    // SAFETY: `GetCapture` 只读查询。
    assert_ne!(unsafe { GetCapture() }, hwnd, "Esc 必须释放鼠标捕获");
    destroy_hidden_window(hwnd, state);
}

/// win-2 P3（任务 4）：拖动期间右键取消拖动并还原原位；中键 / 滚轮被吞掉（不产生应用
/// 事件，也不结束会话）。
#[test]
fn right_click_cancels_the_caption_drag_and_buttons_are_swallowed() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcCaptionButtons", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配、生命周期到 `destroy_hidden_window`。
    let raw = unsafe { &mut *state };
    // SAFETY: 窗口由本测试创建且仍存活；消息同步派发到生产 `wndproc`。
    unsafe {
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDOWN,
            Some(WPARAM(HTCAPTION as usize)),
            None,
        );
    }
    let origin = raw.caption_drag.expect("drag session").origin;
    stage_caption_delta(hwnd, raw, (50, 30));
    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_TIMER, Some(WPARAM(CAPTION_TIMER_ID)), None);
    }

    // 中键 / 滚轮：拖动会话继续，应用收不到任何事件（捕获后坐标是负的客户区坐标）。
    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_MBUTTONDOWN, None, None);
        SendMessageW(hwnd, WM_MBUTTONUP, None, None);
        SendMessageW(hwnd, WM_MOUSEWHEEL, Some(WPARAM(120 << 16)), None);
        SendMessageW(hwnd, WM_MOUSEHWHEEL, Some(WPARAM(120 << 16)), None);
    }
    assert!(raw.caption_drag.is_some(), "中键 / 滚轮不得结束拖动会话");
    assert!(raw.take_events().is_empty(), "中键 / 滚轮不得投递给应用");

    // 右键：没有系统菜单，按「取消拖动」处理 —— 还原原位、结束会话、释放捕获。
    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_RBUTTONDOWN, None, None);
    }
    assert!(raw.caption_drag.is_none(), "右键必须取消拖动");
    assert!(raw.take_events().is_empty(), "右键不得投递给应用");
    // SAFETY: `GetCapture` 只读查询。
    assert_ne!(unsafe { GetCapture() }, hwnd, "右键取消必须释放鼠标捕获");
    let mut restored = RECT::default();
    // SAFETY: 同上。
    unsafe {
        GetWindowRect(hwnd, &mut restored).expect("GetWindowRect");
    }
    assert_eq!(
        (restored.left, restored.top),
        (origin.left, origin.top),
        "右键取消必须还原按下时的位置"
    );

    // 重新按一次标题栏，再用右键抬起取消：down / up 两条消息都按「取消」处理
    //（原生 move 循环里右键按下会打开菜单并结束移动，抬起消息不应漏给应用）。
    // SAFETY: 同上。
    unsafe {
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDOWN,
            Some(WPARAM(HTCAPTION as usize)),
            None,
        );
    }
    assert!(raw.caption_drag.is_some(), "前置：拖动会话重新开始");
    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_RBUTTONUP, None, None);
    }
    assert!(raw.caption_drag.is_none(), "右键抬起也必须取消拖动");
    destroy_hidden_window(hwnd, state);
}

/// win-2 P3（任务 5）：系统没投递 `WM_NCLBUTTONDBLCLK` 时（UI 线程卡顿），快速第二次
/// 按下由自计时兜底判成双击：最大化，而不是开始新的拖动会话。
#[test]
fn self_timed_second_press_toggles_maximize() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcSelfTimedDblClick", dispatcher);
    // SAFETY: `state` 由 `hidden_window` 分配、生命周期到 `destroy_hidden_window`。
    let raw = unsafe { &mut *state };
    // SAFETY: 窗口由本测试创建且仍存活；消息同步派发到生产 `wndproc`。
    unsafe {
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDOWN,
            Some(WPARAM(HTCAPTION as usize)),
            None,
        );
        assert!(raw.caption_drag.is_some(), "第一次按下开始拖动");
        SendMessageW(hwnd, WM_LBUTTONUP, None, None);
        // 第二次按下：与第一次在双击时间 / 矩形内，但没有 WM_NCLBUTTONDBLCLK。
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDOWN,
            Some(WPARAM(HTCAPTION as usize)),
            None,
        );
    }
    assert!(
        unsafe { IsZoomed(hwnd) }.as_bool(),
        "自计时双击必须最大化窗口"
    );
    assert!(raw.caption_drag.is_none(), "双击不得开始新的拖动会话");

    // 第三次快按：双击记录已被消费，应按普通按下开始拖动（不再切换最大化状态）。
    // SAFETY: 同上。
    unsafe {
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDOWN,
            Some(WPARAM(HTCAPTION as usize)),
            None,
        );
    }
    assert!(
        unsafe { IsZoomed(hwnd) }.as_bool(),
        "第三次按下不得再次切换最大化状态"
    );
    assert!(raw.caption_drag.is_some(), "第三次按下应按普通拖动处理");
    // SAFETY: 同上。
    unsafe {
        SendMessageW(hwnd, WM_LBUTTONUP, None, None);
    }
    destroy_hidden_window(hwnd, state);
}

/// P2-9 回归：双击标题栏由引擎自己 `ShowWindow` 最大化/还原，不进入系统 move 模态循环。
#[test]
fn double_click_on_the_caption_toggles_maximize() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcCaptionDblClick", dispatcher);
    // SAFETY: 窗口由本测试创建且仍存活；消息同步派发到生产 `wndproc`。
    unsafe {
        assert!(!IsZoomed(hwnd).as_bool(), "初始不是最大化");
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDBLCLK,
            Some(WPARAM(HTCAPTION as usize)),
            None,
        );
        assert!(IsZoomed(hwnd).as_bool(), "双击标题栏必须最大化");
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDBLCLK,
            Some(WPARAM(HTCAPTION as usize)),
            None,
        );
        assert!(!IsZoomed(hwnd).as_bool(), "再次双击必须还原");
    }
    destroy_hidden_window(hwnd, state);
}

/// P2-9 回归：单击（按下 + 抬起、没有拖动）最大化窗口的标题栏**不得**还原 ——
/// 还原只应发生在拖动超过系统阈值时（与原生 move 循环一致）。
#[test]
fn clicking_the_caption_of_a_maximized_window_does_not_restore_it() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcCaptionClick", dispatcher);
    // SAFETY: 窗口由本测试创建且仍存活；消息同步派发到生产 `wndproc`。
    unsafe {
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDBLCLK,
            Some(WPARAM(HTCAPTION as usize)),
            None,
        );
        assert!(IsZoomed(hwnd).as_bool(), "前置：窗口已最大化");
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDOWN,
            Some(WPARAM(HTCAPTION as usize)),
            None,
        );
        SendMessageW(hwnd, WM_LBUTTONUP, None, None);
        assert!(
            IsZoomed(hwnd).as_bool(),
            "单击标题栏不得还原最大化窗口（只有拖动才还原）"
        );
    }
    destroy_hidden_window(hwnd, state);
}

/// P2-9：最大化窗口的标题栏拖动**超过系统阈值**后由引擎还原；阈值内不还原。
///
/// 直接驱动 `restore_maximized_for_drag`（拖动分支的唯一还原点），用合成光标位置替代
/// 真实鼠标，避免测试依赖光标/前台窗口状态。
#[test]
fn dragging_a_maximized_caption_restores_it_after_the_threshold() {
    let _runtime = runtime();
    let dispatcher = Dispatcher::new();
    let (hwnd, state, _class) = hidden_window("KytrixWndProcCaptionRestore", dispatcher);
    // SAFETY: 窗口由本测试创建且仍存活；消息同步派发到生产 `wndproc`。
    unsafe {
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDBLCLK,
            Some(WPARAM(HTCAPTION as usize)),
            None,
        );
    }
    assert!(unsafe { IsZoomed(hwnd) }.as_bool(), "前置：窗口已最大化");

    let press = POINT { x: 600, y: 10 };
    let work = super::super::caption::cursor_work_area(press).expect("光标所在显示器工作区");
    let scale = super::super::caption::window_scale(hwnd);
    assert!(
        !super::super::caption::restore_maximized_for_drag(
            hwnd,
            press,
            POINT {
                x: press.x + 1,
                y: press.y + 1,
            },
            work,
            scale,
        ),
        "未超过拖拽阈值时不得还原"
    );
    // SAFETY: 窗口仍存活。
    assert!(unsafe { IsZoomed(hwnd) }.as_bool(), "阈值内不得还原");

    assert!(
        super::super::caption::restore_maximized_for_drag(
            hwnd,
            press,
            POINT {
                x: press.x + 120,
                y: press.y + 60,
            },
            work,
            scale,
        ),
        "超过阈值必须还原"
    );
    // SAFETY: 同上。
    assert!(
        !unsafe { IsZoomed(hwnd) }.as_bool(),
        "还原后不再处于最大化状态"
    );
    destroy_hidden_window(hwnd, state);
}

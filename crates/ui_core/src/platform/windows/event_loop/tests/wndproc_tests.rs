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

use std::sync::Arc;

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
            HTCLIENT, HTCLOSE, HTLEFT, HTMAXBUTTON, HTMINBUTTON, HTSYSMENU, HTTOPLEFT,
            PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, RegisterClassW, SM_CXPADDEDBORDER,
            SM_CXSIZEFRAME, SPI_SETANIMATION, SPI_SETWORKAREA, SendMessageW, SetWindowLongPtrW,
            WINDOW_EX_STYLE, WM_APP, WM_CAPTURECHANGED, WM_DISPLAYCHANGE, WM_ENTERSIZEMOVE,
            WM_EXITSIZEMOVE, WM_IME_CHAR, WM_IME_COMPOSITION, WM_IME_SETCONTEXT,
            WM_IME_STARTCOMPOSITION, WM_INPUTLANGCHANGE, WM_LBUTTONDOWN, WM_MOUSEMOVE,
            WM_NCHITTEST, WM_POWERBROADCAST, WM_SETTINGCHANGE, WM_SIZE, WM_TIMER, WNDCLASSW,
            WS_OVERLAPPEDWINDOW,
        },
    },
};

use super::super::{WndProcState, pump::A11Y_TIMER_ID, wndproc};
use crate::{
    app::App,
    event::ImeEvent,
    platform::{
        dispatcher::{Dispatcher, WM_APP_COMPOSITION_WAKE},
        event::PlatformEvent,
        windows::{
            dpi,
            ime::{self, ImeApplied, ImeCommand},
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
    let state = match WndProcState::new(
        dispatcher,
        hwnd,
        WindowId::for_test(1),
        None,
        crate::a11y::adapter::A11yConsumerEpoch::new(),
    ) {
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
        raw.min_size.0 <= 800 && raw.min_size.1 <= 600,
        "前置：建议矩形必须大于最小跟踪尺寸（否则会被系统钳制），min_size={:?}",
        raw.min_size
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
    let a11y_epoch = app
        .borrow()
        .window(id)
        .map_or_else(crate::a11y::adapter::A11yConsumerEpoch::new, |window| {
            window.a11y_consumer_epoch()
        });
    let native = NativeWindow::create(
        id,
        &options,
        Arc::clone(&dispatcher),
        Some(&app),
        a11y_epoch,
    )
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
    let a11y_epoch = app
        .borrow()
        .window(id)
        .map_or_else(crate::a11y::adapter::A11yConsumerEpoch::new, |window| {
            window.a11y_consumer_epoch()
        });
    let mut native = NativeWindow::create(
        id,
        &options,
        Arc::clone(&dispatcher),
        Some(&app),
        a11y_epoch,
    )
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

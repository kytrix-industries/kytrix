//! 自持缩放的**泵侧几何应用**单测（P-A / AR7 防线）。
//!
//! 只覆盖 [`apply_pending_resizes`] 的**函数级行为**：取走一次 `pending_resize` →
//! `SetWindowPos` → 再次调用是 no-op。泵里的**调用点接线**（② 阶段、早于 `collect_events`）
//! 没有单测覆盖 —— 删掉那一行调用，这里仍然全绿；那条接线由真机拖动探针
//! `verify_pa_pc.ps1` 验收（AR7 登记的已知测试缺口，见该函数文档）。

use windows::Win32::{
    Foundation::{HWND, RECT},
    UI::WindowsAndMessaging::GetWindowRect,
};

use super::super::{
    pump::apply_pending_resizes,
    resize::{OuterRect, ResizeEdge, ResizeSession},
};
use crate::{
    math::Size,
    platform::{dispatcher::Dispatcher, windows::window::NativeWindow},
    window::{WindowId, WindowOptions},
};

/// 窗口外框（屏幕物理像素；四元组避免依赖 `RECT` 的相等语义）。
fn outer(hwnd: HWND) -> (i32, i32, i32, i32) {
    let mut rect = RECT::default();
    // SAFETY: 窗口存活；`rect` 是栈上值。
    unsafe { GetWindowRect(hwnd, &mut rect) }.expect("GetWindowRect");
    (rect.left, rect.top, rect.right, rect.bottom)
}

/// 建一个**真实隐藏窗口**（生产 `NativeWindow` + `WndProcState`）。
///
/// 900×700 逻辑：外框远大于 `WM_GETMINMAXINFO` 的最小跟踪尺寸（150% DPI 下最小外框约
/// 496×314），所以下面「右下角 +40/+30」的目标外框不会被最小尺寸钳住，断言可以写精确值。
fn native_window(id: WindowId) -> NativeWindow {
    let options = WindowOptions {
        size: Size::new(900.0, 700.0),
        ..WindowOptions::default()
    };
    NativeWindow::create(id, &options, Dispatcher::new()).expect("NativeWindow")
}

/// 在右下角开一次会话并移动光标 ⇒ 产生一个 `pending_resize`（直接驱动状态，不经 `WndProc`）。
fn queue_bottom_right_resize(native: &mut NativeWindow, dx: i32, dy: i32) {
    let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
    let (left, top, right, bottom) = outer(hwnd);
    let start_cursor = (right - 1, bottom - 1);
    let session = ResizeSession::new(
        OuterRect::from_ltrb(left, top, right, bottom),
        start_cursor,
        ResizeEdge::BottomRight,
    );
    native.state_mut().begin_resize(session);
    native
        .state_mut()
        .note_resize_cursor((start_cursor.0 + dx, start_cursor.1 + dy));
}

/// 应用一次 `pending_resize`：窗口外框变成目标矩形（位置不变、右下角按拖动位移），
/// 且 `pending` 被**取走一次**（同一轮最多一次 `SetWindowPos`）。
#[test]
fn applying_a_pending_resize_moves_the_window_once() {
    let mut native = native_window(WindowId::for_test(1));
    let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
    let start = outer(hwnd);
    queue_bottom_right_resize(&mut native, 40, 30);

    apply_pending_resizes(std::slice::from_mut(&mut native));

    assert_eq!(
        outer(hwnd),
        (start.0, start.1, start.2 + 40, start.3 + 30),
        "泵必须把待应用外框落到真实窗口上（左上角固定、右下角按拖动位移）"
    );
    assert!(
        native.state_mut().take_pending_resize().is_none(),
        "pending_resize 必须被消费一次"
    );
}

/// 没有待应用外框时是 no-op：连续调用不移动窗口（否则稳态每轮都白付一次 `SetWindowPos`）。
#[test]
fn applying_without_a_pending_resize_is_a_no_op() {
    let mut native = native_window(WindowId::for_test(2));
    let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
    let start = outer(hwnd);
    for _ in 0..3 {
        apply_pending_resizes(std::slice::from_mut(&mut native));
        assert_eq!(outer(hwnd), start, "没有待应用外框时不得移动窗口");
    }

    // 有 pending 时应用一次；随后再调用同样是 no-op（不能把同一目标重复应用成抖动）。
    queue_bottom_right_resize(&mut native, 24, 16);
    apply_pending_resizes(std::slice::from_mut(&mut native));
    let moved = outer(hwnd);
    assert_eq!(moved, (start.0, start.1, start.2 + 24, start.3 + 16));
    apply_pending_resizes(std::slice::from_mut(&mut native));
    assert_eq!(outer(hwnd), moved, "pending 消费后再次调用必须是 no-op");
}

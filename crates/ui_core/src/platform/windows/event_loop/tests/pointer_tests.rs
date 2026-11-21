//! E17：`WM_MOUSEMOVE` / `WM_MOUSELEAVE` → 生产 `WndProc` → 泵 `collect_events` →
//! `App::dispatch` → `Window::pointer_position` 的全链路（真实隐藏窗口）。
//!
//! 坐标换算的证据：`WM_MOUSEMOVE` 的 `lParam` 是**物理**客户区坐标，`native_window::apply_event`
//! 按**该消息的 scale** 换算成逻辑坐标后投递。本文件断言存储值等于 `scale.to_logical(物理)`，
//! 并在 scale ≠ 1 时显式断言它**不是**原始物理值（DPI 换算真的发生了）。

use std::sync::Arc;

use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    UI::{
        Controls::WM_MOUSELEAVE,
        WindowsAndMessaging::{SendMessageW, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE},
    },
};

use super::super::native_window::collect_events;
use crate::{
    app::App,
    math::{Point, Size},
    platform::{dispatcher::Dispatcher, windows::window::NativeWindow},
    window::{WindowId, WindowOptions},
};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn raw_hwnd(native: &NativeWindow) -> HWND {
    HWND(native.hwnd_raw() as *mut core::ffi::c_void)
}

/// `WM_*` 的 `lParam`：低 16 位 = x、高 16 位 = y（两段有符号 16 位）。
fn encoded_point(x: i32, y: i32) -> isize {
    ((((y as u32) << 16) | (x as u32 & 0xFFFF)) as i32) as isize
}

/// 同步投递一条鼠标消息到生产 `WndProc`（默认过程不会进入系统模态循环）。
fn send_mouse(hwnd: HWND, msg: u32, x: i32, y: i32) {
    // SAFETY: 窗口由调用方保证存活；`SendMessageW` 同步进入本进程 `WndProc`，这些消息
    // 不启动系统 move/size 模态循环（不是 `WM_NCLBUTTONDOWN` / `WM_SYSCOMMAND`）。
    unsafe {
        SendMessageW(
            hwnd,
            msg,
            Some(WPARAM(0)),
            Some(LPARAM(encoded_point(x, y))),
        );
    }
}

/// 建 `App` 窗口 + 真实隐藏原生窗口，清掉建窗期事件，返回全部句柄。
///
/// 先出一帧让根节点拿到真实 bounds（悬停命中有意义），再走消息路径。
fn setup() -> (
    tokio::runtime::Runtime,
    App,
    WindowId,
    Arc<Dispatcher>,
    Vec<NativeWindow>,
) {
    let runtime = runtime();
    let mut app = App::new(runtime.handle().clone());
    let options = WindowOptions {
        size: Size::new(320.0, 240.0),
        ..WindowOptions::default()
    };
    let id = app.create_window(options.clone());
    let dispatcher = Dispatcher::new();
    let native = NativeWindow::create(
        id,
        &options,
        None,
        dispatcher.clone(),
        None,
        crate::platform::windows::window::test_a11y_epoch(),
    )
    .expect("NativeWindow::create");
    let mut windows = vec![native];
    // 建窗期（`apply_dpi_and_size` 的 SetWindowPos / WM_SIZE）产生的事件不属于本用例。
    windows[0].state_mut().take_events();
    let _ = app.drain_events();
    app.frame(id).expect("window exists").expect("frame");
    (runtime, app, id, dispatcher, windows)
}

/// E17 主链路：物理客户区坐标 → 逻辑坐标入库；`WM_MOUSELEAVE`（不带坐标）保留最后值。
#[test]
fn wm_mousemove_stores_the_logical_position_and_leave_keeps_it() {
    let (_runtime, mut app, id, dispatcher, mut windows) = setup();
    let hwnd = raw_hwnd(&windows[0]);
    let scale = windows[0].scale();

    // ① 第一条移动：物理 (300,150) → 逻辑坐标。
    send_mouse(hwnd, WM_MOUSEMOVE, 300, 150);
    collect_events(&mut app, &mut windows, &dispatcher).expect("collect_events");
    let first = Point::new(scale.to_logical(300.0), scale.to_logical(150.0));
    assert_eq!(
        app.pointer_position(id),
        Some(first),
        "WM_MOUSEMOVE 必须按消息 scale 换算后入库（物理 {scale:?}）"
    );
    if scale.factor() != 1.0 {
        assert_ne!(
            app.pointer_position(id),
            Some(Point::new(300.0, 150.0)),
            "非 100% DPI 下不得把物理坐标直接当逻辑坐标"
        );
    }

    // ② 悬停 / 光标不变的移动：位置更新，但不得请求帧（I3）。
    let requests = app.frame_requests();
    send_mouse(hwnd, WM_MOUSEMOVE, 320, 160);
    collect_events(&mut app, &mut windows, &dispatcher).expect("collect_events");
    let moved = Point::new(scale.to_logical(320.0), scale.to_logical(160.0));
    assert_eq!(app.pointer_position(id), Some(moved));
    assert_eq!(
        app.frame_requests(),
        requests,
        "指针位置记账不得请求帧（I3）"
    );

    // ③ `WM_MOUSELEAVE` 不带坐标：离开点 = 最近一次客户区坐标；位置保留最后值（不清 None）。
    send_mouse(hwnd, WM_MOUSELEAVE, 0, 0);
    collect_events(&mut app, &mut windows, &dispatcher).expect("collect_events");
    assert_eq!(
        app.pointer_position(id),
        Some(moved),
        "离开窗口后保留最后已知位置（离开点），不清成 None"
    );
    assert_eq!(
        app.hovered(id),
        None,
        "PointerLeave 必须结束悬停（保留坐标 ≠ 还在窗口内）"
    );
}

/// E17 捕获路径：拖拽 / 指针捕获期间平台继续投递**窗口外**的 `WM_MOUSEMOVE`（客户区坐标
/// 为负或超出客户区），位置必须照常更新。
#[test]
fn a_move_outside_the_client_area_still_updates_the_position() {
    let (_runtime, mut app, id, dispatcher, mut windows) = setup();
    let hwnd = raw_hwnd(&windows[0]);
    let scale = windows[0].scale();

    send_mouse(hwnd, WM_LBUTTONDOWN, 10, 12);
    let outside = (-48, -20);
    send_mouse(hwnd, WM_MOUSEMOVE, outside.0, outside.1);
    collect_events(&mut app, &mut windows, &dispatcher).expect("collect_events");
    let expected = Point::new(
        scale.to_logical(outside.0 as f32),
        scale.to_logical(outside.1 as f32),
    );
    assert_eq!(
        app.pointer_position(id),
        Some(expected),
        "窗口外坐标（捕获期间）必须照常入库"
    );

    send_mouse(hwnd, WM_LBUTTONUP, outside.0, outside.1);
    collect_events(&mut app, &mut windows, &dispatcher).expect("collect_events");
    assert_eq!(app.pointer_position(id), Some(expected), "抬起后保留最后值");
}

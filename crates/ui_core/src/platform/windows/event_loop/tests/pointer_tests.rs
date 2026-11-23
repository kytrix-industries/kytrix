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
    math::{DevicePoint, LogicalPoint, LogicalSize, dp, px},
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

fn encoded_point(x: i32, y: i32) -> isize {
    ((((y as u32) << 16) | (x as u32 & 0xFFFF)) as i32) as isize
}

fn send_mouse(hwnd: HWND, msg: u32, x: i32, y: i32) {
    unsafe {
        SendMessageW(
            hwnd,
            msg,
            Some(WPARAM(0)),
            Some(LPARAM(encoded_point(x, y))),
        );
    }
}

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
        size: LogicalSize::new(px(320.0), px(240.0)),
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
    windows[0].state_mut().take_events();
    let _ = app.drain_events();
    app.frame(id).expect("window exists").expect("frame");
    (runtime, app, id, dispatcher, windows)
}

#[test]
fn wm_mousemove_stores_the_logical_position_and_leave_keeps_it() {
    let (_runtime, mut app, id, dispatcher, mut windows) = setup();
    let hwnd = raw_hwnd(&windows[0]);
    let scale = windows[0].scale();

    send_mouse(hwnd, WM_MOUSEMOVE, 300, 150);
    collect_events(&mut app, &mut windows, &dispatcher).expect("collect_events");
    let first = scale.to_logical_point(DevicePoint::new(dp(300), dp(150)));
    assert_eq!(
        app.pointer_position(id),
        Some(first),
        "WM_MOUSEMOVE 必须按消息 scale 换算后入库（物理 {scale:?}）"
    );
    if scale.factor() != 1.0 {
        assert_ne!(
            app.pointer_position(id),
            Some(LogicalPoint::new(px(300.0), px(150.0))),
            "非 100% DPI 下不得把物理坐标直接当逻辑坐标"
        );
    }

    let requests = app.frame_requests();
    send_mouse(hwnd, WM_MOUSEMOVE, 320, 160);
    collect_events(&mut app, &mut windows, &dispatcher).expect("collect_events");
    let moved = scale.to_logical_point(DevicePoint::new(dp(320), dp(160)));
    assert_eq!(app.pointer_position(id), Some(moved));
    assert_eq!(
        app.frame_requests(),
        requests,
        "指针位置记账不得请求帧（I3）"
    );

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

#[test]
fn a_move_outside_the_client_area_still_updates_the_position() {
    let (_runtime, mut app, id, dispatcher, mut windows) = setup();
    let hwnd = raw_hwnd(&windows[0]);
    let scale = windows[0].scale();

    send_mouse(hwnd, WM_LBUTTONDOWN, 10, 12);
    let outside = (-48, -20);
    send_mouse(hwnd, WM_MOUSEMOVE, outside.0, outside.1);
    collect_events(&mut app, &mut windows, &dispatcher).expect("collect_events");
    let expected = scale.to_logical_point(DevicePoint::new(dp(outside.0), dp(outside.1)));
    assert_eq!(
        app.pointer_position(id),
        Some(expected),
        "窗口外坐标（捕获期间）必须照常入库"
    );

    send_mouse(hwnd, WM_LBUTTONUP, outside.0, outside.1);
    collect_events(&mut app, &mut windows, &dispatcher).expect("collect_events");
    assert_eq!(app.pointer_position(id), Some(expected), "抬起后保留最后值");
}

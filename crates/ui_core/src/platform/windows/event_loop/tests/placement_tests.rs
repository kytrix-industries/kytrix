use windows::Win32::{
    Foundation::{HWND, LPARAM, POINT, WPARAM},
    Graphics::Gdi::ClientToScreen,
    UI::WindowsAndMessaging::{SendMessageW, WM_MOVE},
};

use super::super::native_window::collect_events;
use crate::{
    app::App,
    event::Event,
    math::{Point, Size},
    platform::{
        dispatcher::Dispatcher,
        windows::{dpi, placement, window::NativeWindow},
    },
    window::WindowOptions,
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

fn client_origin_screen(hwnd: HWND) -> (i32, i32) {
    let mut origin = POINT::default();
    let ok = unsafe { ClientToScreen(hwnd, &mut origin) };
    assert!(ok.as_bool(), "ClientToScreen(0, 0)");
    (origin.x, origin.y)
}

fn encoded_point(x: i32, y: i32) -> isize {
    ((((y as u32) << 16) | (x as u32 & 0xFFFF)) as i32) as isize
}

fn primary_work_area() -> crate::math::PhysicalRect {
    placement::work_areas()
        .into_iter()
        .find(|area| area.x0 <= 0 && 0 <= area.x1 && area.y0 <= 0 && 0 <= area.y1)
        .expect("本机至少有一个包含虚拟桌面原点的工作区")
}

#[test]
fn a_positioned_window_lands_its_client_origin_on_the_logical_point() {
    let runtime = runtime();
    let mut app = App::new(runtime.handle().clone());
    let work = primary_work_area();
    let target = (work.x0 + 120, work.y0 + 80);
    let provisional = dpi::scale_from_dpi(dpi::system_dpi()).expect("system dpi scale");
    let position = Point::new(
        target.0 as f32 / provisional.factor(),
        target.1 as f32 / provisional.factor(),
    );
    let options = WindowOptions {
        position: Some(position),
        size: Size::new(320.0, 240.0),
        ..WindowOptions::default()
    };
    let id = app.create_window(options.clone());
    let native = NativeWindow::create(
        id,
        &options,
        None,
        Dispatcher::new(),
        None,
        crate::platform::windows::window::test_a11y_epoch(),
    )
    .expect("NativeWindow::create");
    let origin = client_origin_screen(raw_hwnd(&native));
    let expected = dpi::physical_origin(position, native.scale());
    assert!(
        (origin.0 - expected.0).abs() <= 1 && (origin.1 - expected.1).abs() <= 1,
        "客户区左上角必须落在逻辑点 p 上：实际 {origin:?}，期望 {expected:?}（position={position:?}，scale={:?}）",
        native.scale()
    );
    assert!(
        work.x0 <= origin.0 && origin.0 < work.x1 && work.y0 <= origin.1 && origin.1 < work.y1,
        "落点必须在工作区内：{origin:?} / {work:?}"
    );
}

#[test]
fn an_off_screen_position_is_clamped_into_a_work_area() {
    let runtime = runtime();
    let mut app = App::new(runtime.handle().clone());
    let options = WindowOptions {
        position: Some(Point::new(-5000.0, -5000.0)),
        size: Size::new(320.0, 240.0),
        ..WindowOptions::default()
    };
    let id = app.create_window(options.clone());
    let native = NativeWindow::create(
        id,
        &options,
        None,
        Dispatcher::new(),
        None,
        crate::platform::windows::window::test_a11y_epoch(),
    )
    .expect("NativeWindow::create");
    let origin = client_origin_screen(raw_hwnd(&native));
    let areas = placement::work_areas();
    assert!(
        areas.iter().any(|area| area.x0 <= origin.0
            && origin.0 < area.x1
            && area.y0 <= origin.1
            && origin.1 < area.y1),
        "clamp 后客户区左上角必须落在某个显示器工作区内：{origin:?} / {areas:?}"
    );
}

#[test]
fn a_programmatic_move_is_dispatched_as_event_moved() {
    let runtime = runtime();
    let mut app = App::new(runtime.handle().clone());
    let options = WindowOptions::default();
    let id = app.create_window(options.clone());
    let dispatcher = Dispatcher::new();
    let mut native = NativeWindow::create(
        id,
        &options,
        None,
        dispatcher.clone(),
        None,
        crate::platform::windows::window::test_a11y_epoch(),
    )
    .expect("NativeWindow::create");
    native.state_mut().take_events();
    let _ = app.drain_events();
    let hwnd = raw_hwnd(&native);
    let target = (-640, 240);
    unsafe {
        SendMessageW(
            hwnd,
            WM_MOVE,
            Some(WPARAM(0)),
            Some(LPARAM(encoded_point(target.0, target.1))),
        );
    }
    let mut windows = vec![native];
    collect_events(&mut app, &mut windows, &dispatcher).expect("collect_events");
    let scale = windows[0].scale();
    let events = app.drain_events();
    let moved: Vec<_> = events
        .iter()
        .filter(|(window, event)| *window == id && matches!(event, Event::Moved { .. }))
        .collect();
    assert_eq!(
        moved.len(),
        1,
        "程序移动必须派发一条 Event::Moved，实际 {events:?}"
    );
    let Event::Moved { position } = moved[0].1 else {
        unreachable!()
    };
    let expected = Point::new(
        scale.to_logical(target.0 as f32),
        scale.to_logical(target.1 as f32),
    );
    assert!(
        (position.x - expected.x).abs() < 1e-3 && (position.y - expected.y).abs() < 1e-3,
        "Event::Moved 必须是按消息 scale 换算后的逻辑坐标：实际 {position:?}，期望 {expected:?}"
    );
}

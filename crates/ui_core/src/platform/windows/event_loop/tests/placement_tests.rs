//! B3 集成单测：真实隐藏窗口的初始落点（`WindowOptions.position` + 工作区 clamp）与
//! `WM_MOVE` → 泵事件 → `Event::Moved` 的全链路。
//!
//! 走的是生产路径：`NativeWindow::create`（`apply_dpi_and_size`）→ `WndProc` →
//! `native_window::collect_events` → `App::dispatch` → `App::drain_events`。
//! 纯函数（clamp / 尺寸约束解析）的单测在被测模块 `placement` 内，这里是真实窗口的接线。

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

/// 客户区左上角的屏幕物理坐标（`ClientToScreen(0, 0)`）。
fn client_origin_screen(hwnd: HWND) -> (i32, i32) {
    let mut origin = POINT::default();
    // SAFETY: 窗口由调用方保证存活；`ClientToScreen` 只做坐标换算、不派发消息。
    let ok = unsafe { ClientToScreen(hwnd, &mut origin) };
    assert!(ok.as_bool(), "ClientToScreen(0, 0)");
    (origin.x, origin.y)
}

/// `WM_MOVE` 的 `lParam`：低 16 位 = x、高 16 位 = y（两段有符号 16 位）。
fn encoded_point(x: i32, y: i32) -> isize {
    ((((y as u32) << 16) | (x as u32 & 0xFFFF)) as i32) as isize
}

/// 主显示器工作区：虚拟桌面坐标里主屏左上角恒为 (0,0)，取包含原点的那个工作区
/// （`EnumDisplayMonitors` 的顺序不保证主屏在前）。
fn primary_work_area() -> crate::math::PhysicalRect {
    placement::work_areas()
        .into_iter()
        .find(|area| area.x0 <= 0 && 0 <= area.x1 && area.y0 <= 0 && 0 <= area.y1)
        .expect("本机至少有一个包含虚拟桌面原点的工作区")
}

/// B3(a)：`position` 是**客户区左上角**的逻辑坐标 —— 建真实窗口后
/// `ClientToScreen(0,0)` 的物理坐标必须等于 `round(position × 窗口所在显示器 scale)`
/// （同屏 DPI 下误差 ≤1 px），且落在工作区内（未被 clamp 改动）。
#[test]
fn a_positioned_window_lands_its_client_origin_on_the_logical_point() {
    let runtime = runtime();
    let mut app = App::new(runtime.handle().clone());
    let work = primary_work_area();
    // 工作区内侧的目标物理客户区原点（离边界足够远，clamp 不介入）。
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
    let native = NativeWindow::create(id, &options, None, Dispatcher::new(), None)
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

/// B3(a) 的 clamp 一半：请求位置在**所有显示器之外**时，真实窗口必须被移回某个工作区
/// （标题栏可见、可拖回），不能真的建在 (-5000, -5000)。
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
    let native = NativeWindow::create(id, &options, None, Dispatcher::new(), None)
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

/// B3(c)：`WM_MOVE`（屏幕物理坐标）→ 泵 `collect_events` → `Event::Moved`（逻辑坐标），
/// 坐标按**该消息的 scale** 换算（公共 API 契约：虚拟桌面内容区左上角为原点的逻辑坐标）。
#[test]
fn a_programmatic_move_is_dispatched_as_event_moved() {
    let runtime = runtime();
    let mut app = App::new(runtime.handle().clone());
    let options = WindowOptions::default();
    let id = app.create_window(options.clone());
    let dispatcher = Dispatcher::new();
    let mut native = NativeWindow::create(id, &options, None, dispatcher.clone(), None)
        .expect("NativeWindow::create");
    // 清掉建窗期（`apply_dpi_and_size` 的 SetWindowPos）产生的事件：只断言本次移动。
    native.state_mut().take_events();
    let _ = app.drain_events();
    let hwnd = raw_hwnd(&native);
    let target = (-640, 240);
    // SAFETY: 窗口由 `native` 持有且仍存活；`WM_MOVE` 的默认过程返回 0，同步进入生产 `WndProc`。
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

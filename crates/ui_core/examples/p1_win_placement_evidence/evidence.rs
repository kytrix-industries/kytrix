use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use ui_core::{
    app::{App, AppHandle, Application},
    event::Event,
    math::{Point, Size},
    window::{WindowId, WindowOptions},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, POINT, RECT},
    Graphics::Gdi::{
        ClientToScreen, EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
    },
    UI::{
        HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow},
        WindowsAndMessaging::{
            FindWindowW, GetClientRect, GetWindowRect, MINMAXINFO, SWP_NOACTIVATE, SWP_NOMOVE,
            SWP_NOSIZE, SWP_NOZORDER, SendMessageW, SetWindowPos, WINDOW_EX_STYLE,
            WM_GETMINMAXINFO, WS_OVERLAPPEDWINDOW,
        },
    },
};

const TITLE: &str = "kytrix P1-B B3 placement evidence";
const CLAMP_TITLE: &str = "kytrix P1-B B3 clamp evidence";
const SIZE: Size = Size::new(640.0, 480.0);
const POSITION: Point = Point::new(120.0, 90.0);
const MIN_SIZE: Size = Size::new(480.0, 360.0);
const MAX_SIZE: Size = Size::new(900.0, 700.0);
const MOVE_BY: (i32, i32) = (160, -60);
const OFF_SCREEN: Point = Point::new(-5000.0, -5000.0);
const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Default)]
struct State {
    phase: u8,
    deadline: Option<Instant>,
    areas: Vec<(i32, i32, i32, i32)>,
    clamp_window: Option<WindowId>,
    moved_events: Vec<Point>,
    position_ok: bool,
    min_max_ok: bool,
    clamp_ok: bool,
    move_requested: Option<(i32, i32)>,
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut app = App::new(runtime.handle().clone());
    let id = app.create_window(WindowOptions {
        title: smol_str::SmolStr::new_static(TITLE),
        size: SIZE,
        position: Some(POSITION),
        min_size: Some(MIN_SIZE),
        max_size: Some(MAX_SIZE),
        ..WindowOptions::default()
    });
    let handle = app.handle();
    let state = Rc::new(RefCell::new(State::default()));
    let passed = Rc::new(Cell::new(false));
    let hook_state = Rc::clone(&state);
    let hook_passed = Rc::clone(&passed);

    app.on_frame(move |app| {
        let mut state = hook_state.borrow_mut();
        let now = Instant::now();
        state.deadline.get_or_insert(now + TIMEOUT);
        if state.deadline.is_some_and(|deadline| now >= deadline) {
            println!("FAIL: 证据采集超时（窗口/事件未在 {TIMEOUT:?} 内就绪）");
            finish(
                app,
                id,
                &handle,
                &hook_passed,
                &state,
                &[state.clamp_window]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>(),
                false,
            );
            return;
        }
        match state.phase {
            0 => {
                let Some(hwnd) = find_window(TITLE) else {
                    println!("等待窗口出现…");
                    handle.wake();
                    return;
                };
                println!("== B3 真机证据（position / min-max / Event::Moved） ==");
                state.areas = host_work_areas();
                println!(
                    "宿主显示器 {} 个，工作区（物理像素）{:?}（多屏 clamp 可实测；单屏时由 \
                     `placement` 纯函数单测覆盖双屏边界）",
                    state.areas.len(),
                    state.areas
                );
                let scale = app
                    .window(id)
                    .map(|window| window.scale().factor())
                    .unwrap_or(1.0);
                let expected = (
                    (POSITION.x * scale).round() as i32,
                    (POSITION.y * scale).round() as i32,
                );
                let actual = client_origin(hwnd);
                let within =
                    (actual.0 - expected.0).abs() <= 1 && (actual.1 - expected.1).abs() <= 1;
                println!(
                    "position：逻辑 {:?} × scale {scale} ⇒ 期望物理客户区原点 {expected:?}；\
                     实测 {actual:?}（GetWindowRect={:?}）⇒ {}",
                    POSITION,
                    window_rect(hwnd),
                    verdict(within, 1)
                );
                state.position_ok = within;

                let mut info = MINMAXINFO::default();
                unsafe {
                    SendMessageW(
                        hwnd,
                        WM_GETMINMAXINFO,
                        None,
                        Some(LPARAM((&raw mut info) as isize)),
                    );
                }
                let dpi = unsafe { GetDpiForWindow(hwnd) }.max(1);
                let scale = f64::from(dpi) / 96.0;
                let min_track = (info.ptMinTrackSize.x, info.ptMinTrackSize.y);
                let max_track = (info.ptMaxTrackSize.x, info.ptMaxTrackSize.y);
                let expected_min = track_size(MIN_SIZE, dpi);
                let expected_max = track_size(MAX_SIZE, dpi);
                let min_max_ok = min_track == expected_min && max_track == expected_max;
                println!(
                    "WM_GETMINMAXINFO（dpi={dpi}）：ptMinTrackSize={min_track:?}（期望 \
                     {expected_min:?}）、ptMaxTrackSize={max_track:?}（期望 {expected_max:?}）⇒ {}",
                    verdict(min_max_ok, 2)
                );
                state.min_max_ok = min_max_ok;

                println!(
                    "SetWindowPos 越界尝试前：窗口外框 {:?}",
                    physical_size(hwnd)
                );
                for (width, height, tag) in [(100, 80, "小于 min"), (3000, 3000, "大于 max")] {
                    unsafe {
                        let _ = SetWindowPos(
                            hwnd,
                            None,
                            0,
                            0,
                            width,
                            height,
                            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
                    println!(
                        "SetWindowPos({width}×{height}，{tag}) 后：窗口外框 {:?}、客户区 {:?}",
                        physical_size(hwnd),
                        client_size(hwnd)
                    );
                }
                let restore = window_size_for_client(physical_client(SIZE, scale), dpi);
                unsafe {
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        0,
                        0,
                        restore.0,
                        restore.1,
                        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                println!(
                    "恢复请求尺寸 {restore:?} 后：窗口外框 {:?}",
                    physical_size(hwnd)
                );

                let clamp_id = app.create_window(WindowOptions {
                    title: smol_str::SmolStr::new_static(CLAMP_TITLE),
                    size: Size::new(320.0, 240.0),
                    position: Some(OFF_SCREEN),
                    ..WindowOptions::default()
                });
                println!(
                    "创建 clamp 证据窗口：position = {OFF_SCREEN:?}（所有显示器之外，逻辑坐标）"
                );
                state.clamp_window = Some(clamp_id);
                state.phase = 1;
                handle.wake();
            }
            1 => {
                let Some(clamp_id) = state.clamp_window else {
                    state.phase = 0;
                    return;
                };
                let Some(clamp_hwnd) = find_window(CLAMP_TITLE) else {
                    handle.wake();
                    return;
                };
                let (left, top, width, height) = window_rect(clamp_hwnd);
                let inside = state.areas.iter().any(|area| {
                    area.0 <= left
                        && left + width <= area.2
                        && area.1 <= top
                        && top + height <= area.3
                });
                println!(
                    "clamp 结果：窗口外框 (left={left}, top={top}, {width}×{height})；\
                     工作区 {:?} ⇒ {}",
                    state.areas,
                    verdict(inside, 4)
                );
                state.clamp_ok = inside;

                let _ = app.close_window(clamp_id);
                let _ = app.drain_events();
                let (client_x, client_y) = client_origin(find_window(TITLE).expect("主窗口"));
                let (window_x, window_y, ..) = window_rect(find_window(TITLE).expect("主窗口"));
                let window_target = (window_x + MOVE_BY.0, window_y + MOVE_BY.1);
                unsafe {
                    let _ = SetWindowPos(
                        find_window(TITLE).expect("主窗口"),
                        None,
                        window_target.0,
                        window_target.1,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                println!(
                    "程序化 SetWindowPos：窗口外框原点 ({window_x},{window_y}) → \
                     {window_target:?}（客户区原点 ({client_x},{client_y}) → {:?}）；\
                     等待事件循环派发 Event::Moved…",
                    (client_x + MOVE_BY.0, client_y + MOVE_BY.1)
                );
                state.move_requested = Some((client_x + MOVE_BY.0, client_y + MOVE_BY.1));
                state.phase = 2;
                handle.wake();
            }
            2 => {
                for (window, event) in app.drain_events() {
                    if window == id
                        && let Event::Moved { position } = event
                    {
                        println!("应用收到 Event::Moved {{ position: {position:?} }}");
                        state.moved_events.push(position);
                    }
                }
                let expected = state.move_requested.map(|(x, y)| {
                    let scale = app
                        .window(id)
                        .map(|window| window.scale().factor())
                        .unwrap_or(1.0);
                    (x as f32 / scale, y as f32 / scale)
                });
                let moved_ok = match (state.moved_events.last().copied(), expected) {
                    (Some(position), Some(expected)) => {
                        (position.x - expected.0).abs() <= 1.0
                            && (position.y - expected.1).abs() <= 1.0
                    }
                    _ => false,
                };
                if moved_ok {
                    println!(
                        "Event::Moved 坐标换算 ⇒ {}（实测 {:?}，期望逻辑坐标 {expected:?}）",
                        verdict(true, 3),
                        state.moved_events.last()
                    );
                    let ok = state.position_ok && state.min_max_ok && state.clamp_ok && moved_ok;
                    let clamp_windows: Vec<WindowId> =
                        [state.clamp_window].into_iter().flatten().collect();
                    finish(app, id, &handle, &hook_passed, &state, &clamp_windows, ok);
                } else {
                    handle.wake();
                }
            }
            _ => {}
        }
    });

    Application::new().run(app)?;
    let ok = passed.get();
    if ok {
        Ok(())
    } else {
        Err("B3 真机证据未全部通过（见上方输出）".into())
    }
}

fn finish(
    app: &mut App,
    id: WindowId,
    handle: &AppHandle,
    passed: &Cell<bool>,
    state: &State,
    clamp_windows: &[WindowId],
    ok: bool,
) {
    println!(
        "汇总：position={} min/max={} clamp={} Event::Moved={}",
        state.position_ok,
        state.min_max_ok,
        state.clamp_ok,
        !state.moved_events.is_empty()
    );
    println!("RESULT: {}", if ok { "PASS" } else { "FAIL" });
    passed.set(ok);
    let _ = app.close_window(id);
    for window in clamp_windows {
        let _ = app.close_window(*window);
    }
    handle.wake();
}

fn find_window(title: &str) -> Option<HWND> {
    let title = wide(title);
    unsafe { FindWindowW(None, windows::core::PCWSTR(title.as_ptr())) }.ok()
}

fn host_work_areas() -> Vec<(i32, i32, i32, i32)> {
    let mut areas: Vec<(i32, i32, i32, i32)> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect_work_area),
            LPARAM((&mut areas as *mut Vec<(i32, i32, i32, i32)>) as isize),
        );
    }
    areas
}

unsafe extern "system" fn collect_work_area(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> windows::core::BOOL {
    let areas = unsafe { &mut *(data.0 as *mut Vec<(i32, i32, i32, i32)>) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        areas.push((
            info.rcWork.left,
            info.rcWork.top,
            info.rcWork.right,
            info.rcWork.bottom,
        ));
    }
    windows::core::BOOL::from(true)
}

fn track_size(client: Size, dpi: u32) -> (i32, i32) {
    let scale = f64::from(dpi) / 96.0;
    let physical = (
        (f64::from(client.width) * scale).round() as u32,
        (f64::from(client.height) * scale).round() as u32,
    );
    window_size_for_client(physical, dpi)
}

fn physical_client(size: Size, scale: f64) -> (u32, u32) {
    (
        (f64::from(size.width) * scale).round() as u32,
        (f64::from(size.height) * scale).round() as u32,
    )
}

fn window_size_for_client(client: (u32, u32), dpi: u32) -> (i32, i32) {
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: client.0.max(1) as i32,
        bottom: client.1.max(1) as i32,
    };
    unsafe {
        AdjustWindowRectExForDpi(
            &mut frame,
            WS_OVERLAPPEDWINDOW,
            false,
            WINDOW_EX_STYLE(0),
            dpi,
        )
    }
    .expect("AdjustWindowRectExForDpi");
    (frame.right - frame.left, frame.bottom - frame.top)
}

fn window_rect(hwnd: HWND) -> (i32, i32, i32, i32) {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.expect("GetWindowRect");
    (
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    )
}

fn client_origin(hwnd: HWND) -> (i32, i32) {
    let mut point = POINT::default();
    assert!(
        unsafe { ClientToScreen(hwnd, &mut point) }.as_bool(),
        "ClientToScreen"
    );
    (point.x, point.y)
}

fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rect = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut rect) };
    (rect.right - rect.left, rect.bottom - rect.top)
}

fn physical_size(hwnd: HWND) -> (i32, i32) {
    let rect = window_rect(hwnd);
    (rect.2, rect.3)
}

fn verdict(pass: bool, checkpoint: u8) -> &'static str {
    match (pass, checkpoint) {
        (true, 1) => "PASS（position 落点）",
        (true, 2) => "PASS（min/max 跟踪尺寸）",
        (true, 3) => "PASS（Event::Moved）",
        (true, 4) => "PASS（越界位置 clamp 到工作区）",
        (true, _) => "PASS",
        (false, 1) => "FAIL（position 落点）",
        (false, 2) => "FAIL（min/max 跟踪尺寸）",
        (false, 3) => "FAIL（Event::Moved）",
        (false, 4) => "FAIL（越界位置 clamp 到工作区）",
        (false, _) => "FAIL",
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

//! X2 真机证据的实现（真实可见窗口 + `SendInput` 真实鼠标拖动；由 `main.rs` 在 Windows 下调用）。

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use ui_core::{
    app::{App, AppHandle, Application},
    event::Event,
    math::{Point, Rect, Size},
    window::{WindowId, WindowOptions},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, POINT, RECT},
    Graphics::Gdi::ClientToScreen,
    UI::{
        HiDpi::GetDpiForWindow,
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_MOUSE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
            MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK,
            MOUSEINPUT, SendInput,
        },
        WindowsAndMessaging::{
            FindWindowW, GetSystemMetrics, GetWindowRect, HTCAPTION, HTCLIENT, HWND_NOTOPMOST,
            HWND_TOPMOST, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN, SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
            SendMessageW, SetForegroundWindow, SetWindowPos, ShowWindow, WM_NCHITTEST,
        },
    },
};

/// 示例窗口标题（`FindWindowW` 定位用）。
const TITLE: &str = "kytrix P1-B X2 drag evidence";
/// 请求的逻辑客户区尺寸。
const SIZE: Size = Size::new(480.0, 320.0);
/// 拖动区：客户区顶部 40 逻辑像素（自绘标题栏的典型条带）。
const DRAG_REGION: Rect = Rect::new(0.0, 0.0, 480.0, 40.0);
/// 拖动时按下的逻辑点（拖动区内部）。
const GRAB_POINT: Point = Point::new(120.0, 20.0);
/// 真实拖动的物理位移与步数（20 步 × 100 ms = 2 s）。
const DRAG_STEP: (i32, i32) = (8, 4);
const DRAG_STEPS: i32 = 20;
/// 整体超时（防止窗口 / 拖动线程挂死）。
const TIMEOUT: Duration = Duration::from_secs(20);

/// 真实拖动线程的结果（与 `on_frame` 共享）。
#[derive(Default)]
struct DragOutcome {
    before: Option<(i32, i32, i32, i32)>,
    after: Option<(i32, i32, i32, i32)>,
    drag_started: bool,
}

/// 证据收集状态。
#[derive(Default)]
struct State {
    phase: u8,
    deadline: Option<Instant>,
    hit_test_ok: bool,
    drag_ok: bool,
    drag: Arc<Mutex<DragOutcome>>,
    drag_done: Arc<AtomicBool>,
    moved_events: Vec<Point>,
}

pub(crate) fn run() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut app = App::new(runtime.handle().clone());
    let id = app.create_window(WindowOptions {
        title: smol_str::SmolStr::new_static(TITLE),
        size: SIZE,
        ..WindowOptions::default()
    });
    // 区域在 `Application::run` 之前声明：泵每轮从 `App` 对账到原生窗口。
    app.set_drag_regions(id, &[DRAG_REGION]);
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
            println!("FAIL: 证据采集超时（窗口 / 拖动线程未在 {TIMEOUT:?} 内就绪）");
            finish(app, id, &handle, &hook_passed, &state, false);
            return;
        }
        match state.phase {
            0 => {
                let Some(hwnd) = find_window() else {
                    println!("等待窗口出现…");
                    handle.wake();
                    return;
                };
                println!("== X2 真机证据（自绘标题栏拖动区） ==");
                // SAFETY: 窗口存活；只做只读查询。
                let dpi = unsafe { GetDpiForWindow(hwnd) }.max(1);
                let scale = f64::from(dpi) / 96.0;
                let mut origin = POINT::default();
                // SAFETY: 窗口存活；`ClientToScreen` 只做坐标换算。
                assert!(
                    unsafe { ClientToScreen(hwnd, &mut origin) }.as_bool(),
                    "ClientToScreen"
                );
                println!(
                    "窗口 dpi={dpi}（scale={scale}），客户区原点（屏幕物理）=({}, {})，\
                     拖动区（逻辑客户区）={DRAG_REGION:?}",
                    origin.x, origin.y
                );
                let inside = hit_test(hwnd, scale, origin, GRAB_POINT);
                let outside = hit_test(hwnd, scale, origin, Point::new(120.0, 200.0));
                let hit_ok = inside == HTCAPTION && outside == HTCLIENT;
                println!(
                    "WM_NCHITTEST：拖动区内（逻辑 (120,20)）={inside}（期望 HTCAPTION={HTCAPTION}）；\
                     拖动区外（逻辑 (120,200)）={outside}（期望 HTCLIENT={HTCLIENT}）⇒ {}",
                    verdict(hit_ok, 1)
                );
                state.hit_test_ok = hit_ok;
                // 真实拖动放到后台线程：UI 线程会随即进入系统 move 模态循环，不能再跑钩子。
                let drag = Arc::clone(&state.drag);
                let drag_done = Arc::clone(&state.drag_done);
                let thread_handle = app.handle();
                // `HWND` 是裸指针包装（非 `Send`）：线程里用 isize 重建（窗口在本示例存活期内
                // 不会销毁，见 `run` 的关闭顺序）。
                let hwnd_raw = hwnd.0 as isize;
                std::thread::Builder::new()
                    .name("x2-drag-evidence".to_owned())
                    .spawn(move || {
                        // SAFETY: `hwnd_raw` 是本示例主窗口句柄，线程运行期间主窗口不销毁
                        //（`finish` 在拖动线程完成后才关窗）。
                        let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
                        drag_with_mouse(hwnd, scale, origin, &drag);
                        drag_done.store(true, Ordering::Release);
                        thread_handle.wake();
                    })
                    .expect("拖拽证据线程");
                state.phase = 1;
                handle.wake();
            }
            1 => {
                if !state.drag_done.load(Ordering::Acquire) {
                    handle.wake();
                    return;
                }
                // 模态循环结束后，泵已把拖动期间的 `WM_MOVE` 收集成 `Event::Moved`。
                for (window, event) in app.drain_events() {
                    if window == id
                        && let Event::Moved { position } = event
                    {
                        state.moved_events.push(position);
                    }
                }
                let (before, after, drag_started) = {
                    let drag = state.drag.lock().expect("drag outcome");
                    (drag.before, drag.after, drag.drag_started)
                };
                let moved = match (before, after) {
                    (Some(before), Some(after)) => Some((
                        after.0 - before.0,
                        after.1 - before.1,
                        after.2 - before.2,
                        after.3 - before.3,
                    )),
                    _ => None,
                };
                let drag_ok = moved.is_some_and(|(dx, dy, dw, dh)| {
                    (dx - DRAG_STEP.0 * DRAG_STEPS).abs() <= 24
                        && (dy - DRAG_STEP.1 * DRAG_STEPS).abs() <= 24
                        && dw == 0
                        && dh == 0
                });
                state.drag_ok = drag_ok;
                println!(
                    "真实拖动（SendInput，2 s）：前 {:?} → 后 {:?}；位移 {:?}\
                     （期望≈({}, {}) 且尺寸不变）；drag_started={} ⇒ {}",
                    before,
                    after,
                    moved.map(|(dx, dy, _, _)| (dx, dy)),
                    DRAG_STEP.0 * DRAG_STEPS,
                    DRAG_STEP.1 * DRAG_STEPS,
                    drag_started,
                    verdict(drag_ok && drag_started, 2)
                );
                println!(
                    "拖动期间应用收到 Event::Moved：{} 条（最后一条 {:?}）⇒ {}",
                    state.moved_events.len(),
                    state.moved_events.last(),
                    verdict(!state.moved_events.is_empty(), 3)
                );
                let ok = state.hit_test_ok && drag_ok && !state.moved_events.is_empty();
                finish(app, id, &handle, &hook_passed, &state, ok);
            }
            _ => {}
        }
    });

    Application::new().run(app)?;
    let ok = passed.get();
    if ok {
        Ok(())
    } else {
        Err("X2 真机证据未全部通过（见上方输出）".into())
    }
}

/// 用 `SendInput` 在拖动区按下并真实移动 ≥2 s（运行在后台线程；UI 线程此时在系统 move
/// 模态循环里）。`scale` / `origin` 来自按下前的窗口几何。
fn drag_with_mouse(hwnd: HWND, scale: f64, origin: POINT, outcome: &Mutex<DragOutcome>) {
    {
        let mut outcome = outcome.lock().expect("drag outcome");
        outcome.before = Some(window_rect(hwnd));
    }
    // 让窗口在最上层并激活：真实点击必须落在本窗口上（其它窗口可能在前面）。
    // SAFETY: 本进程窗口；只改 Z 序 / 激活，不改位置与尺寸。
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        let _ = SetForegroundWindow(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    std::thread::sleep(Duration::from_millis(500));
    let grab = (
        origin.x + (GRAB_POINT.x * scale as f32).round() as i32,
        origin.y + (GRAB_POINT.y * scale as f32).round() as i32,
    );
    println!(
        "真实拖动：在屏幕物理点 {:?}（拖动区内）按下，2 s 内分 {DRAG_STEPS} 步位移 ({}, {})",
        grab,
        DRAG_STEP.0 * DRAG_STEPS,
        DRAG_STEP.1 * DRAG_STEPS
    );
    send_mouse(MOUSE_EVENT_FLAGS(0), Some(grab));
    std::thread::sleep(Duration::from_millis(150));
    send_mouse(MOUSEEVENTF_LEFTDOWN, None);
    {
        let mut outcome = outcome.lock().expect("drag outcome");
        outcome.drag_started = true;
    }
    for step in 1..=DRAG_STEPS {
        std::thread::sleep(Duration::from_millis(100));
        let x = grab.0 + DRAG_STEP.0 * step;
        let y = grab.1 + DRAG_STEP.1 * step;
        send_mouse(MOUSE_EVENT_FLAGS(0), Some((x, y)));
    }
    send_mouse(MOUSEEVENTF_LEFTUP, None);
    std::thread::sleep(Duration::from_millis(400));
    {
        let mut outcome = outcome.lock().expect("drag outcome");
        outcome.after = Some(window_rect(hwnd));
    }
    // SAFETY: 同上；恢复为非置顶。
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_NOTOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
        );
    }
}

/// 发一条鼠标输入。`position = Some(屏幕物理坐标)` 表示绝对移动（隐含 `MOUSEEVENTF_MOVE`）；
/// `None` 表示在当前光标位置发按钮事件（`flags` 是按键位）。
fn send_mouse(flags: MOUSE_EVENT_FLAGS, position: Option<(i32, i32)>) {
    // SAFETY: 无参数、无副作用的系统度量查询。
    let (left, top, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let normalize = |value: i32, origin: i32, extent: i32| -> i32 {
        ((i64::from(value) - i64::from(origin)) * 65535 / i64::from(extent.max(2) - 1)) as i32
    };
    let (dx, dy, flags) = match position {
        Some((x, y)) => (
            normalize(x, left, width),
            normalize(y, top, height),
            MOUSE_EVENT_FLAGS(
                flags.0 | MOUSEEVENTF_MOVE.0 | MOUSEEVENTF_ABSOLUTE.0 | MOUSEEVENTF_VIRTUALDESK.0,
            ),
        ),
        None => (0, 0, flags),
    };
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    // SAFETY: `input` 是栈上结构，长度正确；`SendInput` 向本会话输入流注入鼠标事件。
    let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    if sent == 0 {
        // SAFETY: 无参数、线程局部错误查询（SendInput 失败后有效）。
        let error = unsafe { windows::Win32::Foundation::GetLastError() }.0;
        println!("警告：SendInput 失败（GetLastError={error}）");
    }
}

fn hit_test(hwnd: HWND, scale: f64, origin: POINT, logical: Point) -> u32 {
    let point = POINT {
        x: origin.x + (logical.x * scale as f32).round() as i32,
        y: origin.y + (logical.y * scale as f32).round() as i32,
    };
    let encoded = ((((point.y as u32) << 16) | (point.x as u32 & 0xFFFF)) as i32) as isize;
    // SAFETY: 窗口存活；`WM_NCHITTEST` 的 lParam 是两个 16 位屏幕坐标。
    let result = unsafe { SendMessageW(hwnd, WM_NCHITTEST, None, Some(LPARAM(encoded))) };
    result.0 as u32
}

fn find_window() -> Option<HWND> {
    let title = wide(TITLE);
    // SAFETY: 标题是栈上 NUL 结尾宽字符串；只查询本进程创建的窗口。
    unsafe { FindWindowW(None, windows::core::PCWSTR(title.as_ptr())) }.ok()
}

fn window_rect(hwnd: HWND) -> (i32, i32, i32, i32) {
    let mut rect = RECT::default();
    // SAFETY: 窗口存活；`rect` 是栈上输出。
    unsafe { GetWindowRect(hwnd, &mut rect) }.expect("GetWindowRect");
    (
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    )
}

fn finish(
    app: &mut App,
    id: WindowId,
    handle: &AppHandle,
    passed: &Cell<bool>,
    state: &State,
    ok: bool,
) {
    println!(
        "汇总：hit_test={} real_drag={} Event::Moved={} 条",
        state.hit_test_ok,
        state.drag_ok,
        state.moved_events.len()
    );
    println!("RESULT: {}", if ok { "PASS" } else { "FAIL" });
    passed.set(ok);
    let _ = app.close_window(id);
    handle.wake();
}

fn verdict(pass: bool, checkpoint: u8) -> &'static str {
    match (pass, checkpoint) {
        (true, 1) => "PASS（WM_NCHITTEST）",
        (true, 2) => "PASS（真实拖动改变窗口位置）",
        (true, 3) => "PASS（Event::Moved）",
        (true, _) => "PASS",
        (false, 1) => "FAIL（WM_NCHITTEST）",
        (false, 2) => "FAIL（真实拖动改变窗口位置）",
        (false, 3) => "FAIL（Event::Moved）",
        (false, _) => "FAIL",
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

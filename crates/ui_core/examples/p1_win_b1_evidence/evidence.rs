use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use ui_core::{
    app::{App, AppHandle, Application},
    event::Event,
    math::{LogicalRect, LogicalSize, px},
    node::builder::NodeBuilder,
    paint::{
        color::Color,
        style::{PaintSpec, StylePatch},
    },
    window::{WindowId, WindowOptions},
};
use windows::Win32::{
    Foundation::{FILETIME, HWND, POINT, RECT},
    Graphics::Gdi::ClientToScreen,
    System::Threading::{
        AttachThreadInput, GetCurrentProcess, GetCurrentThreadId, GetProcessTimes,
    },
    UI::{
        HiDpi::GetDpiForWindow,
        Input::KeyboardAndMouse::{
            GetKeyState, INPUT, INPUT_0, INPUT_MOUSE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
            MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK,
            MOUSEINPUT, SendInput, VK_LBUTTON,
        },
        WindowsAndMessaging::{
            FindWindowW, GUI_INMOVESIZE, GUITHREADINFO, GetForegroundWindow, GetGUIThreadInfo,
            GetSystemMetrics, GetWindowRect, GetWindowThreadProcessId, HWND_NOTOPMOST,
            HWND_TOPMOST, IsIconic, IsWindowVisible, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
            SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
            SWP_NOZORDER, SetCursorPos, SetForegroundWindow, SetWindowPos, ShowWindow,
            WindowFromPoint,
        },
    },
};

const TITLE: &str = "kytrix P1-B B1 modal frame evidence";
const SIZE: LogicalSize = LogicalSize::new(px(420.0), px(260.0));
const DATA_PERIOD: Duration = Duration::from_millis(16);
const DRAG_STEPS: u32 = 32;
const SAMPLE_PERIOD: Duration = Duration::from_millis(50);
const DRAG_SWAY: i32 = 40;
const DRAG_TRAVEL: i32 = 160;
const MIN_REAL_MOVE: i32 = 100;
const DRAG_ATTEMPTS: u32 = 3;
const POST_RELEASE_WINDOW: Duration = Duration::from_millis(500);
const MAX_FRAME_GAP: Duration = Duration::from_millis(200);
const MIN_DRAG_FRAMES: u64 = 32;
const EDGE_CLOSE_AFTER: Duration = Duration::from_millis(1200);
const FIND_TIMEOUT: Duration = Duration::from_secs(10);
const RUN_TIMEOUT: Duration = Duration::from_secs(40);

#[derive(Default)]
struct Metrics {
    committed: AtomicU64,
    last_frame_index: AtomicU32,
    presentations: AtomicU64,
    hooks: AtomicU64,
    moved: AtomicU64,
    data_ticks: AtomicU64,
}

type FrameLog = Arc<Mutex<Vec<(Instant, u32)>>>;

#[derive(Clone, Copy, Default)]
struct DragResult {
    started: bool,
    pressed: bool,
    trigger: &'static str,
    edge_close_sent: bool,
    rect_before: Option<(i32, i32, i32, i32)>,
    rect_after: Option<(i32, i32, i32, i32)>,
    duration: Duration,
    drag_frames: u64,
    post_frames: u64,
    frames_in_drag: u64,
    first_index: Option<u32>,
    last_index: Option<u32>,
    max_gap: Option<Duration>,
    move_loop_samples: u32,
    sample_total: u32,
    cpu: Duration,
    cpu_wall: Duration,
}

impl DragResult {
    fn failed() -> Self {
        Self::default()
    }

    fn moved(&self) -> Option<(i32, i32, i32, i32)> {
        let before = self.rect_before?;
        let after = self.rect_after?;
        Some((
            after.0 - before.0,
            after.1 - before.1,
            after.2 - before.2,
            after.3 - before.3,
        ))
    }
}

struct Evidence {
    window: WindowId,
    edge: bool,
    started: Instant,
    metrics: Arc<Metrics>,
    frames: FrameLog,
    result: Arc<Mutex<Option<DragResult>>>,
    drag_thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    last_index: u32,
    drag_spawned: bool,
    finished: bool,
    passed: Cell<bool>,
}

pub(crate) fn run() -> Result<(), Box<dyn std::error::Error>> {
    let edge = std::env::args().any(|argument| argument == "--edge");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut app = App::new(runtime.handle().clone());
    let id = app.create_window(WindowOptions {
        title: smol_str::SmolStr::new_static(TITLE),
        size: SIZE,
        ..WindowOptions::default()
    });
    let source = app.signal(0u8);
    let root = app.window(id).expect("窗口存在").tree().root();
    let _node = NodeBuilder::boxed()
        .layout_override(LogicalRect::new(px(0.0), px(0.0), SIZE.width, SIZE.height))
        .paint(PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        })
        .bind_style(move |cx| StylePatch {
            opacity: Some(f32::from(*cx.read(&source)) / 255.0),
            ..StylePatch::default()
        })
        .build(app.window_mut(id).expect("窗口存在").tree_mut(), root)
        .expect("数据面板节点");

    let metrics = Arc::new(Metrics::default());
    let frames: FrameLog = Arc::new(Mutex::new(Vec::with_capacity(600)));
    let result = Arc::new(Mutex::new(None));
    let drag_thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    let state = Rc::new(RefCell::new(Evidence {
        window: id,
        edge,
        started: Instant::now(),
        metrics: Arc::clone(&metrics),
        frames: Arc::clone(&frames),
        result: Arc::clone(&result),
        drag_thread: Arc::clone(&drag_thread),
        last_index: 0,
        drag_spawned: false,
        finished: false,
        passed: Cell::new(false),
    }));
    let hook_state = Rc::clone(&state);
    app.on_frame(move |app| on_frame(app, &hook_state));
    let handle = app.handle();

    let stop = Arc::new(AtomicBool::new(false));
    let data_handle = handle.clone();
    let data_stop = Arc::clone(&stop);
    let data_metrics = Arc::clone(&metrics);
    handle.spawn(async move {
        let mut interval = tokio::time::interval(DATA_PERIOD);
        let mut tick = 0u32;
        loop {
            interval.tick().await;
            if data_stop.load(Ordering::Acquire) {
                return;
            }
            let value = (tick % 255) as u8;
            tick = tick.wrapping_add(1);
            data_metrics.data_ticks.fetch_add(1, Ordering::Relaxed);
            let data_handle = data_handle.clone();
            data_handle.send(move |app| {
                app.write(&source, value);
            });
        }
    });

    let run_result = Application::new().run(app);
    stop.store(true, Ordering::Release);
    let joined = drag_thread
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .map(std::thread::JoinHandle::join);
    run_result?;
    let thread_panicked = joined.is_some_and(|joined| joined.is_err());
    if thread_panicked {
        println!("FAIL: 拖动线程 panic（见上方 panic 输出）");
    }

    if edge {
        let dragged = result.lock().ok().and_then(|result| *result);
        let window_gone = find_window().is_none();
        let dragged_detail = dragged.filter(|result| result.started);
        let ok = dragged.is_some_and(|result| result.started && result.edge_close_sent)
            && window_gone
            && !thread_panicked;
        println!(
            "边界：Application::run 正常返回（无 panic）；拖动线程已发送关窗任务={}；证据窗口已销毁={}",
            dragged.is_some_and(|result| result.edge_close_sent),
            window_gone
        );
        println!(
            "边界：数据泵回投 {} 个任务 / `on_frame` 钩子 {} 次；系统 move 模态循环在关窗后仍持续（GUI_INMOVESIZE 采样 {}/{} 为真）；窗口位移 {:?}（尺寸变化 {:?}）",
            dragged_detail.map_or(0, |_| metrics.data_ticks.load(Ordering::Acquire)),
            dragged_detail.map_or(0, |_| metrics.hooks.load(Ordering::Acquire)),
            dragged_detail.map_or(0, |result| result.move_loop_samples),
            dragged_detail.map_or(0, |result| result.sample_total),
            dragged_detail
                .and_then(|result| result.moved())
                .map(|(dx, dy, _, _)| (dx, dy)),
            dragged_detail
                .and_then(|result| result.moved())
                .map(|(_, _, dw, dh)| (dw, dh))
        );
        println!("RESULT: {}", if ok { "PASS" } else { "FAIL" });
        return if ok {
            Ok(())
        } else {
            Err("B1 边界证据未通过（见上方输出）".into())
        };
    }

    let ok = state.borrow().passed.get() && !thread_panicked;
    if ok {
        Ok(())
    } else {
        Err("B1 真机证据未通过（见上方输出）".into())
    }
}

fn on_frame(app: &mut App, state: &Rc<RefCell<Evidence>>) {
    let mut evidence = state.borrow_mut();
    if evidence.finished {
        return;
    }
    let metrics = Arc::clone(&evidence.metrics);
    metrics.hooks.fetch_add(1, Ordering::Relaxed);
    if let Some(window) = app.window(evidence.window) {
        let index = window.frame_index().get();
        if index != evidence.last_index {
            evidence.last_index = index;
            metrics.committed.fetch_add(1, Ordering::Release);
            metrics.last_frame_index.store(index, Ordering::Release);
            if window.last_frame_presentation().is_some() {
                metrics.presentations.fetch_add(1, Ordering::Relaxed);
            }
            if let Ok(mut frames) = evidence.frames.lock() {
                frames.push((Instant::now(), index));
            }
        }
    }
    for (window, event) in app.drain_events() {
        if window == evidence.window && matches!(event, Event::Moved { .. }) {
            metrics.moved.fetch_add(1, Ordering::Relaxed);
        }
    }
    if !evidence.drag_spawned {
        println!("== B1 真机证据（系统 move 模态循环期间的出帧） ==");
        println!(
            "模式：{}；数据泵 {} ms（≈{} Hz）回投任务写信号并请求一帧；真实 `SendInput` 按住系统标题栏拖动 {} 步 × {} ms = {:.1} s（末步单程拉开 {DRAG_TRAVEL} px），松手后观测 {} ms",
            if evidence.edge {
                "边界（拖动中程序化关闭最后一个窗口）"
            } else {
                "标准（拖动系统标题栏）"
            },
            DATA_PERIOD.as_millis(),
            1000 / DATA_PERIOD.as_millis(),
            DRAG_STEPS,
            SAMPLE_PERIOD.as_millis() * 2,
            drag_seconds(),
            POST_RELEASE_WINDOW.as_millis()
        );
        let window = evidence.window;
        let edge = evidence.edge;
        spawn_drag(
            window,
            app.handle(),
            Arc::clone(&evidence.metrics),
            Arc::clone(&evidence.frames),
            Arc::clone(&evidence.result),
            edge,
            Arc::clone(&evidence.drag_thread),
        );
        evidence.drag_spawned = true;
        return;
    }
    if evidence.edge {
        if evidence.started.elapsed() > RUN_TIMEOUT {
            println!("FAIL: 边界用例超时（Application::run 未在 {RUN_TIMEOUT:?} 内退出）");
            evidence.finished = true;
            evidence.passed.set(false);
            let _ = app.close_window(evidence.window);
        }
        return;
    }
    match evidence.result.lock().ok().and_then(|result| *result) {
        None => {
            if evidence.started.elapsed() > RUN_TIMEOUT {
                println!(
                    "FAIL: 拖动线程未在 {RUN_TIMEOUT:?} 内完成（窗口没出现 / SendInput 失败）"
                );
                evidence.finished = true;
                evidence.passed.set(false);
                let _ = app.close_window(evidence.window);
            }
        }
        Some(result) => finish_normal(&mut evidence, app, result),
    }
}

fn finish_normal(evidence: &mut Evidence, app: &mut App, result: DragResult) {
    evidence.finished = true;
    let metrics = Arc::clone(&evidence.metrics);
    let hooks = metrics.hooks.load(Ordering::Acquire);
    let moved = metrics.moved.load(Ordering::Acquire);
    let presentations = metrics.presentations.load(Ordering::Acquire);
    let committed_total = metrics.committed.load(Ordering::Acquire);

    let drag_secs = result.duration.as_secs_f64();
    let drag_fps = if drag_secs > 0.0 {
        result.drag_frames as f64 / drag_secs
    } else {
        0.0
    };
    let post_secs = POST_RELEASE_WINDOW.as_secs_f64();
    let post_fps = result.post_frames as f64 / post_secs;
    let allowed_post = drag_fps * post_secs * 1.8 + 5.0;
    let moved_delta = result.moved();
    let sustained = result.drag_frames >= MIN_DRAG_FRAMES;
    let gap_ok = result.max_gap.is_some_and(|gap| gap <= MAX_FRAME_GAP);
    let no_burst = result.post_frames as f64 <= allowed_post;
    let real_move = moved_delta.is_some_and(|(dx, dy, dw, dh)| {
        dx.abs().max(dy.abs()) > MIN_REAL_MOVE && dw == 0 && dh == 0
    });
    let cpu_percent = if result.cpu_wall.as_secs_f64() > 0.0 {
        result.cpu.as_secs_f64() / result.cpu_wall.as_secs_f64() * 100.0
    } else {
        0.0
    };

    println!(
        "真实拖动（触发={}；左键按下={}；{:.2} s）：外框 {:?} → {:?}；位移 {:?}（尺寸变化 {:?}；要求位移 >{MIN_REAL_MOVE} px 且尺寸不变）",
        result.trigger,
        result.pressed,
        drag_secs,
        result.rect_before,
        result.rect_after,
        moved_delta.map(|(dx, dy, _, _)| (dx, dy)),
        moved_delta.map(|(_, _, dw, dh)| (dw, dh))
    );
    println!(
        "拖动期间系统 move 模态循环（GetGUIThreadInfo → GUI_INMOVESIZE）采样 {}/{} 次为真（>0 才证明泵确实被系统循环占住）",
        result.move_loop_samples, result.sample_total
    );
    println!(
        "拖动窗口内提交帧：{} 帧 / {:.2} s = {:.1} fps；`frame_index` {:?} → {:?}（区间内 {} 帧）；最大帧间隔 {:?}",
        result.drag_frames,
        drag_secs,
        drag_fps,
        result.first_index,
        result.last_index,
        result.frames_in_drag,
        result.max_gap
    );
    println!(
        "松手后 {} ms：{} 帧（{:.1} fps，参考拖动态势 {:.1} fps；无爆发阈值 ≤{:.0} 帧）",
        POST_RELEASE_WINDOW.as_millis(),
        result.post_frames,
        post_fps,
        drag_fps,
        allowed_post
    );
    println!(
        "进程 CPU（GetProcessTimes，user+kernel）：{:.1} ms / {:.2} s = {:.1}%；`on_frame` 钩子 {} 次；提交帧总计 {}",
        result.cpu.as_secs_f64() * 1000.0,
        result.cpu_wall.as_secs_f64(),
        cpu_percent,
        hooks,
        committed_total
    );
    println!(
        "呈现（A4）：观察到 `last_frame_presentation()=Some` 的提交帧 {presentations} 个；应用收到 `Event::Moved` {moved} 条"
    );
    println!(
        "判定：持续出帧（≥{MIN_DRAG_FRAMES} 帧）={sustained}；最大帧间隔 ≤{} ms = {gap_ok}；松手后无堆积爆发 = {no_burst}；窗口真实移动（>{MIN_REAL_MOVE} px）={real_move}",
        MAX_FRAME_GAP.as_millis()
    );
    let ok = result.started && result.pressed && sustained && gap_ok && no_burst && real_move;
    println!("RESULT: {}", if ok { "PASS" } else { "FAIL" });
    evidence.passed.set(ok);
    let _ = app.close_window(evidence.window);
}

fn spawn_drag(
    window: WindowId,
    handle: AppHandle,
    metrics: Arc<Metrics>,
    frames: FrameLog,
    result: Arc<Mutex<Option<DragResult>>>,
    edge: bool,
    thread_slot: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
) {
    let thread = std::thread::Builder::new()
        .name("b1-drag-evidence".to_owned())
        .spawn(move || {
            let computed = drag(window, &handle, &metrics, &frames, edge);
            if let Ok(mut slot) = result.lock() {
                *slot = Some(computed);
            }
            handle.wake();
        });
    match thread {
        Ok(thread) => {
            if let Ok(mut slot) = thread_slot.lock() {
                *slot = Some(thread);
            }
        }
        Err(error) => println!("FAIL: 拖动证据线程创建失败：{error}"),
    }
}

fn drag(
    window: WindowId,
    handle: &AppHandle,
    metrics: &Arc<Metrics>,
    frames: &FrameLog,
    edge: bool,
) -> DragResult {
    let Some(hwnd) = wait_for_window() else {
        println!("FAIL: {FIND_TIMEOUT:?} 内没找到证据窗口（标题 {TITLE}）");
        return DragResult::failed();
    };
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
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    std::thread::sleep(Duration::from_millis(300));
    let foreground = activate_window(hwnd);
    println!(
        "前台={foreground}（当前前台窗口={:?}）；本窗口 visible={} iconic={}",
        unsafe { GetForegroundWindow() },
        unsafe { IsWindowVisible(hwnd) }.as_bool(),
        unsafe { IsIconic(hwnd) }.as_bool()
    );
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(1);
    let scale = f64::from(dpi) / 96.0;
    for attempt in 1..=DRAG_ATTEMPTS {
        let Some(before) = window_rect(hwnd) else {
            println!("FAIL: GetWindowRect 失败（窗口已销毁）");
            return DragResult::failed();
        };
        let Some(origin) = client_origin(hwnd) else {
            println!("FAIL: ClientToScreen 失败");
            return DragResult::failed();
        };
        let grab = caption_grab_point(before, origin);
        let cursor_moved = unsafe { SetCursorPos(grab.0, grab.1) }.is_ok();
        std::thread::sleep(Duration::from_millis(150));
        let under = unsafe {
            WindowFromPoint(POINT {
                x: grab.0,
                y: grab.1,
            })
        };
        if !cursor_moved || under != hwnd {
            println!(
                "尝试 {attempt}/{DRAG_ATTEMPTS}: 标题栏按下点不可用（SetCursorPos={cursor_moved}，WindowFromPoint={under:?}，期望 {hwnd:?}）；重新置顶后重试"
            );
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
            }
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        println!(
            "窗口：外框（左上角 + 宽高）{before:?}，客户区原点（屏幕物理）=({}, {})，DPI={dpi}（scale={scale}），标题栏按下点（屏幕物理）={grab:?}",
            origin.x, origin.y
        );

        let cpu_before = process_cpu();
        let wall_before = Instant::now();
        let committed_before = metrics.committed.load(Ordering::Acquire);
        println!(
            "拖动开始（尝试 {attempt}/{DRAG_ATTEMPTS}）：提交帧={committed_before}（frame_index={}）",
            metrics.last_frame_index.load(Ordering::Acquire)
        );
        send_mouse(MOUSEEVENTF_LEFTDOWN, None);
        std::thread::sleep(Duration::from_millis(80));
        let pressed = left_button_down();
        let drag_begin = Instant::now();
        let mut moved = false;
        let mut move_loop_samples = 0u32;
        let mut sample_total = 0u32;
        let mut edge_close_sent = false;
        for step in 1..=DRAG_STEPS {
            if edge && !edge_close_sent && drag_begin.elapsed() >= EDGE_CLOSE_AFTER {
                handle.send(move |app| {
                    let _ = app.close_window(window);
                });
                edge_close_sent = true;
                println!(
                    "边界：拖动 +{:.2} s 时通过 AppHandle 回投任务关闭最后一个窗口（拖动继续，检验模态 tick 不 panic）",
                    drag_begin.elapsed().as_secs_f64()
                );
            }
            let offset = drag_offset(step);
            send_mouse(MOUSE_EVENT_FLAGS(0), Some((grab.0 + offset, grab.1)));
            for _ in 0..2 {
                std::thread::sleep(SAMPLE_PERIOD);
                sample(metrics, drag_begin.elapsed(), "拖动");
                sample_total += 1;
                if move_loop_active(hwnd) {
                    move_loop_samples += 1;
                }
            }
            if !moved {
                if window_rect(hwnd).is_some_and(|current| current != before) {
                    moved = true;
                } else if !pressed || step >= 3 {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
        send_mouse(MOUSEEVENTF_LEFTUP, None);
        let drag_end = Instant::now();
        let committed_release = metrics.committed.load(Ordering::Acquire);
        let duration = drag_end.saturating_duration_since(drag_begin);
        std::thread::sleep(Duration::from_millis(120));
        let after = window_rect(hwnd);
        if !moved {
            println!(
                "尝试 {attempt}/{DRAG_ATTEMPTS}: 真实按下后窗口没有移动（外框 {before:?} → {after:?}）；松手后重试"
            );
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
            }
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        let post_begin = Instant::now();
        while post_begin.elapsed() < POST_RELEASE_WINDOW {
            std::thread::sleep(SAMPLE_PERIOD);
            sample(metrics, post_begin.elapsed(), "松手后");
        }
        let committed_post = metrics.committed.load(Ordering::Acquire);
        let cpu_after = process_cpu();
        let cpu_wall = wall_before.elapsed();

        let (frames_in_drag, first_index, last_index, max_gap) =
            frame_history(frames, drag_begin, drag_end);
        let result = DragResult {
            started: true,
            pressed,
            trigger: "SendInput 按住系统标题栏（真实鼠标输入）",
            edge_close_sent,
            rect_before: Some(before),
            rect_after: after,
            duration,
            drag_frames: committed_release.saturating_sub(committed_before),
            post_frames: committed_post.saturating_sub(committed_release),
            frames_in_drag,
            first_index,
            last_index,
            max_gap,
            move_loop_samples,
            sample_total,
            cpu: cpu_after.saturating_sub(cpu_before),
            cpu_wall,
        };
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
        return result;
    }
    println!("FAIL: {DRAG_ATTEMPTS} 次尝试后仍未真正开始系统标题栏拖动");
    DragResult::failed()
}

fn sample(metrics: &Metrics, at: Duration, phase: &str) {
    println!(
        "采样 {phase} t=+{:.2}s：提交帧={} frame_index={} 呈现反馈={} 钩子={}",
        at.as_secs_f64(),
        metrics.committed.load(Ordering::Acquire),
        metrics.last_frame_index.load(Ordering::Acquire),
        metrics.presentations.load(Ordering::Acquire),
        metrics.hooks.load(Ordering::Acquire),
    );
}

fn frame_history(
    frames: &FrameLog,
    begin: Instant,
    end: Instant,
) -> (u64, Option<u32>, Option<u32>, Option<Duration>) {
    let Ok(frames) = frames.lock() else {
        return (0, None, None, None);
    };
    let mut in_range = frames
        .iter()
        .filter(|(at, _)| *at >= begin && *at <= end)
        .copied();
    let Some((_, first)) = in_range.next() else {
        return (0, None, None, None);
    };
    let mut previous = None;
    let mut last = first;
    let mut count = 1u64;
    let mut max_gap = None;
    for (at, index) in in_range {
        count += 1;
        if let Some(previous) = previous {
            let gap = at.saturating_duration_since(previous);
            max_gap = Some(max_gap.map_or(gap, |current: Duration| current.max(gap)));
        }
        previous = Some(at);
        last = index;
    }
    (count, Some(first), Some(last), max_gap)
}

fn process_cpu() -> Duration {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok.is_err() {
        return Duration::ZERO;
    }
    let to_u64 =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    Duration::from_nanos(to_u64(kernel).saturating_add(to_u64(user)) * 100)
}

fn drag_seconds() -> f64 {
    (SAMPLE_PERIOD * 2 * DRAG_STEPS).as_secs_f64()
}

fn force_foreground(hwnd: HWND) {
    unsafe {
        let foreground = GetForegroundWindow();
        let foreground_thread = GetWindowThreadProcessId(foreground, None);
        let self_thread = GetCurrentThreadId();
        let attached = foreground_thread != 0
            && foreground_thread != self_thread
            && AttachThreadInput(self_thread, foreground_thread, true).as_bool();
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(self_thread, foreground_thread, false);
        }
    }
}

fn wait_for_window() -> Option<HWND> {
    let deadline = Instant::now() + FIND_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(hwnd) = find_window() {
            return Some(hwnd);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn find_window() -> Option<HWND> {
    let title = wide(TITLE);
    unsafe { FindWindowW(None, windows::core::PCWSTR(title.as_ptr())) }.ok()
}

fn activate_window(hwnd: HWND) -> bool {
    force_foreground(hwnd);
    std::thread::sleep(Duration::from_millis(150));
    (unsafe { GetForegroundWindow() }) == hwnd
}

fn caption_grab_point(rect: (i32, i32, i32, i32), client_origin: POINT) -> (i32, i32) {
    let caption_y = rect.1 + ((client_origin.y - rect.1) / 2).max(4);
    (rect.0 + rect.2 / 2, caption_y)
}

fn client_origin(hwnd: HWND) -> Option<POINT> {
    let mut origin = POINT::default();
    unsafe { ClientToScreen(hwnd, &mut origin) }
        .as_bool()
        .then_some(origin)
}

fn move_loop_active(hwnd: HWND) -> bool {
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..GUITHREADINFO::default()
    };
    let thread = unsafe { GetWindowThreadProcessId(hwnd, None) };
    if thread == 0 {
        return false;
    }
    let ok = unsafe { GetGUIThreadInfo(thread, &mut info) }.is_ok();
    ok && info.flags.contains(GUI_INMOVESIZE)
}

fn left_button_down() -> bool {
    unsafe { GetKeyState(i32::from(VK_LBUTTON.0)) < 0 }
}

fn drag_offset(step: u32) -> i32 {
    if step >= DRAG_STEPS {
        DRAG_TRAVEL
    } else if step.is_multiple_of(2) {
        DRAG_SWAY
    } else {
        -DRAG_SWAY
    }
}
fn window_rect(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some((
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    ))
}

fn send_mouse(flags: MOUSE_EVENT_FLAGS, position: Option<(i32, i32)>) {
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
    let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    if sent == 0 {
        let error = unsafe { windows::Win32::Foundation::GetLastError() }.0;
        println!("警告：SendInput 失败（GetLastError={error}）");
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

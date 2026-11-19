//! B1 真机证据的实现：真实可见窗口（真 GPU）+ `SendInput` 按住系统标题栏拖动 3.2 s。
//!
//! 观测链是怎么来的：UI 线程在拖动期间处于系统 move 模态循环里，应用代码（`on_frame`）**只**在
//! 帧真正发生时被调用。因此本文件：
//! - `on_frame` 钩子每拍把 `Window::frame_index()` / `last_frame_presentation()` / 已提交帧计数
//!   写进 [`Metrics`]（原子量），这是「应用能观察到什么」的唯一口径；
//! - 后台拖动线程每 ~50 ms 读一次这些原子量并打印采样序列（窗口冻结时采样照常进行，序列会
//!   停在同一个数字上 —— 这正是冻结证据）；
//! - 拖动线程再用 `GetProcessTimes` 量进程 CPU、用帧时刻表算最大帧间隔、用
//!   `GetGUIThreadInfo` 的 `GUI_INMOVESIZE` 证明系统 move 模态循环确实在跑；
//! - 松手后 500 ms 再采一段，比较「拖动态势」与「松手后帧率」判定有没有堆积爆发。
//!
//! **拖动必须是真实输入**：`SetCursorPos` 把光标放到系统标题栏中点 → `SendInput(LEFTDOWN)` →
//! 连续 `SendInput(MOUSEEVENTF_MOVE)` ≥3 s → `SendInput(LEFTUP)`。程序化
//! `SendMessage(WM_NCLBUTTONDOWN)` 虽然也能进入同一个系统 move 循环，但没有真实按键状态，
//! 窗口**不会移动** —— 那样就无法证明「拖动期出帧来自模态 tick 而不是泵的 ④」。本程序因此
//! 断言 `GetWindowRect` 位移 > 100 px 且尺寸不变，并在拖动中采样 `GUI_INMOVESIZE`。
//!
//! 由 `main.rs` 在 Windows 下调用；正常 / `--edge`（拖动中关闭最后一个窗口）两种模式。

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
    math::{Rect, Size},
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

/// 证据窗口标题（`FindWindowW` 定位用）。
const TITLE: &str = "kytrix P1-B B1 modal frame evidence";
/// 请求的逻辑客户区尺寸。
const SIZE: Size = Size::new(420.0, 260.0);
/// 数据泵周期（≈60 Hz 行情 / HUD 更新）。
const DATA_PERIOD: Duration = Duration::from_millis(16);
/// 拖动步数 × 步间隔 = 3.2 s。
const DRAG_STEPS: u32 = 32;
/// 每一步的采样 / 移动周期（每步拆成两半；采样 ≈50 ms）。
const SAMPLE_PERIOD: Duration = Duration::from_millis(50);
/// 拖动中段左右往复的幅度（物理像素）：让系统 move 循环持续收到真实鼠标移动。
const DRAG_SWAY: i32 = 40;
/// 最后一步单程拉开的距离（物理像素）：拖动前后 `GetWindowRect` 的位移必须稳定超过
/// [`MIN_REAL_MOVE`]（要求 >100 px），这里留 60 px 余量。
const DRAG_TRAVEL: i32 = 160;
/// 「窗口真实移动」的位移下限（物理像素；远大于 DPI 换算抖动与非客户区边框噪声）。
const MIN_REAL_MOVE: i32 = 100;
/// 最多尝试几次「真实按下标题栏」：前台锁 / 窗口被遮挡 / 首击只做激活都可能让某次尝试
/// 起不来（窗口没移动），松手后重试。
const DRAG_ATTEMPTS: u32 = 3;
/// 松手后的观测窗口：不得出现堆积爆发。
const POST_RELEASE_WINDOW: Duration = Duration::from_millis(500);
/// 单帧最大间隔上限（持续出帧判据；60 Hz 数据泵下正常 ≤ ~35 ms）。
const MAX_FRAME_GAP: Duration = Duration::from_millis(200);
/// 拖动期间最少提交帧数（3.2 s 内 ≥10 fps 才算持续出帧；正常 60 Hz ≈190 帧）。
const MIN_DRAG_FRAMES: u64 = 32;
/// 边界模式：拖动进行到 1.2 s 时程序化关闭最后一个窗口。
const EDGE_CLOSE_AFTER: Duration = Duration::from_millis(1200);
/// 等窗口出现的上限。
const FIND_TIMEOUT: Duration = Duration::from_secs(10);
/// 整体超时（防止窗口 / 拖动线程挂死）。
const RUN_TIMEOUT: Duration = Duration::from_secs(40);

/// UI 线程每拍写、拖动线程每 ~50 ms 读的观测原子量（`Window` 的公开观测面的快照）。
#[derive(Default)]
struct Metrics {
    /// 已提交帧计数（`frame_index()` 前进一次 +1）。
    committed: AtomicU64,
    /// 最近一次观察到的 `Window::frame_index()`。
    last_frame_index: AtomicU32,
    /// 观察到 `last_frame_presentation() == Some(_)` 的提交帧数（A4 呈现反馈）。
    presentations: AtomicU64,
    /// `on_frame` 钩子被调用的次数（拖动态势下它必须持续推进）。
    hooks: AtomicU64,
    /// 应用收到的 `Event::Moved` 条数。
    moved: AtomicU64,
    /// 数据泵已回投的任务数（诊断：关窗后它是否仍在跑）。
    data_ticks: AtomicU64,
}

/// 一帧的提交时刻 + `FrameIndex`（最大帧间隔的原始数据；UI 线程写、拖动线程读）。
type FrameLog = Arc<Mutex<Vec<(Instant, u32)>>>;

/// 拖动线程的测量结果（写进 `Arc<Mutex<Option<DragResult>>>`）。
#[derive(Clone, Copy, Default)]
struct DragResult {
    /// 真的按下了鼠标并进入了拖动。
    started: bool,
    /// `SendInput(LEFTDOWN)` 之后左键确实处于按下状态（注入被系统拒绝时为 false）。
    pressed: bool,
    /// 本轮的拖动触发方式（恒为真实 `SendInput`；保留字段是为了日志自证）。
    trigger: &'static str,
    /// 边界模式：拖动中已通过 `AppHandle` 回投任务请求关闭最后一个窗口。
    edge_close_sent: bool,
    rect_before: Option<(i32, i32, i32, i32)>,
    rect_after: Option<(i32, i32, i32, i32)>,
    /// 按下 → 松手的墙钟时长。
    duration: Duration,
    /// 拖动窗口内的提交帧数（按 `frame_index` 前进计）。
    drag_frames: u64,
    /// 松手后 500 ms 内的提交帧数。
    post_frames: u64,
    frames_in_drag: u64,
    first_index: Option<u32>,
    last_index: Option<u32>,
    /// 拖动窗口内相邻两帧的最大间隔（样本 <2 时为 `None` = 冻结）。
    max_gap: Option<Duration>,
    /// 拖动期间 `GUI_INMOVESIZE`（系统 move 模态循环）为真的采样数。
    move_loop_samples: u32,
    /// 拖动期间的总采样数。
    sample_total: u32,
    /// 进程 user+kernel 时间增量与量测墙钟（CPU 百分比）。
    cpu: Duration,
    cpu_wall: Duration,
}

impl DragResult {
    /// 拖动线程没能开始拖动（窗口没出现 / 被遮挡 / SendInput 失败）时的结果。
    fn failed() -> Self {
        Self::default()
    }

    /// 拖动前后的外框变化 `(dx, dy, dw, dh)`。
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

/// 证据收集状态（只在 UI 线程访问；拖动线程只碰里面的 `Arc` 字段）。
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
    // 真实内容：一个按 60 Hz 数据泵改透明度（HUD / 行情推进）的白色面板。
    let source = app.signal(0u8);
    let root = app.window(id).expect("窗口存在").tree().root();
    let _node = NodeBuilder::boxed()
        .layout_override(Rect::new(0.0, 0.0, SIZE.width, SIZE.height))
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
    // 拖动线程句柄：edge 模式里窗口可能先于拖动线程结束（`Application::run` 先返回），
    // 必须等线程收尾才能读到完整结果并确认没有 panic。
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

    // 数据泵：60 Hz 回投任务（写信号 → 标脏 → 请求一帧），模拟行情 / HUD 更新。
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
    // 先回收拖动线程（`Application::run` 可能在拖动线程收尾前返回），把 panic 直接暴露出来。
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
        // 边界模式：窗口由拖动线程在拖动中关闭，事件循环必须正常退出、窗口必须已销毁。
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

/// 每拍：记录已提交帧 / 钩子 / `Event::Moved`；首拍拉起拖动线程；拿不到结果时超时失败。
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

/// 拖动结束：打印拖动期间 / 松手后的帧数、最大帧间隔、CPU、`Event::Moved`，判定并收尾。
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
    // 「窗口真实移动」= 位移 > MIN_REAL_MOVE 且**尺寸不变**（系统 move 循环只改位置；尺寸
    // 变化说明触发的其实是 resize 循环或程序化改动，不能作为 B1 的模态 move 证据）。
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

/// 拉起后台拖动线程（UI 线程随后进入系统 move 模态循环，不能再跑钩子以外的采样）。
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

/// 拖动线程体：等窗口 → 置顶 / 尽力带到前台 → 真实 `SendInput` 按住系统标题栏拖动 3.2 s →
/// 松手 → 观测 500 ms → 写结果。
///
/// **为什么必须是真实输入**：程序化 `SendMessage(WM_NCLBUTTONDOWN)` 也会进入同一个系统 move
/// 模态循环，但没有真实按键状态，窗口**不会随鼠标移动**（旧实现实测位移 (0,0)）—— 于是无法
/// 证明「拖动期间泵被系统循环占住、出帧完全依赖模态 tick」。这里走与用户完全相同的输入通路
/// （`SetCursorPos` + `SendInput` 按住 / 连续移动 / 松手），并断言 `GetWindowRect` 位移
/// > [`MIN_REAL_MOVE`]、尺寸不变；拖动中再用 `GUI_INMOVESIZE` 采样证明系统 move 循环确实在跑。
///
/// 前台**不是必要条件**（真实点击标题栏会激活窗口并由系统进入 move 循环），前台锁只影响
/// 日志；单次尝试若「按下后窗口没动」（首击只做激活 / 落点被遮挡），松手后重试。
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
    // 置顶 + 显示：真实点击必须落在本窗口标题栏上（其它窗口可能盖住它）。
    // SAFETY: 本进程窗口；只改 Z 序 / 可见性，不改位置与尺寸。
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
    // 尽力把窗口带到前台；失败不影响有效性（真实点击自己会激活它）。
    let foreground = activate_window(hwnd);
    // SAFETY: 只读查询当前前台窗口与窗口可见 / 最小化状态。
    println!(
        "前台={foreground}（当前前台窗口={:?}）；本窗口 visible={} iconic={}",
        unsafe { GetForegroundWindow() },
        unsafe { IsWindowVisible(hwnd) }.as_bool(),
        unsafe { IsIconic(hwnd) }.as_bool()
    );
    // SAFETY: 只读查询；本 example 的窗口不跨屏，DPI 在拖动期间稳定。
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
        // 光标先移到标题栏按下点：随后的按钮事件发生在光标当前位置（不先移动就会按到别的
        // 窗口上，那次拖动会被那个窗口的模态循环吃掉，本 example 会误判成「窗口没移动」）。
        // SAFETY: 设置本会话光标位置；窗口存活。
        let cursor_moved = unsafe { SetCursorPos(grab.0, grab.1) }.is_ok();
        std::thread::sleep(Duration::from_millis(150));
        // SAFETY: 只读查询指定屏幕点上的窗口。
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
            // SAFETY: 本进程窗口；重新置顶。
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
        // 真实按下：与用户按住系统标题栏完全同一条输入通路。
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
                    // 系统 move 循环没启动（按下后窗口始终没动）：中止本次尝试。
                    break;
                }
            }
        }
        // 末步已把光标拉到 `DRAG_TRAVEL`：等系统把最后一段位移应用到窗口，再松手。
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
            // SAFETY: 本进程窗口；重新置顶。
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
        // 松手后再观测 500 ms（同样每 ~50 ms 采样一次帧计数）。
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
        // SAFETY: 本进程窗口；恢复为非置顶。
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

/// 打印一条采样序列（拖动线程；窗口冻结时数字停在原处，正是证据）。
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

/// 从帧时刻表取 `[begin, end]` 区间：帧数、首末 `FrameIndex`、相邻最大间隔。
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

/// 进程 user + kernel 的 CPU 时间（`GetProcessTimes`；失败返回 0）。
fn process_cpu() -> Duration {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: 本进程伪句柄；四个输出指针都指向本函数栈上的值。
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
    // FILETIME 单位 = 100 ns。
    Duration::from_nanos(to_u64(kernel).saturating_add(to_u64(user)) * 100)
}

/// 拖动 3.2 s 的墙钟时长（只用于打印）。
fn drag_seconds() -> f64 {
    (SAMPLE_PERIOD * 2 * DRAG_STEPS).as_secs_f64()
}

/// 强制把 `hwnd` 变成前台窗口（尽力而为）。
///
/// 后台线程直接 `SetForegroundWindow` 会被系统前台锁拒绝（返回 FALSE、不报错）；标准做法是
/// 先 `AttachThreadInput` 到当前前台线程，再调用，最后解除。拿不到前台不影响本证据：真实
/// `SendInput` 点击标题栏会走系统的激活路径，拖动是否真的开始由 [`drag`] 的窗口位移检测判定。
fn force_foreground(hwnd: HWND) {
    // SAFETY: 本进程窗口；线程 id 查询与 AttachThreadInput 都是窗口管理器协作调用。
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
    // SAFETY: 标题是栈上 NUL 结尾宽字符串；`FindWindowW` 只读查询。
    unsafe { FindWindowW(None, windows::core::PCWSTR(title.as_ptr())) }.ok()
}

/// 把证据窗口尽力带到前台（返回是否成功）。
///
/// 拖动本身**不依赖**前台：真实点击标题栏会激活窗口并由系统进入 move 循环（前台锁只决定
/// 「谁在最前」）。这里做一次 `AttachThreadInput` + `SetForegroundWindow` 的尽力尝试，目的只是
/// 减少「首击只做激活、不进循环」的尝试次数；失败不影响证据有效性。
fn activate_window(hwnd: HWND) -> bool {
    force_foreground(hwnd);
    std::thread::sleep(Duration::from_millis(150));
    // SAFETY: 只读查询当前前台窗口。
    (unsafe { GetForegroundWindow() }) == hwnd
}

/// 系统标题栏中点的屏幕物理坐标。
///
/// 系统标题栏 = 窗口外框顶边到客户区原点之间（`WS_OVERLAPPEDWINDOW` 的上边框 + 标题栏）；
/// 取其中点保证命中 `HTCAPTION` 而不是缩放的粗边框。`rect` 的后两位是**宽高**，横坐标取
/// 外框中心。
fn caption_grab_point(rect: (i32, i32, i32, i32), client_origin: POINT) -> (i32, i32) {
    let caption_y = rect.1 + ((client_origin.y - rect.1) / 2).max(4);
    (rect.0 + rect.2 / 2, caption_y)
}

/// 客户区原点（屏幕物理坐标）。
fn client_origin(hwnd: HWND) -> Option<POINT> {
    let mut origin = POINT::default();
    // SAFETY: 窗口存活；`ClientToScreen` 只做坐标换算，`origin` 是栈上输出。
    unsafe { ClientToScreen(hwnd, &mut origin) }
        .as_bool()
        .then_some(origin)
}

/// 系统 move / size 模态循环是否在窗口所属 UI 线程上运行（`GUITHREADINFO::GUI_INMOVESIZE`）。
///
/// 这是「拖动期间泵的 ④ 确实被系统循环占住」的直接证据：只看帧数无法排除「泵自己在出帧」。
/// 查询的是 UI 线程的 GUI 状态，从拖动线程调用同样有效。
fn move_loop_active(hwnd: HWND) -> bool {
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..GUITHREADINFO::default()
    };
    // SAFETY: 只读查询；窗口句柄可能已失效（edge 模式），此时返回 0 而不是解引用。
    let thread = unsafe { GetWindowThreadProcessId(hwnd, None) };
    if thread == 0 {
        return false;
    }
    // SAFETY: `thread` 来自窗口句柄；`GetGUIThreadInfo` 只读查询该线程的 GUI 状态。
    let ok = unsafe { GetGUIThreadInfo(thread, &mut info) }.is_ok();
    ok && info.flags.contains(GUI_INMOVESIZE)
}

/// `SendInput(LEFTDOWN)` 之后左键是否真的处于按下状态（注入被 UIPI / 前台策略拒绝时不会）。
fn left_button_down() -> bool {
    // SAFETY: `GetKeyState` 是无副作用的线程局部查询（高位置位 = 按下）。
    unsafe { GetKeyState(i32::from(VK_LBUTTON.0)) < 0 }
}

/// 拖动中每一步的光标目标偏移（相对按下点，物理像素）。
///
/// 中段左右往复（[`DRAG_SWAY`]）让系统 move 循环持续收到真实鼠标移动；最后一步单程拉出
/// [`DRAG_TRAVEL`]，使拖动前后 `GetWindowRect` 的位移稳定超过 [`MIN_REAL_MOVE`]。
fn drag_offset(step: u32) -> i32 {
    if step >= DRAG_STEPS {
        DRAG_TRAVEL
    } else if step.is_multiple_of(2) {
        DRAG_SWAY
    } else {
        -DRAG_SWAY
    }
}
/// 窗口外框 `(left, top, width, height)`；`None` = 查询失败（edge 模式下窗口在拖动中已被
/// 真正销毁，之后不能再 `expect` 一个已失效的句柄）。
fn window_rect(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    let mut rect = RECT::default();
    // SAFETY: 窗口存活期内查询；`rect` 是栈上输出。句柄失效时返回错误（不 UB）。
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some((
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    ))
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

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

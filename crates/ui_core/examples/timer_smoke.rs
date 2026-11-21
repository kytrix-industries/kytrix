//! E10 **一次性定时器实机冒烟**：泵在空闲阻塞时，定时器到点必须自己唤醒一次。
//!
//! ```text
//! cargo run -p ui_core --example timer_smoke
//! ```
//!
//! 冒烟验证的事实（对应 P2-4b R02 的「500 ms 延迟永远等不到」根因 + E10c 的 macOS 真机失败）：
//! 1. **登记基准**：定时器从**回投任务**（`AppHandle::send` → 泵尾）里登记，登记前泵已经做完
//!    本轮的窗口/状态/出帧工作。基准 `Instant` 紧邻 `App::after` 调用入口取（应用能观测到的
//!    最早登记时刻）；到点必须在 `基准 + delay` **之后**（允许相等）。旧实现的 deadline 用
//!    「泵起点同步的引擎时钟」，会把泵前置工作算进 delay ⇒ macOS 真机实测
//!    `398.648 ms < 400 ms`（提前触发）。
//! 2. 第一只定时器等待期间**没有输入、没有 `AppHandle::wake` 节拍**，只靠平台定时器唤醒；
//! 3. 等待期间的**空闲应用轮**很少：这里统计「`on_frame` 被调用但 `FrameIndex` 没有推进」的
//!    轮数（真正的按刷新率空转），而不是总轮数 —— 首帧收尾的合法补帧不计入（它会推进
//!    `FrameIndex`）。E10c 前 macOS 实测 54 个空闲轮 / 400 ms；修复后与 Windows 同口径 ≤1。
//! 4. 回调里再注册的 200 ms 定时器同样触发（链式一次性定时器）；
//! 5. 关窗后事件循环干净退出（退出码 0）。
//!
//! 退出码：`0` = PASS；`2` = 定时器从未触发（看门狗关窗）；`3` = 自检失败。

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use smol_str::SmolStr;
use ui_core::{
    app::{App, Application},
    math::Size,
    paint::color::Color,
    window::WindowOptions,
};

/// 第一次等待时长（毫秒）。
const FIRST_DELAY_MS: u64 = 400;
/// 第二次等待时长（毫秒）。
const SECOND_DELAY_MS: u64 = 200;
/// 看门狗（秒）：超时即关窗并报告定时器未触发。
const WATCHDOG: Duration = Duration::from_secs(20);
/// 登记那一轮的「应用前置工作」模拟时长。
///
/// E10c M1 的确定性复现：泵在**轮首**同步引擎时钟，应用在 on_frame 里做完工作后才登记定时器。
/// 旧实现用泵起点的引擎时钟当 deadline 基准 ⇒ 这段工作被算进 delay，定时器必然早到。留出 20 ms
/// 让「提前量」远大于平台定时器的唤醒抖动（macOS CFRunLoopTimer 通常 <10 ms、Windows
/// WM_TIMER ~15.6 ms），M1 的 ired_at < 基准 + delay 断言才能稳定变红；修复后（登记瞬间
/// 取基准则）不受影响。
const REGISTRATION_ROUND_WORK: Duration = Duration::from_millis(20);
/// 第一次等待期允许的**空闲应用轮**上限（安全余量；实测值见输出）。
///
/// 空闲轮 = `on_frame` 被调用但 `FrameIndex` 没有推进的轮次：合法补帧（图集 / a11y / 首帧
/// 收尾 / Wayland 合成器回调）会推进 `FrameIndex`，不计入；真正按刷新率空转才会（E10c 前
/// macOS 实测 54 个空闲轮 / 400 ms，Windows 0）。macOS 修复后实测 1，Windows 0；Linux 的
/// Wayland 事件循环在首帧后有既有的启动补轮（基线 b3e98dd 旧冒烟实测 6 轮），登记为本批
/// 之外的残余、不作为阈值依据。8 = 旧冒烟的门槛，远低于「按刷新率空转」（54）。
const MAX_FIRST_IDLE_ROUNDS: u32 = 8;
/// 链式第二次等待期允许的空闲应用轮上限。
///
/// 比第一次多 1：第一次回调本身占一轮（回调在 `on_frame` 之前派发，该轮 `FrameIndex` 不变），
/// 而 Windows 的 `WM_TIMER` 粒度又可能提前唤醒一次（`run_due` 不派发、下一轮重新挂表）。
const MAX_SECOND_IDLE_ROUNDS: u32 = 2;

#[derive(Default)]
struct Report {
    /// `on_frame` 调用次数（泵轮数，含首帧收尾的合法帧）。
    rounds: u32,
    /// 「`on_frame` 被调用但 `FrameIndex` 没有推进」的累计轮数（按刷新率空转的判据）。
    idle_rounds: u32,
    /// 上一次 `on_frame` 看到的 `FrameIndex`。
    last_frame_index: Option<u32>,
    /// 登记第一只定时器时：基准时刻、累计 `on_frame` 轮数、累计空闲轮数、`FrameIndex`。
    registered: Option<(Instant, u32, u32, u32)>,
    /// 第一只定时器回调：触发时刻、累计轮数、累计空闲轮数、`FrameIndex`。
    first_fired: Option<(Instant, u32, u32, u32)>,
    second_fired: Option<(Instant, u32, u32, u32)>,
    /// 第一只定时器是从回投任务里登记的（证明测的是「泵尾登记」）。
    first_task_ran: bool,
    failures: Vec<String>,
}

fn failure(report: &Arc<Mutex<Report>>, message: String) {
    report.lock().expect("report lock").failures.push(message);
}

fn rounds(report: &Arc<Mutex<Report>>) -> u32 {
    report.lock().expect("report lock").rounds
}

fn idle_rounds(report: &Arc<Mutex<Report>>) -> u32 {
    report.lock().expect("report lock").idle_rounds
}

/// 一次 `on_frame`（泵轮）的记账：总轮数 +「本帧有没有推进 FrameIndex」。
fn note_round(report: &Arc<Mutex<Report>>, frame_index: u32) {
    let mut report = report.lock().expect("report lock");
    report.rounds = report.rounds.saturating_add(1);
    if report.last_frame_index == Some(frame_index) {
        report.idle_rounds = report.idle_rounds.saturating_add(1);
    }
    report.last_frame_index = Some(frame_index);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut app = App::new(runtime.handle().clone());
    let window = app.create_window(WindowOptions {
        title: SmolStr::new_static("Kytrix · E10 timer smoke"),
        size: Size::new(480.0, 320.0),
        background: Color::from_rgba8([0x14, 0x18, 0x1F, 0xFF]),
        ..WindowOptions::default()
    });

    // 看门狗：定时器永不触发时不能把泵留在平台等待上（冒烟必须自己结束）。
    let watchdog = app.handle();
    watchdog.clone().spawn(async move {
        tokio::time::sleep(WATCHDOG).await;
        watchdog.send(move |app| {
            app.close_window(window);
            app.handle().wake();
        });
    });

    let report = Arc::new(Mutex::new(Report::default()));
    let state = Arc::clone(&report);
    let mut scheduled = false;
    app.on_frame(move |app| {
        let frame_index = app
            .window(window)
            .map(|w| w.frame_index().get())
            .unwrap_or(0);
        note_round(&state, frame_index);
        if scheduled {
            return;
        }
        scheduled = true;
        // 模拟应用在本轮钩子里的前置工作：泵的引擎时钟在轮首同步，这段时间正是旧实现把它
        // 算进 delay 的来源（M1 的确定性复现窗口）。
        std::thread::sleep(REGISTRATION_ROUND_WORK);
        // 通过回投任务登记：任务在泵尾（帧之后）执行，此时引擎时钟仍是本轮**起点**同步的值 ——
        // 真正的「登记瞬间」只能由应用自己取。这既是生产里最常见的异步登记路径，也让
        // 「拿泵起点时钟当基准会把本轮前置工作算进 delay」变成可复现的失败（E10c M1）。
        let task_state = Arc::clone(&state);
        app.handle().send(move |app| {
            task_state.lock().expect("report lock").first_task_ran = true;
            let registered_at = Instant::now();
            let index = app
                .window(window)
                .map(|w| w.frame_index().get())
                .unwrap_or(0);
            let tied = Arc::clone(&task_state);
            let chained = Arc::clone(&task_state);
            let result = app.after(window, Duration::from_millis(FIRST_DELAY_MS), move |app| {
                let fired_at = Instant::now();
                let rounds_at_fire = rounds(&tied);
                let idle_at_fire = idle_rounds(&tied);
                let index = app
                    .window(window)
                    .map(|w| w.frame_index().get())
                    .unwrap_or(0);
                {
                    let mut report = tied.lock().expect("report lock");
                    report.first_fired = Some((fired_at, rounds_at_fire, idle_at_fire, index));
                    if let Some((registered_at, _, _, _)) = report.registered
                        && fired_at.saturating_duration_since(registered_at)
                            < Duration::from_millis(FIRST_DELAY_MS)
                    {
                        report.failures.push(format!(
                            "first timer fired early: {:?} < {FIRST_DELAY_MS} ms",
                            fired_at.saturating_duration_since(registered_at)
                        ));
                    }
                }
                // 链式一次性定时器：第二次等待同样必须自己唤醒。
                let second_at = Instant::now();
                let second_state = Arc::clone(&chained);
                let error_state = Arc::clone(&chained);
                if let Err(error) =
                    app.after(window, Duration::from_millis(SECOND_DELAY_MS), move |app| {
                        let fired_at = Instant::now();
                        let rounds_at_fire = rounds(&second_state);
                        let idle_at_fire = idle_rounds(&second_state);
                        let index = app
                            .window(window)
                            .map(|w| w.frame_index().get())
                            .unwrap_or(0);
                        {
                            let mut report = second_state.lock().expect("report lock");
                            report.second_fired =
                                Some((fired_at, rounds_at_fire, idle_at_fire, index));
                            if fired_at.saturating_duration_since(second_at)
                                < Duration::from_millis(SECOND_DELAY_MS)
                            {
                                report.failures.push(format!(
                                    "second timer fired early: {:?} < {SECOND_DELAY_MS} ms",
                                    fired_at.saturating_duration_since(second_at)
                                ));
                            }
                        }
                        app.close_window(window);
                        app.handle().wake();
                    })
                {
                    failure(
                        &error_state,
                        format!("second timer registration failed: {error}"),
                    );
                    app.close_window(window);
                    app.handle().wake();
                }
            });
            match result {
                Ok(_) => {
                    let rounds_at_registration = rounds(&task_state);
                    let idle_at_registration = idle_rounds(&task_state);
                    if let Ok(mut report) = task_state.lock() {
                        report.registered = Some((
                            registered_at,
                            rounds_at_registration,
                            idle_at_registration,
                            index,
                        ));
                    }
                    println!(
                        "smoke: first timer registered (delay {FIRST_DELAY_MS} ms, round \
                         {rounds_at_registration}, idle {idle_at_registration}, frame_index \
                         {index}, from post-task)"
                    );
                }
                Err(error) => {
                    failure(
                        &task_state,
                        format!("first timer registration failed: {error}"),
                    );
                    app.close_window(window);
                    app.handle().wake();
                }
            }
        });
    });

    println!("smoke: entering event loop (watchdog {WATCHDOG:?})");
    Application::new().run(app)?;

    let report = report.lock().expect("report lock");
    println!(
        "smoke: loop exited after {} rounds / {} idle rounds (task={}, failures={:?})",
        report.rounds, report.idle_rounds, report.first_task_ran, report.failures
    );
    let Some((registered_at, _, registered_idle, _)) = report.registered else {
        println!("smoke: FAIL the first timer was never registered");
        std::process::exit(3);
    };
    let Some((first_at, _, first_idle, first_index)) = report.first_fired else {
        println!("smoke: FAIL first timer never fired (watchdog closed the window)");
        std::process::exit(2);
    };
    let Some((second_at, _, second_idle, second_index)) = report.second_fired else {
        println!("smoke: FAIL chained timer never fired");
        std::process::exit(2);
    };

    let mut failed = !report.failures.is_empty();
    let first_wait = first_at.saturating_duration_since(registered_at);
    let second_wait = second_at.saturating_duration_since(first_at);
    let first_idle_delta = first_idle.saturating_sub(registered_idle);
    let second_idle_delta = second_idle.saturating_sub(first_idle);
    println!(
        "smoke: first fire +{first_wait:?} ({first_idle_delta} idle rounds after registration, \
         frame_index {first_index}), second fire +{second_wait:?} ({second_idle_delta} idle \
         rounds after the first, frame_index {second_index})"
    );
    if first_wait < Duration::from_millis(FIRST_DELAY_MS) {
        println!("smoke: FAIL first timer fired early: {first_wait:?} < {FIRST_DELAY_MS} ms");
        failed = true;
    }
    if first_idle_delta > MAX_FIRST_IDLE_ROUNDS {
        println!(
            "smoke: FAIL the pump ran {first_idle_delta} idle rounds during the wait (periodic \
             wake?)"
        );
        failed = true;
    }
    if second_idle_delta > MAX_SECOND_IDLE_ROUNDS {
        println!("smoke: FAIL the pump ran {second_idle_delta} idle rounds during the second wait");
        failed = true;
    }
    if failed {
        println!("smoke: FAIL {:?}", report.failures);
        std::process::exit(3);
    }
    println!("smoke: PASS both one-shot timers woke the idle pump (no input, no periodic wake)");
    Ok(())
}

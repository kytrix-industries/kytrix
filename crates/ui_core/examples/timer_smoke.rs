use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use smol_str::SmolStr;
use ui_core::{
    app::{App, Application},
    math::{LogicalSize, px},
    paint::color::Color,
    window::WindowOptions,
};

const FIRST_DELAY_MS: u64 = 400;
const SECOND_DELAY_MS: u64 = 200;
const WATCHDOG: Duration = Duration::from_secs(20);
const REGISTRATION_ROUND_WORK: Duration = Duration::from_millis(20);
const MAX_FIRST_IDLE_ROUNDS: u32 = if cfg!(target_os = "linux") { 8 } else { 1 };
const MAX_SECOND_IDLE_ROUNDS: u32 = 2;

#[derive(Default)]
struct Report {
    rounds: u32,
    idle_rounds: u32,
    last_frame_index: Option<u32>,
    registered: Option<(Instant, u32, u32, u32)>,
    first_fired: Option<(Instant, u32, u32, u32)>,
    second_fired: Option<(Instant, u32, u32, u32)>,
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
        size: LogicalSize::new(px(480.0), px(320.0)),
        background: Color::from_rgba8([0x14, 0x18, 0x1F, 0xFF]),
        ..WindowOptions::default()
    });

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
        std::thread::sleep(REGISTRATION_ROUND_WORK);
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
        "smoke: loop exited after {} app rounds / {} idle app rounds (task={}, failures={:?})",
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
        "smoke: first fire +{first_wait:?} ({first_idle_delta} idle app rounds after \
         registration, frame_index {first_index}), second fire +{second_wait:?} \
         ({second_idle_delta} idle app rounds after the first, frame_index {second_index})"
    );
    if first_wait < Duration::from_millis(FIRST_DELAY_MS) {
        println!("smoke: FAIL first timer fired early: {first_wait:?} < {FIRST_DELAY_MS} ms");
        failed = true;
    }
    if first_idle_delta > MAX_FIRST_IDLE_ROUNDS {
        println!(
            "smoke: FAIL the application ran {first_idle_delta} idle app rounds during the wait \
             (application-level periodic wake?; OS display-link vsync wake is a separate \
             registered residual, not this metric)"
        );
        failed = true;
    }
    if second_idle_delta > MAX_SECOND_IDLE_ROUNDS {
        println!(
            "smoke: FAIL the application ran {second_idle_delta} idle app rounds during the \
             second wait"
        );
        failed = true;
    }
    if failed {
        println!("smoke: FAIL {:?}", report.failures);
        std::process::exit(3);
    }
    println!(
        "smoke: PASS both one-shot timers woke the idle pump (no input; idle app rounds ≤1 on \
         macOS/Windows — Linux keeps its pre-existing startup rounds ≤8; OS display-link vsync \
         wake ≤250 ms is a registered residual)"
    );
    Ok(())
}

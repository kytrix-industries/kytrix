//! E10 **一次性定时器实机冒烟**：泵在空闲阻塞（`GetMessageW`）时，定时器到点必须自己唤醒一次。
//!
//! ```text
//! cargo run -p ui_core --example timer_smoke
//! ```
//!
//! 冒烟验证的事实（对应 P2-4b R02 的「500 ms 延迟永远等不到」根因）：
//! 1. 第一轮泵里注册 `App::after(400 ms)`；此后**没有输入、没有 `AppHandle::wake` 节拍**；
//! 2. 定时器在 ≥ 400 ms 时由平台泵的 `SetTimer` → `WM_TIMER` 唤醒并派发（不是等下一次无关
//!    消息）；
//! 3. 等待期间的泵轮数很小（日志打印；没有按刷新率空转 / 没有连续帧）；
//! 4. 回调里再注册的 200 ms 定时器同样触发（链式一次性定时器）；
//! 5. 关窗后事件循环干净退出（退出码 0）。
//!
//! 退出码：`0` = PASS；`2` = 定时器从未触发（看门狗关窗）；`3` = 自检失败。

use std::{
    cell::RefCell,
    rc::Rc,
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

#[derive(Default)]
struct Report {
    /// 泵轮数（`on_frame` 调用次数）。
    rounds: u32,
    /// 注册第一只定时器时的 `(缸钟, FrameIndex, 泵轮数)`。
    registered: Option<(Instant, u32, u32)>,
    first_fired: Option<(Instant, u32, u32)>,
    second_fired: Option<(Instant, u32, u32)>,
    failures: Vec<String>,
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

    // 看门狗：定时器永不触发时不能把泵留在 `GetMessageW` 上（冒烟必须自己结束）。
    let watchdog = app.handle();
    watchdog.clone().spawn(async move {
        tokio::time::sleep(WATCHDOG).await;
        watchdog.send(move |app| {
            app.close_window(window);
            app.handle().wake();
        });
    });

    let report = Rc::new(RefCell::new(Report::default()));
    let state = Rc::clone(&report);
    let mut first_round_seen = false;
    app.on_frame(move |app| {
        let now = Instant::now();
        let (rounds, index) = {
            let mut report = state.borrow_mut();
            report.rounds = report.rounds.saturating_add(1);
            (
                report.rounds,
                app.window(window)
                    .map(|w| w.frame_index().get())
                    .unwrap_or(0),
            )
        };
        // 第一轮泵：登记第一只定时器（此时引擎时钟已 sync，等待期是真实的空闲阻塞）。
        if !first_round_seen {
            first_round_seen = true;
            state.borrow_mut().registered = Some((now, index, rounds));
            let callback_state = Rc::clone(&state);
            match app.after(window, Duration::from_millis(FIRST_DELAY_MS), move |app| {
                let fired_at = Instant::now();
                let index = app
                    .window(window)
                    .map(|w| w.frame_index().get())
                    .unwrap_or(0);
                let rounds = callback_state.borrow().rounds;
                {
                    let mut report = callback_state.borrow_mut();
                    report.first_fired = Some((fired_at, index, rounds));
                    if let Some((registered_at, _, _)) = report.registered
                        && fired_at.duration_since(registered_at)
                            < Duration::from_millis(FIRST_DELAY_MS)
                    {
                        report.failures.push(format!(
                            "first timer fired early: {:?} < {FIRST_DELAY_MS} ms",
                            fired_at.duration_since(registered_at)
                        ));
                    }
                }
                // 链式一次性定时器：第二次等待同样必须自己唤醒。
                let second_state = Rc::clone(&callback_state);
                let error_state = Rc::clone(&callback_state);
                if let Err(error) =
                    app.after(window, Duration::from_millis(SECOND_DELAY_MS), move |app| {
                        let fired_at = Instant::now();
                        let index = app
                            .window(window)
                            .map(|w| w.frame_index().get())
                            .unwrap_or(0);
                        let rounds = second_state.borrow().rounds;
                        second_state.borrow_mut().second_fired = Some((fired_at, index, rounds));
                        app.close_window(window);
                        app.handle().wake();
                    })
                {
                    error_state
                        .borrow_mut()
                        .failures
                        .push(format!("second timer registration failed: {error}"));
                    app.close_window(window);
                    app.handle().wake();
                }
            }) {
                Ok(_) => println!(
                    "smoke: first timer registered (delay {FIRST_DELAY_MS} ms, round {rounds}, \
                     frame_index {index})"
                ),
                Err(error) => {
                    state
                        .borrow_mut()
                        .failures
                        .push(format!("timer registration failed: {error}"));
                    app.close_window(window);
                    app.handle().wake();
                }
            }
        }
    });

    println!("smoke: entering event loop (watchdog {WATCHDOG:?})");
    Application::new().run(app)?;

    let report = report.borrow();
    println!(
        "smoke: loop exited after {} rounds (failures={:?})",
        report.rounds, report.failures
    );
    let Some((registered_at, _, registered_rounds)) = report.registered else {
        println!("smoke: FAIL the first pump round never ran");
        std::process::exit(3);
    };
    let Some((first_at, _, first_rounds)) = report.first_fired else {
        println!("smoke: FAIL first timer never fired (watchdog closed the window)");
        std::process::exit(2);
    };
    let Some((second_at, _, second_rounds)) = report.second_fired else {
        println!("smoke: FAIL chained timer never fired");
        std::process::exit(2);
    };

    let mut failed = !report.failures.is_empty();
    let first_wait = first_at.duration_since(registered_at);
    let second_wait = second_at.duration_since(first_at);
    let first_rounds_delta = first_rounds.saturating_sub(registered_rounds);
    let second_rounds_delta = second_rounds.saturating_sub(first_rounds);
    println!(
        "smoke: first fire +{first_wait:?} ({first_rounds_delta} rounds after registration), \
         second fire +{second_wait:?} ({second_rounds_delta} rounds after the first)"
    );
    // 等待期泵轮数：注册轮 → 触发轮之间应当接近 1（只有触发那一次唤醒）。首帧的收尾唤醒
    // （图集补发等）可能多一两轮，因此门槛放在 8：远超此值就是按刷新率空转。
    if first_rounds_delta > 8 {
        println!(
            "smoke: FAIL the pump ran {first_rounds_delta} rounds during the wait (periodic wake?)"
        );
        failed = true;
    }
    if second_rounds_delta > 8 {
        println!("smoke: FAIL the pump ran {second_rounds_delta} rounds during the second wait");
        failed = true;
    }
    if failed {
        println!("smoke: FAIL {:?}", report.failures);
        std::process::exit(3);
    }
    println!("smoke: PASS both one-shot timers woke the idle pump (no input, no periodic wake)");
    Ok(())
}

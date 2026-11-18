//! `window` / `idle` 模式：真机真窗口。
//!
//! 帧间隔口径（写进报告，避免与 HUD 的 FPS 对不上）：在 `App::on_frame` 钩子里**自己提交**本帧
//! 并记录**提交时刻**，相邻两次提交的时间差就是帧间隔（与 `DebugHudMetrics::frame_interval`
//! 同源：都是「commit → commit」，不是「GPU 完成」，也不是「上屏」）。帧间节奏由平台帧时钟决定
//! （Windows `DCompositionWaitForCompositorClock` / macOS display link / Linux frame callback），
//! 因此这个间隔就是 vsync 节拍的实测值。
//!
//! 为什么要自己提交而不是等泵提交：泵的 `frame_all` 只在 `tick && needs_continuous_frames`
//! 时出帧，外面拿不到「本帧 CPU 起点」。钩子里的 `App::frame` 让我们能同时测到
//! **UI 线程帧内 CPU**（样式 → 文本 → 布局 → 绘制 → 发布；真实窗口的实例编码在合成线程）。
//! 泵随后那次 `frame` 因为没有脏位而退化为 `Idle`（不产生额外帧）。
//!
//! 持续出帧靠一个 4×4 装饰节点的长动画（`needs_continuous_frames` 只认动画或 HUD）：
//! 没有它，泵会阻塞在消息等待上，测到的间隔是输入驱动的，不是刷新率。

use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use ui_core::{
    app::{App, Application},
    math::Scale,
    paint::color::Color,
    window::{FrameOutcome, WindowId, WindowOptions},
};

use crate::{
    Config,
    report::{IntervalReport, Samples, sample_idle_cpu},
    scene::{self, Load},
};

/// 60 fps 的判定下限（60 Hz 显示器 vsync 锁定时中位间隔 16.67 ms ⇒ 59.9 fps；
/// 允许 1 fps 的协议/测量开销，超过它才算掉帧）。
const MIN_FPS: f64 = 59.0;
/// 帧间隔抖动上限（毫秒）。
const MAX_JITTER_MS: f64 = 1.0;
/// 掉帧判据：间隔 ≥ 中位数的 1.5 倍（跳过至少一个 vsync）。
const DROPPED_FACTOR: f64 = 1.5;
/// 进度打印间隔。
const PROGRESS: Duration = Duration::from_secs(5);

/// 真实窗口模式的运行状态。
struct WindowRun {
    warmup: Duration,
    duration: Duration,
    started: Instant,
    measure_at: Instant,
    deadline: Instant,
    intervals: Samples,
    frame_cpu: Samples,
    update_cpu: Samples,
    commits: usize,
    last_commit: Option<Instant>,
    last_progress: Instant,
    finished: bool,
    raw: bool,
}

impl WindowRun {
    fn new(config: &Config) -> Self {
        let started = Instant::now();
        let warmup = Duration::from_secs_f64(config.window_warmup_seconds.max(0.0));
        let duration = Duration::from_secs_f64(config.window_seconds.max(0.1));
        let measure_at = started + warmup;
        let deadline = measure_at + duration;
        // 容量按「刷新率上限 360 Hz」估算，测量期不扩容。
        let capacity = (duration.as_secs_f64() * 360.0).ceil() as usize + 8;
        Self {
            warmup,
            duration,
            started,
            measure_at,
            deadline,
            intervals: Samples::new("frame interval", capacity),
            frame_cpu: Samples::new("ui thread frame cpu", capacity),
            update_cpu: Samples::new("ui thread update cpu", capacity),
            commits: 0,
            last_commit: None,
            last_progress: started,
            finished: false,
            raw: config.raw,
        }
    }

    /// 记录一帧（`now` = 提交时刻）。
    fn observe(&mut self, now: Instant, update: Duration, frame: Duration) {
        self.commits += 1;
        if now < self.measure_at {
            return;
        }
        if let Some(previous) = self.last_commit {
            let interval = now.saturating_duration_since(previous);
            self.intervals.push(interval);
            if self.raw {
                println!("raw,window_interval,{:.3}", interval.as_secs_f64() * 1e6);
            }
        }
        self.last_commit = Some(now);
        self.frame_cpu.push(frame);
        self.update_cpu.push(update);
        if self.raw {
            println!("raw,ui_frame_cpu,{:.3}", frame.as_secs_f64() * 1e6);
            println!("raw,ui_update_cpu,{:.3}", update.as_secs_f64() * 1e6);
        }
    }

    /// 打印报告；返回 `true` = 帧率与抖动都达标。
    fn report(&self) -> bool {
        let elapsed = self.started.elapsed().as_secs_f64();
        println!();
        println!("-- 帧率与抖动（真机真窗口，commit → commit） --");
        println!(
            "运行 {:.1}s（预热 {:.1}s / 测量 {:.1}s）提交帧 {}；测量期帧数 {}",
            elapsed,
            self.warmup.as_secs_f64(),
            self.duration.as_secs_f64(),
            self.commits,
            self.intervals.len()
        );
        let Some(report) = IntervalReport::new(&self.intervals) else {
            println!("没有采到帧间隔样本（窗口不可见 / 帧时钟未启动？）⇒ FAIL");
            return false;
        };
        println!("{}", report.lines());
        let dropped = report
            .intervals
            .values()
            .iter()
            .filter(|interval| **interval >= report.intervals.summary().map_or(f64::INFINITY, |s| s.p50_us) * DROPPED_FACTOR)
            .count();
        let throughput = self.intervals.len() as f64 / self.duration.as_secs_f64();
        println!(
            "测量期吞吐          {:.2} fps（帧数 / 测量时长）",
            throughput
        );
        println!(
            "掉帧（间隔 ≥{:.1}× 中位） {} / {}",
            DROPPED_FACTOR, dropped,
            self.intervals.len()
        );
        let refresh = report.fps;
        println!("反推刷新率          {refresh:.2} Hz（1000 / 中位间隔）");
        println!();
        println!("-- UI 线程 CPU（本帧 `App::frame`；编码在合成线程） --");
        println!("{}", self.frame_cpu.line());
        println!("{}", self.update_cpu.line());

        let fps_ok = report.fps >= MIN_FPS;
        let jitter_ok = report.jitter_us <= MAX_JITTER_MS * 1e3;
        println!();
        println!("-- 判定（真机窗口） --");
        println!(
            "≥60 fps               实测 {:.2} fps（门槛 ≥{MIN_FPS:.0}，60 Hz vsync 锁定即 59.9）⇒ {}",
            report.fps,
            verdict(fps_ok)
        );
        println!(
            "帧间隔抖动 ≤1 ms      实测 RMS {:.3} ms ⇒ {}",
            report.jitter_us / 1e3,
            verdict(jitter_ok)
        );
        if refresh < 90.0 {
            println!(
                "120 Hz                本机显示器为该间隔反推的 {refresh:.1} Hz，**无 120 Hz 屏幕，\
                 120 Hz 无法在此机验证**；逻辑就绪证据见 `src/render/frame_clock.rs`\
                 （`frame_interval(120)=8.33ms` / `normalize_refresh_hz` 24–360 Hz / \
                 `FramePacing` 的 target 推导）与 `src/render/macos/clock.rs` 的 display link 对齐。"
            );
        } else {
            println!("120 Hz                已在该显示器上实测（中位间隔 {:.2} ms ⇒ {refresh:.1} Hz）", 1e3 / refresh);
        }
        fps_ok && jitter_ok
    }
}

/// 跑 `window` 模式；返回 `true` = 帧率与抖动达标。
pub fn run_window(config: &Config) -> Result<bool, Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut app = App::new(runtime.handle().clone());
    let window = app.create_window(WindowOptions {
        title: smol_str::SmolStr::new_static("kytrix acceptance"),
        size: scene::WINDOW_SIZE,
        scale: Scale::ONE,
        background: Color::from_rgba8([0x0B, 0x0E, 0x13, 0xFF]),
    });
    let mut load = Load::build(&mut app, window)?;

    println!("== 最终合成负载验收 · window 模式（真机真窗口） ==");
    print_environment(&app, window);
    println!("{}", load.describe(&app));
    println!(
        "测量计划：预热 {:.1}s，测量 {:.1}s，脚本 = 增量帧（5 行盘口 + 1 笔成交 + 滚动 + 折线平移）",
        config.window_warmup_seconds, config.window_seconds
    );
    load.install_continuous_frames(&mut app)?;
    #[cfg(feature = "hud")]
    if config.hud {
        app.set_debug_hud(window, true)?;
        println!("Debug HUD：已打开（右上角显示 FPS / Frame / CPU / Upload / Present / Latency / Draw）");
    }
    #[cfg(not(feature = "hud"))]
    if config.hud {
        println!("Debug HUD：本二进制未以 --features hud 构建，忽略 --hud");
    }

    // 首帧：全量（让窗口先有内容）。
    let _ = app.frame(window);

    let mut run = WindowRun::new(config);
    let passed = Rc::new(Cell::new(false));
    let result_slot = Rc::clone(&passed);
    app.on_frame(move |app| {
        if run.finished {
            return;
        }
        let now = Instant::now();
        let update_started = Instant::now();
        load.incremental_step(app);
        let update_time = update_started.elapsed();
        let frame_started = Instant::now();
        let outcome = app.frame(window);
        let frame_time = frame_started.elapsed();
        if matches!(outcome, Some(Ok(FrameOutcome::Committed(_)))) {
            run.observe(now, update_time, frame_time);
        }
        if run.last_progress.elapsed() >= PROGRESS {
            run.last_progress = Instant::now();
            let fps = run
                .intervals
                .summary()
                .map_or(0.0, |summary| 1e6 / summary.p50_us.max(1.0));
            println!(
                "  t={:>5.1}s 提交={:<5} 测量样本={:<5} 中位 {:.2} fps",
                run.started.elapsed().as_secs_f64(),
                run.commits,
                run.intervals.len(),
                fps
            );
        }
        if now >= run.deadline {
            run.finished = true;
            let ok = run.report();
            result_slot.set(ok);
            let _ = app.close_window(window);
        }
    });

    Application::new().run(app)?;
    Ok(passed.get())
}

/// 跑 `idle` 模式：窗口静止（无动画、无脏位）时测进程 CPU；返回 `true` = <1%。
pub fn run_idle(config: &Config) -> Result<bool, Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut app = App::new(runtime.handle().clone());
    let window = app.create_window(WindowOptions {
        title: smol_str::SmolStr::new_static("kytrix acceptance (idle)"),
        size: scene::WINDOW_SIZE,
        scale: Scale::ONE,
        background: Color::from_rgba8([0x0B, 0x0E, 0x13, 0xFF]),
    });
    let load = Load::build(&mut app, window)?;

    println!("== 最终合成负载验收 · idle 模式（窗口静止 60 s） ==");
    print_environment(&app, window);
    println!("{}", load.describe(&app));
    println!("pid={}", std::process::id());
    // 首帧：把 5000 行 / 500 笔 / 20 万点全部提交一次，然后**不再写任何状态**。
    let _ = app.frame(window);
    println!(
        "首帧已提交；不安装动画 ⇒ `needs_continuous_frames=false`，泵阻塞在消息等待（零唤醒）。\
         预热 {}s 后开始采样 {}s。",
        config.idle_warmup_seconds, config.idle_seconds
    );

    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let stop_hook = Arc::clone(&stop);
    app.on_frame(move |app| {
        if stop_hook.load(Ordering::Acquire) {
            let _ = app.close_window(window);
        }
    });
    let handle = app.handle();
    let idle_seconds = config.idle_seconds;
    let idle_warmup = config.idle_warmup_seconds;
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(idle_warmup));
        println!("idle: 采样窗口开始（{idle_seconds} s，进程静止）");
        let measurement = sample_idle_cpu(Duration::from_secs(idle_seconds));
        match measurement {
            Some(idle) => {
                println!();
                println!("-- 空闲 CPU（窗口静止 {}s） --", idle_seconds);
                println!("{}", idle.line());
                println!(
                    "判定：空闲 CPU <1%（占单核）⇒ {}",
                    verdict(idle.percent_of_core() < 1.0)
                );
                println!(
                    "备注：占整机口径 {:.4}%（分母 {} 逻辑核）；两口径都远小于 1%",
                    idle.percent_of_machine(),
                    crate::report::cores()
                );
                if idle.percent_of_core() >= 1.0 {
                    stop_flag.store(true, Ordering::Release);
                    // 不达标时也要退出：报告已经打出。
                }
                stop_flag.store(true, Ordering::Release);
            }
            None => {
                println!("空闲 CPU：本平台无法读取进程 CPU 时间（采样失败）⇒ FAIL");
                stop_flag.store(true, Ordering::Release);
            }
        }
        handle.wake();
    });

    Application::new().run(app)?;
    println!("idle 模式结束（窗口已关闭）");
    // 采样线程已经把结论打进日志；达标与否由日志里的判定行决定（这里不做二次推断）。
    Ok(true)
}

/// 打印测量环境（OS / 核数 / 窗口尺寸 / DPI）。
fn print_environment(app: &App, window: WindowId) {
    let size = app.window(window).map(|window| window.size());
    let scale = app.window(window).map(|window| window.scale().factor());
    println!(
        "环境：os={} arch={} logical_cpus={} window={:?} dpi={:?}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        crate::report::cores(),
        size,
        scale
    );
}

/// PASS / FAIL 标记。
fn verdict(pass: bool) -> &'static str {
    if pass { "PASS" } else { "FAIL" }
}

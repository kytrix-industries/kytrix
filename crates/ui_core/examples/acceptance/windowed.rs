use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use ui_core::{
    app::{App, AppHandle, Application},
    math::DevicePixelRatio,
    paint::color::Color,
    window::{FrameOutcome, WindowId, WindowOptions},
};

use crate::{
    Config,
    report::{IntervalReport, Samples, sample_idle_cpu},
    scene::{self, Load},
};

const MAX_JITTER_MS: f64 = 1.0;
const MISSED_FACTOR: f64 = 1.5;
const PROGRESS: Duration = Duration::from_secs(5);

struct WindowRun {
    warmup: Duration,
    duration: Duration,
    started: Instant,
    measure_at: Instant,
    deadline: Instant,
    target_us: f64,
    intervals: Samples,
    frame_cpu: Samples,
    update_cpu: Samples,
    e2e_presented: Samples,
    presentation_observed: usize,
    presentation_presented: usize,
    presentation_cursor: Option<u32>,
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
        let capacity = (duration.as_secs_f64() * f64::from(config.hz) * 1.5).ceil() as usize + 8;
        Self {
            warmup,
            duration,
            started,
            measure_at,
            deadline,
            target_us: 1e6 / f64::from(config.hz.max(1)),
            intervals: Samples::new("frame interval", capacity),
            frame_cpu: Samples::new("ui thread frame cpu", capacity),
            update_cpu: Samples::new("ui thread update cpu", capacity),
            e2e_presented: Samples::new("publish -> presented", capacity),
            presentation_observed: 0,
            presentation_presented: 0,
            presentation_cursor: None,
            commits: 0,
            last_commit: None,
            last_progress: started,
            finished: false,
            raw: config.raw,
        }
    }

    fn observe_presentations(&mut self, app: &App, window: WindowId) {
        let Some(target) = app.window(window) else {
            return;
        };
        let latest = target
            .last_frame_presentation()
            .map_or(0, |presentation| presentation.index.get());
        for presentation in target.recent_frame_presentations() {
            let index = presentation.index.get();
            if self
                .presentation_cursor
                .is_some_and(|cursor| index <= cursor)
            {
                continue;
            }
            if presentation.published_at < self.measure_at {
                self.presentation_cursor = Some(index);
                continue;
            }
            let settled = presentation.presented_at.is_some()
                || latest.saturating_sub(index) >= 2
                || presentation.published_at < self.deadline - Duration::from_millis(50);
            if !settled {
                break;
            }
            self.presentation_cursor = Some(index);
            self.presentation_observed += 1;
            if let Some(e2e) = presentation.publish_to_presented() {
                self.presentation_presented += 1;
                self.e2e_presented.push(e2e);
                if self.raw {
                    println!("raw,e2e_presented,{:.3}", e2e.as_secs_f64() * 1e6);
                }
            } else if self.raw {
                println!("raw,e2e_presented,missing");
            }
        }
    }

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

    fn report(&self, hz: u32, refresh_hz: Option<f32>, deferrals: u64) -> bool {
        let elapsed = self.started.elapsed().as_secs_f64();
        println!();
        match refresh_hz {
            Some(hz) => println!("帧预算门禁：刷新率提示 {hz:.1} Hz（写入快于刷新时合并到下一拍）"),
            None => println!(
                "帧预算门禁：平台未提供刷新率提示（headless / macOS 屏幕不可得）⇒ 门禁禁用，\
                 吞吐仅受平台 present 门禁约束"
            ),
        }
        println!("-- 帧率与抖动（真机真窗口，commit → commit，驱动 {hz} Hz） --");
        println!(
            "运行 {:.1}s（预热 {:.1}s / 测量 {:.1}s）提交帧 {}；测量期间隔样本 {}",
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

        let target = self.target_us;
        let rms = (self
            .intervals
            .values()
            .iter()
            .map(|value| {
                let delta = value - target;
                delta * delta
            })
            .sum::<f64>()
            / self.intervals.len() as f64)
            .sqrt();
        let missed = self
            .intervals
            .values()
            .iter()
            .filter(|interval| **interval >= target * MISSED_FACTOR)
            .count();
        let throughput = self.intervals.len() as f64 / self.duration.as_secs_f64();
        println!("目标间隔            {:.3} ms（{hz} Hz）", target / 1e3);
        println!("抖动（RMS vs 目标）  {:.3} ms", rms / 1e3);
        println!(
            "掉拍（间隔 ≥{MISSED_FACTOR:.1}× 目标） {missed} / {}",
            self.intervals.len()
        );
        println!("测量期吞吐          {throughput:.2} fps（帧数 / 测量时长）");
        println!();
        println!("-- A4 e2e 呈现（publish → presented；presented 是真实上屏时刻的下界） --");
        println!("{}", self.e2e_presented.line());
        let ratio = if self.presentation_observed == 0 {
            None
        } else {
            Some(self.presentation_presented as f64 / self.presentation_observed as f64)
        };
        println!(
            "presented 有值率 {}（{} / {} 已结算帧；未结算 = 测量结束时反馈还没到）",
            ratio.map_or("--".to_owned(), |ratio| format!("{:.2}%", ratio * 100.0)),
            self.presentation_presented,
            self.presentation_observed
        );
        if let Some(summary) = self.e2e_presented.summary() {
            let p50_vsync = summary.p50_us / target;
            let p99_vsync = summary.p99_us / target;
            println!(
                "e2e p50 = {:.3} ms（{p50_vsync:.2} vsync） / p99 = {:.3} ms（{p99_vsync:.2} vsync）",
                summary.p50_us / 1e3,
                summary.p99_us / 1e3
            );
            println!(
                "参考阈值（目标 ≤2 vsync @ {hz} Hz）= {:.3} ms；本项为 A4 观测项，阈值待 A2 帧预算落地后定，\
                 不并入本次 PASS/FAIL",
                2.0 * target / 1e3
            );
        } else {
            println!("没有采到 e2e 呈现样本（presented_at 恒为 None / 窗口未出帧？）");
        }
        println!();
        println!("-- UI 线程 CPU（本帧 `App::frame`；编码在合成线程） --");
        println!("{}", self.frame_cpu.line());
        println!("{}", self.update_cpu.line());
        println!();
        println!("-- 判定（真机窗口） --");
        let jitter_ok = rms <= MAX_JITTER_MS * 1e3;
        let fps_ok = throughput >= f64::from(hz) - 1.0;
        println!(
            "≥{hz} fps（吞吐）         实测 {throughput:.2} fps（门槛 ≥{:.0}）⇒ {}",
            f64::from(hz) - 1.0,
            verdict(fps_ok)
        );
        println!(
            "帧间隔抖动 ≤1 ms      实测 RMS {:.3} ms ⇒ {}",
            rms / 1e3,
            verdict(jitter_ok)
        );
        let budget_ok = match refresh_hz {
            Some(refresh) if f64::from(hz) > f64::from(refresh) + 1.0 => {
                let limit = f64::from(refresh) * 1.15;
                let capped = throughput <= limit;
                println!(
                    "写入 {hz} Hz > 刷新 {refresh:.1} Hz ⇒ 吞吐钳到 ≤{limit:.1} fps（含 10% 相位容差）；\
                     实测 {throughput:.2} fps，累计推迟 {deferrals} 次 ⇒ {}",
                    verdict(capped && deferrals > 0)
                );
                capped && deferrals > 0
            }
            Some(refresh) => {
                println!(
                    "写入 {hz} Hz ≤ 刷新 {refresh:.1} Hz（每 refresh 最多一帧的自然满足）；\
                     累计推迟 {deferrals} 次"
                );
                true
            }
            None => {
                println!(
                    "刷新率提示不可得（平台未提供）⇒ 帧预算门禁禁用，吞吐仅受平台 present 门禁约束"
                );
                true
            }
        };
        if hz < 90 {
            println!(
                "120 Hz 能力           目标频率 60 Hz 下测得抖动 {:.3} ms；\
                 120 Hz 的**逻辑**由「帧预算门禁（每 refresh 周期最多一帧，10% 相位容差）+ \
                 每窗口 GPU present 门禁」保证（A2 已删除进程级帧时钟，present 节奏由 Windows \
                 DXGI waitable / macOS CAMetalDisplayLink / Linux wl_surface.frame 决定），\
                 本机显示器只有 60 Hz，120 Hz 的**上屏**无法验证（详见报告「120 Hz」一节）。",
                rms / 1e3
            );
        }
        fps_ok && jitter_ok && budget_ok
    }
}

pub fn run_window(config: &Config) -> Result<bool, Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut app = App::new(runtime.handle().clone());
    let window = app.create_window(WindowOptions {
        title: smol_str::SmolStr::new_static("kytrix acceptance"),
        size: scene::WINDOW_SIZE,
        scale: DevicePixelRatio::ONE,
        background: Color::from_rgba8([0x0B, 0x0E, 0x13, 0xFF]),
        ..WindowOptions::default()
    });
    let mut load = Load::build(&mut app, window)?;

    println!("== 最终合成负载验收 · window 模式（真机真窗口） ==");
    print_environment(&app, window);
    println!("{}", load.describe(&app));
    println!(
        "测量计划：目标 {} Hz（间隔 {:.3} ms），预热 {:.1}s，测量 {:.1}s；\
         数据泵每拍投一次「5 行盘口 + 1 笔成交 + 滚动 + 折线平移」",
        config.hz,
        1e3 / f64::from(config.hz.max(1)),
        config.window_warmup_seconds,
        config.window_seconds
    );
    #[cfg(feature = "hud")]
    if config.hud {
        app.set_debug_hud(window, true)?;
        println!("Debug HUD：已打开（F12 切换；截图证据见报告）");
    }
    #[cfg(not(feature = "hud"))]
    if config.hud {
        println!("Debug HUD：本二进制未以 --features hud 构建，忽略 --hud");
    }

    let _ = app.frame(window);
    let mut run = WindowRun::new(config);
    let passed = Rc::new(Cell::new(false));
    let result_slot = Rc::clone(&passed);

    let pending = Arc::new(AtomicU32::new(0));
    let stop_pump = Arc::new(AtomicBool::new(false));
    let hz = config.hz.max(1);
    let close_handle = app.handle();
    let pump_handle = app.handle();
    let mut pump_started = false;
    app.on_frame(move |app| {
        if !pump_started {
            pump_started = true;
            spawn_data_pump(
                pump_handle.clone(),
                Arc::clone(&pending),
                Arc::clone(&stop_pump),
                hz,
            );
        }
        if run.finished {
            return;
        }
        let now = Instant::now();
        run.observe_presentations(app, window);
        if pending.swap(0, Ordering::AcqRel) > 0 {
            let update_started = Instant::now();
            load.incremental_step(app);
            let update_time = update_started.elapsed();
            let frame_started = Instant::now();
            let outcome = app.frame(window);
            let frame_time = frame_started.elapsed();
            if matches!(outcome, Some(Ok(FrameOutcome::Committed(_)))) {
                run.observe(now, update_time, frame_time);
            }
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
            let deferrals = app.frame_budget_deferrals(window);
            let refresh_hz = app.frame_refresh_hz(window);
            let ok = run.report(hz, refresh_hz, deferrals);
            result_slot.set(ok);
            stop_pump.store(true, Ordering::Release);
            let _ = app.close_window(window);
            close_handle.wake();
        }
    });

    Application::new().run(app)?;
    #[cfg(all(target_os = "macos", feature = "test-support"))]
    println!(
        "metal_gpu_core_generation={}（首次创建=1；设备恢复一次 ⇒ 2）",
        ui_core::test_support::diagnostics::metal_gpu_core_generation()
    );
    Ok(passed.get())
}

fn spawn_data_pump(handle: AppHandle, pending: Arc<AtomicU32>, stop: Arc<AtomicBool>, hz: u32) {
    let period = Duration::from_secs_f64(1.0 / f64::from(hz));
    std::thread::Builder::new()
        .name("acceptance-data-pump".to_owned())
        .spawn(move || {
            let mut next = Instant::now();
            loop {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                next += period;
                let now = Instant::now();
                if next > now {
                    let remaining = next - now;
                    if remaining > Duration::from_millis(1) {
                        std::thread::sleep(remaining - Duration::from_millis(1));
                    }
                    while Instant::now() < next {
                        std::hint::spin_loop();
                    }
                } else {
                    next = now;
                }
                pending.fetch_add(1, Ordering::AcqRel);
                handle.wake();
            }
        })
        .expect("data pump thread");
}

pub fn run_idle(config: &Config) -> Result<bool, Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut app = App::new(runtime.handle().clone());
    let window = app.create_window(WindowOptions {
        title: smol_str::SmolStr::new_static("kytrix acceptance (idle)"),
        size: scene::WINDOW_SIZE,
        scale: DevicePixelRatio::ONE,
        background: Color::from_rgba8([0x0B, 0x0E, 0x13, 0xFF]),
        ..WindowOptions::default()
    });
    let load = Load::build(&mut app, window)?;

    println!("== 最终合成负载验收 · idle 模式（窗口静止） ==");
    print_environment(&app, window);
    println!("{}", load.describe(&app));
    println!("pid={}", std::process::id());
    let _ = app.frame(window);
    println!(
        "首帧已提交；不安装动画 ⇒ `needs_continuous_frames=false`，泵阻塞在消息等待（零唤醒）。\
         预热 {}s 后开始采样 {}s。",
        config.idle_warmup_seconds, config.idle_seconds
    );

    let stop = Arc::new(AtomicBool::new(false));
    let stop_hook = Arc::clone(&stop);
    let handle = app.handle();
    let close_handle = handle.clone();
    app.on_frame(move |app| {
        if stop_hook.load(Ordering::Acquire) {
            let _ = app.close_window(window);
            close_handle.wake();
        }
    });
    let idle_seconds = config.idle_seconds;
    let idle_warmup = config.idle_warmup_seconds;
    let passed = Arc::new(AtomicBool::new(false));
    let passed_thread = Arc::clone(&passed);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(idle_warmup));
        println!("idle: 采样窗口开始（{idle_seconds} s，进程静止）");
        match sample_idle_cpu(Duration::from_secs(idle_seconds)) {
            Some(idle) => {
                println!();
                println!("-- 空闲 CPU（窗口静止 {}s） --", idle_seconds);
                println!("{}", idle.line());
                let ok = idle.percent_of_core() < 1.0;
                passed_thread.store(ok, Ordering::Release);
                println!(
                    "判定：空闲 CPU <1%（占单核）⇒ {}；占整机 {:.4}%（分母 {} 逻辑核）",
                    verdict(ok),
                    idle.percent_of_machine(),
                    crate::report::cores()
                );
            }
            None => {
                println!("空闲 CPU：本平台无法读取进程 CPU 时间（采样失败）⇒ FAIL");
            }
        }
        stop.store(true, Ordering::Release);
        handle.wake();
    });

    Application::new().run(app)?;
    println!("idle 模式结束（窗口已关闭）");
    Ok(passed.load(Ordering::Acquire))
}

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

fn verdict(pass: bool) -> &'static str {
    if pass { "PASS" } else { "FAIL" }
}

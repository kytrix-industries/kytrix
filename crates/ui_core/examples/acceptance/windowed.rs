//! `window` / `idle` 模式：真机真窗口。
//!
//! # 帧间隔口径（写进报告，避免与 HUD 的 FPS 对不上）
//!
//! 测量点是 **UI 线程提交帧**（`FrameOutcome::Committed` 的时刻，commit → commit），不是
//! 「GPU 完成」也不是「上屏」：
//!
//! - **驱动**：一个按目标刷新率（`--hz`，默认 60）节拍的数据泵线程（混合 sleep + 1 ms 自旋，
//!   精度 ~0.1 ms）。每个节拍给 UI 线程投一次「增量帧更新」；UI 线程在钩子里应用更新并提交一帧。
//!   这样测的是「数据以 60/120 Hz 到达时，引擎能否稳定每拍交一帧」——市场数据类界面的真实形态。
//!   为什么不用「每次泵循环都写状态」：那会自己唤醒自己（写状态 ⇒ `request_frame` ⇒ 唤醒 ⇒
//!   再写），实测变成 ~300 fps 的忙循环，测到的就不是刷新节拍了。
//! - **present 级观测（A4）**：`App::last_frame_presentation` / `Window::recent_frame_presentations`
//!   给出每帧的 `published_at / completed_at / presented_at`（`presented_at` 是真实上屏时刻的
//!   **下界**：Windows 帧统计 / Linux `wp_presentation` / macOS display-link tick 推断）。本 harness
//!   按 `FrameIndex` 增量消费最近若干帧的呈现快照，打印 `publish→presented` 的 p50/p99 与有值率。
//!   `presented_at` 缺失（该端/该帧没有可信值）的帧计入「有值率」分母、不进 e2e 样本。
//!   **阈值先测量后定**（目标 ≤2 vsync @60 Hz，即 p99 ≤ ~33 ms）：本 harness 只输出测量值，
//!   不把它并入本次 PASS/FAIL（A2 的帧预算落地后再定门禁）。
//!
//! # 为什么钩子里自己提交一帧
//!
//! 泵的 `frame_all` 只在「有唤醒 / 平台节拍」时出帧，外面拿不到「本帧 CPU 起点」。钩子里的
//! `App::frame` 让我们能同时测到 **UI 线程帧内 CPU**（样式 → 文本 → 布局 → 绘制 → 发布；
//! 真实窗口的实例编码在合成线程）。泵随后那次 `frame` 因为没有脏位而退化为 `Idle`。

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
    math::Scale,
    paint::color::Color,
    window::{FrameOutcome, WindowId, WindowOptions},
};

use crate::{
    Config,
    report::{IntervalReport, Samples, sample_idle_cpu},
    scene::{self, Load},
};

/// 帧间隔抖动上限（毫秒；相对**目标间隔**的 RMS 偏差）。
const MAX_JITTER_MS: f64 = 1.0;
/// 掉拍判据：间隔 ≥ 目标间隔的 1.5 倍（漏了至少一拍）。
const MISSED_FACTOR: f64 = 1.5;
/// 进度打印间隔。
const PROGRESS: Duration = Duration::from_secs(5);

/// 真实窗口模式的运行状态。
struct WindowRun {
    warmup: Duration,
    duration: Duration,
    started: Instant,
    measure_at: Instant,
    deadline: Instant,
    /// 目标间隔（微秒；`--hz` 的倒数）。
    target_us: f64,
    intervals: Samples,
    frame_cpu: Samples,
    update_cpu: Samples,
    /// `publish → presented`（上屏时刻下界）的 e2e 样本（A4；微秒）。
    e2e_presented: Samples,
    /// 已结算（有反馈且不再变化）的呈现快照帧数（有值率分母）。
    presentation_observed: usize,
    /// 其中 `presented_at` 有值的帧数（有值率分子）。
    presentation_presented: usize,
    /// 已消费到的最后一条呈现快照的 `FrameIndex`（增量消费游标）。
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
        // 容量按「目标频率 × 1.5」估算（比目标快 50% 也不扩容）。
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

    /// 增量消费窗口上最近若干帧的呈现快照（A4）。
    ///
    /// 结算口径：`presented_at` 有值 ⇒ 立即结算；没有值但要等它「后面已经又来了 2 帧反馈」
    /// 才判为 `None`（Linux 的 `wp_presentation` 事件晚于反馈一个 vsync 左右到达，不能过早
    /// 判死）。只统计测量窗口内提交的帧（`published_at ≥ measure_at`）。
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
                // 保持帧序：前面的还没结算，后面的不能先消费（否则 e2e 会被错配）。
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

    /// 打印报告；返回 `true` = 帧率、抖动与帧预算门禁都达标。
    ///
    /// `refresh_hz` = 平台上报的窗口所在 output 刷新率提示（A5；`None` = 平台没给，门禁禁用）；
    /// `deferrals` = 累计被预算门禁推迟的请求数（证明「合并到下一拍」真的发生过）。
    fn report(&self, hz: u32, refresh_hz: Option<f32>, deferrals: u64) -> bool {
        let elapsed = self.started.elapsed().as_secs_f64();
        println!();
        // RA-3：刷新率提示在**平台注入之后**（`Application::run` 已启动、泵已跑过）打印，
        // 与测量/推迟计数同点，日志自洽（旧实现在 `run` 之前打印 ⇒ 恒显示「禁用」）。
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

        // 相对**目标间隔**（而不是中位数）的抖动：漏拍会同时抬高它和掉拍计数。
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
        // A4：publish → presented（上屏时刻下界）的 e2e 分布与有值率。
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
        // 60 Hz 目标：≥60 fps（60 Hz vsync 锁定的 59.9 视为达标）；
        // 120 Hz 目标：测的是「能否稳定每 8.33 ms 交一帧」，达标口径同为 fps ≥ 目标频率 - 1。
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
        // A5：帧预算门禁。写入快于刷新时（`--hz` > 刷新率），吞吐必须被钳到刷新率附近，
        // 且必须观测到推迟（否则说明门禁没生效，UI 帧率 = 写入率）。
        //
        // 门槛 = 刷新率 × 1.15：预算门禁的相位带 10% 周期的提前量（吸收平台唤醒抖动，
        // 保证同频写入稳定 60 fps，见 `window::frame_budget::FrameBudget::admits`），稳态
        // 上限因此是 1/(0.9×周期) ≈ 1.11×刷新率；预留到 1.15 倍余量。
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

    // 首帧：全量（让窗口先有内容）。
    let _ = app.frame(window);
    // 注意：此刻还没有原生窗口（`Application::run` 尚未启动），平台刷新率提示不可用；
    // 预算门禁的提示在 `report()`（计数/判定同点）打印，避免「先禁用、后启用」的误导日志（RA-3）。
    let mut run = WindowRun::new(config);
    let passed = Rc::new(Cell::new(false));
    let result_slot = Rc::clone(&passed);

    // 数据泵：按目标刷新率投递「本拍该更新」的信号（混合 sleep + 1 ms 自旋保证精度）。
    let pending = Arc::new(AtomicU32::new(0));
    let stop_pump = Arc::new(AtomicBool::new(false));
    let hz = config.hz.max(1);
    // 关闭时自唤醒一次：钩子跑在泵的 ④ 步（关闭检查在 ③），不补一拍唤醒的话泵会在
    // `GetMessageW` 上永久阻塞（窗口已从 `App` 移除，但没有新的唤醒到达）。
    let close_handle = app.handle();
    // 数据泵**必须在 `Application::run` 装好平台唤醒目标之后**才启动：macOS 的 run loop
    // source 是在 `run()` 里安装的，提前从其它线程 `wake()` 会踩到未初始化的唤醒目标
    // （实测 SIGSEGV）。因此线程在第一次帧钩子里懒启动。
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
        // A4：每轮都增量消费一次呈现快照（presented 事件可能晚于反馈到达，不能只在提交时读）。
        run.observe_presentations(app, window);
        // 只应用「一拍」：积压的节拍合并成一次更新（最新值胜），与真实行情的合并语义一致。
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
    Ok(passed.get())
}

/// 数据泵线程：按 `hz` 的节拍置位「本拍该更新」并唤醒 UI 线程。
///
/// 混合 sleep + 1 ms 自旋：`Sleep` 在 Windows 上的默认精度是 15.6 ms，纯 sleep 会产生
/// 十几毫秒的相位误差；自旋只覆盖最后 1 ms（占空比 ~6%，且在独立线程上，不进 UI 帧计时）。
///
/// # Panics
/// 线程创建失败（进程已无法继续验收）。
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
                    // 落后了（上一拍被抢占）：立刻补一拍并重新对齐相位。
                    next = now;
                }
                pending.fetch_add(1, Ordering::AcqRel);
                handle.wake();
            }
        })
        .expect("data pump thread");
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

    println!("== 最终合成负载验收 · idle 模式（窗口静止） ==");
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
    let stop_hook = Arc::clone(&stop);
    let handle = app.handle();
    let close_handle = handle.clone();
    app.on_frame(move |app| {
        if stop_hook.load(Ordering::Acquire) {
            let _ = app.close_window(window);
            // 钩子在泵的 ④ 步、关闭检查在 ③ 步：自己补一拍唤醒，否则泵在关闭后
            // 还会阻塞在 `GetMessageW` 上。
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

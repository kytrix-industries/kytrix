//! `cpu` 模式：headless 多帧序列，测「增量帧」与「1000 单元文本更新帧」的 CPU 时间。
//!
//! 脚本（帧数可调，默认值即报告口径）：
//!
//! ```text
//! 首帧（全量，不计）→ 50 帧预热（增量，不计）
//! → 250 帧增量帧（5 行盘口 + 1 笔成交 + 滚动 1 行 + 折线平移 0.5px）
//! → 30 帧 1000 单元文本更新（125 行 × 8 列）
//! → 30 帧只平移折线 / 30 帧只滚动成交列表（分通道归因）
//! → 10 帧空闲帧（帧门禁必须全部退化为 Idle）
//! ```
//!
//! 每一帧的计时拆成两段：**写状态**（应用侧 `App::write` / `scroll_to` /
//! `set_geometry_transform`）与**帧内 CPU**（`App::frame` 的样式 → 文本 → 布局 → 绘制 → 编码）。
//! 判定用「写 + 帧」的合计（更接近 UI 线程每帧真实占用）。
//!
//! `--features test-support` 构建时额外打印 `FrameRecord` 的确定性计数
//! （`encoded_lanes` / `reused_segments` / `uploaded_bytes` / `scanned_nodes`），
//! 并**明确警告**该构建的计时不可与生产特性比较。

use std::time::Instant;

use ui_core::{
    app::App,
    math::Scale,
    paint::color::Color,
    window::{FrameOutcome, WindowId, WindowOptions},
};

use crate::{
    Config,
    report::Samples,
    scene::{self, Load},
};

/// 增量帧目标（毫秒）。
const INCREMENTAL_TARGET_MS: f64 = 2.0;
/// 1000 单元文本帧目标（毫秒）。
const TEXT_TARGET_MS: f64 = 4.0;

/// 一个场景的测量结果。
struct Scenario {
    name: &'static str,
    update: Samples,
    frame: Samples,
    total: Samples,
    idle_frames: usize,
    /// A1（paint 脏子树剪枝）的确定性计数：默认构建也打印（`Window::tree()` 是公开只读面）。
    paint: PaintProbe,
    /// 本场景的 `FrameRecord` 计数区间（仅 test-support 构建）。
    #[cfg(feature = "test-support")]
    counters: Counters,
}

/// paint 剪枝的观测值（每帧访问数区间 + 累计路径计数）。
///
/// 与 [`Counters`]（仅 `test-support`）不同：这里没有 test-support 依赖，因此**默认特性**构建的
/// 性能报告也能证明「本帧走的是增量路径」。
#[derive(Clone, Copy, Debug)]
struct PaintProbe {
    /// 每帧 `visited_nodes` 的 [min, max]。
    visited: (usize, usize),
    /// 相对本场景起点的累计全量 / 增量帧数。
    full_frames: u64,
    incremental_frames: u64,
    /// 相对本场景起点的绘制序兜底次数（增量路径回退率）。
    order_rebuilds: u64,
    /// 场景起点时的累计计数器快照。
    start: Option<ui_core::node::tree::PaintCounters>,
}

impl PaintProbe {
    fn new() -> Self {
        Self {
            visited: (usize::MAX, 0),
            full_frames: 0,
            incremental_frames: 0,
            order_rebuilds: 0,
            start: None,
        }
    }

    fn observe(&mut self, app: &App, window: WindowId) {
        let Some(target) = app.window(window) else {
            return;
        };
        let counters = target.tree().paint_counters();
        let start = *self.start.get_or_insert(counters);
        self.visited.0 = self.visited.0.min(counters.visited_nodes);
        self.visited.1 = self.visited.1.max(counters.visited_nodes);
        self.full_frames = counters.full_frames.saturating_sub(start.full_frames);
        self.incremental_frames = counters
            .incremental_frames
            .saturating_sub(start.incremental_frames);
        self.order_rebuilds = counters.order_rebuilds.saturating_sub(start.order_rebuilds);
    }

    fn line(&self, name: &str) -> String {
        if self.visited.0 == usize::MAX {
            return format!("{name}: paint (no frame)");
        }
        format!(
            "{name}: paint visited_nodes=[{}, {}] full_frames=+{} incremental_frames=+{} \
             order_rebuilds=+{}",
            self.visited.0,
            self.visited.1,
            self.full_frames,
            self.incremental_frames,
            self.order_rebuilds,
        )
    }
}

/// `FrameRecord` 计数的最小 / 最大值（确定性断言面）。
#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug)]
struct Counters {
    encoded_lanes: (usize, usize),
    reused_segments: (usize, usize),
    uploaded_bytes: (usize, usize),
    primitives: (usize, usize),
    instances: (usize, usize),
    fast_path: (f64, f64),
    shaped: (usize, usize),
    laid: (usize, usize),
    rasterized: (usize, usize),
    atlas_rebuilds: (usize, usize),
    frames: usize,
}

#[cfg(feature = "test-support")]
impl Counters {
    fn new() -> Self {
        Self {
            encoded_lanes: (usize::MAX, 0),
            reused_segments: (usize::MAX, 0),
            uploaded_bytes: (usize::MAX, 0),
            primitives: (usize::MAX, 0),
            instances: (usize::MAX, 0),
            fast_path: (f64::INFINITY, 0.0),
            shaped: (usize::MAX, 0),
            laid: (usize::MAX, 0),
            rasterized: (usize::MAX, 0),
            atlas_rebuilds: (usize::MAX, 0),
            frames: 0,
        }
    }

    fn observe(&mut self, app: &App, window: WindowId) {
        let Some(stats) = app
            .window(window)
            .and_then(ui_core::window::Window::frame_stats)
            .copied()
        else {
            return;
        };
        self.frames += 1;
        let span = |slot: &mut (usize, usize), value: usize| {
            slot.0 = slot.0.min(value);
            slot.1 = slot.1.max(value);
        };
        span(&mut self.encoded_lanes, stats.encoded_lanes);
        span(&mut self.reused_segments, stats.reused_segments);
        span(&mut self.uploaded_bytes, stats.uploaded_bytes);
        span(&mut self.primitives, stats.primitives);
        span(&mut self.instances, stats.instances);
        if let Some(text) = stats.text {
            span(&mut self.shaped, text.shaped);
            let ratio = if text.fast_path_queries == 0 {
                0.0
            } else {
                text.fast_path_shaped as f64 / text.fast_path_queries as f64
            };
            self.fast_path.0 = self.fast_path.0.min(ratio);
            self.fast_path.1 = self.fast_path.1.max(ratio);
        }
        if let Some(layout) = stats.text_layout {
            span(&mut self.laid, layout.laid);
            span(&mut self.rasterized, layout.rasterized);
            span(&mut self.atlas_rebuilds, layout.atlas_rebuilds);
        }
    }

    fn line(&self, name: &str) -> String {
        if self.frames == 0 {
            return format!("{name}: (no FrameRecord)");
        }
        format!(
            "{name}: frames={} encoded_lanes=[{}, {}] reused_segments=[{}, {}] \
             uploaded_bytes=[{}, {}] primitives=[{}, {}] instances=[{}, {}] \
             shaped=[{}, {}] laid=[{}, {}] \
             rasterized=[{}, {}] atlas_rebuilds=[{}, {}] digit_fast_path={:.1}%…{:.1}%",
            self.frames,
            self.encoded_lanes.0,
            self.encoded_lanes.1,
            self.reused_segments.0,
            self.reused_segments.1,
            self.uploaded_bytes.0,
            self.uploaded_bytes.1,
            self.primitives.0,
            self.primitives.1,
            self.instances.0,
            self.instances.1,
            self.shaped.0,
            self.shaped.1,
            self.laid.0,
            self.laid.1,
            self.rasterized.0,
            self.rasterized.1,
            self.atlas_rebuilds.0,
            self.atlas_rebuilds.1,
            self.fast_path.0 * 100.0,
            self.fast_path.1 * 100.0,
        )
    }
}

impl Scenario {
    fn new(name: &'static str, capacity: usize) -> Self {
        Self {
            name,
            update: Samples::new(format!("{name} · 写状态"), capacity),
            frame: Samples::new(format!("{name} · 帧内 CPU"), capacity),
            total: Samples::new(format!("{name} · 合计"), capacity),
            idle_frames: 0,
            paint: PaintProbe::new(),
            #[cfg(feature = "test-support")]
            counters: Counters::new(),
        }
    }
}

/// 跑 `cpu` 模式；返回 `true` = 两项 CPU 指标都达标。
pub fn run(config: &Config) -> Result<bool, Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut app = App::new(runtime.handle().clone());
    let window = app.create_window(WindowOptions {
        title: smol_str::SmolStr::new_static("kytrix acceptance"),
        size: scene::WINDOW_SIZE,
        scale: Scale::ONE,
        background: Color::from_rgba8([0x0B, 0x0E, 0x13, 0xFF]),
        ..WindowOptions::default()
    });

    let build_started = Instant::now();
    let mut load = Load::build(&mut app, window)?;
    let build_time = build_started.elapsed();

    println!("== 最终合成负载验收 · cpu 模式（headless，生产特性） ==");
    print_environment();
    #[cfg(feature = "test-support")]
    println!(
        "警告：本二进制以 --features test-support 构建（每帧采集 FrameRecord）——\
         计时不可与生产特性比较，只看确定性计数。"
    );
    println!("{}", load.describe(&app));
    println!(
        "建树 + 首次挂载      {:.1} ms",
        build_time.as_secs_f64() * 1e3
    );

    // 首帧：全量（文本塑形 / 布局 / 全 lane 编码），不计入统计。
    let first_started = Instant::now();
    let first = frame_once(&mut app, window)?;
    println!(
        "首帧（全量，不计）   {:.3} ms  {}",
        first_started.elapsed().as_secs_f64() * 1e3,
        outcome_label(&first)
    );

    // 预热：真实增量帧，让文本缓存 / 实例环 / 字形图集都热起来。
    for _ in 0..config.warmup_frames {
        load.incremental_step(&mut app);
        let outcome = frame_once(&mut app, window)?;
        if matches!(outcome, FrameOutcome::Idle) {
            return Err("预热期的增量帧退化为 Idle（脚本没有真正改状态）".into());
        }
    }

    let mut incremental = Scenario::new("incremental_frame", config.incremental_frames);
    for _ in 0..config.incremental_frames {
        measure(
            &mut app,
            window,
            &mut load,
            &mut incremental,
            |app, load| load.incremental_step(app),
            config.raw,
        )?;
    }

    let mut text_update = Scenario::new("text_update_1000", config.text_frames);
    for _ in 0..config.text_frames {
        measure(
            &mut app,
            window,
            &mut load,
            &mut text_update,
            |app, load| load.text_update_step(app),
            config.raw,
        )?;
    }

    let mut pan_only = Scenario::new("chart_pan_only", config.pan_frames);
    for _ in 0..config.pan_frames {
        measure(
            &mut app,
            window,
            &mut load,
            &mut pan_only,
            |app, load| load.pan_step(app),
            config.raw,
        )?;
    }

    let mut scroll_only = Scenario::new("tape_scroll_only", config.scroll_frames);
    for _ in 0..config.scroll_frames {
        measure(
            &mut app,
            window,
            &mut load,
            &mut scroll_only,
            |app, load| load.scroll_step(app),
            config.raw,
        )?;
    }

    // 空闲帧：不写任何状态，帧门禁必须直接退化为 Idle（零工作）。
    let mut idle = Samples::new("idle_frame · 帧内 CPU", config.idle_frames);
    let mut idle_bad = 0usize;
    for _ in 0..config.idle_frames {
        load.idle_step();
        let started = Instant::now();
        let outcome = frame_once(&mut app, window)?;
        let elapsed = started.elapsed();
        if matches!(outcome, FrameOutcome::Idle) {
            idle.push(elapsed);
        } else {
            idle_bad += 1;
        }
    }

    // 报告
    println!();
    println!("-- 分场景（毫秒；判定用「写 + 帧」合计的 p99） --");
    for scenario in [&incremental, &text_update, &pan_only, &scroll_only] {
        println!("{}", scenario.frame.line());
        println!("{}", scenario.update.line());
        println!("{}", scenario.total.line());
        if scenario.idle_frames > 0 {
            println!("  警告：{} 帧退化为 Idle", scenario.idle_frames);
        }
        println!("{}", scenario.paint.line(scenario.name));
        #[cfg(feature = "test-support")]
        println!("{}", scenario.counters.line(scenario.name));
    }
    println!("{}", idle.line());
    if idle_bad > 0 {
        println!("  警告：{idle_bad} 个空闲帧提交了像素（帧门禁没有退化为 Idle）");
    }

    let incremental_p99 = incremental
        .total
        .summary()
        .map_or(f64::INFINITY, |summary| summary.p99_us / 1e3);
    let text_p99 = text_update
        .total
        .summary()
        .map_or(f64::INFINITY, |summary| summary.p99_us / 1e3);
    let incremental_pass = incremental_p99 <= INCREMENTAL_TARGET_MS;
    let text_pass = text_p99 <= TEXT_TARGET_MS;

    println!();
    println!("-- 判定（headless CPU） --");
    println!(
        "增量帧 CPU ≤ {INCREMENTAL_TARGET_MS:.0} ms      实测 p99={incremental_p99:.3} ms  ⇒ {}",
        verdict(incremental_pass)
    );
    println!(
        "1000 单元文本帧 ≤ {TEXT_TARGET_MS:.0} ms   实测 p99={text_p99:.3} ms  ⇒ {}",
        verdict(text_pass)
    );
    if let Some(summary) = incremental.total.summary() {
        println!("  增量帧明细：{}", summary.line());
    }
    if let Some(summary) = text_update.total.summary() {
        println!("  文本帧明细：{}", summary.line());
    }
    println!("  空闲帧全部 Idle：{}", verdict(idle_bad == 0));
    println!();
    println!(
        "脚本步数={} 折线平移={:.1}px 节点数={}",
        load.step(),
        load.chart_pan(),
        load.node_count(&app)
    );

    Ok(incremental_pass && text_pass && idle_bad == 0)
}

/// 测一帧：写状态 → 帧 → 记录两段耗时；帧必须提交（否则测的是空帧）。
fn measure(
    app: &mut App,
    window: WindowId,
    load: &mut Load,
    scenario: &mut Scenario,
    update: impl FnOnce(&mut App, &mut Load),
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let update_started = Instant::now();
    update(app, load);
    let update_time = update_started.elapsed();

    let frame_started = Instant::now();
    let outcome = frame_once(app, window)?;
    let frame_time = frame_started.elapsed();

    if matches!(outcome, FrameOutcome::Idle) {
        scenario.idle_frames += 1;
        return Ok(());
    }

    scenario.update.push(update_time);
    scenario.frame.push(frame_time);
    scenario.total.push(update_time + frame_time);
    scenario.paint.observe(app, window);
    #[cfg(feature = "test-support")]
    scenario.counters.observe(app, window);
    if raw {
        println!(
            "raw,{},{:.3}",
            scenario.name,
            (update_time + frame_time).as_secs_f64() * 1e6
        );
    }
    Ok(())
}

/// 提交一帧（窗口不存在 = 应用错误）。
fn frame_once(app: &mut App, window: WindowId) -> Result<FrameOutcome, Box<dyn std::error::Error>> {
    match app.frame(window) {
        Some(result) => Ok(result?),
        None => Err("窗口在测量期间消失".into()),
    }
}

/// 帧结果的短标签。
fn outcome_label(outcome: &FrameOutcome) -> String {
    match outcome {
        FrameOutcome::Idle => "Idle".to_owned(),
        FrameOutcome::Committed(index) => format!("Committed #{}", index.get()),
    }
}

/// 打印测量环境（报告可复核的一部分）。
fn print_environment() {
    println!(
        "环境：os={} arch={} logical_cpus={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        crate::report::cores()
    );
}

/// PASS / FAIL 标记。
fn verdict(pass: bool) -> &'static str {
    if pass { "PASS" } else { "FAIL" }
}

/// `breakdown` 模式：把 1000 单元文本帧拆成「样式+形状 → 重排+光栅 → 绘制+编码」三段。
///
/// 三段用的是引擎自己的公开入口（`App::refresh_text` / `App::layout_text` / `App::frame`），
/// 顺序与 `App::frame` 内部的 pass 顺序一致，因此可以把「文本帧为什么这么慢」定位到具体 pass。
/// **不是**验收判据：单独调用这三段与 `App::frame` 一次跑完在缓存/脏位消费上略有差异。
pub fn run_breakdown(config: &Config) -> Result<bool, Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut app = App::new(runtime.handle().clone());
    let window = app.create_window(WindowOptions {
        title: smol_str::SmolStr::new_static("kytrix acceptance"),
        size: scene::WINDOW_SIZE,
        scale: Scale::ONE,
        background: Color::from_rgba8([0x0B, 0x0E, 0x13, 0xFF]),
        ..WindowOptions::default()
    });
    let mut load = Load::build(&mut app, window)?;
    println!("== 最终合成负载验收 · breakdown 模式（pass 归因，非判据） ==");
    print_environment();
    println!("{}", load.describe(&app));
    let _ = frame_once(&mut app, window)?;
    for _ in 0..config.warmup_frames.min(20) {
        load.incremental_step(&mut app);
        let _ = frame_once(&mut app, window)?;
    }

    let mut styles = Samples::new("refresh_styles（样式 pass）", 20);
    let mut shape = Samples::new("refresh_text（形状 pass）", 20);
    let mut layout = Samples::new("layout_text（重排 + 光栅）", 20);
    let mut paint = Samples::new("frame（布局 + 绘制 + 编码）", 20);
    let mut total = Samples::new("四段合计", 20);
    for _ in 0..20 {
        load.text_update_step(&mut app);
        let started = Instant::now();
        let _ = app.refresh_styles(window);
        let styles_time = started.elapsed();
        let started = Instant::now();
        let _ = app.refresh_text(window);
        let shape_time = started.elapsed();
        let started = Instant::now();
        let _ = app.layout_text(window);
        let layout_time = started.elapsed();
        let started = Instant::now();
        let _ = frame_once(&mut app, window)?;
        let paint_time = started.elapsed();
        if config.raw {
            println!(
                "raw,breakdown_styles,{:.3}",
                styles_time.as_secs_f64() * 1e6
            );
            println!("raw,breakdown_shape,{:.3}", shape_time.as_secs_f64() * 1e6);
            println!(
                "raw,breakdown_layout,{:.3}",
                layout_time.as_secs_f64() * 1e6
            );
            println!("raw,breakdown_frame,{:.3}", paint_time.as_secs_f64() * 1e6);
        }
        styles.push(styles_time);
        shape.push(shape_time);
        layout.push(layout_time);
        paint.push(paint_time);
        total.push(styles_time + shape_time + layout_time + paint_time);
    }

    println!();
    println!("-- 1000 单元文本帧的 pass 归因（毫秒） --");
    println!("{}", styles.line());
    println!("{}", shape.line());
    println!("{}", layout.line());
    println!("{}", paint.line());
    println!("{}", total.line());
    Ok(true)
}

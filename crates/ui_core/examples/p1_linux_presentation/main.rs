//! A4（Linux `presented_at` 生产接线）真机验收：`wp_presentation` → `Window::note_frame_presented`
//! 的**端到端有值率与 e2e 分布**。
//!
//! ```text
//! cargo run --release -p ui_core --example p1_linux_presentation -- --seconds 20 --hz 60
//! ```
//!
//! # 为什么不直接用 `acceptance window`
//!
//! `acceptance window` 的结算口径是「观测到历史条目时 `presented_at` 仍为空 ⇒ 立刻记 missing
//! （除非后面已再来 2 帧反馈）」。Windows/macOS 的呈现证据与 GPU 完成反馈**同刻**（都在
//! `FrameFeedback` 里），该口径成立；Linux 的 `wp_presentation_feedback.presented` 是**异步
//! 事件**（比完成反馈晚 1~2 轮事件循环到达），同一口径会把「证据晚到」误判为「没有证据」，
//! 造成 presented 有值率被人为压低（实测固定 ~22%~35%，与 WAYLAND_DEBUG 里的 presented 事件
//! 数不符）。
//!
//! 本 example 采用**异步结算**口径：每帧从 `recent_frame_presentations()` 观测到后，最多等
//! [`SETTLE_GRACE`]，期间每轮泵都重试（呈现历史在事件到达后原地回填 `presented_at`）。
//! 超过宽限仍为空才记 missing（该帧「没有可信上屏时刻」，契约允许）。
//!
//! 输出：提交帧数、已结算帧数、presented 有值率、`publish → presented` 的 p50/p99（ms 与
//! vsync 倍数）、验收判定（有值率 ≥99% 且 p99 ≤2 vsync）。退出码 0 = PASS。
//!
//! 合成器没有 `wp_presentation` 时：stderr 会有一次
//! `kytrix: compositor has no wp_presentation; presented_at stays None ...`，本程序输出
//! 有值率 0% 并 FAIL —— 这属于「协议不可用豁免」，按 A4 契约如实登记（不得伪造）。

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use ui_core::{
    app::{App, Application},
    math::{Rect, Scale, Size},
    node::builder::NodeBuilder,
    paint::{
        color::Color,
        style::{PaintSpec, StylePatch},
    },
    window::{FrameOutcome, FramePresentation, WindowId, WindowOptions},
};

/// 窗口逻辑尺寸。
const WINDOW_SIZE: Size = Size::new(960.0, 540.0);
/// 预热时长（让窗口映射 / 首帧 / 呈现历史稳定）。
const WARMUP: Duration = Duration::from_secs(1);
/// 每帧等待 presented 证据的宽限（异步事件：正常 1~2 轮 ≈ 16~33 ms；给足 250 ms 余量）。
const SETTLE_GRACE: Duration = Duration::from_millis(250);
/// 最终结算的额外等待（测量结束后再跑这么久，把尾部帧的呈现事件收干净）。
const DRAIN: Duration = Duration::from_millis(300);
/// 默认测量时长与数据泵频率。
const DEFAULT_SECONDS: f64 = 20.0;
const DEFAULT_HZ: f64 = 60.0;

/// 运行参数。
struct Config {
    seconds: f64,
    hz: f64,
}

impl Config {
    fn parse() -> Self {
        let mut config = Self {
            seconds: DEFAULT_SECONDS,
            hz: DEFAULT_HZ,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--seconds" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.seconds = value;
                    }
                }
                "--hz" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.hz = value;
                    }
                }
                other => eprintln!("忽略未知参数：{other}"),
            }
        }
        config
    }
}

/// 测量状态（帧钩子独占；数据泵线程只持有 `AppHandle` 与停止标志）。
struct Measure {
    measure_at: Instant,
    deadline: Instant,
    drain_end: Instant,
    /// 已观测到的最大帧序号（历史按提交顺序追加，单调）。
    seen: Option<u32>,
    /// 已观测到、还没结算的帧快照。
    ///
    /// 为什么复制而不是每轮重读历史：呈现历史只保留最近 8 条，而 presented 的宽限可能更长；
    /// 帧被历史挤出后仍要按宽限结算为 missing（否则有值率会被「漏计 missing」抬高）。
    pending: Vec<FramePresentation>,
    /// 已尝试的帧数（颜色动画相位；每次钩子都推进，保证每帧都有真实像素变化）。
    ticks: u64,
    /// 测量窗口内提交的帧数。
    commits: u64,
    /// 测量窗口内已结算的帧数（presented 有值 + 宽限到点仍为空 / 已被历史挤出）。
    settled: u64,
    /// 已结算且有可信上屏时刻的帧数（有值率分子）。
    presented: u64,
    /// `publish → presented` 样本（成功结算的帧）。
    e2e: Vec<Duration>,
}

impl Measure {
    fn new(now: Instant, seconds: f64) -> Self {
        let measure_at = now + WARMUP;
        let deadline = measure_at + Duration::from_secs_f64(seconds.max(0.1));
        Self {
            measure_at,
            deadline,
            drain_end: deadline + DRAIN,
            seen: None,
            pending: Vec::with_capacity(64),
            ticks: 0,
            commits: 0,
            settled: 0,
            presented: 0,
            e2e: Vec::with_capacity(2048),
        }
    }

    /// 异步结算：把「已观测到、且已拿到 presented 或已过宽限」的帧记入统计。
    ///
    /// 两段式：① 把历史里的新帧并入 [`Self::pending`]（老帧的快照用历史里的新值刷新，
    /// presented 到达后历史条目会原地回填）；② 对 pending 逐帧结算 —— 有 presented ⇒ 记
    /// 有值样本；否则等宽限（[`SETTLE_GRACE`]）到点，或该帧已被 8 条历史上限挤出（此后再
    /// 回填也不会生效）⇒ 记 missing。`pending` 有界（≈ 宽限 × 帧率）。
    fn settle(&mut self, app: &App, window: WindowId, now: Instant) {
        let Some(target) = app.window(window) else {
            return;
        };
        for presentation in target.recent_frame_presentations() {
            let index = presentation.index.get();
            if self.seen.is_none_or(|seen| index > seen) {
                self.seen = Some(index);
                if presentation.published_at >= self.measure_at {
                    self.pending.push(presentation);
                }
                continue;
            }
            if let Some(slot) = self
                .pending
                .iter_mut()
                .find(|pending| pending.index == presentation.index)
            {
                *slot = presentation;
            }
        }
        let mut position = 0;
        while position < self.pending.len() {
            let presentation = self.pending[position];
            if let Some(e2e) = presentation.publish_to_presented() {
                self.settled += 1;
                self.presented += 1;
                self.e2e.push(e2e);
                self.pending.remove(position);
                continue;
            }
            let age = now.saturating_duration_since(presentation.published_at);
            let evicted = !target
                .recent_frame_presentations()
                .any(|recent| recent.index == presentation.index);
            if age >= SETTLE_GRACE || evicted {
                self.settled += 1;
                self.pending.remove(position);
                continue;
            }
            position += 1;
        }
    }

    /// 报告并返回是否 PASS（有值率 ≥99% 且 p99 ≤2 vsync）。
    fn report(&self, refresh_hz: Option<f32>) -> bool {
        let refresh = refresh_hz.filter(|hz| *hz > 0.0).unwrap_or(60.0);
        let vsync = Duration::from_secs_f64(1.0 / f64::from(refresh));
        let rate = if self.settled == 0 {
            0.0
        } else {
            self.presented as f64 / self.settled as f64
        };
        let mut sorted = self.e2e.clone();
        sorted.sort_unstable();
        let percentile = |p: f64| -> Option<Duration> {
            if sorted.is_empty() {
                return None;
            }
            let position = ((sorted.len() - 1) as f64 * p).round() as usize;
            Some(sorted[position])
        };
        let vsyncs = |duration: Duration| duration.as_secs_f64() / vsync.as_secs_f64();

        println!();
        println!("-- A4 呈现证据（publish → presented；presented 是真实上屏时刻的下界） --");
        println!(
            "刷新率提示 {} Hz（{}）",
            refresh,
            if refresh_hz.is_some() {
                "来自平台（wl_output.mode）"
            } else {
                "平台未提供，按 60 Hz 计 vsync 倍数"
            }
        );
        println!(
            "提交帧 {} / 已结算帧 {}（测量 {} s + 宽限 {:.0} ms）",
            self.commits,
            self.settled,
            (self.deadline - self.measure_at).as_secs_f64(),
            SETTLE_GRACE.as_millis()
        );
        match (percentile(0.50), percentile(0.99)) {
            (Some(p50), Some(p99)) => {
                println!(
                    "publish -> presented   n={}  p50={:.3}ms ({:.2} vsync)  p99={:.3}ms ({:.2} vsync)  max={:.3}ms",
                    sorted.len(),
                    p50.as_secs_f64() * 1e3,
                    vsyncs(p50),
                    p99.as_secs_f64() * 1e3,
                    vsyncs(p99),
                    sorted[sorted.len() - 1].as_secs_f64() * 1e3,
                );
            }
            _ => println!("publish -> presented   (no samples：presented_at 恒为 None)"),
        }
        let rate_ok = self.settled > 0 && rate >= 0.99;
        let p99_ok = percentile(0.99).is_none_or(|p99| vsyncs(p99) <= 2.0);
        println!(
            "presented 有值率 {:.2}%（{} / {} 已结算帧）⇒ {}",
            rate * 100.0,
            self.presented,
            self.settled,
            if rate_ok { "PASS ≥99%" } else { "FAIL <99%" }
        );
        println!(
            "e2e p99 ≤2 vsync ⇒ {}",
            if p99_ok {
                "PASS".to_owned()
            } else {
                format!(
                    "FAIL（{:.2} vsync）",
                    percentile(0.99).map_or(f64::NAN, vsyncs)
                )
            }
        );
        if !rate_ok {
            eprintln!(
                "提示：若本程序 stderr 出现过 `kytrix: compositor has no wp_presentation`，\n\
                 则是「协议不可用豁免」（presented_at 如实为 None，不伪造）；\n\
                 否则检查 WAYLAND_DEBUG 里 wp_presentation_feedback.presented 是否到达。"
            );
        }
        rate_ok && p99_ok
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut app = App::new(runtime.handle().clone());
    let window = app.create_window(WindowOptions {
        title: smol_str::SmolStr::new_static("kytrix A4 presented_at evidence"),
        size: WINDOW_SIZE,
        scale: Scale::ONE,
        background: Color::from_rgba8([0x0B, 0x0E, 0x13, 0xFF]),
        ..WindowOptions::default()
    });
    // 场景：一个铺满窗口、颜色随帧变化的方块。每帧写一次信号 ⇒ 有真实像素变化 ⇒ 真的
    // commit（否则 A5 的「没有 GPU 工作 ⇒ Idle」会让测量没有样本）。
    let root = app
        .window(window)
        .map(|target| target.tree().root())
        .ok_or("窗口不存在")?;
    let phase = app.signal(0u32);
    let _block = NodeBuilder::boxed()
        .layout_override(Rect::new(0.0, 0.0, WINDOW_SIZE.width, WINDOW_SIZE.height))
        .paint(PaintSpec {
            background: Color::from_rgba8(phase_color(0)),
            ..PaintSpec::default()
        })
        .bind_style(move |cx| StylePatch {
            background: Some(Color::from_rgba8(phase_color(*cx.read(&phase)))),
            ..StylePatch::default()
        })
        .build(
            app.window_mut(window)
                .map(|target| target.tree_mut())
                .ok_or("窗口不存在")?,
            root,
        )?;

    let measure = Rc::new(RefCell::new(None::<Measure>));
    let passed = Rc::new(Cell::new(false));
    let finished = Rc::new(Cell::new(false));

    // 数据泵：按 `--hz` 唤醒平台泵（App::frame 在帧钩子里提交；引擎本身空闲零唤醒）。
    let handle = app.handle();
    let stop = Arc::new(AtomicBool::new(false));
    let pump_stop = Arc::clone(&stop);
    let pump_hz = config.hz.max(1.0);
    let pump = thread::spawn(move || {
        let interval = Duration::from_secs_f64(1.0 / pump_hz);
        while !pump_stop.load(Ordering::Acquire) {
            thread::sleep(interval);
            handle.wake();
        }
    });

    let hook_measure = Rc::clone(&measure);
    let hook_passed = Rc::clone(&passed);
    let hook_finished = Rc::clone(&finished);
    let hook_handle = app.handle();
    let hook_window = window;
    let measurement_seconds = config.seconds;
    app.on_frame(move |app| {
        if hook_finished.get() {
            return;
        }
        let now = Instant::now();
        let mut slot = hook_measure.borrow_mut();
        let measure = slot.get_or_insert_with(|| Measure::new(now, measurement_seconds));
        measure.settle(app, hook_window, now);
        if now >= measure.drain_end {
            let refresh = app.frame_refresh_hz(hook_window);
            hook_passed.set(measure.report(refresh));
            hook_finished.set(true);
            app.close_window(hook_window);
            hook_handle.wake();
            return;
        }
        if now >= measure.deadline {
            // 测量窗口结束：停止提交，继续泵到 drain_end 把尾部呈现事件收干净。
            return;
        }
        // 动画：每帧换一个颜色（保证本帧有像素变化 ⇒ 有真实 commit）。
        measure.ticks += 1;
        let next = measure.ticks as u32;
        app.write(&phase, next);
        if let Some(Ok(FrameOutcome::Committed(_))) = app.frame(hook_window)
            && now >= measure.measure_at
        {
            measure.commits += 1;
        }
    });
    println!(
        "== A4 Linux presented_at 真机验收（wp_presentation → Window::note_frame_presented） =="
    );
    println!(
        "窗口 {}×{} 逻辑像素；数据泵 {} Hz；预热 {} s + 测量 {} s + 宽限 {} ms（异步结算口径）",
        WINDOW_SIZE.width,
        WINDOW_SIZE.height,
        config.hz,
        WARMUP.as_secs_f64(),
        config.seconds,
        SETTLE_GRACE.as_millis()
    );
    let result = Application::new().run(app);
    stop.store(true, Ordering::Release);
    let _ = pump.join();
    result?;
    if passed.get() {
        Ok(())
    } else {
        Err("A4 呈现证据未达标（presented 有值率 <99% 或 p99 >2 vsync）".into())
    }
}

/// 帧颜色（简单色相循环；只要求每帧不同 ⇒ 有真实的像素变化）。
fn phase_color(phase: u32) -> [u8; 4] {
    let step = (phase % 360) as u8;
    [
        0x20u8.saturating_add(step / 2),
        0x60u8.saturating_add(step / 3),
        0xB0u8.saturating_sub(step / 3),
        0xFF,
    ]
}

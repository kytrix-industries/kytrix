//! 引擎内置 Debug HUD（P17）：右上角非交互浮层 + 500 ms 指标汇总。
//!
//! 本模块只在 `hud` feature 下编译。公开入口只有 `App::set_debug_hud` /
//! `App::debug_hud_enabled`；节点、采样累计器与格式化都是 crate 私有。
//!
//! **纯观察者（P-C）**：HUD 不请求 damage、不参与 `needs_continuous_frames`、没有任何定时器或
//! 周期唤醒。文本只在**应用自己出帧**时刷新（`prepare_frame` 在帧入口采样、`apply_frame` 在
//! `refresh_styles` 之后写文本）；静止窗口不产生帧，HUD 保持上一次文本。
//!
//! 文本更新分两段：`prepare_frame` 采样并暂存 `pending_text`；`apply_frame` 在 `refresh_styles`
//! 之后、`refresh_text` 之前写文本并 `mark_paint`。这样不经过信号依赖图，也不会被样式 pass 的
//! `clear_paint` 吞掉。`pending_text` **只在提交成功后清**（`finish_frame`）：失败帧下一帧重试。
//!
//! **FPS / Frame 的口径**（P-C）：以 GPU 线程的完成反馈（`FrameFeedback`）为准 ——
//! `FPS = 采样窗口内真实完成的帧数 / 窗口秒数`、`Frame = 窗口毫秒 / 帧数`；窗口内一帧都没完成
//! （静止窗口）⇒ 显示 `--`。**不再**用「两次提交帧入口的间隔」冒充帧率：HUD 自己造帧时那条口径
//! 会退化成 2.0 FPS / 500 ms 的自指值。`CPU / GPU / Upload / Present` 是采样窗口内**真实提交帧**
//! 的平均成本；`Latency` 是发布 → GPU 完成；尺寸行是最近一次采样时的渲染目标（最多滞后一个
//! 采样间隔 + 一帧）。

use std::time::{Duration, Instant};

use smol_str::{SmolStr, format_smolstr};

use crate::{
    a11y::A11yOverride,
    error::EngineError,
    layout::hit_test::HitShape,
    math::Rect,
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        color::Color,
        primitive::Border,
        style::{PaintSpec, Style},
    },
    text::{TextStyle, TextWrap},
    window::{FrameIndex, Window},
};

/// HUD 面板逻辑宽度。
const WIDTH: f32 = 244.0;
/// HUD 面板逻辑高度。
const HEIGHT: f32 = 200.0;
/// 右上角边距。
const MARGIN: f32 = 12.0;
/// 文本左边距。
const TEXT_X: f32 = 10.0;
/// 文本顶边距。
const TEXT_Y: f32 = 8.0;
/// 固定行距。
const LINE_HEIGHT: f32 = 16.0;
/// 文本宽度。
const TEXT_WIDTH: f32 = 224.0;
/// 文本行数。
pub(crate) const LINES: usize = 11;
/// 指标汇总间隔。
const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

/// 初始占位行（安装帧之前显示；安装帧就会采样出第一份真实文本）。
const INITIAL_LINES: [&str; LINES] = [
    "FPS     --",
    "Frame   --",
    "CPU     --",
    "GPU     --",
    "Upload  --",
    "Present --",
    "Latency --",
    "Draw    --",
    "Prims   --",
    "Inst    --",
    "0x0 @ 0.00x",
];

/// 11 个文本节点的稳定调试 id。
const LINE_IDS: [&str; LINES] = [
    "debug-hud.line-0",
    "debug-hud.line-1",
    "debug-hud.line-2",
    "debug-hud.line-3",
    "debug-hud.line-4",
    "debug-hud.line-5",
    "debug-hud.line-6",
    "debug-hud.line-7",
    "debug-hud.line-8",
    "debug-hud.line-9",
    "debug-hud.line-10",
];

/// 窗口最近提交帧 + 完成帧计数的可观察指标（`Window::debug_hud_metrics` 的载荷）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct DebugHudMetrics {
    /// 最近提交的帧序号。
    pub frame_index: FrameIndex,
    /// GPU 线程**已完成**的帧数（单调累计；采样窗口用它做差得到帧率分母）。
    pub completed_frames: u64,
    /// 从 `begin_frame` 到 `paint_frame` 结束的 CPU 时间。
    pub cpu_time: Option<Duration>,
    /// 后端的 Upload CPU 时间。
    pub upload_time: Option<Duration>,
    /// 后端 GPU 时间（不可用时 `None`）。
    pub gpu_time: Option<Duration>,
    /// 后端的 Present CPU 时间。
    pub present_time: Option<Duration>,
    /// UI 发布 → 本帧反馈给出的延迟（`FrameFeedback::latency`；窗口路径只有落后 ≤1 帧的值）。
    ///
    /// **三端同义**：都是「发布 → **GPU 工作完成**」（`FrameFeedback::completed_at`），不是上屏
    /// 时刻。真实上屏时刻由 `FrameFeedback::presented_at` 单独承载（macOS 上是 display link 的
    /// tick 推断值；Windows / Linux 没有可信来源 ⇒ 恒为 `None`），本 HUD 不显示它。
    pub latency: Option<Duration>,
    /// 实际发出的 draw call 数。
    pub draw_calls: u32,
    /// 实例缓冲条目数。
    pub instances: u32,
    /// 绘制序中的图元数。
    pub primitives: usize,
    /// 最近一次采样时的物理宽度（`window.surface`；最多滞后一个采样间隔 + 一帧）。
    pub physical_width: u32,
    /// 最近一次采样时的物理高度（同上）。
    pub physical_height: u32,
    /// DPI 比例。
    pub scale: f32,
}

/// HUD 的每窗口运行状态。
pub(crate) struct DebugHudState {
    root: NodeId,
    lines: [NodeId; LINES],
    /// 当前采样窗口的起点。
    last_sample_at: Instant,
    /// 采样窗口起点时已完成的帧数（完成计数的基线）。
    last_sample_completed: u64,
    last_seen_frame: FrameIndex,
    cpu_sum: Duration,
    cpu_count: u32,
    upload_sum: Duration,
    upload_count: u32,
    gpu_sum: Duration,
    gpu_count: u32,
    present_sum: Duration,
    present_count: u32,
    latency_sum: Duration,
    latency_count: u32,
    last_draw_calls: u32,
    last_primitives: usize,
    last_instances: u32,
    pending_text: Option<[SmolStr; LINES]>,
}

impl DebugHudState {
    /// HUD 根节点（关闭时移除整棵子树）。
    pub(crate) fn root(&self) -> NodeId {
        self.root
    }
}

/// 格式化用的纯值快照。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Snapshot {
    fps: Option<f32>,
    frame_ms: Option<f32>,
    cpu_ms: Option<f32>,
    gpu_ms: Option<f32>,
    upload_ms: Option<f32>,
    present_ms: Option<f32>,
    latency_ms: Option<f32>,
    draw_calls: u32,
    primitives: usize,
    instances: u32,
    physical_width: u32,
    physical_height: u32,
    scale: f32,
}

/// 在 root 末尾建立 HUD 子树；失败时回滚已挂载的节点。
///
/// # Errors
/// `StaleNode`（root/父节点被破坏）或任何 `NodeBuilder` 建树错误。
pub(crate) fn install(window: &mut Window, now: Instant) -> Result<DebugHudState, EngineError> {
    window.reset_debug_hud_timing();
    let root = window.tree().root();
    let frame_index = window.frame_index();
    let tree = window.tree_mut();
    let panel = NodeBuilder::boxed()
        .style(panel_style())
        .paint(panel_paint())
        .hit_shape(HitShape::custom(|_| false))
        .a11y(hidden())
        .id("debug-hud")
        .build(tree, root)?;

    let style = text_style();
    let initial = initial_lines();
    let mut lines = [panel; LINES];
    for index in 0..LINES {
        let built = NodeBuilder::text(initial[index].clone(), style.clone())
            .layout_override(line_rect(index))
            .hit_shape(HitShape::custom(|_| false))
            .a11y(hidden())
            .id(LINE_IDS[index])
            .build(tree, panel);
        match built {
            Ok(node) => lines[index] = node,
            Err(error) => {
                let _ = tree.remove(panel);
                return Err(error);
            }
        }
    }

    Ok(DebugHudState {
        root: panel,
        lines,
        // 预置一个采样间隔：安装帧（`set_debug_hud` 请求的那一帧）就会产出第一份真实文本，
        // 而不是让静止窗口永远停在 `--` 占位（HUD 自己不再造帧，没有第二次机会）。
        last_sample_at: now.checked_sub(SAMPLE_INTERVAL).unwrap_or(now),
        last_sample_completed: 0,
        last_seen_frame: frame_index,
        cpu_sum: Duration::ZERO,
        cpu_count: 0,
        upload_sum: Duration::ZERO,
        upload_count: 0,
        gpu_sum: Duration::ZERO,
        gpu_count: 0,
        present_sum: Duration::ZERO,
        present_count: 0,
        latency_sum: Duration::ZERO,
        latency_count: 0,
        last_draw_calls: 0,
        last_primitives: 0,
        last_instances: 0,
        pending_text: None,
    })
}

/// 帧入口的采样：消费最近提交帧的指标，每 500 ms 生成一份新文本（**不请求 damage**）。
///
/// **HUD 不产生帧**（P-C）：本函数不写任何脏位、不请求 damage，只是应用这一帧的观察者。没有
/// 应用帧时它不会被调用，静止窗口就保持上一次文本 —— 这既是「HUD 不能把自己变成帧源」的
/// 要求，也是帧率必须是真实完成帧的原因。
///
/// 逐帧成本与绘制计数只在 `frame_index` 前进（真的有新提交帧）时累加；`FPS` / `Frame` 用
/// 「采样窗口内**完成**的帧数 / 窗口时长」，一帧都没完成时是 `--`（见 [`rate`]）。
pub(crate) fn prepare_frame(state: &mut DebugHudState, window: &Window, now: Instant) {
    let metrics = window.debug_hud_metrics();
    if metrics.frame_index != state.last_seen_frame {
        state.last_seen_frame = metrics.frame_index;
        if let Some(cpu) = metrics.cpu_time {
            state.cpu_sum += cpu;
            state.cpu_count = state.cpu_count.saturating_add(1);
        }
        if let Some(upload) = metrics.upload_time {
            state.upload_sum += upload;
            state.upload_count = state.upload_count.saturating_add(1);
        }
        if let Some(gpu) = metrics.gpu_time {
            state.gpu_sum += gpu;
            state.gpu_count = state.gpu_count.saturating_add(1);
        }
        if let Some(present) = metrics.present_time {
            state.present_sum += present;
            state.present_count = state.present_count.saturating_add(1);
        }
        if let Some(latency) = metrics.latency {
            state.latency_sum += latency;
            state.latency_count = state.latency_count.saturating_add(1);
        }
        state.last_draw_calls = metrics.draw_calls;
        state.last_primitives = metrics.primitives;
        state.last_instances = metrics.instances;
    }

    let window_time = now.saturating_duration_since(state.last_sample_at);
    if window_time >= SAMPLE_INTERVAL {
        let completed = metrics
            .completed_frames
            .saturating_sub(state.last_sample_completed);
        let (fps, frame_ms) = rate(window_time, completed);
        let snapshot = Snapshot {
            fps,
            frame_ms,
            cpu_ms: average_ms(state.cpu_sum, state.cpu_count),
            gpu_ms: average_ms(state.gpu_sum, state.gpu_count),
            upload_ms: average_ms(state.upload_sum, state.upload_count),
            present_ms: average_ms(state.present_sum, state.present_count),
            latency_ms: average_ms(state.latency_sum, state.latency_count),
            draw_calls: state.last_draw_calls,
            primitives: state.last_primitives,
            instances: state.last_instances,
            physical_width: metrics.physical_width,
            physical_height: metrics.physical_height,
            scale: metrics.scale,
        };
        state.pending_text = Some(format_lines(&snapshot));
        state.last_sample_at = now;
        state.last_sample_completed = metrics.completed_frames;
        state.cpu_sum = Duration::ZERO;
        state.cpu_count = 0;
        state.upload_sum = Duration::ZERO;
        state.upload_count = 0;
        state.gpu_sum = Duration::ZERO;
        state.gpu_count = 0;
        state.present_sum = Duration::ZERO;
        state.present_count = 0;
        state.latency_sum = Duration::ZERO;
        state.latency_count = 0;
    }
}

/// `refresh_styles` 之后、`refresh_text` 之前：写文本并标 PAINT。
///
/// `pending_text` **不在这里清**：只有提交成功后才由 [`finish_frame`] 清除。若本帧在
/// `refresh_text`/`layout_text`/paint 失败，下一帧会重新写同一文本并重新标 PAINT，
/// 避免无绑定节点被下一次 `refresh_styles` 的 ghost 分支吞掉。
pub(crate) fn apply_frame(state: &mut DebugHudState, window: &mut Window) {
    let Some(lines) = state.pending_text.as_ref() else {
        return;
    };
    let tree = window.tree_mut();
    for (node, text) in state.lines.iter().zip(lines.iter()) {
        let Some(mut content) = tree.text_content(*node).cloned() else {
            continue;
        };
        content.text = text.clone();
        tree.set_text_content(*node, content);
        tree.mark_paint(*node);
    }
}

/// 一帧提交成功：本次 pending 文本已经落进 paint，清空避免下一帧重复重建。
pub(crate) fn finish_frame(state: &mut DebugHudState) {
    state.pending_text = None;
}

fn panel_style() -> Style {
    use taffy::{
        geometry::{Rect as TaffyRect, Size as TaffySize},
        style::{Dimension, LengthPercentageAuto, Position},
    };

    let mut style = Style::default();
    style.inner.position = Position::Absolute;
    style.inner.inset = TaffyRect {
        left: LengthPercentageAuto::auto(),
        right: LengthPercentageAuto::length(MARGIN),
        top: LengthPercentageAuto::length(MARGIN),
        bottom: LengthPercentageAuto::auto(),
    };
    style.inner.size = TaffySize {
        width: Dimension::length(WIDTH),
        height: Dimension::length(HEIGHT),
    };
    style
}

fn panel_paint() -> PaintSpec {
    PaintSpec {
        background: Color::from_rgba8([10, 14, 20, 220]),
        border: Some(Border {
            width: 1.0,
            color: Color::from_rgba8([55, 70, 88, 255]),
        }),
        corner_radius: [8.0; 4],
        ..PaintSpec::default()
    }
}

fn text_style() -> TextStyle {
    TextStyle::new(11.5)
        .with_line_height(1.3)
        .with_color(Color::from_rgba8([220, 226, 235, 255]))
        .with_wrap(TextWrap::NoWrap)
}

fn line_rect(index: usize) -> Rect {
    Rect::new(
        TEXT_X,
        TEXT_Y + index as f32 * LINE_HEIGHT,
        TEXT_WIDTH,
        LINE_HEIGHT,
    )
}

fn hidden() -> A11yOverride {
    A11yOverride {
        hidden: true,
        ..A11yOverride::default()
    }
}

fn initial_lines() -> [SmolStr; LINES] {
    std::array::from_fn(|index| SmolStr::new_static(INITIAL_LINES[index]))
}

fn format_lines(snapshot: &Snapshot) -> [SmolStr; LINES] {
    [
        format_smolstr!("FPS     {}", fps_text(snapshot.fps)),
        format_smolstr!("Frame   {}", ms_text(snapshot.frame_ms)),
        format_smolstr!("CPU     {}", ms_text(snapshot.cpu_ms)),
        format_smolstr!("GPU     {}", ms_text(snapshot.gpu_ms)),
        format_smolstr!("Upload  {}", ms_text(snapshot.upload_ms)),
        format_smolstr!("Present {}", ms_text(snapshot.present_ms)),
        format_smolstr!("Latency {}", ms_text(snapshot.latency_ms)),
        format_smolstr!("Draw    {}", count_text(snapshot.draw_calls)),
        format_smolstr!("Prims   {}", count_text(snapshot.primitives)),
        format_smolstr!("Inst    {}", count_text(snapshot.instances)),
        format_smolstr!(
            "{}x{} @ {:.2}x {}",
            snapshot.physical_width,
            snapshot.physical_height,
            snapshot.scale,
            backend_name()
        ),
    ]
}

fn fps_text(value: Option<f32>) -> SmolStr {
    match value {
        Some(value) if value.is_finite() => format_smolstr!("{value:.1}"),
        _ => SmolStr::new_static("--"),
    }
}

fn ms_text(value: Option<f32>) -> SmolStr {
    match value {
        Some(value) if value.is_finite() => format_smolstr!("{value:.2} ms"),
        _ => SmolStr::new_static("--"),
    }
}

fn count_text(value: impl std::fmt::Display) -> SmolStr {
    format_smolstr!("{value}")
}

/// 采样窗口的上屏帧率：`(FPS, 平均帧时长毫秒)`。
///
/// 帧数是窗口内通过 `FrameFeedback` **真实完成**的帧数（不是提交帧数，更不是 HUD 自己的采样
/// 次数）；一帧都没完成（静止窗口 / 没有渲染器）返回 `(None, None)` ⇒ 文本显示 `--`。
fn rate(window: Duration, completed: u64) -> (Option<f32>, Option<f32>) {
    if completed == 0 || window.is_zero() {
        return (None, None);
    }
    let seconds = window.as_secs_f64();
    let fps = (completed as f64 / seconds) as f32;
    let frame_ms = (seconds * 1000.0 / completed as f64) as f32;
    (Some(fps), Some(frame_ms))
}

fn average_ms(sum: Duration, count: u32) -> Option<f32> {
    (count > 0).then(|| (sum.as_secs_f64() * 1000.0 / f64::from(count)) as f32)
}

const fn backend_name() -> &'static str {
    #[cfg(windows)]
    {
        "D3D12"
    }
    #[cfg(target_os = "macos")]
    {
        "Metal"
    }
    #[cfg(target_os = "linux")]
    {
        "Vulkan"
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        HEIGHT, LINES, MARGIN, Snapshot, WIDTH, average_ms, backend_name, format_lines,
        initial_lines, panel_style, rate,
    };

    /// 面板是 root 的绝对定位子节点、right/top 双向锚定 12 px、固定 244×200：窗口变大时它跟着
    /// 右上角走（布局每次 resize 都会重解析 inset），不需要 HUD 自己算 damage 矩形。
    #[test]
    fn the_panel_style_anchors_it_to_the_top_right() {
        use taffy::style::{Dimension, LengthPercentageAuto, Position};

        let style = panel_style();
        assert_eq!(style.inner.position, Position::Absolute);
        assert_eq!(style.inner.inset.left, LengthPercentageAuto::auto());
        assert_eq!(
            style.inner.inset.right,
            LengthPercentageAuto::length(MARGIN)
        );
        assert_eq!(style.inner.inset.top, LengthPercentageAuto::length(MARGIN));
        assert_eq!(style.inner.inset.bottom, LengthPercentageAuto::auto());
        assert_eq!(style.inner.size.width, Dimension::length(WIDTH));
        assert_eq!(style.inner.size.height, Dimension::length(HEIGHT));
    }

    #[test]
    fn initial_lines_are_eleven_placeholders_without_fake_zeroes() {
        let lines = initial_lines();
        assert_eq!(lines.len(), LINES);
        assert_eq!(lines[0].as_str(), "FPS     --");
        assert_eq!(lines[6].as_str(), "Latency --");
        assert_eq!(lines[7].as_str(), "Draw    --");
    }

    #[test]
    fn formatting_covers_all_metrics_and_unknown_values() {
        let lines = format_lines(&Snapshot {
            fps: Some(60.0),
            frame_ms: Some(16.666),
            cpu_ms: Some(0.42),
            gpu_ms: Some(1.1),
            upload_ms: Some(0.08),
            present_ms: Some(0.3),
            latency_ms: Some(4.5),
            draw_calls: 12,
            primitives: 48,
            instances: 52,
            physical_width: 960,
            physical_height: 740,
            scale: 1.25,
        });
        assert_eq!(lines[0].as_str(), "FPS     60.0");
        assert_eq!(lines[1].as_str(), "Frame   16.67 ms");
        assert_eq!(lines[2].as_str(), "CPU     0.42 ms");
        assert_eq!(lines[3].as_str(), "GPU     1.10 ms");
        assert_eq!(lines[4].as_str(), "Upload  0.08 ms");
        assert_eq!(lines[5].as_str(), "Present 0.30 ms");
        assert_eq!(lines[6].as_str(), "Latency 4.50 ms");
        assert_eq!(lines[7].as_str(), "Draw    12");
        assert_eq!(lines[8].as_str(), "Prims   48");
        assert_eq!(lines[9].as_str(), "Inst    52");
        assert!(lines[10].starts_with("960x740 @ 1.25x "));
        assert!(lines[10].ends_with(backend_name()));

        let unknown = format_lines(&Snapshot {
            physical_width: 800,
            physical_height: 600,
            scale: 1.0,
            ..Snapshot::default()
        });
        assert_eq!(unknown[0].as_str(), "FPS     --");
        assert_eq!(unknown[3].as_str(), "GPU     --");
        assert_eq!(unknown[6].as_str(), "Latency --", "缺失值不能用 0 冒充");
        assert_eq!(unknown[8].as_str(), "Prims   0");
        assert!(unknown[10].starts_with("800x600 @ 1.00x "));
    }

    #[test]
    fn average_helpers_do_not_divide_by_zero() {
        assert_eq!(average_ms(Duration::ZERO, 0), None);
        let ms = average_ms(Duration::from_millis(500), 30).expect("ms");
        assert!((ms - 16.666_7).abs() < 0.01);
    }

    /// P-C 的核心口径：帧率 = 窗口内**真实完成**的帧数 / 窗口时长；零帧必须是 `None`。
    #[test]
    fn the_frame_rate_is_completed_frames_over_the_sample_window() {
        assert_eq!(
            rate(Duration::from_millis(500), 0),
            (None, None),
            "窗口内没有完成帧 ⇒ --（静止窗口 / 无渲染器）"
        );
        assert_eq!(rate(Duration::ZERO, 30), (None, None), "零长窗口不能除零");
        let (fps, frame_ms) = rate(Duration::from_millis(500), 30);
        let fps = fps.expect("fps");
        let frame_ms = frame_ms.expect("frame ms");
        assert!((fps - 60.0).abs() < 0.01, "{fps}");
        assert!((frame_ms - 16.666_7).abs() < 0.01, "{frame_ms}");

        // 120 Hz：500 ms 内完成 60 帧。
        let (fps, frame_ms) = rate(Duration::from_millis(500), 60);
        assert!((fps.expect("fps") - 120.0).abs() < 0.01);
        assert!((frame_ms.expect("frame ms") - 8.333_3).abs() < 0.01);
    }
}

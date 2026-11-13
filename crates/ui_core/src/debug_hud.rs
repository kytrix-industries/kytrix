//! 引擎内置 Debug HUD（P17）：右上角非交互浮层 + 500 ms 指标汇总。
//!
//! 本模块只在 `hud` feature 下编译。公开入口只有 `App::set_debug_hud` /
//! `App::debug_hud_enabled`；节点、采样累计器与格式化都是 crate 私有。
//!
//! 文本更新分两段：`prepare_frame` 在帧门禁前采样并请求 HUD damage；
//! `apply_frame` 在 `refresh_styles` 之后、`refresh_text` 之前写文本并 `mark_paint`。
//! 这样不会经过信号依赖图，也不会被样式 pass 的 `clear_paint` 吞掉。

use std::time::{Duration, Instant};

use smol_str::{SmolStr, format_smolstr};

use crate::{
    a11y::A11yOverride,
    error::EngineError,
    math::{Rect, Size},
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
const HEIGHT: f32 = 184.0;
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
pub(crate) const LINES: usize = 10;
/// 指标汇总间隔。
const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

/// 初始占位行（首 500 ms 显示；不伪造 0 值）。
const INITIAL_LINES: [&str; LINES] = [
    "FPS     --",
    "Frame   --",
    "CPU     --",
    "GPU     --",
    "Upload  --",
    "Present --",
    "Draw    --",
    "Prims   --",
    "Inst    --",
    "0x0 @ 0.00x",
];

/// 10 个文本节点的稳定调试 id。
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
];

/// 窗口最后提交帧的可观察指标（`Window::debug_hud_metrics` 的载荷）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct DebugHudMetrics {
    /// 最近提交的帧序号。
    pub frame_index: FrameIndex,
    /// 与上一提交帧的间隔（首帧为 `None`）。
    pub frame_interval: Option<Duration>,
    /// 从 `begin_frame` 到 `paint_frame` 结束的 CPU 时间。
    pub cpu_time: Option<Duration>,
    /// 后端的 Upload CPU 时间。
    pub upload_time: Option<Duration>,
    /// 后端 GPU 时间（不可用时 `None`）。
    pub gpu_time: Option<Duration>,
    /// 后端的 Present CPU 时间。
    pub present_time: Option<Duration>,
    /// 实际发出的 draw call 数。
    pub draw_calls: u32,
    /// 实例缓冲条目数。
    pub instances: u32,
    /// 绘制序中的图元数。
    pub primitives: usize,
    /// 物理宽度。
    pub physical_width: u32,
    /// 物理高度。
    pub physical_height: u32,
    /// DPI 比例。
    pub scale: f32,
}

/// HUD 的每窗口运行状态。
pub(crate) struct DebugHudState {
    root: NodeId,
    lines: [NodeId; LINES],
    last_sample_at: Instant,
    last_seen_frame: FrameIndex,
    interval_sum: Duration,
    interval_count: u32,
    cpu_sum: Duration,
    cpu_count: u32,
    upload_sum: Duration,
    upload_count: u32,
    gpu_sum: Duration,
    gpu_count: u32,
    present_sum: Duration,
    present_count: u32,
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
        .hit_shape(|_| false)
        .a11y(hidden())
        .id("debug-hud")
        .build(tree, root)?;

    let style = text_style();
    let initial = initial_lines();
    let mut lines = [panel; LINES];
    for index in 0..LINES {
        let built = NodeBuilder::text(initial[index].clone(), style.clone())
            .layout_override(line_rect(index))
            .hit_shape(|_| false)
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
        last_sample_at: now,
        last_seen_frame: frame_index,
        interval_sum: Duration::ZERO,
        interval_count: 0,
        cpu_sum: Duration::ZERO,
        cpu_count: 0,
        upload_sum: Duration::ZERO,
        upload_count: 0,
        gpu_sum: Duration::ZERO,
        gpu_count: 0,
        present_sum: Duration::ZERO,
        present_count: 0,
        last_draw_calls: 0,
        last_primitives: 0,
        last_instances: 0,
        pending_text: None,
    })
}

/// 帧门禁前：消费上一提交帧的指标、每 500 ms 生成新文本，并请求 HUD damage。
pub(crate) fn prepare_frame(state: &mut DebugHudState, window: &mut Window, now: Instant) {
    let metrics = window.debug_hud_metrics();
    if metrics.frame_index != state.last_seen_frame {
        state.last_seen_frame = metrics.frame_index;
        if let Some(interval) = metrics.frame_interval.filter(|value| !value.is_zero()) {
            state.interval_sum += interval;
            state.interval_count = state.interval_count.saturating_add(1);
        }
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
        state.last_draw_calls = metrics.draw_calls;
        state.last_primitives = metrics.primitives;
        state.last_instances = metrics.instances;
    }

    if now.saturating_duration_since(state.last_sample_at) >= SAMPLE_INTERVAL {
        let snapshot = Snapshot {
            fps: average_rate(state.interval_sum, state.interval_count),
            frame_ms: average_ms(state.interval_sum, state.interval_count),
            cpu_ms: average_ms(state.cpu_sum, state.cpu_count),
            gpu_ms: average_ms(state.gpu_sum, state.gpu_count),
            upload_ms: average_ms(state.upload_sum, state.upload_count),
            present_ms: average_ms(state.present_sum, state.present_count),
            draw_calls: state.last_draw_calls,
            primitives: state.last_primitives,
            instances: state.last_instances,
            physical_width: metrics.physical_width,
            physical_height: metrics.physical_height,
            scale: metrics.scale,
        };
        state.pending_text = Some(format_lines(&snapshot));
        state.last_sample_at = now;
        state.interval_sum = Duration::ZERO;
        state.interval_count = 0;
        state.cpu_sum = Duration::ZERO;
        state.cpu_count = 0;
        state.upload_sum = Duration::ZERO;
        state.upload_count = 0;
        state.gpu_sum = Duration::ZERO;
        state.gpu_count = 0;
        state.present_sum = Duration::ZERO;
        state.present_count = 0;
    }

    window.request_damage(hud_rect(window.size()));
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

/// HUD 面板在窗口中的逻辑矩形（right/top 锚点；窄窗口夹到左边）。
#[must_use]
pub(crate) fn hud_rect(size: Size) -> Rect {
    let x = (size.width - WIDTH - MARGIN).max(0.0);
    Rect::new(x, MARGIN, WIDTH, HEIGHT)
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

fn average_rate(sum: Duration, count: u32) -> Option<f32> {
    (count > 0 && !sum.is_zero()).then(|| (f64::from(count) / sum.as_secs_f64()) as f32)
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
        LINES, Snapshot, average_ms, average_rate, backend_name, format_lines, hud_rect,
        initial_lines,
    };
    use crate::math::Size;

    #[test]
    fn hud_is_anchored_to_the_top_right() {
        let rect = hud_rect(Size::new(960.0, 740.0));
        assert_eq!(rect.origin.x, 960.0 - 244.0 - 12.0);
        assert_eq!(rect.origin.y, 12.0);
        assert_eq!(rect.size.width, 244.0);
        assert_eq!(rect.size.height, 184.0);
    }

    #[test]
    fn hud_rect_clamps_to_the_left_edge_on_narrow_windows() {
        let rect = hud_rect(Size::new(100.0, 80.0));
        assert_eq!(rect.origin.x, 0.0);
    }

    #[test]
    fn initial_lines_are_ten_placeholders_without_fake_zeroes() {
        let lines = initial_lines();
        assert_eq!(lines.len(), LINES);
        assert_eq!(lines[0].as_str(), "FPS     --");
        assert_eq!(lines[6].as_str(), "Draw    --");
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
        assert_eq!(lines[6].as_str(), "Draw    12");
        assert_eq!(lines[7].as_str(), "Prims   48");
        assert_eq!(lines[8].as_str(), "Inst    52");
        assert!(lines[9].starts_with("960x740 @ 1.25x "));
        assert!(lines[9].ends_with(backend_name()));

        let unknown = format_lines(&Snapshot {
            physical_width: 800,
            physical_height: 600,
            scale: 1.0,
            ..Snapshot::default()
        });
        assert_eq!(unknown[0].as_str(), "FPS     --");
        assert_eq!(unknown[3].as_str(), "GPU     --");
        assert_eq!(unknown[7].as_str(), "Prims   0");
        assert!(unknown[9].starts_with("800x600 @ 1.00x "));
    }

    #[test]
    fn average_helpers_do_not_divide_by_zero() {
        assert_eq!(average_rate(Duration::ZERO, 0), None);
        assert_eq!(average_ms(Duration::ZERO, 0), None);
        let rate = average_rate(Duration::from_millis(500), 30).expect("rate");
        assert!((rate - 60.0).abs() < 0.01);
        let ms = average_ms(Duration::from_millis(500), 30).expect("ms");
        assert!((ms - 16.666_7).abs() < 0.01);
    }
}

use std::time::{Duration, Instant};

use smol_str::{SmolStr, format_smolstr};

use crate::{
    a11y::A11yOverride,
    error::EngineError,
    layout::hit_test::HitShape,
    math::{LogicalPx, LogicalRect, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        color::Color,
        primitive::Border,
        style::{PaintSpec, Style},
        style_builder::StyleBuilder,
    },
    text::{TextStyle, TextWrap},
    window::{FrameIndex, Window},
};

const WIDTH: LogicalPx = px(244.0);
const HEIGHT: LogicalPx = px(200.0);
const MARGIN: LogicalPx = px(12.0);
const TEXT_X: LogicalPx = px(10.0);
const TEXT_Y: LogicalPx = px(8.0);
const LINE_HEIGHT: LogicalPx = px(16.0);
const TEXT_WIDTH: LogicalPx = px(224.0);
pub(crate) const LINES: usize = 11;
const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

const INITIAL_LINES: [&str; LINES - 1] = [
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
];

const PLACEHOLDER_SIZE_LINE: &str = "0x0 @ 0.00x";

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

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct DebugHudMetrics {
    pub frame_index: FrameIndex,
    pub completed_frames: u64,
    pub cpu_time: Option<Duration>,
    pub upload_time: Option<Duration>,
    pub gpu_time: Option<Duration>,
    pub present_time: Option<Duration>,
    pub latency: Option<Duration>,
    pub draw_calls: u32,
    pub instances: u32,
    pub primitives: usize,
    pub physical_width: u32,
    pub physical_height: u32,
    pub scale: f32,
}

pub(crate) struct DebugHudState {
    root: NodeId,
    lines: [NodeId; LINES],
    last_sample_at: Instant,
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
    shown_size: Option<(u32, u32, f32)>,
    pending_size_line: Option<SmolStr>,
    pending_text: Option<[SmolStr; LINES - 1]>,
}

impl DebugHudState {
    pub(crate) fn root(&self) -> NodeId {
        self.root
    }

    fn restart_sample_window(&mut self, now: Instant, completed_frames: u64) {
        self.last_sample_at = now;
        self.last_sample_completed = completed_frames;
        self.cpu_sum = Duration::ZERO;
        self.cpu_count = 0;
        self.upload_sum = Duration::ZERO;
        self.upload_count = 0;
        self.gpu_sum = Duration::ZERO;
        self.gpu_count = 0;
        self.present_sum = Duration::ZERO;
        self.present_count = 0;
        self.latency_sum = Duration::ZERO;
        self.latency_count = 0;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Snapshot {
    fps: Option<f32>,
    frame_ms: Option<f32>,
    cpu_ms: Option<f32>,
    gpu_ms: Option<f32>,
    upload_ms: Option<f32>,
    present_ms: Option<f32>,
    latency_ms: Option<f32>,
    draw_calls: Option<u32>,
    primitives: usize,
    instances: Option<u32>,
}

pub(crate) fn install(window: &mut Window, now: Instant) -> Result<DebugHudState, EngineError> {
    window.reset_debug_hud_timing();
    let root = window.tree().root();
    let frame_index = window.frame_index();
    let initial = initial_lines();
    let tree = window.tree_mut();
    let panel = NodeBuilder::boxed()
        .style(panel_style())
        .paint(panel_paint())
        .hit_shape(HitShape::custom(|_| false))
        .a11y(hidden())
        .id("debug-hud")
        .build(tree, root)?;

    let style = text_style();
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
        last_sample_at: now,
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
        shown_size: None,
        pending_size_line: None,
        pending_text: None,
    })
}

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

    let expected_size = (
        metrics.physical_width,
        metrics.physical_height,
        metrics.scale,
    );
    if state.shown_size != Some(expected_size) {
        state.shown_size = Some(expected_size);
        state.pending_size_line =
            Some(size_line(expected_size.0, expected_size.1, expected_size.2));
    }

    let window_time = now.saturating_duration_since(state.last_sample_at);
    if window_time >= SAMPLE_INTERVAL * 2 {
        state.restart_sample_window(now, metrics.completed_frames);
        return;
    }
    if window_time >= SAMPLE_INTERVAL {
        let completed = metrics
            .completed_frames
            .saturating_sub(state.last_sample_completed);
        let (fps, frame_ms) = rate(window_time, completed);
        let observed_completion = metrics.completed_frames > 0;
        let snapshot = Snapshot {
            fps,
            frame_ms,
            cpu_ms: average_ms(state.cpu_sum, state.cpu_count),
            gpu_ms: average_ms(state.gpu_sum, state.gpu_count),
            upload_ms: average_ms(state.upload_sum, state.upload_count),
            present_ms: average_ms(state.present_sum, state.present_count),
            latency_ms: average_ms(state.latency_sum, state.latency_count),
            draw_calls: observed_completion.then_some(state.last_draw_calls),
            primitives: state.last_primitives,
            instances: observed_completion.then_some(state.last_instances),
        };
        state.pending_text = Some(format_lines(&snapshot));
        state.restart_sample_window(now, metrics.completed_frames);
    }
}

pub(crate) fn apply_frame(state: &mut DebugHudState, window: &mut Window) {
    let tree = window.tree_mut();
    if let Some(size) = state.pending_size_line.as_ref()
        && let Some(mut content) = tree.text_content(state.lines[LINES - 1]).cloned()
    {
        content.text = size.clone();
        tree.set_text_content(state.lines[LINES - 1], content);
        tree.mark_paint(state.lines[LINES - 1]);
    }
    let Some(lines) = state.pending_text.as_ref() else {
        return;
    };
    for (node, text) in state.lines[..LINES - 1].iter().zip(lines.iter()) {
        let Some(mut content) = tree.text_content(*node).cloned() else {
            continue;
        };
        content.text = text.clone();
        tree.set_text_content(*node, content);
        tree.mark_paint(*node);
    }
}

pub(crate) fn finish_frame(state: &mut DebugHudState) {
    state.pending_text = None;
    state.pending_size_line = None;
}

fn panel_style() -> Style {
    StyleBuilder::new()
        .absolute()
        .right(MARGIN)
        .top(MARGIN)
        .size(WIDTH, HEIGHT)
        .build()
}

fn panel_paint() -> PaintSpec {
    PaintSpec {
        background: Color::from_rgba8([10, 14, 20, 220]),
        border: Some(Border {
            width: px(1.0),
            color: Color::from_rgba8([55, 70, 88, 255]),
        }),
        corner_radius: [px(8.0); 4],
        ..PaintSpec::default()
    }
}

fn text_style() -> TextStyle {
    TextStyle::new(px(11.5))
        .with_line_height(1.3)
        .with_color(Color::from_rgba8([220, 226, 235, 255]))
        .with_wrap(TextWrap::NoWrap)
}

fn line_rect(index: usize) -> LogicalRect {
    LogicalRect::new(
        TEXT_X,
        TEXT_Y + LINE_HEIGHT * (index as f32),
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
    let mut lines: [SmolStr; LINES] = std::array::from_fn(|index| {
        SmolStr::new_static(INITIAL_LINES.get(index).copied().unwrap_or(""))
    });
    lines[LINES - 1] = SmolStr::new_static(PLACEHOLDER_SIZE_LINE);
    lines
}

fn format_lines(snapshot: &Snapshot) -> [SmolStr; LINES - 1] {
    [
        format_smolstr!("FPS     {}", fps_text(snapshot.fps)),
        format_smolstr!("Frame   {}", ms_text(snapshot.frame_ms)),
        format_smolstr!("CPU     {}", ms_text(snapshot.cpu_ms)),
        format_smolstr!("GPU     {}", ms_text(snapshot.gpu_ms)),
        format_smolstr!("Upload  {}", ms_text(snapshot.upload_ms)),
        format_smolstr!("Present {}", ms_text(snapshot.present_ms)),
        format_smolstr!("Latency {}", ms_text(snapshot.latency_ms)),
        format_smolstr!("Draw    {}", optional_count_text(snapshot.draw_calls)),
        format_smolstr!("Prims   {}", count_text(snapshot.primitives)),
        format_smolstr!("Inst    {}", optional_count_text(snapshot.instances)),
    ]
}

fn size_line(physical_width: u32, physical_height: u32, scale: f32) -> SmolStr {
    format_smolstr!(
        "{physical_width}x{physical_height} @ {scale:.2}x {}",
        backend_name()
    )
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

fn optional_count_text(value: Option<u32>) -> SmolStr {
    match value {
        Some(value) => format_smolstr!("{value}"),
        None => SmolStr::new_static("--"),
    }
}

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
        HEIGHT, LINES, MARGIN, PLACEHOLDER_SIZE_LINE, Snapshot, WIDTH, average_ms, backend_name,
        format_lines, initial_lines, panel_style, rate, size_line,
    };

    #[test]
    fn the_panel_style_anchors_it_to_the_top_right() {
        use taffy::style::{Dimension, LengthPercentageAuto, Position};

        let style = panel_style();
        assert_eq!(style.inner().position, Position::Absolute);
        assert_eq!(style.inner().inset.left, LengthPercentageAuto::auto());
        assert_eq!(
            style.inner().inset.right,
            LengthPercentageAuto::length(MARGIN.value())
        );
        assert_eq!(
            style.inner().inset.top,
            LengthPercentageAuto::length(MARGIN.value())
        );
        assert_eq!(style.inner().inset.bottom, LengthPercentageAuto::auto());
        assert_eq!(style.inner().size.width, Dimension::length(WIDTH.value()));
        assert_eq!(style.inner().size.height, Dimension::length(HEIGHT.value()));
    }

    #[test]
    fn initial_lines_are_placeholders_until_the_install_frame_refreshes_the_size_line() {
        let lines = initial_lines();
        assert_eq!(lines.len(), LINES);
        for (index, line) in lines.iter().take(LINES - 1).enumerate() {
            assert!(
                line.as_str().ends_with("--"),
                "第 {index} 行必须是缺省 `--`：{line}"
            );
        }
        assert_eq!(lines[LINES - 1].as_str(), PLACEHOLDER_SIZE_LINE);
    }

    #[test]
    fn the_size_line_formats_the_real_target() {
        let expected = format!("1200x925 @ 1.25x {}", backend_name());
        assert_eq!(size_line(1200, 925, 1.25).as_str(), expected);
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
            draw_calls: Some(12),
            primitives: 48,
            instances: Some(52),
        });
        assert_eq!(lines.len(), LINES - 1, "采样文本不含尺寸行");
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

        let unknown = format_lines(&Snapshot::default());
        assert_eq!(unknown[0].as_str(), "FPS     --");
        assert_eq!(unknown[3].as_str(), "GPU     --");
        assert_eq!(unknown[6].as_str(), "Latency --", "缺失值不能用 0 冒充");
        assert_eq!(
            unknown[7].as_str(),
            "Draw    --",
            "从未观测到完成帧时 Draw 必须留空（0 会被读成「没有 draw call」）"
        );
        assert_eq!(
            unknown[8].as_str(),
            "Prims   0",
            "Prims 不依赖完成反馈，0 是真的 0"
        );
        assert_eq!(unknown[9].as_str(), "Inst    --");
    }

    #[test]
    fn average_helpers_do_not_divide_by_zero() {
        assert_eq!(average_ms(Duration::ZERO, 0), None);
        let ms = average_ms(Duration::from_millis(500), 30).expect("ms");
        assert!((ms - 16.666_7).abs() < 0.01);
    }

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

        let (fps, frame_ms) = rate(Duration::from_millis(500), 60);
        assert!((fps.expect("fps") - 120.0).abs() < 0.01);
        assert!((frame_ms.expect("frame ms") - 8.333_3).abs() < 0.01);
    }

    #[test]
    fn a_sample_window_that_spans_a_long_idle_is_discarded() {
        use std::time::{Duration as StdDuration, Instant as StdInstant};

        use super::{SAMPLE_INTERVAL, install, prepare_frame};
        use crate::window::{Window, WindowId, WindowOptions};

        let t0 = StdInstant::now();
        let mut window = Window::new(WindowId::from_raw(1), WindowOptions::default());
        let mut state = install(&mut window, t0).expect("install");

        let after_idle = t0 + StdDuration::from_secs(3);
        prepare_frame(&mut state, &window, after_idle);
        assert!(
            state.pending_text.is_none(),
            "跨过长时间静止的窗口必须丢弃：本帧不采样"
        );
        assert_eq!(
            state.last_sample_at, after_idle,
            "丢弃窗口时起点移到本帧，下一个窗口从干净起点计时"
        );

        let sample_at = after_idle + SAMPLE_INTERVAL;
        prepare_frame(&mut state, &window, sample_at);
        let text = state.pending_text.as_ref().expect("干净窗口必须产出样本");
        assert_eq!(text[0].as_str(), "FPS     --");
        assert_eq!(state.last_sample_at, sample_at);
    }
}

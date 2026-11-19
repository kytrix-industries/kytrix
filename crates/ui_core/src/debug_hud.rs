//! 引擎内置 Debug HUD（P17）：右上角非交互浮层 + 500 ms 指标汇总。
//!
//! 本模块只在 `hud` feature 下编译。公开入口只有 `App::set_debug_hud` /
//! `App::debug_hud_enabled`；节点、采样累计器与格式化都是 crate 私有。
//!
//! **纯观察者（P-C）**：HUD 不请求 damage、不参与 `needs_continuous_frames`、没有任何定时器或
//! 周期唤醒。文本只在**应用自己出帧**时刷新（`prepare_frame` 在帧入口采样、`apply_frame` 在
//! `refresh_styles` 之后写文本）；静止窗口不产生帧，HUD 保持上一次文本。
//!
//! **激活时只画一帧，不采样、不补帧**：`set_debug_hud` 请求的那一帧只把面板画出来 —— 文本是
//! 10 行缺省 `--` 加一行**尺寸行占位**。尺寸行（真实渲染目标）走**按帧刷新**：install 可能早于
//! 平台写入真实 `surface` / `scale`（demo 在 `Application::run` 之前就装 HUD），此时读到的只是
//! covering 猜测值，写进去比占位更糟；帧入口每帧比较期望值，安装帧（平台写完后的第一帧）就刷成
//! 真实值，窗口 resize / DPI 变化后**下一个应用帧**即更新（不滞后 500 ms 采样窗口）。
//!
//! 因此**首次采样之前**的静止窗口是「10 行 `--` + 真实尺寸行」；一旦有过应用帧采样，之后再静止
//! 就**保留上一次文本**（HUD 不产生帧，也就没有新样本 —— 真机上 idle_after 可以看到 FPS /
//! Prims 停在历史值）。真实指标只在应用出帧且距上一样本 ≥500 ms 时刷新；帧率与后端计数还要等
//! GPU 完成反馈。
//!
//! **为什么不做「激活后补一帧提前首份样本」**（曾经的 bootstrap，已回退）：安装帧的下一帧距安装
//! 帧只有几毫秒，而那一帧会**首次**把场景文字与新 HUD 文本一起写进字形图集 —— 上传槽还忙，新字形
//! 被延后到补帧，补帧上限 `MAX_ATLAS_FOLLOWUPS` 在小窗口 / 慢 GPU 下会被吃光 ⇒ 文本永久缺字，
//! 直到下一次真实应用帧。500 ms 的自然节流天然避开这个竞态。
//!
//! 文本更新分两段：`prepare_frame` 采样并暂存 `pending_text`；`apply_frame` 在 `refresh_styles`
//! 之后、`refresh_text` 之前写文本并 `mark_paint`。这样不经过信号依赖图，也不会被样式 pass 的
//! `clear_paint` 吞掉。`pending_text` **只在提交成功后清**（`finish_frame`）：失败帧下一帧重试。
//!
//! **FPS / Frame 的口径**（P-C）：以 GPU 线程的完成反馈（`FrameFeedback`）为准 ——
//! `FPS = 采样窗口内真实完成的帧数 / 窗口秒数`、`Frame = 窗口毫秒 / 帧数`；窗口内一帧都没完成
//! （静止窗口）⇒ 显示 `--`。**不再**用「两次提交帧入口的间隔」冒充帧率：HUD 自己造帧时那条口径
//! 会退化成 2.0 FPS / 500 ms 的自指值。`CPU / GPU / Upload / Present` 是采样窗口内**真实提交帧**
//! 的平均成本；`Latency` 是发布 → GPU 完成；`Draw` / `Inst` 在从未观测到完成帧时显示 `--`；
//! 尺寸行按帧刷新（真实渲染目标，安装帧即正确，之后不滞后采样窗口）。

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

/// 缺省文本（前 10 行）：`--` = 「还没有样本」，**不是** 0。
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

/// 尺寸行占位：install 阶段可能还读不到真实渲染目标（demo 在 `Application::run` 之前就装 HUD，
/// 平台尚未 `set_scale` / `set_surface`，读到的是 covering 猜测值）。真实值由 [`prepare_frame`]
/// 在**每一帧入口**按帧刷新，安装帧（平台写完真实尺寸后的第一帧）就会覆盖它。
const PLACEHOLDER_SIZE_LINE: &str = "0x0 @ 0.00x";

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
    /// 时刻。真实上屏时刻由 `FrameFeedback::presented_at` 单独承载（macOS 是 display link 的 tick
    /// 推断值；Windows 是 `GetFrameStatistics` 的 QPC 证据；Linux 是 `wp_presentation` 的呈现时钟）。
    ///
    /// A4：HUD 目前**不**显示该值——公开消费面（`Window::last_frame_presentation` /
    /// `App::last_frame_presentation`）已经提供 `publish→presented` p50/p99 所需的全部数据，
    /// 但把 HUD 行从 10 行扩成 11 行属于布局/产品决策（审查报告 Q 待裁决），本项只保证数据面
    /// 可用；`acceptance window` 已在用同一数据面打印 e2e 分位数与有值率。
    pub latency: Option<Duration>,
    /// 实际发出的 draw call 数。
    pub draw_calls: u32,
    /// 实例缓冲条目数。
    pub instances: u32,
    /// 绘制序中的图元数。
    pub primitives: usize,
    /// 当前物理宽度（`window.surface`；帧入口**按帧刷新**，尺寸行不滞后采样窗口）。
    pub physical_width: u32,
    /// 当前物理高度（同上）。
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
    /// 当前已显示（或已排队待显示）的尺寸行来源值 `(物理宽, 物理高, scale)`；
    /// `None` = 还是占位行。用于避免每帧重复写同一行。
    shown_size: Option<(u32, u32, f32)>,
    /// 待写入的尺寸行（**按帧刷新**，与 500 ms 采样文本互不覆盖）。
    pending_size_line: Option<SmolStr>,
    /// 待写入的 10 行指标文本（尺寸行由 [`DebugHudState::pending_size_line`] 单独管）。
    pending_text: Option<[SmolStr; LINES - 1]>,
}

impl DebugHudState {
    /// HUD 根节点（关闭时移除整棵子树）。
    pub(crate) fn root(&self) -> NodeId {
        self.root
    }

    /// 结束当前采样窗口：窗口起点与完成帧基线归位，全部成本累计器清零。
    ///
    /// 两个调用点共用同一份复位逻辑：[`prepare_frame`] 正常采样之后，以及长时间静止后**丢弃**
    /// 这个窗口时（那时不采样，只把起点移到本帧，让下一份样本覆盖干净的 500 ms 窗口）。
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
    /// 最近完成帧的真实 draw call 数；`None` = 从未观测到完成帧（显示 `--`）。
    draw_calls: Option<u32>,
    /// UI 线程的场景图元数（**总是**真实值：它不依赖完成反馈）。
    primitives: usize,
    /// 最近完成帧的真实实例数；`None` = 从未观测到完成帧（显示 `--`）。
    instances: Option<u32>,
}

/// 在 root 末尾建立 HUD 子树；失败时回滚已挂载的节点。
///
/// # Errors
/// `StaleNode`（root/父节点被破坏）或任何 `NodeBuilder` 建树错误。
pub(crate) fn install(window: &mut Window, now: Instant) -> Result<DebugHudState, EngineError> {
    window.reset_debug_hud_timing();
    let root = window.tree().root();
    let frame_index = window.frame_index();
    // 初始文本：10 行缺省 + 尺寸行占位（真实渲染目标由每帧入口的 `prepare_size_line` 刷新，
    // 安装帧就会写对；见 [`PLACEHOLDER_SIZE_LINE`]）。
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

/// 帧入口的采样：消费最近提交帧的指标，每 500 ms 生成一份新文本（**不请求 damage**）。
///
/// **HUD 不产生帧**（P-C）：本函数不写任何脏位、不请求 damage，只是应用这一帧的观察者。没有
/// 应用帧时它不会被调用，静止窗口就保持上一次文本（**只有尺寸行是真的**，见 [`install`]）——
/// 这既是「HUD 不能把自己变成帧源」的要求，也是帧率必须是真实完成帧的原因。
///
/// **尺寸行按帧刷新**，不走 500 ms 采样节流：install 可能早于平台写入真实 `surface` / `scale`
/// （demo 在 `Application::run` 之前就装 HUD），它只是把占位行先画出来；帧入口每帧比较期望值，
/// 一旦变化就排进 `pending_size_line`，安装帧 / resize 后的下一个应用帧就写真实值。
///
/// **为什么不在激活时补一帧来提前首份样本**（曾经的 bootstrap 方案，已回退）：安装帧之后的下一帧
/// 距安装帧只有几毫秒，而那一帧会**首次**把 demo 场景文字与新 HUD 文本一起写进字形图集 —— 图集
/// 上传槽还忙，新字形被延后到后续补帧，补帧上限 `MAX_ATLAS_FOLLOWUPS` 在小窗口/慢 GPU 下会被
/// 吃光 ⇒ 文本永久缺字，直到下一次真实应用帧。500 ms 的自然节流天然避开这个竞态，因此不补帧
/// （尺寸行不受影响：它和它所在的那一帧字形一起上传，没有跨帧改写）。
///
/// 逐帧成本与绘制计数只在 `frame_index` 前进（真的有新提交帧）时累加；`FPS` / `Frame` 用
/// 「采样窗口内**完成**的帧数 / 窗口时长」，一帧都没完成时是 `--`（见 [`rate`]）；`Draw` / `Inst`
/// 在从未观测到完成帧时同样留空（后端计数只随完成反馈回来，0 不等于「没有 draw call」）。
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

    // 尺寸行**按帧刷新**（与 500 ms 采样解耦）：真实渲染目标变化（首个真实帧 / resize / DPI）后
    // 的下一个应用帧就更新，不再滞后一个采样窗口。`shown_size` 记录已排队/已显示的值，避免同一行
    // 每帧重复写。
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
        // 长时间静止后恢复出帧：分母里带着整段静止时长会给出「真实但误导」的帧率（真机实测首份
        // 样本 FPS 4.0 / Frame 251.09 ms）。**丢弃这个窗口**：本帧不采样，下一个 500 ms 窗口从
        // 这一帧开始重新计时/累计，恢复后的首份样本才是干净窗口的帧率。
        state.restart_sample_window(now, metrics.completed_frames);
        return;
    }
    if window_time >= SAMPLE_INTERVAL {
        let completed = metrics
            .completed_frames
            .saturating_sub(state.last_sample_completed);
        let (fps, frame_ms) = rate(window_time, completed);
        // 后端计数只随完成反馈回来：一条反馈都没观测到时 `render_stats` 还是默认值，
        // 显示成 0 会与「真的没有 draw call」混淆（旧 bug 的观感），因此如实留空。
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

/// `refresh_styles` 之后、`refresh_text` 之前：写文本并标 PAINT。
///
/// 尺寸行（第 11 行）与采样文本（第 0..10 行）**分开写、互不覆盖**：前者按帧刷新，后者按
/// 500 ms 采样窗口。
///
/// `pending_text` / `pending_size_line` **不在这里清**：只有提交成功后才由 [`finish_frame`] 清除。
/// 若本帧在 `refresh_text`/`layout_text`/paint 失败，下一帧会重新写同一文本并重新标 PAINT，
/// 避免无绑定节点被下一次 `refresh_styles` 的 ghost 分支吞掉。
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

/// 一帧提交成功：本次 pending 文本已经落进 paint，清空避免下一帧重复重建。
pub(crate) fn finish_frame(state: &mut DebugHudState) {
    state.pending_text = None;
    state.pending_size_line = None;
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

/// 安装时的初始文本：10 行缺省 `--` + 尺寸行占位。
///
/// 尺寸行不在这里写真实值：install 可能早于平台写入 `surface` / `scale`（demo 在 `Application::run`
/// 之前就装 HUD），此时读到的只是 covering 猜测值，写进去比占位行更糟（看起来像真数据但是错的）。
/// 真实值由 [`prepare_frame`] 的按帧刷新路径填入，安装帧就会覆盖这个占位。
fn initial_lines() -> [SmolStr; LINES] {
    let mut lines: [SmolStr; LINES] = std::array::from_fn(|index| {
        SmolStr::new_static(INITIAL_LINES.get(index).copied().unwrap_or(""))
    });
    lines[LINES - 1] = SmolStr::new_static(PLACEHOLDER_SIZE_LINE);
    lines
}

/// 采样文本：10 行指标（尺寸行由按帧刷新路径单独写，见 [`prepare_frame`]）。
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

/// 尺寸行：`WxH @ N.NNx 后端`（按帧刷新路径的唯一格式化点）。
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

/// 后端计数行（`Draw` / `Inst`）：**从未观测到完成帧**时留空。
///
/// 这两个数只随 `FrameFeedback` 回来；没有完成帧时 `render_stats` 是默认值 0，显示 0 会被误读成
/// 「本帧没有 draw call」（正是用户投诉的旧 bug 观感），所以缺数据就显示 `--`。
fn optional_count_text(value: Option<u32>) -> SmolStr {
    match value {
        Some(value) => format_smolstr!("{value}"),
        None => SmolStr::new_static("--"),
    }
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
        HEIGHT, LINES, MARGIN, PLACEHOLDER_SIZE_LINE, Snapshot, WIDTH, average_ms, backend_name,
        format_lines, initial_lines, panel_style, rate, size_line,
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

    /// 安装文本 = 10 行缺省 `--` + 尺寸行**占位**：install 可能早于平台写入真实 surface/scale，
    /// 真实值由每帧入口的按帧刷新填入（安装帧就会覆盖占位）。
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

    /// 尺寸行由**按帧刷新**路径写：格式 = `WxH @ scale 后端`。
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

    /// 长时间静止后恢复出帧：**静止时长不能算进帧率分母**（真机曾出现真实但误导的
    /// `FPS 4.0 / Frame 251.09 ms`）。
    ///
    /// 跨过 `2 × SAMPLE_INTERVAL` 的窗口整窗丢弃（本帧不采样），下一份样本覆盖干净的 500 ms 窗口。
    #[test]
    fn a_sample_window_that_spans_a_long_idle_is_discarded() {
        use std::time::{Duration as StdDuration, Instant as StdInstant};

        use super::{SAMPLE_INTERVAL, install, prepare_frame};
        use crate::window::{Window, WindowId, WindowOptions};

        let t0 = StdInstant::now();
        let mut window = Window::new(WindowId::from_raw(1), WindowOptions::default());
        let mut state = install(&mut window, t0).expect("install");

        // 静止 3 s 后恢复出帧：丢弃窗口，本帧不产出文本。
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

        // 之后一个完整采样窗口：正常产出样本（本环境没有完成帧 ⇒ 帧率仍为 --）。
        let sample_at = after_idle + SAMPLE_INTERVAL;
        prepare_frame(&mut state, &window, sample_at);
        let text = state.pending_text.as_ref().expect("干净窗口必须产出样本");
        assert_eq!(text[0].as_str(), "FPS     --");
        assert_eq!(state.last_sample_at, sample_at);
    }
}

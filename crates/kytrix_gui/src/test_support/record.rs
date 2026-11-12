//! 帧记录（I12）：调度记录 [`FrameRecord`]、绘制快照 [`RenderedFrame`] 与 lane 复用日志。
//!
//! 设计见 `design/phase-06-paint-runtime.md` §10.3：`FrameRecord` 记录**调度事实**（本帧是否提交、
//! damage、统计、lane 复用），`RenderedFrame` 记录**绘制事实**（quad/primitives/布局盒），两者只在
//! 提交帧同时存在。

use crate::{
    math::{PhysicalRect, Rect},
    node::{id::NodeId, tree::NodeTree},
    paint::{
        color::{ColorResolver, LinearPremulRgba},
        pass::PaintReport,
        primitive::{ClipOp, Primitive, QuadPaint},
        scene::{LaneId, Scene},
    },
    window::FrameIndex,
};

/// 一帧的**调度记录**（`drain()` 的返回值；空闲帧也有）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FrameRecord {
    /// 提交游标（0 = 从未提交；空闲帧 = 上一帧的值）。
    pub frame_index: FrameIndex,
    /// 本帧是否提交了像素。
    pub rendered: bool,
    /// 本帧提交的绘制快照（`None` = 空闲/中止）。
    pub frame: Option<RenderedFrame>,
    /// 四个 pass 的统计。
    pub stats: FrameStats,
    /// 物理像素脏矩形；`damage_full == true` 时是整帧矩形。
    pub damage: Vec<PhysicalRect>,
    /// 是否退化为全帧。
    pub damage_full: bool,
    /// 本帧被重建 lane 的节点（`tree.order()` 序）。
    pub dirty_nodes: Vec<NodeId>,
    /// lane 级重建/复用记录。
    pub paint_log: PaintLog,
}

/// 已提交帧的**绘制快照**（设计 §6.3 的 `RenderedFrame`）。
#[derive(Clone, Debug, PartialEq)]
pub struct RenderedFrame {
    /// 提交序号（与 `FrameRecord.frame_index` 相等）。
    pub frame_index: FrameIndex,
    /// 节点的逻辑布局盒（来源是节点树，不是绘制动作）。
    pub debug_bounds: ahash::HashMap<NodeId, Rect>,
    /// 全部 `Quad` 图元（按绘制序，含解析后的颜色）。
    pub quads: Vec<QuadRecord>,
    /// 全部 8 类图元的只读快照（按绘制序，含 `Clip` 指令）。
    pub primitives: Vec<PrimitiveRecord>,
    /// 本帧整桶复用的 lane。
    pub reused_lanes: Vec<LaneId>,
}

/// 一个 `Quad` 的断言面。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QuadRecord {
    /// 产生它的节点。
    pub node: NodeId,
    /// 逻辑像素矩形。
    pub bounds: Rect,
    /// 四角半径（逻辑像素）。
    pub corner_radius: [f32; 4],
    /// 填充色（已按 `ColorResolver` 解析成 premultiplied linear）。
    pub fill: LinearPremulRgba,
    /// 边框（宽度, 颜色）。
    pub border: Option<(f32, LinearPremulRgba)>,
    /// 渐变类型；实色为 `None`。
    pub gradient: Option<crate::paint::primitive::GradientKind>,
}

/// 一个图元的只读快照。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PrimitiveRecord {
    /// 产生它的节点。
    pub node: NodeId,
    /// 种类。
    pub kind: PrimitiveKind,
    /// 逻辑像素 AABB（`GlyphRun` = 字形并集；`ClipPop`/`Geometry` = 节点盒）。
    pub bounds: Rect,
}

/// 8 类图元 + `Clip` 的两个指令。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimitiveKind {
    /// `Primitive::Quad`。
    Quad,
    /// `Primitive::GlyphRun`。
    GlyphRun,
    /// `Primitive::Image`。
    Image,
    /// `Primitive::Path`。
    Path,
    /// `Primitive::Shadow`。
    Shadow,
    /// `Primitive::Clip(ClipOp::Push(_))`。
    ClipPush,
    /// `Primitive::Clip(ClipOp::Pop)`。
    ClipPop,
    /// `Primitive::Geometry`。
    Geometry,
    /// `Primitive::Canvas`。
    Canvas,
}

/// lane 级重建/复用记录。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PaintLog {
    /// 本帧访问过的每条 lane 一条记录（`tree.order()` 序）。
    pub lanes: Vec<LanePaintEntry>,
}

/// 一条 lane 的处理结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LanePaintEntry {
    /// lane 句柄。
    pub lane: LaneId,
    /// 所属节点。
    pub node: NodeId,
    /// 本帧的处理方式。
    pub outcome: LaneOutcome,
    /// 处理后的图元数。
    pub primitives: usize,
}

/// lane 的本帧处理方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneOutcome {
    /// 首次分配（本帧新建 lane）。
    Allocated,
    /// 重建（内容变化）。
    Rebuilt,
    /// 清空（隐藏/无图元）。
    Cleared,
    /// 整桶复用（`REUSABLE` + clip 上下文一致）。
    Reused,
}

/// 四个 pass 的统计（`FrameRecord.stats`）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameStats {
    /// 样式求值 pass。
    pub styles: crate::app::StylePassReport,
    /// 文本形状 pass（`None` = 本帧未跑，例如 `Window::frame`）。
    pub text: Option<crate::text::TextPassReport>,
    /// 文本重排 pass。
    pub text_layout: Option<crate::text::TextLayoutReport>,
    /// 布局 pass（`None` = 无 `LAYOUT`，跳过）。
    pub layout: Option<crate::layout::LayoutReport>,
    /// 本帧重建的 lane 数。
    pub painted_lanes: usize,
    /// 本帧复用的 lane 数。
    pub reused_lanes: usize,
    /// 本帧进入 `Scene` 的图元数。
    pub primitives: usize,
    /// 本帧编码出的实例数。
    pub instances: usize,
}

impl FrameRecord {
    /// 空闲帧记录（不提交、序号不变）。
    #[must_use]
    pub fn idle(frame_index: FrameIndex, stats: FrameStats) -> Self {
        Self {
            frame_index,
            rendered: false,
            frame: None,
            stats,
            damage: Vec::new(),
            damage_full: false,
            dirty_nodes: Vec::new(),
            paint_log: PaintLog::default(),
        }
    }
}

/// 从引擎状态捕获一条已提交帧的记录（消费者：`Window::commit_frame`，仅 feature 构建）。
pub(crate) fn capture(
    scene: &Scene,
    tree: &NodeTree,
    resolver: &ColorResolver,
    report: &PaintReport,
    damage_rects: &[PhysicalRect],
    frame_rect: PhysicalRect,
    frame_index: FrameIndex,
    stats: FrameStats,
) -> FrameRecord {
    let mut lane_nodes: ahash::HashMap<LaneId, NodeId> = ahash::HashMap::default();
    let mut reused_lanes = Vec::new();
    let mut dirty_nodes = Vec::new();
    for entry in &report.log.lanes {
        lane_nodes.insert(entry.lane, entry.node);
        if entry.outcome == LaneOutcome::Reused {
            reused_lanes.push(entry.lane);
        } else {
            dirty_nodes.push(entry.node);
        }
    }

    let mut debug_bounds = ahash::HashMap::default();
    for node in tree.order() {
        if let Ok(target) = tree.get(*node) {
            let bounds = target.layout().bounds;
            if !bounds.is_empty() {
                debug_bounds.insert(*node, bounds);
            }
        }
    }

    let mut quads = Vec::new();
    let mut primitives = Vec::new();
    for lane in scene.order() {
        let Some(node) = lane_nodes.get(lane).copied() else {
            continue;
        };
        let node_bounds = tree
            .get(node)
            .map_or(Rect::default(), |target| target.layout().bounds);
        for primitive in scene.lane_primitives(*lane).unwrap_or(&[]) {
            let (kind, bounds) = primitive_kind(primitive, node_bounds);
            primitives.push(PrimitiveRecord { node, kind, bounds });
            if let Primitive::Quad(quad) = primitive {
                quads.push(quad_record(node, quad, resolver));
            }
        }
    }

    let damage = if report.damage.full {
        vec![frame_rect]
    } else {
        damage_rects.to_vec()
    };
    FrameRecord {
        frame_index,
        rendered: true,
        frame: Some(RenderedFrame {
            frame_index,
            debug_bounds,
            quads,
            primitives,
            reused_lanes,
        }),
        stats,
        damage,
        damage_full: report.damage.full,
        dirty_nodes,
        paint_log: report.log.clone(),
    }
}

fn primitive_kind(primitive: &Primitive, node_bounds: Rect) -> (PrimitiveKind, Rect) {
    match primitive {
        Primitive::Quad(quad) => (PrimitiveKind::Quad, quad.bounds),
        Primitive::GlyphRun(run) => {
            let mut bounds = Rect::default();
            for glyph in &run.glyphs {
                bounds = bounds.union(Rect::from_origin_size(glyph.position, glyph.size));
            }
            (PrimitiveKind::GlyphRun, bounds)
        }
        Primitive::Image(image) => (PrimitiveKind::Image, image.bounds),
        Primitive::Path(path) => (PrimitiveKind::Path, path.bounds),
        Primitive::Shadow(shadow) => (PrimitiveKind::Shadow, shadow.rect),
        Primitive::Clip(ClipOp::Push(clip)) => (PrimitiveKind::ClipPush, clip.rect),
        Primitive::Clip(ClipOp::Pop) => (PrimitiveKind::ClipPop, node_bounds),
        Primitive::Geometry(_) => (PrimitiveKind::Geometry, node_bounds),
        Primitive::Canvas(canvas) => (PrimitiveKind::Canvas, canvas.bounds()),
    }
}

fn quad_record(
    node: NodeId,
    quad: &crate::paint::primitive::Quad,
    resolver: &ColorResolver,
) -> QuadRecord {
    let (fill, border, gradient) = match &quad.paint {
        QuadPaint::Solid {
            fill: solid,
            border: painted_border,
        } => (
            resolver.resolve(*solid),
            painted_border.map(|painted| (painted.width, resolver.resolve(painted.color))),
            None,
        ),
        QuadPaint::Gradient(painted) => (resolver.resolve(painted.from), None, Some(painted.kind)),
    };
    QuadRecord {
        node,
        bounds: quad.bounds,
        corner_radius: quad.corner_radius,
        fill,
        border,
        gradient,
    }
}

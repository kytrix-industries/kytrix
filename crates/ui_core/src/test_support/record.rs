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
    /// 本帧被重建 lane 的节点（DFS 先序 = 绘制序）。
    pub dirty_nodes: Vec<NodeId>,
    /// 本帧**引擎簿记访问过的不同节点数**（脏集合消费 + 全树/子树遍历的并集）。
    ///
    /// 用于把"增量帧不做全树扫描"变成可断言的事实：单节点变更帧应满足
    /// `scanned_nodes <= 1 + 祖先链长度`（R03-1 / S1 的验收）。旧实现每帧有 6–12 次全树
    /// 遍历，该值会是节点总数的数倍。
    pub scanned_nodes: usize,
    /// lane 级重建/复用记录。
    pub paint_log: PaintLog,
    /// 本帧产出的一笔 a11y 交付（`None` = 无 a11y 工作，或捕获未打开）。
    ///
    /// **放在这里而不是 `RenderedFrame`**：纯 `A11Y` 变化不产生 damage ⇒ 本帧 `Idle`，
    /// 而 `RenderedFrame` 只存在于已提交帧（`design/phase-11-a11y.md` F-113）。
    pub a11y: Option<crate::a11y::A11yTreeSnapshot>,
}

/// 已提交帧的**绘制快照**（设计 §6.3 的 `RenderedFrame`）。
#[derive(Clone, Debug, PartialEq)]
pub struct RenderedFrame {
    /// 提交序号（与 `FrameRecord.frame_index` 相等）。
    pub frame_index: FrameIndex,
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
    /// 本帧访问过的每条 lane 一条记录（DFS 先序 = 绘制序）。
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
    /// 本帧编码出的实例数（存活实例，不含实例环空洞）。
    pub instances: usize,
    /// 本帧**重编码**的 lane 数（增量实例编码：`0` = 全部实例段整段复用）。
    pub encoded_lanes: usize,
    /// 本帧整段复用的 lane 段数。
    pub reused_segments: usize,
    /// 本帧增量上传的实例字节数：渲染器路径 = 载荷实际字节数（变化段之和），
    /// headless = 编码器写入实例环的字节数（两者在「只改 1 条 lane」时相同）。
    pub uploaded_bytes: usize,
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
            scanned_nodes: 0,
            paint_log: PaintLog::default(),
            a11y: None,
        }
    }
}

/// 从引擎状态捕获一条已提交帧的记录（消费者：`Window::commit_frame`，仅 feature 构建）。
pub(crate) fn capture(
    scene: &Scene,
    tree: &NodeTree,
    resolver: &ColorResolver,
    report: PaintReport,
    damage_rects: &[PhysicalRect],
    frame_rect: PhysicalRect,
    frame_index: FrameIndex,
    stats: FrameStats,
    scanned_nodes: usize,
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
            quads,
            primitives,
            reused_lanes,
        }),
        stats,
        damage,
        damage_full: report.damage.full,
        dirty_nodes,
        scanned_nodes,
        paint_log: report.log,
        a11y: None,
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

#[cfg(test)]
mod tests {
    use super::FrameRecord;
    use crate::{
        app::App,
        math::{Rect, Scale, Size},
        node::builder::NodeBuilder,
        paint::style::StylePatch,
        window::{FrameOutcome, WindowOptions},
    };

    /// R03-1 / S1 的验收：**单节点变更帧**的引擎簿记只碰"变化节点 + 祖先链"，
    /// 不随节点总数增长。旧实现每帧 6–12 次全树扫描，`scanned_nodes` 会是节点数的数倍。
    #[test]
    fn incremental_frame_scans_only_dirty_nodes() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions {
            title: smol_str::SmolStr::new_static("scan"),
            size: Size::new(400.0, 300.0),
            scale: Scale::ONE,
            ..WindowOptions::default()
        });
        let root = app.window(window).expect("窗口存在").tree().root();
        for index in 0..200 {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, index as f32 * 12.0, 100.0, 10.0))
                .build(app.window_mut(window).expect("窗口存在").tree_mut(), root)
                .expect("静态 box");
        }
        let value = app.signal(0u8);
        let source = value;
        let bound = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 10.0, 10.0))
            .bind_style(move |cx| StylePatch {
                opacity: Some(f32::from(cx.read(&source)) / 255.0),
                ..StylePatch::default()
            })
            .build(app.window_mut(window).expect("窗口存在").tree_mut(), root)
            .expect("绑定节点");
        let node_count = app.window(window).expect("窗口存在").tree().len();
        assert!(matches!(
            app.frame(window).expect("窗口存在").expect("预热帧"),
            FrameOutcome::Committed(_)
        ));

        assert!(app.write(&value, 255), "值变化必须失效");
        assert!(matches!(
            app.frame(window).expect("窗口存在").expect("增量帧"),
            FrameOutcome::Committed(_)
        ));

        let record: &FrameRecord = app
            .window(window)
            .expect("窗口存在")
            .record()
            .expect("提交帧必须有记录");
        assert_eq!(record.dirty_nodes, vec![bound], "只有变化节点被重建 lane");
        assert!(
            record.scanned_nodes <= 2,
            "单节点变更帧的簿记访问数必须 ≤ 1 + 祖先链（root）：{}（树共 {node_count} 节点）",
            record.scanned_nodes
        );
        assert!(
            record.scanned_nodes * 10 < node_count,
            "不得出现全树扫描：{} vs {node_count}",
            record.scanned_nodes
        );
    }
    /// B6：增量帧的编码统计必须可断言 —— 只有脏 lane 被重编码，其余实例段整段复用。
    #[test]
    fn an_incremental_frame_reencodes_only_the_dirty_lane() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions {
            title: smol_str::SmolStr::new_static("encode"),
            size: Size::new(400.0, 300.0),
            scale: Scale::ONE,
            ..WindowOptions::default()
        });
        let root = app.window(window).expect("窗口存在").tree().root();
        for index in 0..64 {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, index as f32 * 4.0, 80.0, 3.0))
                .paint(crate::paint::style::PaintSpec {
                    background: crate::paint::color::Color::WHITE,
                    ..crate::paint::style::PaintSpec::default()
                })
                .build(app.window_mut(window).expect("窗口存在").tree_mut(), root)
                .expect("静态 box");
        }
        let value = app.signal(0u8);
        let source = value;
        let _bound = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 10.0, 10.0))
            // 背景让这条 lane 真的产生实例（否则「只有背景色变化」的节点没有 lane）。
            .paint(crate::paint::style::PaintSpec {
                background: crate::paint::color::Color::WHITE,
                ..crate::paint::style::PaintSpec::default()
            })
            .bind_style(move |cx| StylePatch {
                opacity: Some(f32::from(cx.read(&source)) / 255.0),
                ..StylePatch::default()
            })
            .build(app.window_mut(window).expect("窗口存在").tree_mut(), root)
            .expect("绑定节点");

        // 首帧：全部 lane 首次编码。
        assert!(matches!(
            app.frame(window).expect("窗口存在").expect("首帧"),
            FrameOutcome::Committed(_)
        ));
        let first = app
            .window(window)
            .expect("窗口存在")
            .record()
            .expect("提交帧必须有记录")
            .clone();
        assert!(
            first.stats.encoded_lanes >= 64,
            "首帧必须编码全部 lane：{}",
            first.stats.encoded_lanes
        );
        assert_eq!(first.stats.reused_segments, 0);

        // 增量帧：只有绑了信号的那一个节点被重建 lane。
        assert!(app.write(&value, 255), "值变化必须失效");
        assert!(matches!(
            app.frame(window).expect("窗口存在").expect("增量帧"),
            FrameOutcome::Committed(_)
        ));
        let second = app
            .window(window)
            .expect("窗口存在")
            .record()
            .expect("提交帧必须有记录")
            .clone();
        assert_eq!(
            second.stats.encoded_lanes, 1,
            "只有脏 lane 被重编码（其余 {} 条整段复用）",
            second.stats.reused_segments
        );
        assert!(
            second.stats.reused_segments >= 64,
            "静态 lane 的实例段必须整段复用：{}",
            second.stats.reused_segments
        );
        // headless 不构建 `FramePayload`：这里是编码器侧的增量字节数 —— 一条 lane 的一个
        // 四边实例 = 128 B（旧实现每帧是全量 128 × N）。
        assert_eq!(second.stats.uploaded_bytes, 128);
    }
}

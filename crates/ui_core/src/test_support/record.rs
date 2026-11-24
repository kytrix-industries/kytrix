use std::sync::Arc;

use crate::{
    math::{DeviceRect, LogicalPx, LogicalRect},
    node::{id::NodeId, tree::NodeTree},
    paint::{
        canvas::Canvas,
        color::{ColorResolver, LinearPremulRgba},
        pass::PaintReport,
        primitive::{ClipOp, Primitive, QuadPaint},
        scene::{LaneId, Scene},
    },
    window::FrameIndex,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeltaRecord {
    pub full: bool,
    pub order_some: bool,
    pub lanes: usize,
    pub removed: usize,
    pub scroll: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FrameRecord {
    pub frame_index: FrameIndex,
    pub rendered: bool,
    pub frame: Option<RenderedFrame>,
    pub stats: FrameStats,
    pub damage: Vec<DeviceRect>,
    pub damage_full: bool,
    pub delta: DeltaRecord,
    pub dirty_nodes: Vec<NodeId>,
    pub scanned_nodes: usize,
    pub repainted: bool,
    pub a11y_only: bool,
    pub skipped_by_budget: u64,
    pub paint_log: PaintLog,
    pub a11y: Option<crate::a11y::A11yTreeSnapshot>,
    pub presented_latency: Option<std::time::Duration>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenderedFrame {
    pub frame_index: FrameIndex,
    pub quads: Vec<QuadRecord>,
    pub primitives: Vec<PrimitiveRecord>,
    pub canvases: Vec<CanvasRecord>,
    pub reused_lanes: Vec<LaneId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CanvasRecord {
    pub node: NodeId,
    pub canvas: Arc<Canvas>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QuadRecord {
    pub node: NodeId,
    pub bounds: LogicalRect,
    pub corner_radius: [LogicalPx; 4],
    pub fill: LinearPremulRgba,
    pub border: Option<(LogicalPx, LinearPremulRgba)>,
    pub gradient: Option<crate::paint::primitive::GradientKind>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PrimitiveRecord {
    pub node: NodeId,
    pub kind: PrimitiveKind,
    pub bounds: LogicalRect,
    pub image_modulation: Option<LinearPremulRgba>,
    pub image_uv: Option<[f32; 4]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimitiveKind {
    Quad,
    GlyphRun,
    Image,
    Path,
    Shadow,
    ClipPush,
    ClipPop,
    Geometry,
    Canvas,
    Text,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PaintLog {
    pub lanes: Vec<LanePaintEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LanePaintEntry {
    pub lane: LaneId,
    pub node: NodeId,
    pub outcome: LaneOutcome,
    pub primitives: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneOutcome {
    Allocated,
    Rebuilt,
    Cleared,
    Reused,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameStats {
    pub styles: crate::app::StylePassReport,
    pub text: Option<crate::text::TextPassReport>,
    pub text_layout: Option<crate::text::TextLayoutReport>,
    pub layout: Option<crate::layout::LayoutReport>,
    pub painted_lanes: usize,
    pub reused_lanes: usize,
    pub primitives: usize,
    pub instances: usize,
    pub encoded_lanes: usize,
    pub reused_segments: usize,
    pub uploaded_bytes: usize,
    pub geometry_encoded_chunks: usize,
    pub geometry_uploaded_bytes: usize,
    pub compositor_inbox_latency: Option<std::time::Duration>,
    pub encode_on_compositor: bool,
    pub scroll_offset_frames: usize,
    pub a11y_only_frames: usize,
}

impl FrameRecord {
    #[must_use]
    pub fn idle(frame_index: FrameIndex, stats: FrameStats) -> Self {
        Self {
            frame_index,
            rendered: false,
            frame: None,
            stats,
            damage: Vec::new(),
            damage_full: false,
            delta: DeltaRecord::default(),
            dirty_nodes: Vec::new(),
            scanned_nodes: 0,
            repainted: false,
            a11y_only: false,
            skipped_by_budget: 0,
            paint_log: PaintLog::default(),
            a11y: None,
            presented_latency: None,
        }
    }
}

pub(crate) fn capture(
    scene: &Scene,
    tree: &NodeTree,
    resolver: &ColorResolver,
    report: PaintReport,
    damage_rects: &[DeviceRect],
    frame_rect: DeviceRect,
    frame_index: FrameIndex,
    stats: FrameStats,
    scanned_nodes: usize,
) -> FrameRecord {
    let mut lane_nodes: ahash::HashMap<LaneId, NodeId> = ahash::HashMap::default();
    {
        let mut stack: smallvec::SmallVec<[NodeId; 32]> = smallvec::SmallVec::new();
        stack.push(tree.root());
        while let Some(node) = stack.pop() {
            let Ok(target) = tree.get(node) else {
                continue;
            };
            if let Some(lane) = target.lane() {
                lane_nodes.insert(lane, node);
            }
            stack.extend(target.children().iter().rev().copied());
        }
    }
    let mut reused_lanes = Vec::new();
    let mut dirty_nodes = Vec::new();
    for entry in &report.log.lanes {
        if entry.outcome == LaneOutcome::Reused {
            reused_lanes.push(entry.lane);
        } else {
            dirty_nodes.push(entry.node);
        }
    }

    let mut quads = Vec::new();
    let mut primitives = Vec::new();
    let mut canvases = Vec::new();
    for lane in scene.order() {
        let Some(node) = lane_nodes.get(lane).copied() else {
            continue;
        };
        let node_bounds = tree
            .get(node)
            .map_or(LogicalRect::default(), |target| target.layout().bounds);
        for primitive in scene.lane_primitives(*lane).unwrap_or(&[]) {
            let (kind, bounds) = primitive_kind(primitive, node_bounds);
            let (image_modulation, image_uv) = match primitive {
                Primitive::Image(image) => (
                    Some(resolver.resolve(image.modulation)),
                    Some(image.tile.uv()),
                ),
                _ => (None, None),
            };
            primitives.push(PrimitiveRecord {
                node,
                kind,
                bounds,
                image_modulation,
                image_uv,
            });
            if let Primitive::Quad(quad) = primitive {
                quads.push(quad_record(node, quad, resolver));
            }
            if let Primitive::Canvas(canvas) = primitive {
                canvases.push(CanvasRecord {
                    node,
                    canvas: Arc::clone(canvas),
                });
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
            canvases,
            reused_lanes,
        }),
        stats,
        damage,
        damage_full: report.damage.full,
        delta: report.delta,
        dirty_nodes,
        scanned_nodes,
        repainted: true,
        a11y_only: false,
        skipped_by_budget: 0,
        paint_log: report.log,
        a11y: None,
        presented_latency: None,
    }
}

fn primitive_kind(primitive: &Primitive, node_bounds: LogicalRect) -> (PrimitiveKind, LogicalRect) {
    match primitive {
        Primitive::Quad(quad) => (PrimitiveKind::Quad, quad.bounds),
        Primitive::GlyphRun(run) => {
            let mut bounds = LogicalRect::default();
            for glyph in &run.glyphs {
                bounds = bounds.union(LogicalRect::from_origin_size(glyph.position, glyph.size));
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
        Primitive::Text(_) => (PrimitiveKind::Text, node_bounds),
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
    use std::sync::Arc;

    use super::FrameRecord;
    use crate::{
        app::App,
        math::{LogicalRect, LogicalSize, px},
        node::builder::NodeBuilder,
        paint::{
            canvas::Canvas,
            color::Color,
            primitive::{Quad, QuadPaint},
            style::StylePatch,
        },
        window::{FrameOutcome, WindowOptions},
    };

    #[test]
    fn incremental_frame_scans_only_dirty_nodes() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions {
            title: smol_str::SmolStr::new_static("scan"),
            size: LogicalSize::new(px(400.0), px(300.0)),
            ..WindowOptions::default()
        });
        let root = app.window(window).expect("窗口存在").tree().root();
        for index in 0..200 {
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(
                    px(0.0),
                    px(index as f32 * 12.0),
                    px(100.0),
                    px(10.0),
                ))
                .build(app.window_mut(window).expect("窗口存在").tree_mut(), root)
                .expect("静态 box");
        }
        let value = app.signal(0u8);
        let source = value;
        let bound = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(10.0), px(10.0)))
            .bind_style(move |cx| StylePatch {
                opacity: Some(f32::from(*cx.read(&source)) / 255.0),
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
    #[test]
    fn an_incremental_frame_reencodes_only_the_dirty_lane() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions {
            title: smol_str::SmolStr::new_static("encode"),
            size: LogicalSize::new(px(400.0), px(300.0)),
            ..WindowOptions::default()
        });
        let root = app.window(window).expect("窗口存在").tree().root();
        for index in 0..64 {
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(
                    px(0.0),
                    px(index as f32 * 4.0),
                    px(80.0),
                    px(3.0),
                ))
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
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(10.0), px(10.0)))
            .paint(crate::paint::style::PaintSpec {
                background: crate::paint::color::Color::WHITE,
                ..crate::paint::style::PaintSpec::default()
            })
            .bind_style(move |cx| StylePatch {
                opacity: Some(f32::from(*cx.read(&source)) / 255.0),
                ..StylePatch::default()
            })
            .build(app.window_mut(window).expect("窗口存在").tree_mut(), root)
            .expect("绑定节点");

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
        assert_eq!(second.stats.uploaded_bytes, 128);
    }

    #[test]
    fn a_committed_frame_records_the_canvas_it_painted() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions {
            title: smol_str::SmolStr::new_static("canvas"),
            size: LogicalSize::new(px(400.0), px(300.0)),
            ..WindowOptions::default()
        });
        let root = app.window(window).expect("窗口存在").tree().root();
        let mut canvas = Canvas::new(LogicalRect::new(px(0.0), px(0.0), px(40.0), px(20.0)));
        canvas.push_quad(Quad {
            bounds: LogicalRect::new(px(0.0), px(0.0), px(20.0), px(10.0)),
            corner_radius: [px(0.0); 4],
            paint: QuadPaint::Solid {
                fill: Color::WHITE,
                border: None,
            },
        });
        let node = NodeBuilder::canvas(canvas)
            .layout_override(LogicalRect::new(px(10.0), px(10.0), px(80.0), px(40.0)))
            .id("chart")
            .build(app.window_mut(window).expect("窗口存在").tree_mut(), root)
            .expect("canvas 节点");

        assert!(matches!(
            app.frame(window).expect("窗口存在").expect("首帧"),
            FrameOutcome::Committed(_)
        ));
        let record: FrameRecord = app
            .window(window)
            .expect("窗口存在")
            .record()
            .expect("提交帧必须有记录")
            .clone();
        let rendered = record.frame.as_ref().expect("提交帧必须有绘制快照");
        let entry = rendered
            .canvases
            .iter()
            .find(|entry| entry.node == node)
            .expect("画布内容必须挂在产生它的节点上");
        assert_eq!(
            entry.canvas.bounds(),
            LogicalRect::new(px(0.0), px(0.0), px(40.0), px(20.0))
        );
        assert_eq!(entry.canvas.primitives().len(), 1, "内层图元可下钻");
        let crate::node::tree::NodeKind::Canvas(node_canvas) = app
            .window(window)
            .expect("窗口存在")
            .tree()
            .get(node)
            .expect("节点")
            .kind()
        else {
            panic!("the node is a canvas");
        };
        assert!(
            Arc::ptr_eq(node_canvas, &entry.canvas),
            "快照记录的是节点树上的同一个 Canvas 值，不深拷贝画布内容"
        );

        assert!(matches!(
            app.frame(window).expect("窗口存在").expect("空闲帧"),
            FrameOutcome::Idle
        ));
        let idle = app
            .window(window)
            .expect("窗口存在")
            .record()
            .expect("空闲帧也有记录");
        assert!(idle.frame.is_none(), "空闲帧没有绘制快照");
    }
}

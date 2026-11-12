//! paint pass：把节点树 + 样式 + 文本 + 自绘图元变成 `Scene` 的 lane 与 damage。
//!
//! 契约见 `design/phase-06-paint-runtime.md` §5–§6：
//! - **一节点一 lane**（`Node.lanes[0]`，粘性保留）；
//! - lane 内桶序 = 祖先 clip 前缀 → Shadow → 背景/渐变 → 边框 → 内容 → 祖先 clip 后缀；
//! - 复用判据 = `REUSABLE` + lane 存在 + **clip 上下文一致**（`clip_key`，审查 B1 的守卫）；
//! - 隐藏子树（自身/祖先 `display: none`）清空 lane、不产生像素、`clip_key` 置空（重新显示必重建）；
//! - damage 按「节点级 + 图元级」双写（§6.6）。

use std::rc::Rc;

use smallvec::SmallVec;

#[cfg(feature = "test-support")]
use crate::test_support::record::{LaneOutcome, LanePaintEntry, PaintLog};
use crate::{
    math::{PhysicalRect, Point, Rect, Scale},
    node::{
        dirty::DirtyFlags,
        id::NodeId,
        tree::{NodeKind, NodeTree},
    },
    paint::{
        color::Color,
        primitive::{
            ClipOp, ClipRect, GeometryDraw, GlyphRun, Gradient, PositionedGlyph, Primitive, Quad,
            QuadPaint, Shadow,
        },
        scene::{LaneId, Scene},
        style::{Clip, PaintSpec, StylePatch},
    },
    render::damage::{DamageAccumulator, DamageSummary},
    text::{ShapedLine, ShapedTextItem},
};

/// 一帧的 lane 重建/复用。字段是 crate 内可见（`pub(crate)` 结构的 `pub` 字段不触发 lint）。
pub(crate) struct PaintPass<'a> {
    pub tree: &'a mut NodeTree,
    pub scene: &'a mut Scene,
    pub damage: &'a mut DamageAccumulator,
    pub scale: Scale,
    /// 本帧 framebuffer 的物理矩形（`DamageAccumulator::finish` 的面积阈值用）。
    pub frame: PhysicalRect,
    /// 本帧的 lane 绘制序（复用调用方的容量）。
    pub order: &'a mut Vec<LaneId>,
    /// lane 重建/复用记录（仅 `test-support`）。
    #[cfg(feature = "test-support")]
    pub log: PaintLog,
}

/// 一次 paint 的产物。
pub(crate) struct PaintReport {
    pub rebuilt: usize,
    pub damage: DamageSummary,
    /// 本帧编码出的实例数（仅 `test-support`；`Window::paint_frame` 编码后回填）。
    #[cfg(feature = "test-support")]
    pub instances: usize,
    #[cfg(feature = "test-support")]
    pub log: PaintLog,
}

impl PaintPass<'_> {
    /// 执行整帧 paint（唯一入口，消费者：`Window::paint_frame`）。
    pub(crate) fn run(&mut self) -> PaintReport {
        let tree = &mut *self.tree;
        let scene = &mut *self.scene;
        let damage = &mut *self.damage;
        let order = &mut *self.order;
        let scale = self.scale;
        let frame = self.frame;
        #[cfg(feature = "test-support")]
        let log = std::mem::take(&mut self.log);
        let mut context = PaintContext {
            tree,
            scene,
            damage,
            scale,
            order,
            #[cfg(feature = "test-support")]
            log,
            rebuilt: 0,
        };
        let mut clip = ClipContext::default();
        let root = context.tree.root();
        paint_node(&mut context, root, false, &mut clip);
        context.scene.set_order(context.order);
        let damage = context.damage.finish(frame);
        PaintReport {
            rebuilt: context.rebuilt,
            damage,
            #[cfg(feature = "test-support")]
            instances: 0,
            #[cfg(feature = "test-support")]
            log: context.log,
        }
    }
}

/// 一次遍历的借用包（避免 `&mut self` 与 `&self.tree` 的借用冲突）。
struct PaintContext<'a> {
    tree: &'a mut NodeTree,
    scene: &'a mut Scene,
    damage: &'a mut DamageAccumulator,
    scale: Scale,
    order: &'a mut Vec<LaneId>,
    #[cfg(feature = "test-support")]
    log: PaintLog,
    rebuilt: usize,
}

/// 祖先 clip 链（外层在前）+ 物理 covering 交集。
#[derive(Default)]
struct ClipContext {
    rects: SmallVec<[ClipRect; 8]>,
    covering: Option<PhysicalRect>,
}

impl ClipContext {
    fn key(&self) -> (u8, Option<PhysicalRect>) {
        (self.rects.len() as u8, self.covering)
    }

    /// 进入一个 `Clip::Bounds` 节点：按它的 **padding box** 裁剪子树。
    fn push(&mut self, padding_box: Rect, scale: Scale) {
        self.rects.push(ClipRect {
            rect: padding_box,
            corner_radius: [0.0; 4],
        });
        self.rebuild_covering(scale);
    }

    fn pop(&mut self, scale: Scale) {
        let _ = self.rects.pop();
        self.rebuild_covering(scale);
    }

    fn rebuild_covering(&mut self, scale: Scale) {
        let mut covering: Option<PhysicalRect> = None;
        for rect in &self.rects {
            let physical = rect.rect.to_physical_covering(scale);
            covering = Some(match covering {
                Some(parent) => parent.intersect(physical),
                None => physical,
            });
        }
        self.covering = covering;
    }
}

/// 节点要画的内容（读取阶段复制出来，随后释放 `&Node` 借用）。
enum Content {
    Box,
    Text(Option<Rc<ShapedLine>>),
    Canvas(Box<crate::paint::canvas::Canvas>),
    Geometry(crate::paint::geometry::GeometryId),
}

fn paint_node(
    context: &mut PaintContext<'_>,
    node: NodeId,
    parent_hidden: bool,
    clip: &mut ClipContext,
) {
    let Ok(target) = context.tree.get(node) else {
        return;
    };
    let hidden =
        parent_hidden || matches!(target.style().inner.display, taffy::style::Display::None);
    if hidden {
        hide_subtree(context, node, clip);
        return;
    }
    let current_key = clip.key();
    // 复用判据（§5.6）：`REUSABLE` + clip 上下文一致。**不要求** lane 存在：从未产生图元的节点
    // 没有 lane，但它的 `clip_key` 已写回，仍可整帧跳过（否则空节点每帧都要重建）。
    let reusable = target.dirty().contains(DirtyFlags::REUSABLE)
        && context.tree.clip_key(node) == Some(current_key);

    if reusable {
        if let Some(lane) = context.tree.lane_of(node) {
            context.order.push(lane);
            #[cfg(feature = "test-support")]
            {
                let primitives = context
                    .scene
                    .lane_primitives(lane)
                    .map_or(0, <[Primitive]>::len);
                context.log.lanes.push(LanePaintEntry {
                    lane,
                    node,
                    outcome: LaneOutcome::Reused,
                    primitives,
                });
            }
        }
    } else {
        rebuild_lane(context, node, clip);
    }

    let (children, clip_bounds) = {
        let Ok(target) = context.tree.get(node) else {
            return;
        };
        let children: SmallVec<[NodeId; 4]> = target.children().iter().copied().collect();
        // B1：0×0 的 padding box **也要入栈**（depth+1、covering 交空）——子 lane 由此带空前缀、
        // damage 全部被裁空，与 `hit_test` 的 clip 链语义一致；跳过它会让子内容"越界可见"。
        let clip_bounds = (target.clip() == Clip::Bounds)
            .then(|| target.layout().bounds.inset(target.layout().border));
        (children, clip_bounds)
    };
    if let Some(padding_box) = clip_bounds {
        clip.push(padding_box, context.scale);
    }
    for child in children {
        paint_node(context, child, false, clip);
    }
    if clip_bounds.is_some() {
        clip.pop(context.scale);
    }
}

/// 隐藏子树：清空每条 lane（保留分配），并把旧像素并入 damage；`clip_key` 置空
/// （再次显示时必然失配 ⇒ 重建，避免"复用空 lane"）。
fn hide_subtree(context: &mut PaintContext<'_>, node: NodeId, clip: &ClipContext) {
    let (children, damage_rect) = {
        let Ok(target) = context.tree.get(node) else {
            return;
        };
        let children: SmallVec<[NodeId; 4]> = target.children().iter().copied().collect();
        let damage_rect = target.damage().union(target.layout().bounds);
        (children, damage_rect)
    };
    if let Some(lane) = context.tree.lane_of(node) {
        let had_primitives = context
            .scene
            .lane_primitives(lane)
            .is_some_and(|primitives| !primitives.is_empty());
        if had_primitives {
            let _ = context.scene.clear_lane(lane);
            context.rebuilt += 1;
            context.order.push(lane);
            #[cfg(feature = "test-support")]
            context.log.lanes.push(LanePaintEntry {
                lane,
                node,
                outcome: LaneOutcome::Cleared,
                primitives: 0,
            });
        }
    }
    context.tree.clear_clip_key(node);
    context.tree.set_reusable(node);
    context.tree.clear_paint(node);
    context.damage.push(damage_rect, 1.0, clip.covering);
    for child in children {
        hide_subtree(context, child, clip);
    }
}

fn rebuild_lane(context: &mut PaintContext<'_>, node: NodeId, clip: &ClipContext) {
    #[cfg(feature = "test-support")]
    let created = context.tree.lane_of(node).is_none();
    let lane = match context.tree.lane_of(node) {
        Some(lane) => lane,
        None => {
            let lane = context.scene.new_lane();
            context.tree.set_lane(node, lane);
            lane
        }
    };
    let _ = context.scene.clear_lane(lane);

    for clip_rect in &clip.rects {
        let _ = context
            .scene
            .push(lane, Primitive::Clip(ClipOp::Push(*clip_rect)));
    }

    let (bounds, content_offset, patch, spec, content, node_damage) = {
        let Ok(target) = context.tree.get(node) else {
            return;
        };
        let content = match target.kind() {
            NodeKind::Box => Content::Box,
            NodeKind::Text(_) => Content::Text(context.tree.text_line(node).cloned()),
            NodeKind::Canvas(canvas) => Content::Canvas(Box::new(canvas.clone())),
            NodeKind::Geometry(id) => Content::Geometry(*id),
        };
        let bounds = target.layout().bounds;
        let patch = target.style_patch().clone();
        let spec = target.paint_spec().cloned();
        let node_damage = target.damage();
        let content_offset = context.tree.content_offset(node);
        (bounds, content_offset, patch, spec, content, node_damage)
    };

    let mut emitter = PrimitiveEmitter {
        lane,
        bounds,
        content_offset,
        patch,
        spec,
        scale: context.scale,
        clip: clip.covering,
        content,
        emitted: 0,
        node_damage,
    };
    emitter.emit(context);

    for _ in clip.rects.iter().rev() {
        let _ = context.scene.push(lane, Primitive::Clip(ClipOp::Pop));
    }

    let primitives = context
        .scene
        .lane_primitives(lane)
        .map_or(0, <[Primitive]>::len);
    context.tree.set_reusable(node);
    context.tree.set_clip_key(node, clip.key());
    context.tree.clear_paint(node);

    // 「没有东西要画」的节点不留 lane（空 lane 只占 order/内存）：丢弃句柄并释放分配。
    // 它的 `clip_key` 已写回，下一帧仍可命中复用判据（无需每帧重建）。
    if primitives == 0 {
        context.scene.release_lane(lane);
        context.tree.clear_lane(node);
        if !emitter.node_damage.is_empty() {
            context.rebuilt += 1;
        }
    } else {
        context.rebuilt += 1;
        context.order.push(lane);
    }

    #[cfg(feature = "test-support")]
    context.log.lanes.push(LanePaintEntry {
        lane,
        node,
        outcome: if primitives == 0 {
            LaneOutcome::Cleared
        } else if created {
            LaneOutcome::Allocated
        } else {
            LaneOutcome::Rebuilt
        },
        primitives,
    });
}

/// 逐图元生成 + damage 累积（§6.2 的生成表）。
struct PrimitiveEmitter {
    lane: LaneId,
    bounds: Rect,
    content_offset: Point,
    patch: StylePatch,
    spec: Option<PaintSpec>,
    scale: Scale,
    clip: Option<PhysicalRect>,
    content: Content,
    /// 已 push 的图元数（决定本节点是否计入 `rebuilt`/damage）。
    emitted: usize,
    /// 本帧累积的节点级 damage（`touch`/`absolutize` 写入）。
    node_damage: Rect,
}

impl PrimitiveEmitter {
    fn push(&mut self, context: &mut PaintContext<'_>, primitive: Primitive) {
        let _ = context.scene.push(self.lane, primitive);
        self.emitted += 1;
    }

    fn emit(&mut self, context: &mut PaintContext<'_>) {
        let visible = self.patch.visible.unwrap_or(true);
        let opacity = match self.patch.opacity {
            Some(value) if value.is_nan() => 0.0,
            Some(value) => value.clamp(0.0, 1.0),
            None => 1.0,
        };
        if !visible || self.bounds.is_empty() {
            if !self.node_damage.is_empty() {
                context.damage.push(self.node_damage, 1.0, self.clip);
            }
            return;
        }

        let spec = self.spec.clone().unwrap_or_default();
        let radii = spec.corner_radius;
        if let Some(shadow) = spec.shadow {
            let color = shadow.color.with_opacity(opacity);
            if !color.is_transparent() {
                let primitive = Shadow {
                    rect: self.bounds,
                    offset: shadow.offset,
                    blur: shadow.blur.max(0.0),
                    color,
                };
                self.push(context, Primitive::Shadow(primitive));
                context.damage.push(
                    primitive.rect.translate(primitive.offset),
                    primitive.blur * self.scale.factor() + 1.0,
                    self.clip,
                );
            }
        }

        let background = self
            .patch
            .background
            .or(if spec.background.is_transparent() {
                None
            } else {
                Some(spec.background)
            })
            .map(|color| color.with_opacity(opacity));
        let border = self
            .patch
            .border
            .or(spec.border)
            .filter(|border| border.width > 0.0 && !border.color.is_transparent())
            .map(|border| crate::paint::primitive::Border {
                width: border.width,
                color: border.color.with_opacity(opacity),
            });
        let gradient = spec
            .gradient
            .map(|gradient| Gradient {
                from: gradient.from.with_opacity(opacity),
                to: gradient.to.with_opacity(opacity),
                kind: gradient.kind,
            })
            .filter(|gradient| {
                // 两端全透明的渐变不产生像素：不入 lane、不产 damage。
                !(gradient.from.is_transparent() && gradient.to.is_transparent())
            });

        match gradient {
            Some(gradient) => {
                self.push(
                    context,
                    Primitive::Quad(Quad {
                        bounds: self.bounds,
                        corner_radius: radii,
                        paint: QuadPaint::Gradient(gradient),
                    }),
                );
                context.damage.push(self.bounds, 1.0, self.clip);
                if let Some(border) = border {
                    self.push(
                        context,
                        Primitive::Quad(Quad {
                            bounds: self.bounds,
                            corner_radius: radii,
                            paint: QuadPaint::Solid {
                                fill: Color::TRANSPARENT,
                                border: Some(border),
                            },
                        }),
                    );
                }
            }
            None => {
                let fill = background.unwrap_or(Color::TRANSPARENT);
                if !fill.is_transparent() || border.is_some() {
                    self.push(
                        context,
                        Primitive::Quad(Quad {
                            bounds: self.bounds,
                            corner_radius: radii,
                            paint: QuadPaint::Solid { fill, border },
                        }),
                    );
                    context.damage.push(self.bounds, 1.0, self.clip);
                }
            }
        }

        self.emit_content(context, opacity);
        if self.emitted > 0 || !self.node_damage.is_empty() {
            context.damage.push(self.node_damage, 1.0, self.clip);
        }
    }

    fn emit_content(&mut self, context: &mut PaintContext<'_>, opacity: f32) {
        match std::mem::replace(&mut self.content, Content::Box) {
            Content::Box => {}
            Content::Geometry(id) => {
                self.push(
                    context,
                    Primitive::Geometry(GeometryDraw {
                        id,
                        transform: crate::math::Transform::IDENTITY,
                    }),
                );
                context.damage.push(self.bounds, 1.0, self.clip);
            }
            Content::Canvas(canvas) => {
                let canvas_bounds = canvas.bounds();
                self.push(context, Primitive::Canvas(*canvas));
                context.damage.push(canvas_bounds, 1.0, self.clip);
            }
            Content::Text(line) => {
                let Some(line) = line else {
                    return;
                };
                self.emit_text(context, &line, opacity);
            }
        }
    }

    fn emit_text(&mut self, context: &mut PaintContext<'_>, line: &ShapedLine, opacity: f32) {
        let origin = Point::new(
            self.bounds.origin.x + self.content_offset.x,
            self.bounds.origin.y + self.content_offset.y,
        );
        for shaped_line in &line.lines {
            for item in &shaped_line.items {
                let ShapedTextItem::GlyphRun(run) = item else {
                    continue;
                };
                if run.glyphs.is_empty() {
                    continue;
                }
                let mut glyphs = Vec::with_capacity(run.glyphs.len());
                let mut glyph_bounds = Rect::default();
                for glyph in &run.glyphs {
                    let position =
                        Point::new(origin.x + glyph.position.x, origin.y + glyph.position.y);
                    let rect = Rect::from_origin_size(position, glyph.size);
                    glyph_bounds = glyph_bounds.union(rect);
                    glyphs.push(PositionedGlyph {
                        tile: glyph.tile,
                        position,
                        size: glyph.size,
                    });
                }
                let color = self
                    .patch
                    .foreground
                    .unwrap_or(run.color)
                    .with_opacity(opacity);
                if color.is_transparent() {
                    continue;
                }
                self.push(context, Primitive::GlyphRun(GlyphRun { color, glyphs }));
                context.damage.push(glyph_bounds, 0.0, self.clip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PaintPass;
    use crate::{
        math::{PhysicalRect, Point, Rect, Scale},
        node::{builder::NodeBuilder, tree::NodeTree},
        paint::{
            canvas::Canvas,
            color::Color,
            geometry::GeometryId,
            primitive::{Border, ClipOp, Gradient, GradientKind, Primitive, QuadPaint},
            scene::{LaneId, Scene},
            style::{Clip, PaintSpec, ShadowSpec, Style, StylePatch},
        },
        render::damage::DamageAccumulator,
    };

    fn scale() -> Scale {
        Scale::new(1.0).expect("合法 scale")
    }

    fn frame() -> PhysicalRect {
        PhysicalRect::new(0, 0, 400, 300)
    }

    struct Harness {
        tree: NodeTree,
        scene: Scene,
        damage: DamageAccumulator,
        order: Vec<LaneId>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                tree: NodeTree::new(),
                scene: Scene::empty(),
                damage: DamageAccumulator::new(scale()),
                order: Vec::new(),
            }
        }

        fn paint(&mut self) -> super::PaintReport {
            self.damage.reset(scale());
            self.order.clear();
            let mut pass = PaintPass {
                tree: &mut self.tree,
                scene: &mut self.scene,
                damage: &mut self.damage,
                scale: scale(),
                frame: frame(),
                order: &mut self.order,
                #[cfg(feature = "test-support")]
                log: crate::test_support::record::PaintLog::default(),
            };
            pass.run()
        }

        fn mark_paint(&mut self, node: crate::node::id::NodeId) {
            let _ = self.tree.mark_paint(node);
        }
    }

    fn fixed_style(width: f32, height: f32) -> Style {
        let mut inner = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(width),
            height: taffy::style::Dimension::length(height),
        };
        Style {
            inner,
            clip: Clip::None,
        }
    }

    fn with_white() -> PaintSpec {
        PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        }
    }

    /// 给每个节点写一个固定的绝对布局盒（paint 只读 `layout`，不需要 taffy）。
    fn layout_all(tree: &mut NodeTree) {
        let order: Vec<_> = tree.order().to_vec();
        for (index, id) in order.iter().enumerate() {
            let bounds = Rect::new(0.0, (index as f32) * 20.0, 100.0, 20.0);
            let layout = crate::node::tree::LayoutBox {
                bounds,
                border: crate::math::Edges::ZERO,
            };
            let _ = tree.set_layout_for_test(*id, layout, crate::node::tree::LayoutBox::default());
        }
    }

    fn lane_primitives(harness: &Harness, node: crate::node::id::NodeId) -> Vec<Primitive> {
        let lane = harness.tree.lane_of(node).expect("lane");
        harness.scene.lane_primitives(lane).expect("存在").to_vec()
    }

    #[test]
    fn background_and_border_fit_one_quad() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let node = NodeBuilder::boxed()
            .style(fixed_style(50.0, 20.0))
            .paint(PaintSpec {
                background: Color::WHITE,
                border: Some(Border {
                    width: 1.0,
                    color: Color::BLACK,
                }),
                ..PaintSpec::default()
            })
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let report = h.paint();
        assert_eq!(report.rebuilt, 1, "只有真正产生像素的节点计入重建");
        assert!(
            report.damage.rects > 0 || report.damage.full,
            "产生像素的节点必须累积 damage"
        );
        let primitives = lane_primitives(&h, node);
        assert_eq!(primitives.len(), 1);
        match &primitives[0] {
            Primitive::Quad(quad) => match &quad.paint {
                QuadPaint::Solid { fill, border } => {
                    assert!(!fill.is_transparent());
                    assert!(border.is_some());
                }
                QuadPaint::Gradient(_) => panic!("应为实色"),
            },
            _ => panic!("应为 Quad"),
        }
    }

    #[test]
    fn gradient_plus_border_is_two_quads() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let node = NodeBuilder::boxed()
            .style(fixed_style(50.0, 20.0))
            .paint(PaintSpec {
                gradient: Some(Gradient {
                    from: Color::WHITE,
                    to: Color::BLACK,
                    kind: GradientKind::Horizontal,
                }),
                border: Some(Border {
                    width: 1.0,
                    color: Color::BLACK,
                }),
                ..PaintSpec::default()
            })
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();
        let primitives = lane_primitives(&h, node);
        assert_eq!(primitives.len(), 2);
        assert!(matches!(primitives[0], Primitive::Quad(_)));
        assert!(matches!(primitives[1], Primitive::Quad(_)));
    }

    #[test]
    fn shadow_precedes_fill() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let node = NodeBuilder::boxed()
            .style(fixed_style(50.0, 20.0))
            .paint(PaintSpec {
                background: Color::WHITE,
                shadow: Some(ShadowSpec {
                    offset: Point::new(1.0, 2.0),
                    blur: 3.0,
                    color: Color::BLACK,
                }),
                ..PaintSpec::default()
            })
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();
        let primitives = lane_primitives(&h, node);
        assert!(matches!(primitives[0], Primitive::Shadow(_)));
        assert!(matches!(primitives[1], Primitive::Quad(_)));
    }

    #[test]
    fn hidden_node_paints_nothing_and_clears_lane() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let node = NodeBuilder::boxed()
            .style(fixed_style(50.0, 20.0))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();
        assert!(!h.scene.is_empty());

        let patch = StylePatch {
            visible: Some(false),
            ..StylePatch::default()
        };
        let _ = h.tree.set_style_patch_for_test(node, patch);
        h.mark_paint(node);
        let _ = h.paint();
        assert!(
            h.tree.lane_of(node).is_none(),
            "隐藏节点不留 lane（空 lane 只占内存/顺序）"
        );
        assert!(!h.damage.finish(frame()).full);
    }

    #[test]
    fn clip_brackets_lane_and_subtree() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let mut style = fixed_style(100.0, 100.0);
        style.clip = Clip::Bounds;
        let clipped = NodeBuilder::boxed()
            .style(style)
            .build(&mut h.tree, root)
            .expect("挂载");
        let child = NodeBuilder::boxed()
            .style(fixed_style(10.0, 10.0))
            .paint(with_white())
            .build(&mut h.tree, clipped)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();
        assert!(matches!(h.tree.clip_key(child), Some((1, Some(_)))));
        let primitives = lane_primitives(&h, child);
        assert!(matches!(primitives[0], Primitive::Clip(ClipOp::Push(_))));
        assert!(matches!(
            primitives.last(),
            Some(Primitive::Clip(ClipOp::Pop))
        ));
    }

    #[test]
    fn lane_is_reused_when_reusable() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let node = NodeBuilder::boxed()
            .style(fixed_style(50.0, 20.0))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();
        let before = lane_primitives(&h, node);
        let report = h.paint();
        assert_eq!(report.rebuilt, 0, "全部可复用");
        assert_eq!(lane_primitives(&h, node), before);
    }

    #[test]
    fn canvas_and_geometry_emit_primitives() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let canvas = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 10.0, 10.0)))
            .style(fixed_style(10.0, 10.0))
            .build(&mut h.tree, root)
            .expect("挂载");
        let geometry = NodeBuilder::geometry(GeometryId::for_test(0, 1))
            .style(fixed_style(10.0, 10.0))
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();
        assert!(matches!(
            lane_primitives(&h, canvas).first(),
            Some(Primitive::Canvas(_))
        ));
        assert!(matches!(
            lane_primitives(&h, geometry).first(),
            Some(Primitive::Geometry(_))
        ));
    }

    #[test]
    fn clip_key_mismatch_after_resize_or_reparent() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let mut style = fixed_style(100.0, 100.0);
        style.clip = Clip::Bounds;
        let clipped = NodeBuilder::boxed()
            .style(style)
            .build(&mut h.tree, root)
            .expect("挂载");
        let child = NodeBuilder::boxed()
            .style(fixed_style(10.0, 10.0))
            .paint(with_white())
            .build(&mut h.tree, clipped)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();
        let before = lane_primitives(&h, child);
        let first_rect = match &before[0] {
            Primitive::Clip(ClipOp::Push(clip)) => clip.rect,
            _ => panic!("应以 clip push 开始"),
        };

        // resize：只改 clip 容器的布局盒，子节点绝对盒不变。
        let resize = crate::node::tree::LayoutBox {
            bounds: Rect::new(0.0, 0.0, 60.0, 60.0),
            border: crate::math::Edges::ZERO,
        };
        let _ =
            h.tree
                .set_layout_for_test(clipped, resize, crate::node::tree::LayoutBox::default());
        let report = h.paint();
        assert!(report.rebuilt > 0, "clip 容器尺寸变化必须触发重建");
        let after = lane_primitives(&h, child);
        let second_rect = match &after[0] {
            Primitive::Clip(ClipOp::Push(clip)) => clip.rect,
            _ => panic!("应以 clip push 开始"),
        };
        assert_ne!(first_rect, second_rect, "前缀矩形必须随 clip 容器更新");
        assert_eq!(
            h.tree.clip_key(child),
            Some((1, Some(second_rect.to_physical_covering(scale()))))
        );
    }

    #[test]
    fn reparent_under_clip_forces_rebuild() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let mut style = fixed_style(100.0, 100.0);
        style.clip = Clip::Bounds;
        let clipped = NodeBuilder::boxed()
            .style(style)
            .build(&mut h.tree, root)
            .expect("挂载");
        let child = NodeBuilder::boxed()
            .style(fixed_style(10.0, 10.0))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();
        assert_eq!(h.tree.clip_key(child), Some((0, None)));

        h.tree.append_child(clipped, child).expect("重挂");
        let _ = h.paint();
        let primitives = lane_primitives(&h, child);
        assert!(matches!(primitives[0], Primitive::Clip(ClipOp::Push(_))));
        assert!(matches!(h.tree.clip_key(child), Some((1, Some(_)))));
    }

    #[test]
    fn zero_size_clip_still_pushes_and_empties_damage() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let mut style = fixed_style(100.0, 100.0);
        style.clip = Clip::Bounds;
        let clipped = NodeBuilder::boxed()
            .style(style)
            .build(&mut h.tree, root)
            .expect("挂载");
        let child = NodeBuilder::boxed()
            .style(fixed_style(10.0, 10.0))
            .paint(with_white())
            .build(&mut h.tree, clipped)
            .expect("挂载");
        layout_all(&mut h.tree);
        // 0×0 的 clip 容器：padding box 为空，但仍必须入栈（B1）。
        let empty = crate::node::tree::LayoutBox {
            bounds: Rect::default(),
            border: crate::math::Edges::ZERO,
        };
        let _ = h
            .tree
            .set_layout_for_test(clipped, empty, crate::node::tree::LayoutBox::default());
        let report = h.paint();

        let primitives = lane_primitives(&h, child);
        match &primitives[0] {
            Primitive::Clip(ClipOp::Push(clip)) => {
                assert!(clip.rect.is_empty(), "空 padding box 必须原样入栈");
            }
            _ => panic!("子 lane 必须以 ClipPush 开始（即使 rect 为空）"),
        }
        assert!(
            h.tree.clip_key(child).is_some_and(|(depth, _)| depth == 1),
            "空 clip 也要计入深度"
        );
        assert_eq!(report.damage.rects, 0, "空 clip 内的 damage 必须被裁空");
        assert!(!report.damage.full);
    }

    #[test]
    fn transparent_gradient_emits_nothing() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let _node = NodeBuilder::boxed()
            .style(fixed_style(50.0, 20.0))
            .paint(PaintSpec {
                gradient: Some(Gradient {
                    from: Color::TRANSPARENT,
                    to: Color::TRANSPARENT,
                    kind: GradientKind::Vertical,
                }),
                ..PaintSpec::default()
            })
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let report = h.paint();
        assert_eq!(report.rebuilt, 0, "全透明渐变不产生像素");
        assert_eq!(report.damage.rects, 0);
    }
}

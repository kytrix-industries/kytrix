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
    error::EngineError,
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
    /// 静态几何注册表：几何内容的 damage 用它自己的 AABB（`GeometryDesc.bounds`），
    /// 而不是节点布局盒 —— 顶点可以在节点盒之外（`desc.bounds` 才是真实可见范围）。
    pub geometry: &'a crate::paint::geometry_store::GeometryStore,
    pub scene: &'a mut Scene,
    pub damage: &'a mut DamageAccumulator,
    pub scale: Scale,
    /// 本帧 framebuffer 的物理矩形（`DamageAccumulator::finish` 的面积阈值用）。
    pub frame: PhysicalRect,
    /// 本帧的 lane 绘制序（复用调用方的容量）。
    pub order: &'a mut Vec<LaneId>,
    /// 本帧的焦点节点（画 `Style.focus_ring`；`None` = 无焦点）。
    pub focused: Option<NodeId>,
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
    ///
    /// # Errors
    /// [`EngineError::ClipDepthExceeded`]：clip 嵌套超过 `MAX_CLIP_DEPTH`。release 也必须失败——
    /// 静默丢弃最外层 clip 会让子内容越界可见（P06 二轮 / P14 审查）。
    pub(crate) fn run(&mut self) -> Result<PaintReport, EngineError> {
        let tree = &mut *self.tree;
        let scene = &mut *self.scene;
        let damage = &mut *self.damage;
        let order = &mut *self.order;
        let scale = self.scale;
        let frame = self.frame;
        #[cfg(feature = "test-support")]
        let log = std::mem::take(&mut self.log);
        let mut context = PaintContext {
            geometry: self.geometry,
            tree,
            scene,
            damage,
            scale,
            order,
            focused: self.focused,
            #[cfg(feature = "test-support")]
            log,
            rebuilt: 0,
        };
        let mut clip = ClipContext::default();
        let root = context.tree.root();
        paint_node(&mut context, root, false, &mut clip, Point::ZERO)?;
        context.scene.set_order(context.order);
        let damage = context.damage.finish(frame);
        Ok(PaintReport {
            rebuilt: context.rebuilt,
            damage,
            #[cfg(feature = "test-support")]
            instances: 0,
            #[cfg(feature = "test-support")]
            log: context.log,
        })
    }
}

/// 一次遍历的借用包（避免 `&mut self` 与 `&self.tree` 的借用冲突）。
struct PaintContext<'a> {
    geometry: &'a crate::paint::geometry_store::GeometryStore,
    tree: &'a mut NodeTree,
    scene: &'a mut Scene,
    damage: &'a mut DamageAccumulator,
    scale: Scale,
    order: &'a mut Vec<LaneId>,
    focused: Option<NodeId>,
    #[cfg(feature = "test-support")]
    log: PaintLog,
    rebuilt: usize,
}

/// 祖先 clip 链（外层在前）+ 物理 covering 交集。
#[derive(Default)]
struct ClipContext {
    rects: SmallVec<[ClipRect; 8]>,
    /// 与 push 次数等长的回溯记录：`Some(prev)` = 这次 push 是折叠（`rects` 未增长，
    /// 需要把栈顶矩形还原成 `prev`）；`None` = 正常压栈，`pop` 时弹掉 `rects` 末尾。
    pushed: SmallVec<[Option<ClipRect>; 8]>,
    covering: Option<PhysicalRect>,
}

impl ClipContext {
    fn key(&self) -> (u8, Option<PhysicalRect>) {
        (self.rects.len() as u8, self.covering)
    }

    /// 进入一个 `Clip::Bounds` 节点：按它的 **padding box** 裁剪子树。
    ///
    /// 深度达到 `MAX_CLIP_DEPTH` 时**折叠**为「与栈顶求交」而不是失败或丢弃（P2-7）：
    /// 轴对齐矩形求交是**无损**的（两个矩形 clip 的复合就是它们的交集），而 `covering`
    /// 本来就是全部矩形的交集。因此 8 层以上嵌套裁剪容器不需要牺牲精度、也不需要让整帧
    /// 失败 —— 旧实现在这里返回 `ClipDepthExceeded`，8 层嵌套的滚动容器会白屏。
    fn push(&mut self, padding_box: Rect, scale: Scale) {
        let clip = ClipRect {
            rect: padding_box,
            corner_radius: [0.0; 4],
        };
        if self.rects.len() >= usize::from(crate::paint::scene::MAX_CLIP_DEPTH) {
            let last = self.rects.last_mut().expect("刚刚判过非空");
            let previous = *last;
            last.rect = last.rect.intersect(clip.rect);
            last.corner_radius = [0.0; 4];
            // 回溯时要把求交前的矩形还原，否则父层的 clip 会被子层的矩形永久收窄
            // （兄弟子树就会被过度裁剪）。
            self.pushed.push(Some(previous));
            self.rebuild_covering(scale);
            return;
        }
        self.rects.push(clip);
        self.pushed.push(None);
        self.rebuild_covering(scale);
    }

    fn pop(&mut self, scale: Scale) {
        match self.pushed.pop() {
            Some(None) => {
                let _ = self.rects.pop();
            }
            Some(Some(previous)) => {
                if let Some(last) = self.rects.last_mut() {
                    *last = previous;
                }
            }
            None => return,
        }
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
    Canvas(Rc<crate::paint::canvas::Canvas>),
    Geometry(crate::paint::geometry::GeometryId),
}

/// 该内容是否会产生图元（决定要不要为它压一层内容 clip）。
///
/// `rebuild_lane` 的 clip 深度校验与 `PrimitiveEmitter::emit_content` 必须用**同一个**
/// 判据，否则两处对「这一层占不占深度」的判断不一致。
fn content_needs_clip(content: &Content) -> bool {
    match content {
        Content::Box => false,
        Content::Text(line) => line.is_some(),
        Content::Canvas(_) | Content::Geometry(_) => true,
    }
}

fn paint_node(
    context: &mut PaintContext<'_>,
    node: NodeId,
    parent_hidden: bool,
    clip: &mut ClipContext,
    offset: Point,
) -> Result<(), EngineError> {
    let Ok(target) = context.tree.get(node) else {
        return Ok(());
    };
    // 不可见的**定义只有一处**：`NodeTree::is_visible_locally`（`display: none` 或
    // `visible: false`）。命中测试、焦点遍历、a11y 都用它，并且都按「隐藏整棵子树」处理；
    // 旧实现的 paint 只对 `display: none` 隐藏子树，`visible: false` 仅跳过本节点自身的图元
    // —— 于是容器的背景消失、子控件仍然画出来，而那些子控件在命中测试里是不可见的
    // （「看得见但点不动」的死像素，P2-3.2）。
    let hidden = parent_hidden
        || matches!(target.style().inner.display, taffy::style::Display::None)
        || target.style_patch().visible == Some(false);
    if hidden {
        hide_subtree(context, node, clip, offset);
        return Ok(());
    }
    let current_key = clip.key();
    // 复用判据（§5.6 + P10 §7.6）：`REUSABLE` + clip 上下文一致 + **滚动偏移戳一致**（祖先滚动会移动
    // 全部后代图元，因此偏移变化必须重建）。**不要求** lane 存在：从未产生图元的节点没有 lane，
    // 但它的 `clip_key` 已写回，仍可整帧跳过（否则空节点每帧都要重建）。
    let reusable = target.dirty().contains(DirtyFlags::REUSABLE)
        && context.tree.clip_key(node) == Some(current_key)
        && context.tree.lane_offset(node) == offset;

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
        rebuild_lane(context, node, clip, offset)?;
    }

    let (children, clip_bounds) = {
        let Ok(target) = context.tree.get(node) else {
            return Ok(());
        };
        let children: SmallVec<[NodeId; 4]> = target.children().iter().copied().collect();
        // B1：0×0 的 padding box **也要入栈**（depth+1、covering 交空）——子 lane 由此带空前缀、
        // damage 全部被裁空，与 `hit_test` 的 clip 链语义一致；跳过它会让子内容"越界可见"。
        let clip_bounds = (target.clip() == Clip::Bounds).then(|| {
            target
                .layout()
                .bounds
                .translate(negate(offset))
                .inset(target.layout().border)
        });
        (children, clip_bounds)
    };
    if let Some(padding_box) = clip_bounds {
        // 深度上限由 `ClipContext::push` 自己处理：达到上限时与栈顶矩形求交（无损），
        // 既不失败也不丢弃（P2-7）。
        clip.push(padding_box, context.scale);
    }
    // 子节点的参照系：容器挂了滚动槽时，内容整体按 `offset` 上移（屏幕 = 内容 − 累计偏移）。
    let child_offset = offset
        + context
            .tree
            .scroll_slot(node)
            .map_or(Point::ZERO, |slot| slot.offset);
    for child in children {
        paint_node(context, child, false, clip, child_offset)?;
    }
    if clip_bounds.is_some() {
        clip.pop(context.scale);
    }
    Ok(())
}

/// 隐藏子树：清空每条 lane（保留分配），并把旧像素并入 damage；`clip_key` 置空
/// （再次显示时必然失配 ⇒ 重建，避免"复用空 lane"）。
fn hide_subtree(context: &mut PaintContext<'_>, node: NodeId, clip: &ClipContext, offset: Point) {
    let (children, damage_rect, child_offset) = {
        let Ok(target) = context.tree.get(node) else {
            return;
        };
        let children: SmallVec<[NodeId; 4]> = target.children().iter().copied().collect();
        // damage 与 bounds 都在**内容坐标**：先并集，再减祖先滚动偏移得到屏幕坐标（P10 §7.2）。
        let mut damage_rect = target.damage().union(target.layout().bounds);
        // 几何的可见范围可能超出节点布局盒：隐藏时也必须把 `desc.bounds` 并入
        // damage，否则溢出部分留下旧像素（「damage 帧不清屏」会长期残留；复审 A3）。
        if let NodeKind::Geometry(id) = *target.kind()
            && let Some(desc) = context.geometry.desc_arc(id)
        {
            damage_rect = damage_rect.union(desc.bounds);
        }
        // damage 与 bounds 都在**内容坐标**：先并集，再减祖先滚动偏移得到屏幕坐标（P10 §7.2）。
        let damage_rect = damage_rect.translate(negate(offset));
        let child_offset = offset
            + context
                .tree
                .scroll_slot(node)
                .map_or(Point::ZERO, |slot| slot.offset);
        (children, damage_rect, child_offset)
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
        hide_subtree(context, child, clip, child_offset);
    }
}

fn rebuild_lane(
    context: &mut PaintContext<'_>,
    node: NodeId,
    clip: &ClipContext,
    offset: Point,
) -> Result<(), EngineError> {
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

    let (bounds, content_offset, patch, spec, content, node_damage, ring, content_clip) = {
        let Ok(target) = context.tree.get(node) else {
            return Ok(());
        };
        let content = match target.kind() {
            NodeKind::Box => Content::Box,
            NodeKind::Text(_) => Content::Text(context.tree.text_line(node).cloned()),
            NodeKind::Canvas(canvas) => Content::Canvas(Rc::clone(canvas)),
            NodeKind::Geometry(id) => Content::Geometry(*id),
        };
        // 屏幕坐标 = 内容坐标 − 祖先滚动偏移（P10 §7.2）。
        let bounds = target.layout().bounds.translate(negate(offset));
        let patch = target.style_patch().clone();
        let spec = target.paint_spec().cloned();
        let node_damage = target.damage();
        let content_offset = context.tree.content_offset(node);
        let content_clip =
            (target.clip() == Clip::Bounds).then(|| bounds.inset(target.layout().border));
        let ring = (context.focused == Some(node))
            .then(|| context.tree.focus_ring(node))
            .flatten();
        (
            bounds,
            content_offset,
            patch,
            spec,
            content,
            node_damage,
            ring,
            content_clip,
        )
    };

    // 内容 clip 的层数由 `ClipContext::push` 折叠处理（达到上限时求交而不是失败），
    // 这里不需要再校验深度：8 层以上的嵌套裁剪容器会退化成一条求交后的矩形。
    for clip_rect in &clip.rects {
        let _ = context
            .scene
            .push(lane, Primitive::Clip(ClipOp::Push(*clip_rect)));
    }

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
        offset,
        ring,
        content_clip,
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
    context.tree.set_lane_offset(node, offset);
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
    Ok(())
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
    /// 本帧累积的节点级 damage（**内容坐标**；`touch`/`absolutize` 写入）。
    node_damage: Rect,
    /// 该节点的祖先滚动偏移（把 `node_damage` 换算到屏幕坐标）。
    offset: Point,
    /// 焦点环（仅焦点节点 + `Style.focus_ring` 时存在）。
    ring: Option<crate::paint::style::FocusRing>,
    /// 本节点**自身**的 `Clip::Bounds` padding box（屏幕坐标）。
    ///
    /// 只作用于**内容**（文本 / Canvas / 几何），不作用于背景/边框/阴影 —— 与 CSS 一致：
    /// `overflow` 裁剪的是元素内容，不是元素自己的装饰。旧实现只把 clip 压给子树，
    /// 于是「容器剪不住自己的 Canvas」：图表的柱子会溢出到容器外（P2-3.2）。
    content_clip: Option<Rect>,
}

/// 取负（`Point` 没有 `Neg`；屏幕 = 内容 − 偏移）。
fn negate(offset: Point) -> Point {
    Point::new(-offset.x, -offset.y)
}

/// 几何内容的 damage 范围：优先用注册表里的 `GeometryDesc.bounds`（真实可见范围，
/// 逻辑窗口坐标），句柄陈旧/未注册时退回节点布局盒。
fn geometry_damage(
    geometry: &crate::paint::geometry_store::GeometryStore,
    id: crate::paint::geometry::GeometryId,
    node_bounds: Rect,
    offset: Point,
) -> Rect {
    geometry
        .desc_arc(id)
        .map_or(node_bounds, |desc| desc.bounds.translate(negate(offset)))
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
                let damage = self.node_damage.translate(negate(self.offset));
                context.damage.push(damage, 1.0, self.clip);
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

        // 焦点环：在**节点自己的 lane 内**画（受祖先 clip 约束），环在 border box 外侧。
        if let Some(ring) = self.ring
            && ring.width > 0.0
            && !ring.color.with_opacity(opacity).is_transparent()
        {
            let mut radii = radii;
            for radius in &mut radii {
                *radius += ring.offset;
            }
            let bounds = self.bounds.outset(ring.offset);
            self.push(
                context,
                Primitive::Quad(Quad {
                    bounds,
                    corner_radius: radii,
                    paint: QuadPaint::Solid {
                        fill: Color::TRANSPARENT,
                        border: Some(crate::paint::primitive::Border {
                            width: ring.width,
                            color: ring.color.with_opacity(opacity),
                        }),
                    },
                }),
            );
            context.damage.push(bounds, 1.0, self.clip);
        }

        if self.emitted > 0 || !self.node_damage.is_empty() {
            let damage = self.node_damage.translate(negate(self.offset));
            context.damage.push(damage, 1.0, self.clip);
        }
    }

    fn emit_content(&mut self, context: &mut PaintContext<'_>, opacity: f32) {
        // 内容按本节点自己的 padding box 裁剪（`Clip::Bounds`）。背景/边框/阴影在
        // `emit` 里先画、不受这个 clip 影响（CSS 的 `overflow` 语义）。
        //
        // 只在**确实有内容**时压括号：`Content::Box`（纯盒子）与还没 shape 好的文本不产生
        // 图元，白压一对 `Clip` 会白占一层 clip 深度（嵌套容器很容易因此提前触顶）。
        let has_content = content_needs_clip(&self.content);
        let content_clip = self
            .content_clip
            .filter(|_| has_content)
            .map(|rect| ClipRect {
                rect,
                // 内容裁剪只按轴对齐矩形裁（与子树 clip 同一形状，圆角留空）。
                corner_radius: [0.0; 4],
            });
        if let Some(clip_rect) = content_clip {
            let _ = context
                .scene
                .push(self.lane, Primitive::Clip(ClipOp::Push(clip_rect)));
        }
        match std::mem::replace(&mut self.content, Content::Box) {
            Content::Box => {}
            Content::Geometry(id) => {
                self.push(
                    context,
                    Primitive::Geometry(GeometryDraw {
                        id,
                        transform: crate::math::Transform::IDENTITY,
                        color: self
                            .patch
                            .foreground
                            .unwrap_or(Color::WHITE)
                            .with_opacity(opacity),
                    }),
                );
                // 几何的可见范围是它**自己的** AABB（`desc.bounds`，逻辑窗口坐标），
                // 可能超出节点布局盒。旧实现按节点盒推 damage，于是几何溢出部分在局部
                // 刷新时不会被重画（残影，复审 P2-2）。节点盒仍是命中测试的范围。
                let geometry_bounds =
                    geometry_damage(context.geometry, id, self.bounds, self.offset);
                context.damage.push(geometry_bounds, 1.0, self.clip);
            }
            Content::Canvas(canvas) => {
                let canvas_bounds = canvas.bounds();
                self.push(context, Primitive::Canvas(Rc::clone(&canvas)));
                context.damage.push(canvas_bounds, 1.0, self.clip);
            }
            Content::Text(line) => {
                // 不早退：早退会跳过下面的 `Clip(Pop)`，把 clip 泄漏给后续节点。
                if let Some(line) = line {
                    self.emit_text(context, &line, opacity);
                }
            }
        }
        if content_clip.is_some() {
            let _ = context.scene.push(self.lane, Primitive::Clip(ClipOp::Pop));
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

        fn try_paint(&mut self) -> Result<super::PaintReport, crate::error::EngineError> {
            self.damage.reset(scale());
            self.order.clear();
            let geometry_store = crate::paint::geometry_store::GeometryStore::new();
            let mut pass = PaintPass {
                tree: &mut self.tree,
                geometry: &geometry_store,
                scene: &mut self.scene,
                damage: &mut self.damage,
                scale: scale(),
                frame: frame(),
                order: &mut self.order,
                focused: None,
                #[cfg(feature = "test-support")]
                log: crate::test_support::record::PaintLog::default(),
            };
            pass.run()
        }

        fn paint(&mut self) -> super::PaintReport {
            self.try_paint().expect("paint")
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

    /// P2-2：几何 damage 必须用 `GeometryDesc.bounds`（真实可见范围），而不是节点布局盒。
    #[test]
    fn geometry_damage_uses_the_registered_bounds() {
        use crate::paint::geometry::{GeometryDesc, GeometryPipeline, IndexType, Topology};
        let stride = GeometryPipeline::Ui.vertex_stride() as usize;
        let desc = GeometryDesc {
            vertex_bytes: bytes::Bytes::from(vec![0u8; stride * 3]),
            index_bytes: bytes::Bytes::from(vec![0u8, 0, 1, 0, 2, 0]),
            vertex_stride: stride as u32,
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            // 几何真实范围 200×200，节点布局盒只有 10×10（溢出）。
            bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
            pipeline: GeometryPipeline::Ui,
        };
        let mut store = crate::paint::geometry_store::GeometryStore::new();
        let id = store.insert(desc).expect("valid geometry");
        let node_bounds = Rect::new(0.0, 0.0, 10.0, 10.0);

        let damage = super::geometry_damage(&store, id, node_bounds, Point::ZERO);
        assert_eq!(damage, Rect::new(0.0, 0.0, 200.0, 200.0), "用 desc.bounds");

        // 祖先滚动偏移要换算到屏幕坐标。
        let scrolled = super::geometry_damage(&store, id, node_bounds, Point::new(0.0, 50.0));
        assert_eq!(scrolled, Rect::new(0.0, -50.0, 200.0, 200.0));

        // 陈旧句柄退回节点盒（不会因为查不到就丢掉 damage）。
        let stale = super::geometry_damage(
            &store,
            crate::paint::geometry::GeometryId::for_test(99, 1),
            node_bounds,
            Point::ZERO,
        );
        assert_eq!(stale, node_bounds);
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
        // 与 `display: none` 同一条 `hide_subtree` 路径：lane 保留分配但被清空，
        // 因此断言「没有图元」而不是「没有 lane」（后者是旧实现 rebuild_lane 的副作用）。
        assert!(
            h.tree
                .lane_of(node)
                .and_then(|lane| h.scene.lane_primitives(lane))
                .is_none_or(<[Primitive]>::is_empty),
            "隐藏节点不得留下任何图元"
        );
        assert!(h.scene.is_empty(), "隐藏之后场景里不应还有任何内容");
        assert!(!h.damage.finish(frame()).full);
    }

    /// P2-3.2 回归：`visible: false` 隐藏**整棵子树**，与命中测试/焦点遍历/a11y 同一语义。
    ///
    /// 旧实现只跳过本节点自身的图元，子控件仍然被画出来 —— 而它们在命中测试里是不可见的，
    /// 于是出现「看得见但点不动」的死像素。
    #[test]
    fn hiding_a_container_hides_its_children() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let container = NodeBuilder::boxed()
            .style(fixed_style(50.0, 20.0))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("挂载容器");
        let child = NodeBuilder::boxed()
            .style(fixed_style(10.0, 10.0))
            .paint(with_white())
            .build(&mut h.tree, container)
            .expect("挂载子节点");
        layout_all(&mut h.tree);
        let _ = h.paint();
        assert!(!h.scene.is_empty(), "初始状态两者都画出来");

        let _ = h.tree.set_style_patch_for_test(
            container,
            StylePatch {
                visible: Some(false),
                ..StylePatch::default()
            },
        );
        h.mark_paint(container);
        let _ = h.paint();
        assert!(
            h.scene.is_empty(),
            "容器隐藏后子控件也必须消失（旧的『只隐藏自身』会留下子控件）"
        );
        let _ = child;
    }
    /// P2-3.2 回归：`Clip::Bounds` 必须裁剪节点**自身的内容**。
    ///
    /// 旧实现只把 clip 压给子树，于是「容器剪不住自己的 Canvas/文本」：图表的柱子会溢出
    /// 到容器外。CSS 语义里 `overflow` 裁的是内容，装饰（背景/边框）不受影响。
    #[test]
    fn own_clip_brackets_the_node_content() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let mut style = fixed_style(60.0, 20.0);
        style.clip = Clip::Bounds;
        // 用 Canvas 作内容：paint harness 不做文本 shape，文本内容为空不会产生图元。
        let mut canvas = crate::paint::canvas::Canvas::new(Rect::new(0.0, 0.0, 200.0, 200.0));
        canvas.push_quad(crate::paint::primitive::Quad {
            bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
            corner_radius: [0.0; 4],
            paint: QuadPaint::Solid {
                fill: crate::paint::color::Color::WHITE,
                border: None,
            },
        });
        let clipped = NodeBuilder::canvas(canvas)
            .style(style)
            .build(&mut h.tree, root)
            .expect("挂载");
        // 对照：同样形状但**不**裁剪的 Canvas 节点不应有额外括号。
        let mut plain_canvas = crate::paint::canvas::Canvas::new(Rect::new(0.0, 0.0, 200.0, 200.0));
        plain_canvas.push_quad(crate::paint::primitive::Quad {
            bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
            corner_radius: [0.0; 4],
            paint: QuadPaint::Solid {
                fill: crate::paint::color::Color::WHITE,
                border: None,
            },
        });
        let plain = NodeBuilder::canvas(plain_canvas)
            .style(fixed_style(60.0, 20.0))
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();

        let primitives = lane_primitives(&h, clipped);
        assert!(
            matches!(primitives.first(), Some(Primitive::Clip(ClipOp::Push(_)))),
            "内容前必须有自身 padding box 的 clip：{primitives:?}"
        );
        assert!(
            matches!(primitives.last(), Some(Primitive::Clip(ClipOp::Pop))),
            "内容 clip 必须闭合：{primitives:?}"
        );
        assert!(
            primitives
                .iter()
                .any(|primitive| matches!(primitive, Primitive::Canvas(_))),
            "Canvas 内容必须在括号内"
        );
        let plain_primitives = lane_primitives(&h, plain);
        assert!(
            !plain_primitives
                .iter()
                .any(|primitive| matches!(primitive, Primitive::Clip(_))),
            "不裁剪的节点不得额外压 clip：{plain_primitives:?}"
        );
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

    /// 叠 `depth` 层 `Clip::Bounds`，返回最内层节点。
    fn nest_clips(h: &mut Harness, depth: usize) -> crate::node::id::NodeId {
        let root = h.tree.root();
        let mut parent = root;
        for _ in 0..depth {
            let mut style = fixed_style(100.0, 100.0);
            style.clip = Clip::Bounds;
            parent = NodeBuilder::boxed()
                .style(style)
                .build(&mut h.tree, parent)
                .expect("挂载");
        }
        layout_all(&mut h.tree);
        parent
    }

    #[test]
    fn clip_depth_at_limit_is_allowed() {
        let mut h = Harness::new();
        let deepest = nest_clips(&mut h, usize::from(crate::paint::scene::MAX_CLIP_DEPTH));
        // 最内层节点的祖先链 = 7 层（第 8 层是它自己的子树），允许。
        let _ = h.paint();
        assert!(
            h.tree
                .clip_key(deepest)
                .is_some_and(|(depth, _)| depth == 7)
        );
    }

    #[test]
    fn empty_clipping_containers_do_not_consume_clip_depth() {
        // 纯 `Box` 容器不产图元 ⇒ 也不压内容 clip。旧实现按「有 `Clip::Bounds` 就算一层」
        // 校验深度，8 层嵌套的空容器会被误判超深（`ClipDepthExceeded`），实际它一个图元
        // 都没有、根本不需要 clip。
        let mut h = Harness::new();
        let _ = nest_clips(&mut h, usize::from(crate::paint::scene::MAX_CLIP_DEPTH));
        // `h.paint()` 内部会 `expect` 成功：这里唯一要验的就是**不报** `ClipDepthExceeded`。
        // 各层 lane 里仍有「祖先 clip 的括号」原始图元，所以不能断言 rebuilt == 0。
        let _ = h.paint();
    }

    /// P2-7 回归：超过 `MAX_CLIP_DEPTH` 的嵌套裁剪容器**折叠**而不是失败。
    ///
    /// 旧实现在这里返回 `ClipDepthExceeded` 让整帧中止 —— 8 层嵌套的滚动容器（深层
    /// 面板 + 列表 + 弹层）会直接白屏。矩形 clip 的复合就是交集，求交是无损的，因此
    /// 折叠既保住精度也不牺牲可用性。
    #[test]
    fn clip_depth_over_limit_folds_instead_of_failing() {
        let mut h = Harness::new();
        let deepest = nest_clips(&mut h, usize::from(crate::paint::scene::MAX_CLIP_DEPTH) + 1);
        let report = h.paint();
        // 折叠后每一层都还在（线段里仍有 clip 图元），关键是没有报错、没有丢绘制。
        assert!(report.rebuilt > 0, "折叠后必须仍能绘制");
        // 最深层带内容时也要能画：这里只验证 clip 上下文结构（`clip_key` 深度被折叠到上限）。
        assert!(
            h.tree
                .clip_key(deepest)
                .is_some_and(|(depth, _)| usize::from(depth)
                    <= usize::from(crate::paint::scene::MAX_CLIP_DEPTH)),
            "折叠后每层的 clip 深度不得超过上限"
        );
    }
}

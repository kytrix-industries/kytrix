use std::{rc::Rc, sync::Arc};

use smallvec::SmallVec;

#[cfg(feature = "test-support")]
use crate::test_support::record::{LaneOutcome, LanePaintEntry, PaintLog};
use crate::{
    error::EngineError,
    math::{
        DevicePixelRatio, DeviceRect, LogicalPoint, LogicalPx, LogicalRect, PhysicalPx, dp,
        physical_px, px,
    },
    node::{
        id::NodeId,
        image::{ImageContent, ImageFit, cover_tile, fit_bounds, variant_target},
        tree::{BakedGeometryChunk, NodeKind, NodeTree, PaintNodeState},
    },
    paint::{
        color::Color,
        primitive::{
            ClipOp, ClipRect, GeometryDraw, GlyphRun, Gradient, Image, PositionedGlyph, Primitive,
            Quad, QuadPaint, Shadow,
        },
        scene::{LaneId, Scene},
        style::{Clip, PaintSpec, StylePatch},
    },
    render::damage::{DamageAccumulator, DamageSummary},
    resources::ResourceCache,
    text::{InlineBoxPaint, ShapedLine, ShapedTextItem},
};

pub(crate) struct PaintPass<'a> {
    pub tree: &'a mut NodeTree,
    pub geometry: &'a crate::paint::geometry_store::GeometryStore,
    pub scene: &'a mut Scene,
    pub damage: &'a mut DamageAccumulator,
    pub scale: DevicePixelRatio,
    pub frame: DeviceRect,
    pub order: &'a mut Vec<LaneId>,
    pub focused: Option<NodeId>,
    pub atlases: Option<crate::window::canvas_text::GlyphAtlases<'a>>,
    pub resources: Option<&'a ResourceCache>,
    pub canvas_text: &'a ahash::HashMap<NodeId, crate::window::canvas_text::ResolvedCanvasText>,
    pub scroll_slots: &'a mut Vec<(LogicalPoint, DeviceRect)>,
    pub scroll_slot_nodes: &'a mut Vec<NodeId>,
    #[cfg(feature = "test-support")]
    pub log: PaintLog,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaintMode {
    Full,
    Incremental,
}

impl PaintMode {
    const fn is_full(self) -> bool {
        matches!(self, Self::Full)
    }
}

pub(crate) struct PaintReport {
    pub rebuilt: usize,
    pub damage: DamageSummary,
    pub stale_text: SmallVec<[NodeId; 4]>,
    pub unresolved_canvas_text: usize,
    pub skipped_inline_images: SmallVec<[u64; 4]>,
    #[cfg(feature = "test-support")]
    pub instances: usize,
    #[cfg(feature = "test-support")]
    pub encoded_lanes: usize,
    #[cfg(feature = "test-support")]
    pub reused_segments: usize,
    #[cfg(feature = "test-support")]
    pub uploaded_bytes: usize,
    #[cfg(feature = "test-support")]
    pub geometry_encoded_chunks: usize,
    #[cfg(feature = "test-support")]
    pub geometry_uploaded_bytes: usize,
    #[cfg(feature = "test-support")]
    pub scroll_slots: usize,
    #[cfg(feature = "test-support")]
    pub log: PaintLog,
    #[cfg(feature = "test-support")]
    pub delta: crate::test_support::record::DeltaRecord,
}

impl PaintPass<'_> {
    pub(crate) fn run(&mut self) -> Result<PaintReport, EngineError> {
        self.execute(PaintMode::Full)
    }

    pub(crate) fn run_incremental(&mut self) -> Result<PaintReport, EngineError> {
        self.execute(PaintMode::Incremental)
    }

    fn execute(&mut self, mode: PaintMode) -> Result<PaintReport, EngineError> {
        let tree = &mut *self.tree;
        let scene = &mut *self.scene;
        let damage = &mut *self.damage;
        let order = &mut *self.order;
        let scroll_slots = &mut *self.scroll_slots;
        let scroll_slot_nodes = &mut *self.scroll_slot_nodes;
        let scale = self.scale;
        let frame = self.frame;
        #[cfg(feature = "test-support")]
        let log = std::mem::take(&mut self.log);
        if mode.is_full() {
            order.clear();
            tree.begin_full_scroll_slots();
            scroll_slots.clear();
            scroll_slots.push((
                LogicalPoint::ZERO,
                DeviceRect::new(dp(0), dp(0), dp(0), dp(0)),
            ));
            scroll_slot_nodes.clear();
        }
        let mut context = PaintContext {
            geometry: self.geometry,
            tree,
            scene,
            damage,
            scale,
            mode,
            order,
            scroll_slots,
            scroll_slot_nodes,
            focused: self.focused,
            atlases: self.atlases,
            resources: self.resources,
            canvas_text: self.canvas_text,
            stale_text: SmallVec::new(),
            unresolved_canvas_text: 0,
            skipped_inline_images: SmallVec::new(),
            #[cfg(feature = "test-support")]
            log,
            rebuilt: 0,
            visited: 0,
            order_changed: false,
            baked_scratch: SmallVec::new(),
            rebuild_scratch: SmallVec::new(),
        };
        let mut clip = ClipContext::default();
        let root = context.tree.root();
        paint_node(
            &mut context,
            root,
            false,
            &mut clip,
            LogicalPoint::ZERO,
            LogicalPoint::ZERO,
            0,
            0,
            1.0,
            mode.is_full(),
        )?;
        match context.mode {
            PaintMode::Full => context.scene.set_order(context.order),
            PaintMode::Incremental if context.order_changed => {
                rebuild_order_only(context.tree, context.order);
                context.scene.set_order(context.order);
                context.tree.note_order_rebuild();
            }
            PaintMode::Incremental => {}
        }
        let damage = context.damage.finish(frame);
        context
            .tree
            .note_paint_frame(context.mode.is_full(), context.visited);
        Ok(PaintReport {
            rebuilt: context.rebuilt,
            damage,
            stale_text: context.stale_text,
            unresolved_canvas_text: context.unresolved_canvas_text,
            skipped_inline_images: context.skipped_inline_images,
            #[cfg(feature = "test-support")]
            instances: 0,
            #[cfg(feature = "test-support")]
            encoded_lanes: 0,
            #[cfg(feature = "test-support")]
            reused_segments: 0,
            #[cfg(feature = "test-support")]
            uploaded_bytes: 0,
            #[cfg(feature = "test-support")]
            geometry_encoded_chunks: 0,
            #[cfg(feature = "test-support")]
            geometry_uploaded_bytes: 0,
            #[cfg(feature = "test-support")]
            scroll_slots: 0,
            #[cfg(feature = "test-support")]
            log: context.log,
            #[cfg(feature = "test-support")]
            delta: crate::test_support::record::DeltaRecord::default(),
        })
    }
}

struct PaintContext<'a> {
    geometry: &'a crate::paint::geometry_store::GeometryStore,
    tree: &'a mut NodeTree,
    scene: &'a mut Scene,
    damage: &'a mut DamageAccumulator,
    scale: DevicePixelRatio,
    mode: PaintMode,
    order: &'a mut Vec<LaneId>,
    scroll_slots: &'a mut Vec<(LogicalPoint, DeviceRect)>,
    scroll_slot_nodes: &'a mut Vec<NodeId>,
    focused: Option<NodeId>,
    atlases: Option<crate::window::canvas_text::GlyphAtlases<'a>>,
    canvas_text: &'a ahash::HashMap<NodeId, crate::window::canvas_text::ResolvedCanvasText>,
    stale_text: SmallVec<[NodeId; 4]>,
    unresolved_canvas_text: usize,
    skipped_inline_images: SmallVec<[u64; 4]>,
    resources: Option<&'a ResourceCache>,
    #[cfg(feature = "test-support")]
    log: PaintLog,
    rebuilt: usize,
    visited: usize,
    order_changed: bool,
    baked_scratch: SmallVec<[crate::node::tree::BakedGeometryChunk; 2]>,
    rebuild_scratch: SmallVec<[bool; 8]>,
}

fn offset_between(total: LogicalPoint, slot_offset: LogicalPoint) -> LogicalPoint {
    LogicalPoint::new(total.x - slot_offset.x, total.y - slot_offset.y)
}

#[derive(Default)]
struct ClipContext {
    rects: SmallVec<[ClipRect; 8]>,
    pushed: SmallVec<[Option<ClipRect>; 8]>,
    covering: Option<DeviceRect>,
}

impl ClipContext {
    fn key(&self) -> (u8, Option<DeviceRect>) {
        (self.rects.len() as u8, self.covering)
    }

    fn push(&mut self, padding_box: LogicalRect, scale: DevicePixelRatio) {
        let clip = ClipRect {
            rect: padding_box,
            corner_radius: [LogicalPx::ZERO; 4],
        };
        if self.rects.len() >= usize::from(crate::paint::scene::MAX_CLIP_DEPTH) {
            let last = self.rects.last_mut().expect("刚刚判过非空");
            let previous = *last;
            last.rect = last.rect.intersect(clip.rect);
            last.corner_radius = [LogicalPx::ZERO; 4];
            self.pushed.push(Some(previous));
            self.rebuild_covering(scale);
            return;
        }
        self.rects.push(clip);
        self.pushed.push(None);
        self.rebuild_covering(scale);
    }

    fn pop(&mut self, scale: DevicePixelRatio) {
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

    fn rebuild_covering(&mut self, scale: DevicePixelRatio) {
        let mut covering: Option<DeviceRect> = None;
        for rect in &self.rects {
            let physical = scale.to_covering_device_rect(rect.rect);
            covering = Some(match covering {
                Some(parent) => parent.intersect(physical),
                None => physical,
            });
        }
        self.covering = covering;
    }
}

enum Content {
    Box,
    Text(Option<Rc<ShapedLine>>),
    Canvas(Arc<crate::paint::canvas::Canvas>),
    Image(ImageContent),
}

fn content_needs_clip(content: &Content) -> bool {
    match content {
        Content::Box => false,
        Content::Text(line) => line.is_some(),
        Content::Canvas(_) => true,
        Content::Image(_) => false,
    }
}

fn canvas_has_text(canvas: &crate::paint::canvas::Canvas) -> bool {
    canvas.primitives().iter().any(|primitive| match primitive {
        Primitive::Text(_) => true,
        Primitive::Canvas(inner) => canvas_has_text(inner),
        _ => false,
    })
}

fn resolved_canvas(
    canvas: &crate::paint::canvas::Canvas,
    lines: Option<&[Rc<ShapedLine>]>,
    atlases: Option<crate::window::canvas_text::GlyphAtlases<'_>>,
    next: &mut usize,
    skipped: &mut usize,
) -> crate::paint::canvas::Canvas {
    let mut resolved = crate::paint::canvas::Canvas::new(canvas.bounds());
    for primitive in canvas.primitives() {
        match primitive {
            Primitive::Text(text) => {
                let line = lines.and_then(|lines| lines.get(*next));
                *next += 1;
                let live =
                    line.filter(|line| atlases.is_none_or(|atlases| atlases.line_is_live(line)));
                match live {
                    Some(line) => push_text_glyph_runs(&mut resolved, text, line),
                    None => *skipped += 1,
                }
            }
            Primitive::Canvas(inner) => {
                let inner = resolved_canvas(inner, lines, atlases, next, skipped);
                resolved.push_primitive(Primitive::Canvas(Arc::new(inner)));
            }
            other => resolved.push_primitive(other.clone()),
        }
    }
    resolved
}
fn push_text_glyph_runs(
    canvas: &mut crate::paint::canvas::Canvas,
    text: &crate::paint::canvas::CanvasText,
    line: &ShapedLine,
) {
    for shaped_line in &line.lines {
        for item in &shaped_line.items {
            let ShapedTextItem::GlyphRun(run) = item else {
                continue;
            };
            if run.glyphs.is_empty() {
                continue;
            }
            let mut glyphs = Vec::with_capacity(run.glyphs.len());
            for glyph in &run.glyphs {
                glyphs.push(PositionedGlyph {
                    tile: glyph.tile,
                    position: LogicalPoint::new(
                        text.origin.x + glyph.position.x,
                        text.origin.y + glyph.position.y,
                    ),
                    size: glyph.size,
                });
            }
            canvas.push_glyph_run(GlyphRun {
                color: run.color,
                glyphs,
            });
        }
    }
}

fn paint_node(
    context: &mut PaintContext<'_>,
    node: NodeId,
    parent_hidden: bool,
    clip: &mut ClipContext,
    total: LogicalPoint,
    slot_offset: LogicalPoint,
    slot: u32,
    boundary: usize,
    inherited_opacity: f32,
    force: bool,
) -> Result<(), EngineError> {
    if !force && !context.tree.paint_marked(node) {
        return Ok(());
    }
    context.visited += 1;
    let current_key = clip.key();
    let Some(state) = context.tree.paint_state(
        node,
        total,
        offset_between(total, slot_offset),
        current_key,
        inherited_opacity,
    ) else {
        return Ok(());
    };
    if parent_hidden || state.hidden {
        hide_subtree(context, node, clip, total);
        return Ok(());
    }
    let (node_slot, node_slot_offset) = if state.screen_baked {
        (0, LogicalPoint::ZERO)
    } else {
        (slot, slot_offset)
    };
    let reusable = state.reusable
        && state
            .lane
            .is_none_or(|lane| context.scene.lane_scroll(lane) == node_slot);
    if let Some(geometry) = state.geometry {
        paint_geometry_node(context, node, &state, geometry, clip, total, state.opacity)?;
    } else if reusable {
        if context.mode.is_full()
            && let Some(lane) = state.lane
        {
            context.order.push(lane);
        }
        #[cfg(feature = "test-support")]
        if let Some(lane) = state.lane {
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
    } else {
        rebuild_lane(
            context,
            node,
            clip,
            total,
            node_slot,
            node_slot_offset,
            boundary,
            state.opacity,
        )?;
    }

    let child_count = state.children;
    let clip_bounds = state.clip_bounds;
    if let Some(padding_box) = clip_bounds {
        clip.push(padding_box, context.scale);
    }

    let mut child_total = total;
    let mut child_slot = node_slot;
    let mut child_slot_offset = node_slot_offset;
    let mut child_boundary = boundary;
    if let Some(own) = state.scroll {
        let viewport = clip.covering.unwrap_or_else(|| {
            state.clip_bounds.map_or_else(
                || DeviceRect::new(dp(0), dp(0), dp(0), dp(0)),
                |rect| context.scale.to_covering_device_rect(rect),
            )
        });
        let index = scroll_slot_for(context, node, own, viewport);
        child_total = LogicalPoint::new(total.x + own.x, total.y + own.y);
        child_slot = index;
        child_slot_offset = own;
        child_boundary = clip.rects.len();
    }
    let expand = context.mode.is_full() || !reusable;
    for index in 0..child_count {
        let child = match context.tree.get(node) {
            Ok(target) => target.children()[index],
            Err(_) => break,
        };
        paint_node(
            context,
            child,
            false,
            clip,
            child_total,
            child_slot_offset,
            child_slot,
            child_boundary,
            state.opacity,
            expand,
        )?;
    }
    if clip_bounds.is_some() {
        clip.pop(context.scale);
    }
    Ok(())
}

fn scroll_slot_for(
    context: &mut PaintContext<'_>,
    node: NodeId,
    offset: LogicalPoint,
    viewport: DeviceRect,
) -> u32 {
    let cached = context.tree.scroll_slot_cached(node);
    if context.mode.is_full() || cached == 0 {
        let index = u32::try_from(context.scroll_slots.len()).unwrap_or(u32::MAX);
        context.scroll_slots.push((offset, viewport));
        context.scroll_slot_nodes.push(node);
        context.tree.set_scroll_slot(node, index);
        return index;
    }
    let slot = cached as usize;
    match slot.checked_sub(1) {
        Some(parallel) if parallel < context.scroll_slot_nodes.len() => {
            context.scroll_slot_nodes[parallel] = node;
        }
        _ => {
            let index = u32::try_from(context.scroll_slots.len()).unwrap_or(u32::MAX);
            context.scroll_slots.push((offset, viewport));
            context.scroll_slot_nodes.push(node);
            context.tree.set_scroll_slot(node, index);
            return index;
        }
    }
    if let Some(slot) = context.scroll_slots.get_mut(slot) {
        *slot = (offset, viewport);
    }
    cached
}

fn rebuild_order_only(tree: &NodeTree, order: &mut Vec<LaneId>) {
    order.clear();
    let mut stack: SmallVec<[(NodeId, bool); 32]> = SmallVec::new();
    stack.push((tree.root(), false));
    while let Some((node, parent_hidden)) = stack.pop() {
        let Ok(target) = tree.get(node) else {
            continue;
        };
        let own_hidden = matches!(target.style().inner().display, taffy::style::Display::None)
            || target.style_patch().visible == Some(false);
        if parent_hidden || own_hidden {
            continue;
        }
        for lane in target.lanes() {
            order.push(*lane);
        }
        for child in target.children().iter().rev() {
            stack.push((*child, false));
        }
    }
}

fn hide_subtree(
    context: &mut PaintContext<'_>,
    node: NodeId,
    clip: &ClipContext,
    total: LogicalPoint,
) {
    let previous_content = painted_content(context.tree, context.scene, node, context.scale);
    let (children, damage_rect, child_offset) = {
        let Ok(target) = context.tree.get(node) else {
            return;
        };
        let children: SmallVec<[NodeId; 4]> = target.children().iter().copied().collect();
        let mut damage_rect = target
            .damage()
            .union(target.layout().bounds)
            .union(previous_content.content);
        if let NodeKind::Geometry(id) = *target.kind()
            && let Some(snapshot) = context.geometry.snapshot_arc(id)
        {
            damage_rect = damage_rect.union(snapshot.bounds());
        }
        let damage_rect = damage_rect.translate(negate(total));
        let child_offset = total
            + context
                .tree
                .scroll_slot(node)
                .map_or(LogicalPoint::ZERO, |slot| slot.offset);
        (children, damage_rect, child_offset)
    };
    for index in 0..context.tree.lane_count(node) {
        let Some(lane) = context.tree.lane_at(node, index) else {
            continue;
        };
        let had_primitives = context
            .scene
            .lane_primitives(lane)
            .is_some_and(|primitives| !primitives.is_empty());
        if had_primitives {
            let _ = context.scene.clear_lane(lane);
            context.rebuilt += 1;
            if context.mode.is_full() {
                context.order.push(lane);
            }
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
    context
        .damage
        .push_logical(damage_rect, physical_px(1.0), clip.covering);
    push_painted_shadow(context, &previous_content, total, clip.covering);
    if !previous_content.screen.is_empty() {
        context
            .damage
            .push_logical(previous_content.screen, physical_px(1.0), clip.covering);
    }
    for child in children {
        hide_subtree(context, child, clip, child_offset);
    }
}

fn rebuild_lane(
    context: &mut PaintContext<'_>,
    node: NodeId,
    clip: &ClipContext,
    total: LogicalPoint,
    slot: u32,
    slot_offset: LogicalPoint,
    boundary: usize,
    opacity: f32,
) -> Result<(), EngineError> {
    let created = context.tree.lane_of(node).is_none();
    let lane = match context.tree.lane_of(node) {
        Some(lane) => lane,
        None => {
            let lane = context.scene.new_lane();
            context.tree.set_lane(node, lane);
            lane
        }
    };
    let previous_content = painted_content(context.tree, context.scene, node, context.scale);
    let had_primitives = context
        .scene
        .lane_primitives(lane)
        .is_some_and(|primitives| !primitives.is_empty());
    let _ = context.scene.clear_lane(lane);
    let _ = context.scene.set_lane_scroll(lane, slot);
    let bake = offset_between(total, slot_offset);

    let (
        bounds,
        content_offset,
        patch,
        spec,
        content,
        node_damage,
        ring,
        content_clip,
        geometry_transform,
    ) = {
        let Ok(target) = context.tree.get(node) else {
            return Ok(());
        };
        let content = match target.kind() {
            NodeKind::Box => Content::Box,
            NodeKind::Text(_) => Content::Text(context.tree.text_line(node).cloned()),
            NodeKind::Canvas(canvas) => Content::Canvas(Arc::clone(canvas)),
            NodeKind::Image(content) => Content::Image((**content).clone()),
            NodeKind::Geometry(_) => Content::Box,
        };
        let bounds = target.layout().bounds.translate(negate(bake));
        let patch = target.style_patch().clone();
        let spec = target.paint_spec().cloned();
        let node_damage = target.damage().union(previous_content.content);
        let content_offset = context.tree.content_offset(node);
        let content_clip =
            (target.clip() == Clip::Bounds).then(|| bounds.inset(target.layout().border));
        let ring = (context.focused == Some(node))
            .then(|| context.tree.focus_ring(node))
            .flatten();
        let geometry_transform = crate::math::Transform::IDENTITY;
        (
            bounds,
            content_offset,
            patch,
            spec,
            content,
            node_damage,
            ring,
            content_clip,
            geometry_transform,
        )
    };

    let inner_clips = if slot == 0 {
        &clip.rects[..]
    } else {
        &clip.rects[boundary.min(clip.rects.len())..]
    };
    for clip_rect in inner_clips {
        let mut baked = *clip_rect;
        baked.rect = baked.rect.translate(slot_offset);
        let _ = context
            .scene
            .push(lane, Primitive::Clip(ClipOp::Push(baked)));
    }

    let mut emitter = PrimitiveEmitter {
        node,
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
        offset: total,
        damage_shift: negate(slot_offset),
        ring,
        content_clip,
        geometry_transform,
        opacity,
    };
    emitter.emit(context);
    push_painted_shadow(context, &previous_content, total, clip.covering);

    if !previous_content.screen.is_empty() {
        context
            .damage
            .push_logical(previous_content.screen, physical_px(1.0), clip.covering);
    }

    for _ in inner_clips.iter().rev() {
        let _ = context.scene.push(lane, Primitive::Clip(ClipOp::Pop));
    }

    let primitives = context
        .scene
        .lane_primitives(lane)
        .map_or(0, <[Primitive]>::len);
    context.tree.set_reusable(node);
    context.tree.set_clip_key(node, clip.key());
    context.tree.set_lane_offset(node, bake);
    context.tree.set_paint_opacity(node, opacity);
    context.tree.clear_paint(node);

    if primitives == 0 {
        context.scene.release_lane(lane);
        context.tree.clear_lane(node);
        if !emitter.node_damage.is_empty() {
            context.rebuilt += 1;
        }
        if !context.mode.is_full() && (created || had_primitives) {
            context.order_changed = true;
        }
    } else {
        context.rebuilt += 1;
        if context.mode.is_full() {
            context.order.push(lane);
        } else if created || !had_primitives {
            context.order_changed = true;
        }
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

struct GeometryNodeState {
    bounds: LogicalRect,
    content_offset: LogicalPoint,
    patch: StylePatch,
    spec: Option<PaintSpec>,
    node_damage: LogicalRect,
    ring: Option<crate::paint::style::FocusRing>,
    content_clip: Option<LogicalRect>,
    geometry_transform: crate::math::Transform,
    bake: LogicalPoint,
}

fn geometry_node_state(
    context: &PaintContext<'_>,
    node: NodeId,
    total: LogicalPoint,
    node_slot_offset: LogicalPoint,
) -> Option<GeometryNodeState> {
    let Ok(target) = context.tree.get(node) else {
        return None;
    };
    let bake = offset_between(total, node_slot_offset);
    let bounds = target.layout().bounds.translate(negate(bake));
    let patch = target.style_patch().clone();
    let spec = target.paint_spec().cloned();
    let node_damage = target.damage();
    let content_offset = context.tree.content_offset(node);
    let content_clip =
        (target.clip() == Clip::Bounds).then(|| bounds.inset(target.layout().border));
    let ring = (context.focused == Some(node))
        .then(|| context.tree.focus_ring(node))
        .flatten();
    let geometry_transform = context.tree.geometry_transform(node);
    Some(GeometryNodeState {
        bounds,
        content_offset,
        patch,
        spec,
        node_damage,
        ring,
        content_clip,
        geometry_transform,
        bake,
    })
}

fn geometry_chunk_local_bounds(
    snapshot: Option<&std::sync::Arc<crate::paint::geometry::GeometrySnapshot>>,
    chunk: usize,
) -> Option<LogicalRect> {
    let snapshot = snapshot?;
    Some(match snapshot.chunk(chunk as u32) {
        Some(meta) if !meta.bounds.is_empty() => meta.bounds,
        Some(_) => snapshot.bounds(),
        None => LogicalRect::default(),
    })
}

fn geometry_chunk_draw_range(
    snapshot: Option<&std::sync::Arc<crate::paint::geometry::GeometrySnapshot>>,
    chunk: usize,
) -> std::ops::Range<u32> {
    let Some(snapshot) = snapshot else {
        return 0..0;
    };
    let Some(meta) = snapshot.chunk(chunk as u32) else {
        return 0..0;
    };
    crate::paint::geometry::chunk_draw_range(
        snapshot.topology(),
        snapshot.index_count(),
        meta,
        crate::paint::geometry::FULL_INDEX_RANGE,
    )
    .unwrap_or_default()
}

fn clear_geometry_lanes(
    context: &mut PaintContext<'_>,
    node: NodeId,
    old_lanes: usize,
    total: LogicalPoint,
    clip: &ClipContext,
    node_damage: &LogicalRect,
    geometry_transform: crate::math::Transform,
    bake: LogicalPoint,
) {
    for chunk in 0..old_lanes {
        let Some(previous) = context.baked_scratch.get(chunk).copied() else {
            continue;
        };
        let damage = geometry_chunk_damage(geometry_transform, previous.bounds, total);
        if !damage.is_empty() {
            context
                .damage
                .push_logical(damage, physical_px(1.0), clip.covering);
        }
    }
    let mut order_changed = false;
    for chunk in 0..old_lanes {
        let Some(lane) = context.tree.lane_at(node, chunk) else {
            continue;
        };
        let had_primitives = context
            .scene
            .lane_primitives(lane)
            .is_some_and(|primitives| !primitives.is_empty());
        if !context.mode.is_full() && had_primitives {
            order_changed = true;
        }
        context.scene.release_lane(lane);
        #[cfg(feature = "test-support")]
        context.log.lanes.push(LanePaintEntry {
            lane,
            node,
            outcome: LaneOutcome::Cleared,
            primitives: 0,
        });
    }
    context.tree.clear_lane(node);
    context.tree.resize_baked_geometry_chunks(node, 0);
    if !node_damage.is_empty() {
        context.damage.push_logical(
            node_damage.translate(negate(total)),
            physical_px(1.0),
            clip.covering,
        );
        context.rebuilt += 1;
    }
    context.tree.set_reusable(node);
    context.tree.set_clip_key(node, clip.key());
    context.tree.set_lane_offset(node, bake);
    context.tree.clear_paint(node);
    if order_changed {
        context.order_changed = true;
    }
}

fn paint_geometry_node(
    context: &mut PaintContext<'_>,
    node: NodeId,
    state: &PaintNodeState,
    geometry_id: crate::paint::geometry::GeometryId,
    clip: &ClipContext,
    total: LogicalPoint,
    opacity: f32,
) -> Result<(), EngineError> {
    let snapshot = context.geometry.snapshot_arc(geometry_id);
    let desired = snapshot
        .as_ref()
        .map_or(1, |snapshot| snapshot.chunks().len().max(1));
    let Some(node_state) = geometry_node_state(context, node, total, LogicalPoint::ZERO) else {
        return Ok(());
    };
    let GeometryNodeState {
        bounds,
        content_offset,
        patch,
        spec,
        node_damage,
        ring,
        content_clip,
        geometry_transform,
        bake,
    } = node_state;
    let node_damage = if state.reusable {
        node_damage
    } else {
        let painted = painted_content(context.tree, context.scene, node, context.scale);
        push_painted_shadow(context, &painted, total, clip.covering);
        node_damage.union(painted.content)
    };
    let visible = patch.visible.unwrap_or(true);
    let old_lanes = context.tree.lane_count(node);
    context.baked_scratch.clear();
    if let Some(baked) = context.tree.baked_geometry_chunks(node) {
        context.baked_scratch.extend(baked.iter().copied());
    }
    if !visible || bounds.is_empty() {
        clear_geometry_lanes(
            context,
            node,
            old_lanes,
            total,
            clip,
            &node_damage,
            geometry_transform,
            bake,
        );
        context.tree.set_paint_opacity(node, opacity);
        return Ok(());
    }
    context.rebuild_scratch.clear();
    for chunk in 0..desired {
        let lane_missing = context.tree.lane_at(node, chunk).is_none();
        let revision = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.chunk(chunk as u32))
            .map_or(0, |meta| meta.revision);
        let baked_matches = context
            .baked_scratch
            .get(chunk)
            .is_some_and(|baked| baked.revision == revision);
        context
            .rebuild_scratch
            .push(!state.reusable || lane_missing || !baked_matches);
    }
    context.tree.resize_baked_geometry_chunks(node, desired);
    let mut created_any = false;
    let inner_clips = &clip.rects[..];
    for chunk in 0..desired {
        let lane = match context.tree.lane_at(node, chunk) {
            Some(lane) => lane,
            None => {
                let lane = context.scene.new_lane();
                context.tree.push_lane(node, lane);
                created_any = true;
                lane
            }
        };
        if !context.rebuild_scratch[chunk] {
            if context.mode.is_full() {
                context.order.push(lane);
            }
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
            continue;
        }
        let previous = context.baked_scratch.get(chunk).copied();
        #[cfg(feature = "test-support")]
        let had_primitives = context
            .scene
            .lane_primitives(lane)
            .is_some_and(|primitives| !primitives.is_empty());
        let _ = context.scene.clear_lane(lane);
        let _ = context.scene.set_lane_scroll(lane, 0);
        context
            .scene
            .set_lane_geometry_chunk(lane, Some((geometry_id, chunk as u32)));
        context.tree.set_lane_offset(node, bake);
        for clip_rect in inner_clips {
            let _ = context
                .scene
                .push(lane, Primitive::Clip(ClipOp::Push(*clip_rect)));
        }
        let mut emitter = PrimitiveEmitter {
            lane,
            node,
            bounds,
            content_offset,
            patch: patch.clone(),
            spec: spec.clone(),
            scale: context.scale,
            clip: clip.covering,
            content: Content::Box,
            emitted: 0,
            node_damage: if chunk == 0 {
                node_damage
            } else {
                LogicalRect::default()
            },
            offset: total,
            damage_shift: negate(LogicalPoint::ZERO),
            ring: if chunk == 0 { ring } else { None },
            geometry_transform,
            content_clip,
            opacity,
        };
        if chunk == 0 {
            emitter.emit_decorations(context, opacity);
        }
        emitter.emit_geometry_chunk(
            context,
            geometry_id,
            geometry_chunk_draw_range(snapshot.as_ref(), chunk),
            opacity,
        );
        if chunk == 0 {
            emitter.emit_focus_ring(context, opacity);
            emitter.emit_node_damage(context);
        }
        for _ in inner_clips.iter().rev() {
            let _ = context.scene.push(lane, Primitive::Clip(ClipOp::Pop));
        }
        if context.mode.is_full() {
            context.order.push(lane);
        }
        let mut damage = match (
            &snapshot,
            geometry_chunk_local_bounds(snapshot.as_ref(), chunk),
        ) {
            (None, _) => bounds,
            (Some(_), Some(new_bounds)) => {
                geometry_chunk_damage(geometry_transform, new_bounds, total)
            }
            (Some(_), None) => LogicalRect::default(),
        };
        if let Some(previous) = previous {
            damage = damage.union(geometry_chunk_damage(
                geometry_transform,
                previous.bounds,
                total,
            ));
        }
        if !damage.is_empty() {
            context
                .damage
                .push_logical(damage, physical_px(1.0), clip.covering);
        }
        let revision = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.chunk(chunk as u32))
            .map_or(0, |meta| meta.revision);
        let baked_bounds =
            geometry_chunk_local_bounds(snapshot.as_ref(), chunk).unwrap_or_default();
        context.tree.set_baked_geometry_chunk(
            node,
            chunk,
            BakedGeometryChunk {
                revision,
                bounds: baked_bounds,
            },
        );
        context.rebuilt += 1;
        #[cfg(feature = "test-support")]
        {
            let primitives = context
                .scene
                .lane_primitives(lane)
                .map_or(0, <[Primitive]>::len);
            context.log.lanes.push(LanePaintEntry {
                lane,
                node,
                outcome: if had_primitives {
                    LaneOutcome::Rebuilt
                } else {
                    LaneOutcome::Allocated
                },
                primitives,
            });
        }
    }
    for chunk in desired..old_lanes {
        let Some(lane) = context.tree.lane_at(node, chunk) else {
            continue;
        };
        let had_primitives = context
            .scene
            .lane_primitives(lane)
            .is_some_and(|primitives| !primitives.is_empty());
        if let Some(previous) = context.baked_scratch.get(chunk) {
            let damage = geometry_chunk_damage(geometry_transform, previous.bounds, total);
            if !damage.is_empty() {
                context
                    .damage
                    .push_logical(damage, physical_px(1.0), clip.covering);
            }
        }
        context.scene.release_lane(lane);
        context.rebuilt += 1;
        if !context.mode.is_full() && had_primitives {
            context.order_changed = true;
        }
    }
    context.tree.truncate_lanes(node, desired);
    context.tree.resize_baked_geometry_chunks(node, desired);
    context.tree.set_reusable(node);
    context.tree.set_clip_key(node, clip.key());
    context.tree.set_lane_offset(node, bake);
    context.tree.set_paint_opacity(node, opacity);
    context.tree.clear_paint(node);
    if !context.mode.is_full() && created_any {
        context.order_changed = true;
    }
    Ok(())
}

fn geometry_chunk_damage(
    transform: crate::math::Transform,
    chunk_bounds: LogicalRect,
    total: LogicalPoint,
) -> LogicalRect {
    if chunk_bounds.is_empty() {
        return LogicalRect::default();
    }
    let aabb = transformed_chunk_aabb(transform, chunk_bounds);
    LogicalRect::new(
        px(aabb[0] - total.x.value()),
        px(aabb[1] - total.y.value()),
        px((aabb[2] - aabb[0]).max(0.0)),
        px((aabb[3] - aabb[1]).max(0.0)),
    )
}

fn transformed_chunk_aabb(transform: crate::math::Transform, bounds: LogicalRect) -> [f32; 4] {
    let far = LogicalPoint::new(
        bounds.origin.x + bounds.size.width,
        bounds.origin.y + bounds.size.height,
    );
    let corners = [
        transform.apply(bounds.origin),
        transform.apply(LogicalPoint::new(far.x, bounds.origin.y)),
        transform.apply(LogicalPoint::new(bounds.origin.x, far.y)),
        transform.apply(far),
    ];
    if !corners
        .iter()
        .all(|point| point.x.is_finite() && point.y.is_finite())
    {
        return [1.0, 1.0, -1.0, -1.0];
    }
    let mut aabb = [
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    ];
    for point in corners {
        aabb = [
            aabb[0].min(point.x.value()),
            aabb[1].min(point.y.value()),
            aabb[2].max(point.x.value()),
            aabb[3].max(point.y.value()),
        ];
    }
    aabb
}

struct PrimitiveEmitter {
    node: NodeId,
    lane: LaneId,
    bounds: LogicalRect,
    content_offset: LogicalPoint,
    patch: StylePatch,
    spec: Option<PaintSpec>,
    scale: DevicePixelRatio,
    clip: Option<DeviceRect>,
    content: Content,
    emitted: usize,
    node_damage: LogicalRect,
    offset: LogicalPoint,
    damage_shift: LogicalPoint,
    ring: Option<crate::paint::style::FocusRing>,
    geometry_transform: crate::math::Transform,
    content_clip: Option<LogicalRect>,
    opacity: f32,
}

struct TextDecorations {
    selection: Vec<LogicalRect>,
    underlines: Vec<LogicalRect>,
    caret: Option<LogicalRect>,
    placeholder: Option<Rc<ShapedLine>>,
    selection_color: Color,
    composition_color: Color,
    caret_color: Color,
    placeholder_color: Color,
}

fn text_decorations(tree: &NodeTree, node: NodeId) -> Option<TextDecorations> {
    let editable = tree.editable(node)?;
    let content = tree.text_content(node)?;
    let shaped = tree.text_slot(node)?.shaped();
    if shaped.text() != editable.display_text() {
        return None;
    }
    let width = tree.text_layout_width(node)?;
    let alignment = content.style.alignment;
    let selection = editable.selection();
    let selection_rects = if selection.is_collapsed() {
        Vec::new()
    } else {
        shaped.selection_rects(
            width,
            alignment,
            selection.range().start,
            selection.range().end,
        )
    };
    let caret = (editable.focused() && editable.caret_visible())
        .then(|| tree.text_caret_rect(node, selection.focus().byte).ok())
        .flatten();
    let mut underlines = Vec::new();
    if let Some(ranges) = editable.composition_ranges() {
        let font_size = editable.spec().text_style.size;
        for (index, range) in ranges.iter().enumerate() {
            let thickness = if index % 2 == 0 { px(1.0) } else { px(1.5) };
            underlines.extend(shaped.composition_underline_rects(
                width,
                alignment,
                range.clone(),
                font_size,
                thickness,
            ));
        }
    }
    let placeholder = editable.visible_placeholder_line().cloned();
    Some(TextDecorations {
        selection: selection_rects,
        underlines,
        caret,
        placeholder,
        selection_color: editable.spec().selection_color,
        composition_color: editable.spec().composition_color,
        caret_color: editable.spec().caret_color,
        placeholder_color: editable.spec().placeholder_color,
    })
}

fn negate(offset: LogicalPoint) -> LogicalPoint {
    LogicalPoint::new(-offset.x, -offset.y)
}

#[derive(Default)]
struct PaintedContent {
    content: LogicalRect,
    screen: LogicalRect,
    shadow: Option<(LogicalRect, PhysicalPx)>,
}

fn painted_content(
    tree: &NodeTree,
    scene: &Scene,
    node: NodeId,
    scale: DevicePixelRatio,
) -> PaintedContent {
    let mut content = LogicalRect::default();
    let mut screen = LogicalRect::default();
    let mut shadow_spill: Option<(LogicalRect, PhysicalPx)> = None;
    for index in 0..tree.lane_count(node) {
        let Some(lane) = tree.lane_at(node, index) else {
            continue;
        };
        let Some(primitives) = scene.lane_primitives(lane) else {
            continue;
        };
        for primitive in primitives {
            match primitive {
                Primitive::GlyphRun(run) => {
                    for glyph in &run.glyphs {
                        content = content
                            .union(LogicalRect::from_origin_size(glyph.position, glyph.size));
                    }
                }
                Primitive::Canvas(canvas) => screen = screen.union(canvas.bounds()),
                Primitive::Image(image) => content = content.union(image.bounds),
                Primitive::Quad(quad) => content = content.union(quad.bounds),
                Primitive::Shadow(shadow) => {
                    let blur = scale.to_physical(shadow.blur.max(LogicalPx::ZERO));
                    let sigma = (blur.value() * crate::paint::instance::SHADOW_SIGMA_SCALE)
                        .max(crate::paint::instance::SHADOW_SIGMA_MIN);
                    let pad =
                        PhysicalPx::new(crate::paint::instance::SHADOW_PAD_SIGMAS * sigma + 1.0);
                    let rect = shadow.rect.translate(shadow.offset);
                    shadow_spill = Some(match shadow_spill {
                        Some((previous, previous_pad)) => {
                            (previous.union(rect), previous_pad.max(pad))
                        }
                        None => (rect, pad),
                    });
                }
                Primitive::Path(_)
                | Primitive::Geometry(_)
                | Primitive::Clip(_)
                | Primitive::Text(_) => {}
            }
        }
    }
    PaintedContent {
        content: content.translate(tree.lane_offset(node)),
        screen,
        shadow: shadow_spill.map(|(rect, pad)| (rect.translate(tree.lane_offset(node)), pad)),
    }
}

fn push_painted_shadow(
    context: &mut PaintContext<'_>,
    painted: &PaintedContent,
    total: LogicalPoint,
    clip: Option<DeviceRect>,
) {
    if let Some((rect, pad)) = painted.shadow {
        context
            .damage
            .push_logical(rect.translate(negate(total)), pad, clip);
    }
}

impl PrimitiveEmitter {
    fn push(&mut self, context: &mut PaintContext<'_>, primitive: Primitive) {
        let _ = context.scene.push(self.lane, primitive);
        self.emitted += 1;
    }

    fn push_damage(
        &self,
        context: &mut PaintContext<'_>,
        bounds: LogicalRect,
        aa_padding: PhysicalPx,
    ) {
        context
            .damage
            .push_logical(bounds.translate(self.damage_shift), aa_padding, self.clip);
    }

    fn emit(&mut self, context: &mut PaintContext<'_>) {
        let visible = self.patch.visible.unwrap_or(true);
        if !visible || self.bounds.is_empty() {
            self.emit_node_damage(context);
            return;
        }
        let opacity = self.opacity;
        self.emit_decorations(context, opacity);
        self.emit_content(context, opacity);
        self.emit_focus_ring(context, opacity);
        self.emit_node_damage(context);
    }

    fn emit_decorations(&mut self, context: &mut PaintContext<'_>, opacity: f32) {
        let spec = self.spec.clone().unwrap_or_default();
        let radii = spec.corner_radius;
        for shadow in spec.shadows.iter() {
            let color = shadow.color.with_opacity(opacity);
            if color.is_transparent() {
                continue;
            }
            let rect = self.bounds.outset(shadow.spread);
            let primitive = Shadow {
                rect,
                offset: shadow.offset,
                blur: shadow.blur.max(LogicalPx::ZERO),
                color,
                corner_radius: radii.map(|radius| (radius + shadow.spread).max(LogicalPx::ZERO)),
            };
            self.push(context, Primitive::Shadow(primitive));
            let blur = self.scale.to_physical(primitive.blur.max(LogicalPx::ZERO));
            let sigma = (blur.value() * crate::paint::instance::SHADOW_SIGMA_SCALE)
                .max(crate::paint::instance::SHADOW_SIGMA_MIN);
            let pad = crate::paint::instance::SHADOW_PAD_SIGMAS * sigma + 1.0;
            self.push_damage(
                context,
                primitive.rect.translate(primitive.offset),
                PhysicalPx::new(pad),
            );
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
            .filter(|border| border.width > LogicalPx::ZERO && !border.color.is_transparent())
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
            .filter(|gradient| !(gradient.from.is_transparent() && gradient.to.is_transparent()));

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
                self.push_damage(context, self.bounds, physical_px(1.0));
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
                    self.push_damage(context, self.bounds, physical_px(1.0));
                }
            }
        }
    }

    fn emit_focus_ring(&mut self, context: &mut PaintContext<'_>, opacity: f32) {
        let radii = self.spec.clone().unwrap_or_default().corner_radius;
        if let Some(ring) = self.ring
            && ring.width > LogicalPx::ZERO
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
            self.push_damage(context, bounds, physical_px(1.0));
        }
    }

    fn emit_node_damage(&mut self, context: &mut PaintContext<'_>) {
        if self.emitted > 0 || !self.node_damage.is_empty() {
            let damage = self.node_damage.translate(negate(self.offset));
            context
                .damage
                .push_logical(damage, physical_px(1.0), self.clip);
        }
    }

    fn emit_geometry_chunk(
        &mut self,
        context: &mut PaintContext<'_>,
        id: crate::paint::geometry::GeometryId,
        range: std::ops::Range<u32>,
        opacity: f32,
    ) {
        let content_clip = self.content_clip.map(|rect| ClipRect {
            rect,
            corner_radius: [LogicalPx::ZERO; 4],
        });
        if let Some(clip_rect) = content_clip {
            let _ = context
                .scene
                .push(self.lane, Primitive::Clip(ClipOp::Push(clip_rect)));
        }
        self.push(
            context,
            Primitive::Geometry(GeometryDraw {
                id,
                transform: self.geometry_transform,
                color: self
                    .patch
                    .foreground
                    .unwrap_or(Color::WHITE)
                    .with_opacity(opacity),
                range,
            }),
        );
        if content_clip.is_some() {
            let _ = context.scene.push(self.lane, Primitive::Clip(ClipOp::Pop));
        }
    }

    fn emit_content(&mut self, context: &mut PaintContext<'_>, opacity: f32) {
        let has_content = content_needs_clip(&self.content);
        let content_clip = self
            .content_clip
            .filter(|_| has_content)
            .map(|rect| ClipRect {
                rect,
                corner_radius: [LogicalPx::ZERO; 4],
            });
        if let Some(clip_rect) = content_clip {
            let _ = context
                .scene
                .push(self.lane, Primitive::Clip(ClipOp::Push(clip_rect)));
        }
        match std::mem::replace(&mut self.content, Content::Box) {
            Content::Box => {}

            Content::Canvas(canvas) => {
                let canvas_bounds = canvas.bounds();
                let mut skipped = 0usize;
                if canvas_has_text(&canvas) {
                    let lines = context
                        .canvas_text
                        .get(&self.node)
                        .map(|entry| entry.lines.as_slice());
                    let resolved =
                        resolved_canvas(&canvas, lines, context.atlases, &mut 0usize, &mut skipped);
                    self.push(context, Primitive::Canvas(Arc::new(resolved)));
                } else {
                    self.push(context, Primitive::Canvas(canvas));
                }
                context.unresolved_canvas_text += skipped;
                context
                    .damage
                    .push_logical(canvas_bounds, physical_px(1.0), self.clip);
            }
            Content::Text(line) => {
                if let Some(line) = line {
                    self.emit_text(context, &line, opacity);
                }
            }
            Content::Image(content) => self.emit_image(context, &content, opacity),
        }
        if content_clip.is_some() {
            let _ = context.scene.push(self.lane, Primitive::Clip(ClipOp::Pop));
        }
    }

    fn emit_text(&mut self, context: &mut PaintContext<'_>, line: &ShapedLine, opacity: f32) {
        if let Some(atlases) = context.atlases
            && (line.atlas_generation != atlases.glyph.generation()
                || line.color_atlas_generation != atlases.color.generation())
        {
            if !context.stale_text.contains(&self.node) {
                context.stale_text.push(self.node);
            }
            return;
        }
        let origin = LogicalPoint::new(
            self.bounds.origin.x + self.content_offset.x,
            self.bounds.origin.y + self.content_offset.y,
        );
        let decorations = text_decorations(context.tree, self.node);
        if let Some(decorations) = &decorations {
            for rect in &decorations.selection {
                self.emit_solid_quad(
                    context,
                    rect.translate(origin),
                    decorations.selection_color,
                    opacity,
                );
            }
        }
        let inline_paints: SmallVec<[(u64, Option<InlineBoxPaint>); 4]> = context
            .tree
            .text_content(self.node)
            .map(|content| {
                content
                    .inline_boxes
                    .iter()
                    .map(|spec| (spec.id, spec.paint))
                    .collect()
            })
            .unwrap_or_default();
        for shaped_line in &line.lines {
            for item in &shaped_line.items {
                match item {
                    ShapedTextItem::GlyphRun(run) => {
                        if run.glyphs.is_empty() {
                            continue;
                        }
                        let mut glyphs = Vec::with_capacity(run.glyphs.len());
                        let mut glyph_bounds = LogicalRect::default();
                        for glyph in &run.glyphs {
                            let position = LogicalPoint::new(
                                origin.x + glyph.position.x,
                                origin.y + glyph.position.y,
                            );
                            let rect = LogicalRect::from_origin_size(position, glyph.size);
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
                        self.push_damage(context, glyph_bounds, physical_px(0.0));
                    }
                    ShapedTextItem::InlineBox(inline) => {
                        let paint = inline_paints
                            .iter()
                            .find(|(id, _)| *id == inline.id)
                            .and_then(|(_, paint)| *paint);
                        self.emit_inline_box(
                            context,
                            inline.rect,
                            inline.id,
                            paint,
                            origin,
                            opacity,
                        );
                    }
                }
            }
            for decoration in &shaped_line.decorations {
                let color = self
                    .patch
                    .foreground
                    .unwrap_or(decoration.color)
                    .with_opacity(opacity);
                if color.is_transparent()
                    || decoration.rect.size.width <= LogicalPx::ZERO
                    || decoration.rect.size.height <= LogicalPx::ZERO
                {
                    continue;
                }
                let bounds = decoration.rect.translate(origin);
                self.push(
                    context,
                    Primitive::Quad(Quad {
                        bounds,
                        corner_radius: [LogicalPx::ZERO; 4],
                        paint: QuadPaint::Solid {
                            fill: color,
                            border: None,
                        },
                    }),
                );
                self.push_damage(context, bounds, physical_px(0.0));
            }
        }
        if let Some(decorations) = decorations {
            for rect in &decorations.underlines {
                self.emit_solid_quad(
                    context,
                    rect.translate(origin),
                    decorations.composition_color,
                    opacity,
                );
            }
            if let Some(placeholder) = &decorations.placeholder {
                let live = context
                    .atlases
                    .is_none_or(|atlases| atlases.line_is_live(placeholder));
                if live {
                    let color = self
                        .patch
                        .foreground
                        .unwrap_or(decorations.placeholder_color);
                    self.emit_placeholder(context, placeholder, color, origin, opacity);
                } else if !context.stale_text.contains(&self.node) {
                    context.stale_text.push(self.node);
                }
            }
            if let Some(caret) = decorations.caret {
                self.emit_solid_quad(
                    context,
                    caret.translate(origin),
                    decorations.caret_color,
                    opacity,
                );
            }
        }
    }

    fn emit_solid_quad(
        &mut self,
        context: &mut PaintContext<'_>,
        bounds: LogicalRect,
        color: Color,
        opacity: f32,
    ) {
        let color = color.with_opacity(opacity);
        if color.is_transparent()
            || bounds.size.width <= LogicalPx::ZERO
            || bounds.size.height <= LogicalPx::ZERO
        {
            return;
        }
        self.push(
            context,
            Primitive::Quad(Quad {
                bounds,
                corner_radius: [LogicalPx::ZERO; 4],
                paint: QuadPaint::Solid {
                    fill: color,
                    border: None,
                },
            }),
        );
        self.push_damage(context, bounds, physical_px(0.0));
    }

    fn emit_placeholder(
        &mut self,
        context: &mut PaintContext<'_>,
        line: &ShapedLine,
        color: Color,
        origin: LogicalPoint,
        opacity: f32,
    ) {
        let color = color.with_opacity(opacity);
        if color.is_transparent() {
            return;
        }
        for shaped_line in &line.lines {
            for item in &shaped_line.items {
                let ShapedTextItem::GlyphRun(run) = item else {
                    continue;
                };
                if run.glyphs.is_empty() {
                    continue;
                }
                let mut glyphs = Vec::with_capacity(run.glyphs.len());
                let mut bounds = LogicalRect::default();
                for glyph in &run.glyphs {
                    let position =
                        LogicalPoint::new(origin.x + glyph.position.x, origin.y + glyph.position.y);
                    bounds = bounds.union(LogicalRect::from_origin_size(position, glyph.size));
                    glyphs.push(PositionedGlyph {
                        tile: glyph.tile,
                        position,
                        size: glyph.size,
                    });
                }
                self.push(context, Primitive::GlyphRun(GlyphRun { color, glyphs }));
                self.push_damage(context, bounds, physical_px(0.0));
            }
        }
    }

    fn emit_image(&mut self, context: &mut PaintContext<'_>, content: &ImageContent, opacity: f32) {
        let Some(resources) = context.resources else {
            return;
        };
        let Some(image) = content
            .source()
            .resource()
            .and_then(|resource| resources.image(resource))
        else {
            return;
        };
        let intrinsic = image.intrinsic(context.scale);
        let bounds = self.bounds;
        let Some(rect) = fit_bounds(bounds, content.fit(), intrinsic) else {
            return;
        };
        let Some(target) = variant_target(bounds, content.fit(), intrinsic, context.scale) else {
            return;
        };
        let Some(tile) = image.variant_for(resources, target) else {
            return;
        };
        let tile = match content.fit() {
            ImageFit::Cover => {
                let Some(tile) = cover_tile(bounds, intrinsic, tile) else {
                    return;
                };
                tile
            }
            ImageFit::Contain | ImageFit::Fill => tile,
        };
        let modulation = self
            .patch
            .foreground
            .unwrap_or(content.modulation())
            .with_opacity(opacity);
        if modulation.is_transparent() || rect.size.is_empty() {
            return;
        }
        self.push(
            context,
            Primitive::Image(Image {
                bounds: rect,
                tile,
                modulation,
            }),
        );
        self.push_damage(context, rect, physical_px(0.0));
    }

    fn emit_inline_box(
        &mut self,
        context: &mut PaintContext<'_>,
        rect: LogicalRect,
        id: u64,
        paint: Option<InlineBoxPaint>,
        origin: LogicalPoint,
        opacity: f32,
    ) {
        let bounds = rect.translate(origin);
        if bounds.size.width <= LogicalPx::ZERO || bounds.size.height <= LogicalPx::ZERO {
            return;
        }
        match paint {
            None => {}
            Some(InlineBoxPaint::Quad {
                color,
                corner_radius,
            }) => {
                let color = color.with_opacity(opacity);
                if color.is_transparent() {
                    return;
                }
                self.push(
                    context,
                    Primitive::Quad(Quad {
                        bounds,
                        corner_radius,
                        paint: QuadPaint::Solid {
                            fill: color,
                            border: None,
                        },
                    }),
                );
                self.push_damage(context, bounds, physical_px(0.0));
            }
            Some(InlineBoxPaint::Image {
                resource,
                modulation,
            }) => {
                let Some(resources) = context.resources else {
                    if !context.skipped_inline_images.contains(&id) {
                        context.skipped_inline_images.push(id);
                    }
                    return;
                };
                let Some(image) = resources.image(resource) else {
                    if !context.skipped_inline_images.contains(&id) {
                        context.skipped_inline_images.push(id);
                    }
                    return;
                };
                let intrinsic = image.intrinsic(self.scale);
                let Some(target) = variant_target(bounds, ImageFit::Fill, intrinsic, self.scale)
                else {
                    return;
                };
                let Some(tile) = image.variant_for(resources, target) else {
                    if !context.skipped_inline_images.contains(&id) {
                        context.skipped_inline_images.push(id);
                    }
                    return;
                };
                let modulation = modulation.with_opacity(opacity);
                if modulation.is_transparent() {
                    return;
                }
                self.push(
                    context,
                    Primitive::Image(Image {
                        bounds,
                        tile,
                        modulation,
                    }),
                );
                self.push_damage(context, bounds, physical_px(0.0));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::PaintPass;
    use crate::{
        math::{
            DevicePixelRatio, DeviceRect, LogicalPoint, LogicalPx, LogicalRect, dp, physical_px, px,
        },
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
        paint::{
            canvas::Canvas,
            color::Color,
            geometry::GeometryId,
            primitive::{Border, ClipOp, Gradient, GradientKind, Primitive, QuadPaint},
            scene::{LaneId, Scene},
            style::{Clip, FocusRing, PaintSpec, ShadowSpec, Shadows, Style, StylePatch},
            style_builder::StyleBuilder,
        },
        render::damage::DamageAccumulator,
        text::{
            InlineAlign, InlineBoxPaint, InlineBoxSpec, ShapedDecoration, ShapedGlyphRun,
            ShapedInlineBox, ShapedLine, ShapedTextItem, ShapedTextLine, TextContent, TextStyle,
        },
    };

    fn frame() -> DeviceRect {
        DeviceRect::new(dp(0), dp(0), dp(400), dp(300))
    }

    struct Harness {
        tree: NodeTree,
        scene: Scene,
        damage: DamageAccumulator,
        order: Vec<LaneId>,
        scroll_slots: Vec<(LogicalPoint, DeviceRect)>,
        scroll_slot_nodes: Vec<crate::node::id::NodeId>,
        atlas: Option<crate::render::AtlasAllocator>,
        color_atlas: crate::render::AtlasAllocator,
        resources: Option<crate::resources::ResourceCache>,
        geometry: crate::paint::geometry_store::GeometryStore,
        focused: Option<crate::node::id::NodeId>,
        scale: DevicePixelRatio,
        canvas_text:
            ahash::HashMap<crate::node::id::NodeId, crate::window::canvas_text::ResolvedCanvasText>,
    }

    impl Harness {
        fn new() -> Self {
            Self::with_scale(1.0)
        }

        fn with_scale(factor: f32) -> Self {
            let scale = DevicePixelRatio::new(factor).expect("合法 scale");
            Self {
                tree: NodeTree::new(),
                scene: Scene::empty(),
                damage: DamageAccumulator::new(scale),
                order: Vec::new(),
                scroll_slots: Vec::new(),
                scroll_slot_nodes: Vec::new(),
                atlas: None,
                color_atlas: crate::render::AtlasAllocator::with_page(
                    crate::paint::atlas::AtlasKind::ColorGlyph,
                    (512, 512),
                ),
                resources: None,
                geometry: crate::paint::geometry_store::GeometryStore::new(),
                focused: None,
                scale,
                canvas_text: ahash::HashMap::default(),
            }
        }

        fn try_paint(&mut self) -> Result<super::PaintReport, crate::error::EngineError> {
            self.damage.reset(self.scale);
            self.order.clear();
            let geometry = &self.geometry;
            let mut pass = PaintPass {
                tree: &mut self.tree,
                geometry,
                scene: &mut self.scene,
                damage: &mut self.damage,
                scale: self.scale,
                frame: frame(),
                order: &mut self.order,
                focused: self.focused,
                atlases: self.atlas.as_ref().map(|glyph| {
                    crate::window::canvas_text::GlyphAtlases {
                        glyph,
                        color: &self.color_atlas,
                    }
                }),
                resources: self.resources.as_ref(),
                canvas_text: &self.canvas_text,
                scroll_slots: &mut self.scroll_slots,
                scroll_slot_nodes: &mut self.scroll_slot_nodes,
                #[cfg(feature = "test-support")]
                log: crate::test_support::record::PaintLog::default(),
            };
            pass.run()
        }

        fn paint(&mut self) -> super::PaintReport {
            self.try_paint().expect("paint")
        }

        fn try_paint_incremental(
            &mut self,
            dirty: &[crate::node::id::NodeId],
        ) -> Result<super::PaintReport, crate::error::EngineError> {
            self.damage.reset(self.scale);
            self.tree.mark_paint_closure(dirty);
            let geometry = &self.geometry;
            let mut pass = PaintPass {
                tree: &mut self.tree,
                geometry,
                scene: &mut self.scene,
                damage: &mut self.damage,
                scale: self.scale,
                frame: frame(),
                order: &mut self.order,
                focused: self.focused,
                atlases: self.atlas.as_ref().map(|glyph| {
                    crate::window::canvas_text::GlyphAtlases {
                        glyph,
                        color: &self.color_atlas,
                    }
                }),
                resources: self.resources.as_ref(),
                canvas_text: &self.canvas_text,
                scroll_slots: &mut self.scroll_slots,
                scroll_slot_nodes: &mut self.scroll_slot_nodes,
                #[cfg(feature = "test-support")]
                log: crate::test_support::record::PaintLog::default(),
            };
            pass.run_incremental()
        }

        fn paint_incremental(&mut self, dirty: &[crate::node::id::NodeId]) -> super::PaintReport {
            self.try_paint_incremental(dirty)
                .expect("incremental paint")
        }

        fn mark_paint(&mut self, node: crate::node::id::NodeId) {
            let _ = self.tree.mark_paint(node);
        }

        fn mark_dirty(&mut self, nodes: &[crate::node::id::NodeId]) {
            for node in nodes {
                let _ = self.tree.mark_paint(*node);
            }
        }
    }

    fn fixed_style(width: f32, height: f32) -> Style {
        StyleBuilder::new().size(px(width), px(height)).build()
    }

    fn with_white() -> PaintSpec {
        PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        }
    }

    fn layout_all(tree: &mut NodeTree) {
        let order: Vec<_> = tree.dfs_ids();
        for (index, id) in order.iter().enumerate() {
            let bounds = LogicalRect::new(px(0.0), px((index as f32) * 20.0), px(100.0), px(20.0));
            let layout = crate::node::tree::LayoutBox {
                bounds,
                border: crate::math::LogicalEdges::ZERO,
            };
            let _ = tree.set_layout_for_test(*id, layout);
        }
    }

    fn lane_has_glyph_runs(harness: &Harness, node: crate::node::id::NodeId) -> bool {
        harness
            .tree
            .lane_of(node)
            .and_then(|lane| harness.scene.lane_primitives(lane))
            .is_some_and(|primitives| {
                primitives
                    .iter()
                    .any(|primitive| matches!(primitive, Primitive::GlyphRun(_)))
            })
    }

    fn lane_primitives(harness: &Harness, node: crate::node::id::NodeId) -> Vec<Primitive> {
        let lane = harness.tree.lane_of(node).expect("lane");
        harness.scene.lane_primitives(lane).expect("存在").to_vec()
    }

    fn damage_covers(rects: &[DeviceRect], target: DeviceRect) -> bool {
        (target.y0.value()..target.y1.value()).all(|y| {
            (target.x0.value()..target.x1.value()).all(|x| {
                rects.iter().any(|rect| {
                    rect.x0.value() <= x
                        && x < rect.x1.value()
                        && rect.y0.value() <= y
                        && y < rect.y1.value()
                })
            })
        })
    }

    fn set_layout(tree: &mut NodeTree, node: crate::node::id::NodeId, bounds: LogicalRect) {
        let layout = crate::node::tree::LayoutBox {
            bounds,
            border: crate::math::LogicalEdges::ZERO,
        };
        assert!(tree.set_layout_for_test(node, layout), "布局写入");
    }

    #[test]
    fn scrolling_only_moves_the_slot_offset_not_the_lane_bytes() {
        use crate::{
            interaction::{ScrollAxis, ScrollSlot},
            math::LogicalSize,
        };

        let mut h = Harness::new();
        let root = h.tree.root();
        let mut style = fixed_style(100.0, 100.0);
        style.clip = Clip::Bounds;
        let container = NodeBuilder::boxed()
            .style(style)
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("container");
        let child = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(100.0), px(20.0)))
            .paint(with_white())
            .build(&mut h.tree, container)
            .expect("child");
        layout_all(&mut h.tree);
        h.tree
            .ensure_interaction(container)
            .expect("interaction")
            .scroll = Some(ScrollSlot {
            offset: LogicalPoint::ZERO,
            content: LogicalSize::new(px(100.0), px(400.0)),
            axis: ScrollAxis::Vertical,
            line_height: px(48.0),
        });
        h.tree.note_scroll_container(container);

        let first = h.paint();
        assert!(first.rebuilt >= 2, "首帧两条 lane 都要建");
        assert_eq!(h.scroll_slots.len(), 2, "槽位 0 是哨兵，滚动容器占 1");
        assert_eq!(h.scroll_slots[1].0, LogicalPoint::ZERO);
        let container_box = h.tree.get(container).expect("container").layout().bounds;
        assert_eq!(
            h.scroll_slots[1].1,
            DeviceRect::new(
                dp(container_box.origin.x.value() as i32),
                dp(container_box.origin.y.value() as i32),
                dp((container_box.origin.x + container_box.size.width).value() as i32),
                dp((container_box.origin.y + container_box.size.height).value() as i32),
            ),
            "视口 = 容器的 padding box（物理像素）"
        );
        let before = lane_primitives(&h, child);
        assert!(
            !before
                .iter()
                .any(|primitive| matches!(primitive, Primitive::Clip(_))),
            "滚动容器的视口 clip 不得烘焙进 lane：它在内容坐标里随滚动移动，烘焙进去就要求\
             每滚一像素重编码全部 lane（改由槽位的屏幕裁剪区做每 op scissor）"
        );
        let quad_bounds = |primitives: &[Primitive]| {
            primitives
                .iter()
                .find_map(|primitive| match primitive {
                    Primitive::Quad(quad) => Some(quad.bounds),
                    _ => None,
                })
                .expect("child 有背景 quad")
        };
        assert_eq!(
            quad_bounds(&before),
            h.tree.get(child).expect("child").layout().bounds,
            "图元烘焙在容器内容空间（= 布局坐标，不含容器自己的滚动位移）"
        );

        let lane = h.tree.lane_of(child).expect("child lane");
        let version = h.scene.lane_state(lane).expect("lane").0;
        h.tree.scroll_slot_mut(container).expect("slot").offset =
            LogicalPoint::new(px(0.0), px(30.0));
        let second = h.paint();
        assert_eq!(second.rebuilt, 0, "滚动不得重建任何 lane");
        assert_eq!(
            h.scroll_slots[1].0,
            LogicalPoint::new(px(0.0), px(30.0)),
            "位移进槽位表"
        );
        assert_eq!(
            lane_primitives(&h, child),
            before,
            "滚动不得改图元字节（偏移在绘制期由 GPU 加上）"
        );
        assert_eq!(
            h.scene.lane_state(lane).expect("lane").0,
            version,
            "图元没变 ⇒ 内容版本不得前进（否则合成端会重编码）"
        );
        assert_eq!(
            h.scene.lane_scroll(lane),
            1,
            "lane 必须登记滚动槽 1（编码器据此生成 FrameOp::Instances.scroll）"
        );
    }

    #[test]
    fn geometry_chunk_damage_uses_the_chunk_aabb_and_transform() {
        let chunk = LogicalRect::new(px(0.0), px(0.0), px(200.0), px(200.0));
        let damage = super::geometry_chunk_damage(
            crate::math::Transform::IDENTITY,
            chunk,
            LogicalPoint::ZERO,
        );
        assert_eq!(damage, chunk, "无变换 / 无滚动时块 AABB 原样");

        let scrolled = super::geometry_chunk_damage(
            crate::math::Transform::IDENTITY,
            chunk,
            LogicalPoint::new(px(0.0), px(50.0)),
        );
        assert_eq!(
            scrolled,
            LogicalRect::new(px(0.0), px(-50.0), px(200.0), px(200.0)),
            "祖先滚动累计值要换算到屏幕坐标"
        );

        let moved = super::geometry_chunk_damage(
            crate::math::Transform::translate(px(30.0), px(40.0)),
            chunk,
            LogicalPoint::ZERO,
        );
        assert_eq!(
            moved,
            LogicalRect::new(px(30.0), px(40.0), px(200.0), px(200.0))
        );

        assert!(
            super::geometry_chunk_damage(
                crate::math::Transform::IDENTITY,
                LogicalRect::default(),
                LogicalPoint::ZERO
            )
            .is_empty()
        );
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
                    width: px(1.0),
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
                    width: px(1.0),
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
                shadows: Shadows::from(ShadowSpec {
                    offset: LogicalPoint::new(px(1.0), px(2.0)),
                    blur: px(3.0),
                    spread: px(0.0),
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
            h.tree
                .lane_of(node)
                .and_then(|lane| h.scene.lane_primitives(lane))
                .is_none_or(<[Primitive]>::is_empty),
            "隐藏节点不得留下任何图元"
        );
        assert!(h.scene.is_empty(), "隐藏之后场景里不应还有任何内容");
        assert!(!h.damage.finish(frame()).full);
    }

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
    #[test]
    fn own_clip_brackets_the_node_content() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let mut style = fixed_style(60.0, 20.0);
        style.clip = Clip::Bounds;
        let mut canvas = crate::paint::canvas::Canvas::new(LogicalRect::new(
            px(0.0),
            px(0.0),
            px(200.0),
            px(200.0),
        ));
        canvas.push_quad(crate::paint::primitive::Quad {
            bounds: LogicalRect::new(px(0.0), px(0.0), px(200.0), px(200.0)),
            corner_radius: [px(0.0); 4],
            paint: QuadPaint::Solid {
                fill: crate::paint::color::Color::WHITE,
                border: None,
            },
        });
        let clipped = NodeBuilder::canvas(canvas)
            .style(style)
            .build(&mut h.tree, root)
            .expect("挂载");
        let mut plain_canvas = crate::paint::canvas::Canvas::new(LogicalRect::new(
            px(0.0),
            px(0.0),
            px(200.0),
            px(200.0),
        ));
        plain_canvas.push_quad(crate::paint::primitive::Quad {
            bounds: LogicalRect::new(px(0.0), px(0.0), px(200.0), px(200.0)),
            corner_radius: [px(0.0); 4],
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
        let canvas = NodeBuilder::canvas(Canvas::new(LogicalRect::new(
            px(0.0),
            px(0.0),
            px(10.0),
            px(10.0),
        )))
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

    const INITIAL_ATLAS_GENERATION: u32 = 1;

    fn canvas_with_text(content: TextContent, origin: LogicalPoint) -> Canvas {
        let mut canvas = Canvas::new(LogicalRect::new(px(0.0), px(0.0), px(40.0), px(20.0)));
        assert!(canvas.push_text(content, origin), "the recipe is recorded");
        canvas
    }

    fn mount_canvas(h: &mut Harness, canvas: Canvas) -> NodeId {
        let root = h.tree.root();
        NodeBuilder::canvas(canvas)
            .style(fixed_style(40.0, 20.0))
            .build(&mut h.tree, root)
            .expect("挂载")
    }

    #[test]
    fn canvas_text_recipes_become_glyph_runs_during_paint() {
        let mut h = Harness::new();
        let origin = LogicalPoint::new(px(3.0), px(4.0));
        let node = mount_canvas(
            &mut h,
            canvas_with_text(TextContent::new("ab", TextStyle::new(px(12.0))), origin),
        );
        let line = shaped_line(
            vec![ShapedTextItem::GlyphRun(glyph_run(Color::BLACK, 0..2, 1.0))],
            Vec::new(),
        );
        let generation = h.tree.canvas_generation(node).expect("canvas node");
        let atlas_generation = h
            .atlas
            .as_ref()
            .map_or(INITIAL_ATLAS_GENERATION, |atlas| atlas.generation());
        h.canvas_text.insert(
            node,
            crate::window::canvas_text::ResolvedCanvasText {
                key: crate::window::canvas_text::CanvasTextKey {
                    font_stack_version: 0,
                    canvas_generation: generation,
                    dpr_bits: 1.0_f32.to_bits(),
                    atlas_generation,
                    color_atlas_generation: INITIAL_ATLAS_GENERATION,
                    glyph_eviction_epoch: 0,
                },
                lines: vec![Rc::new(line)],
                pending: false,
            },
        );
        layout_all(&mut h.tree);
        let report = h.paint();
        assert_eq!(report.unresolved_canvas_text, 0);
        let primitives = lane_primitives(&h, node);
        let Some(Primitive::Canvas(rendered)) = primitives.first() else {
            panic!("the canvas is pushed as one primitive");
        };
        assert!(!rendered.primitives().is_empty());
        let Some(Primitive::GlyphRun(run)) = rendered.primitives().first() else {
            panic!("the recipe is replaced by a glyph run");
        };
        assert_eq!(
            run.glyphs[0].position,
            LogicalPoint::new(origin.x + px(1.0), origin.y + px(2.0))
        );
        assert_eq!(run.color, Color::BLACK);
        assert!(
            !rendered
                .primitives()
                .iter()
                .any(|primitive| matches!(primitive, Primitive::Text(_))),
            "no unresolved recipe reaches the scene"
        );
    }

    #[test]
    fn an_unresolved_canvas_recipe_skips_only_its_text() {
        let mut h = Harness::new();
        let mut canvas = Canvas::new(LogicalRect::new(px(0.0), px(0.0), px(40.0), px(20.0)));
        canvas.push_primitive(Primitive::Quad(crate::paint::primitive::Quad {
            bounds: LogicalRect::new(px(0.0), px(0.0), px(4.0), px(4.0)),
            corner_radius: [LogicalPx::ZERO; 4],
            paint: crate::paint::primitive::QuadPaint::Solid {
                fill: Color::BLACK,
                border: None,
            },
        }));
        assert!(canvas.push_text(
            TextContent::new("ab", TextStyle::new(px(12.0))),
            LogicalPoint::ZERO
        ));
        let node = mount_canvas(&mut h, canvas);
        layout_all(&mut h.tree);
        let report = h.paint();
        assert_eq!(report.unresolved_canvas_text, 1);
        let primitives = lane_primitives(&h, node);
        let Some(Primitive::Canvas(rendered)) = primitives.first() else {
            panic!("the canvas itself keeps painting");
        };
        assert!(
            rendered
                .primitives()
                .iter()
                .any(|primitive| matches!(primitive, Primitive::Quad(_))),
            "non-text primitives are emitted even when the text recipe is unresolved"
        );
        assert!(
            !rendered
                .primitives()
                .iter()
                .any(|primitive| matches!(primitive, Primitive::GlyphRun(_) | Primitive::Text(_))),
            "only the unresolved text recipe is skipped"
        );
    }

    #[test]
    fn a_stale_canvas_text_line_is_never_painted() {
        let mut h = Harness::new();
        let node = mount_canvas(
            &mut h,
            canvas_with_text(
                TextContent::new("ab", TextStyle::new(px(12.0))),
                LogicalPoint::ZERO,
            ),
        );
        let mut atlas = crate::render::AtlasAllocator::with_page(
            crate::paint::atlas::AtlasKind::Glyph,
            (64, 64),
        );
        assert!(atlas.grow(), "page growth invalidates the cached tiles");
        let allocation = atlas.allocate((8, 8)).expect("the tile fits");
        let mut run = glyph_run(Color::BLACK, 0..2, 1.0);
        run.glyphs[0].tile = allocation.tile;
        let mut line = shaped_line(vec![ShapedTextItem::GlyphRun(run)], Vec::new());
        line.atlas_generation = atlas.generation().wrapping_add(1);
        assert!(
            !allocation.tile.is_stale(atlas.generation()),
            "本用例只让单色代际失效：tile 本身必须活着，否则测不出代际比较"
        );
        assert_eq!(
            line.color_atlas_generation,
            h.color_atlas.generation(),
            "本用例只让单色代际失效，彩色代际必须保持一致"
        );
        h.atlas = Some(atlas);
        let generation = h.tree.canvas_generation(node).expect("canvas node");
        let atlas_generation = h.atlas.as_ref().expect("atlas").generation();
        h.canvas_text.insert(
            node,
            crate::window::canvas_text::ResolvedCanvasText {
                key: crate::window::canvas_text::CanvasTextKey {
                    font_stack_version: line.font_stack_version,
                    canvas_generation: generation,
                    dpr_bits: 1.0_f32.to_bits(),
                    atlas_generation,
                    color_atlas_generation: INITIAL_ATLAS_GENERATION,
                    glyph_eviction_epoch: 0,
                },
                lines: vec![Rc::new(line)],
                pending: false,
            },
        );
        layout_all(&mut h.tree);
        let report = h.paint();
        assert_eq!(
            report.unresolved_canvas_text, 1,
            "a stale line must be reported instead of painted"
        );
        let primitives = lane_primitives(&h, node);
        let Some(Primitive::Canvas(rendered)) = primitives.first() else {
            panic!("the canvas is still painted");
        };
        assert!(
            !rendered
                .primitives()
                .iter()
                .any(|primitive| matches!(primitive, Primitive::GlyphRun(_))),
            "stale UVs must never reach the scene"
        );
    }

    fn mount_text_line(h: &mut Harness, text: &'static str, mut line: ShapedLine) -> NodeId {
        line.text = smol_str::SmolStr::new_static(text);
        let root = h.tree.root();
        let node = NodeBuilder::text(text, TextStyle::new(px(16.0)))
            .style(fixed_style(50.0, 20.0))
            .build(&mut h.tree, root)
            .expect("text node");
        let content = TextContent::new(text, TextStyle::new(px(16.0)));
        assert!(h.tree.set_text_content(node, content.clone()));
        assert!(
            h.tree
                .install_text_line_for_test(node, &content, Rc::new(line), px(20.0)),
            "文本行注入"
        );
        node
    }

    fn text_line_with_generations(h: &Harness) -> (ShapedLine, u32, u32) {
        let glyph_generation = h.atlas.as_ref().expect("glyph atlas").generation();
        let color_generation = h.color_atlas.generation();
        let mut line = shaped_line(
            vec![ShapedTextItem::GlyphRun(glyph_run(Color::BLACK, 0..2, 0.0))],
            Vec::new(),
        );
        line.atlas_generation = glyph_generation;
        line.color_atlas_generation = color_generation;
        (line, glyph_generation, color_generation)
    }

    #[test]
    fn a_color_atlas_generation_change_alone_marks_text_stale() {
        let mut h = Harness::new();
        h.atlas = Some(crate::render::AtlasAllocator::with_page(
            crate::paint::atlas::AtlasKind::Glyph,
            (64, 64),
        ));
        let (line, _, _) = text_line_with_generations(&h);
        let node = mount_text_line(&mut h, "ab", line);
        layout_all(&mut h.tree);
        let painted = h.paint();
        assert!(painted.stale_text.is_empty(), "代际一致时文本可直接重绘");
        assert!(
            lane_primitives(&h, node)
                .iter()
                .any(|primitive| matches!(primitive, Primitive::GlyphRun(_))),
            "代际一致时必须画出字形"
        );

        assert!(h.color_atlas.grow(), "彩色页扩容换代");
        h.mark_paint(node);
        let stale = h.paint();
        assert_eq!(
            stale.stale_text.as_slice(),
            &[node],
            "仅彩色代际变化也必须把文本行标 stale（否则会画旧 UV）"
        );
        assert!(!lane_has_glyph_runs(&h, node), "stale 行不得进入 scene");
    }

    #[test]
    fn a_glyph_atlas_generation_change_alone_marks_text_stale() {
        let mut h = Harness::new();
        h.atlas = Some(crate::render::AtlasAllocator::with_page(
            crate::paint::atlas::AtlasKind::Glyph,
            (64, 64),
        ));
        let (line, _, _) = text_line_with_generations(&h);
        let node = mount_text_line(&mut h, "ab", line);
        layout_all(&mut h.tree);
        let painted = h.paint();
        assert!(painted.stale_text.is_empty());

        assert!(
            h.atlas.as_mut().expect("glyph atlas").grow(),
            "单色页扩容换代"
        );
        h.mark_paint(node);
        let stale = h.paint();
        assert_eq!(
            stale.stale_text.as_slice(),
            &[node],
            "仅单色代际变化也必须把文本行标 stale"
        );
        assert!(!lane_has_glyph_runs(&h, node));
    }

    const ONE_PIXEL_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    fn glyph_tile() -> crate::paint::atlas::AtlasTile {
        crate::paint::atlas::AtlasTile {
            atlas: crate::paint::atlas::AtlasKind::Glyph,
            page: 0,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            generation: 1,
            slot: 0,
            slot_epoch: 0,
        }
    }

    fn glyph_run(color: Color, text_range: core::ops::Range<usize>, x: f32) -> ShapedGlyphRun {
        ShapedGlyphRun {
            color,
            text_range,
            glyphs: vec![crate::paint::primitive::PositionedGlyph {
                tile: glyph_tile(),
                position: LogicalPoint::new(px(x), px(2.0)),
                size: crate::math::LogicalSize::new(px(8.0), px(8.0)),
            }],
        }
    }

    fn shaped_line(items: Vec<ShapedTextItem>, decorations: Vec<ShapedDecoration>) -> ShapedLine {
        ShapedLine {
            text: smol_str::SmolStr::new_static("ab"),
            lines: vec![ShapedTextLine {
                text_range: 0..2,
                baseline: px(12.0),
                line_height: px(16.0),
                advance: px(20.0),
                items,
                decorations,
            }],
            width: px(20.0),
            height: px(16.0),
            first_baseline: Some(px(12.0)),
            atlas_generation: INITIAL_ATLAS_GENERATION,
            color_atlas_generation: INITIAL_ATLAS_GENERATION,
            font_stack_version: 1,
            scale: DevicePixelRatio::ONE,
        }
    }

    fn spawn_text(h: &mut Harness, content: TextContent, line: ShapedLine) -> NodeId {
        let root = h.tree.root();
        let node = NodeBuilder::text(content.text.clone(), content.style.clone())
            .style(fixed_style(50.0, 20.0))
            .build(&mut h.tree, root)
            .expect("text node");
        let content_for_shaping = content.clone();
        assert!(h.tree.set_text_content(node, content), "text content");
        assert!(
            h.tree
                .install_text_line_for_test(node, &content_for_shaping, Rc::new(line), px(20.0)),
            "text line"
        );
        layout_all(&mut h.tree);
        node
    }

    fn text_origin(h: &Harness, node: NodeId) -> LogicalPoint {
        h.tree.get(node).expect("node").layout().bounds.origin
    }

    #[test]
    fn multi_run_text_encodes_each_run_with_its_color() {
        let red = Color::Srgb {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            alpha: 1.0,
        };
        let blue = Color::Srgb {
            r: 0.0,
            g: 0.0,
            b: 1.0,
            alpha: 1.0,
        };
        let green = Color::Srgb {
            r: 0.0,
            g: 1.0,
            b: 0.0,
            alpha: 1.0,
        };
        let mut h = Harness::new();
        let node = spawn_text(
            &mut h,
            TextContent::new("ab", TextStyle::new(px(16.0))),
            shaped_line(
                vec![
                    ShapedTextItem::GlyphRun(glyph_run(red, 0..1, 0.0)),
                    ShapedTextItem::GlyphRun(glyph_run(blue, 1..2, 8.0)),
                ],
                Vec::new(),
            ),
        );
        let report = h.paint();
        let primitives = lane_primitives(&h, node);
        let runs: Vec<&Primitive> = primitives
            .iter()
            .filter(|primitive| matches!(primitive, Primitive::GlyphRun(_)))
            .collect();
        assert_eq!(runs.len(), 2, "每个 TextRun 一段 GlyphRun：{primitives:?}");
        let colors: Vec<Color> = runs
            .iter()
            .map(|primitive| match primitive {
                Primitive::GlyphRun(run) => run.color,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(colors, vec![red, blue], "逐 run 颜色必须分别编码");
        assert!(report.rebuilt > 0);
        assert!(h.tree.set_style_patch(
            node,
            StylePatch {
                foreground: Some(green),
                ..StylePatch::default()
            }
        ));
        h.mark_paint(node);
        let _ = h.paint();
        for primitive in lane_primitives(&h, node) {
            if let Primitive::GlyphRun(run) = primitive {
                assert_eq!(run.color, green, "foreground 覆盖逐 run 颜色");
            }
        }
    }

    #[test]
    fn text_decorations_are_encoded_as_quads_at_their_geometry() {
        let underline_color = Color::Srgb {
            r: 0.2,
            g: 0.4,
            b: 0.8,
            alpha: 1.0,
        };
        let strike_color = Color::Srgb {
            r: 0.8,
            g: 0.2,
            b: 0.2,
            alpha: 1.0,
        };
        let under = LogicalRect::new(px(1.0), px(14.5), px(10.0), px(1.0));
        let strike = LogicalRect::new(px(11.0), px(8.0), px(6.0), px(2.0));
        let mut h = Harness::new();
        let node = spawn_text(
            &mut h,
            TextContent::new("ab", TextStyle::new(px(16.0))),
            shaped_line(
                vec![ShapedTextItem::GlyphRun(glyph_run(Color::BLACK, 0..2, 0.0))],
                vec![
                    ShapedDecoration {
                        rect: under,
                        color: underline_color,
                    },
                    ShapedDecoration {
                        rect: strike,
                        color: strike_color,
                    },
                ],
            ),
        );
        let origin = text_origin(&h, node);
        let report = h.paint();
        let quads: Vec<Primitive> = lane_primitives(&h, node)
            .into_iter()
            .filter(|primitive| matches!(primitive, Primitive::Quad(_)))
            .collect();
        assert_eq!(quads.len(), 2, "每条装饰一个 Quad：{quads:?}");
        let expected = [(under, underline_color), (strike, strike_color)];
        for (index, primitive) in quads.iter().enumerate() {
            let Primitive::Quad(quad) = primitive else {
                unreachable!()
            };
            assert_eq!(
                quad.bounds,
                expected[index].0.translate(origin),
                "装饰 Quad 的 bounds = 局部矩形 + 文本盒原点"
            );
            assert_eq!(quad.corner_radius, [LogicalPx::ZERO; 4]);
            let QuadPaint::Solid { fill, border } = &quad.paint else {
                panic!("装饰必须是实色 Quad");
            };
            assert_eq!(*fill, expected[index].1);
            assert!(border.is_none(), "装饰没有边框");
        }
        assert!(report.rebuilt > 0);
    }

    #[test]
    fn foreground_overrides_glyph_and_decoration_colors() {
        let text_color = Color::Srgb {
            r: 0.1,
            g: 0.1,
            b: 0.1,
            alpha: 1.0,
        };
        let explicit_line = Color::Srgb {
            r: 0.9,
            g: 0.8,
            b: 0.1,
            alpha: 1.0,
        };
        let foreground = Color::Srgb {
            r: 0.0,
            g: 0.7,
            b: 0.2,
            alpha: 1.0,
        };
        let mut h = Harness::new();
        let node = spawn_text(
            &mut h,
            TextContent::new("ab", TextStyle::new(px(16.0)).with_color(text_color)),
            shaped_line(
                vec![ShapedTextItem::GlyphRun(glyph_run(text_color, 0..2, 0.0))],
                vec![
                    ShapedDecoration {
                        rect: LogicalRect::new(px(0.0), px(14.5), px(10.0), px(1.0)),
                        color: text_color,
                    },
                    ShapedDecoration {
                        rect: LogicalRect::new(px(10.0), px(8.0), px(10.0), px(1.0)),
                        color: explicit_line,
                    },
                ],
            ),
        );
        let decoration_fills = |h: &Harness| -> Vec<Color> {
            lane_primitives(h, node)
                .into_iter()
                .filter_map(|primitive| match primitive {
                    Primitive::Quad(quad) => match quad.paint {
                        QuadPaint::Solid { fill, .. } => Some(fill),
                        QuadPaint::Gradient(_) => None,
                    },
                    _ => None,
                })
                .collect()
        };
        let glyph_colors = |h: &Harness| -> Vec<Color> {
            lane_primitives(h, node)
                .into_iter()
                .filter_map(|primitive| match primitive {
                    Primitive::GlyphRun(run) => Some(run.color),
                    _ => None,
                })
                .collect()
        };

        let _ = h.paint();
        assert_eq!(decoration_fills(&h), vec![text_color, explicit_line]);
        assert_eq!(glyph_colors(&h), vec![text_color]);

        assert!(h.tree.set_style_patch(
            node,
            StylePatch {
                foreground: Some(foreground),
                ..StylePatch::default()
            }
        ));
        h.mark_paint(node);
        let _ = h.paint();
        assert_eq!(
            decoration_fills(&h),
            vec![foreground, foreground],
            "foreground 必须覆盖装饰线色（含显式 TextDecoration::color）"
        );
        assert_eq!(
            glyph_colors(&h),
            vec![foreground],
            "foreground 必须覆盖字形颜色"
        );

        assert!(h.tree.set_style_patch(
            node,
            StylePatch {
                foreground: Some(text_color),
                ..StylePatch::default()
            }
        ));
        h.mark_paint(node);
        let _ = h.paint();
        assert_eq!(decoration_fills(&h), vec![text_color, text_color]);
        assert_eq!(glyph_colors(&h), vec![text_color]);
    }

    #[test]
    fn inline_box_quad_paint_uses_the_landing_rect() {
        let fill = Color::Srgb {
            r: 0.1,
            g: 0.1,
            b: 0.1,
            alpha: 1.0,
        };
        let spec = |id: u64, index: usize, paint: Option<InlineBoxPaint>| InlineBoxSpec {
            id,
            index,
            size: crate::math::LogicalSize::new(px(12.0), px(12.0)),
            baseline: None,
            align: InlineAlign::Baseline,
            paint,
        };
        let content = TextContent::new("a b", TextStyle::new(px(16.0))).with_inline_boxes([
            spec(
                7,
                1,
                Some(InlineBoxPaint::Quad {
                    color: fill,
                    corner_radius: [px(2.0), px(3.0), px(4.0), px(5.0)],
                }),
            ),
            spec(8, 2, None),
        ]);
        let boxes = [
            ShapedInlineBox {
                id: 7,
                rect: LogicalRect::new(px(4.0), px(0.0), px(12.0), px(12.0)),
                baseline: None,
            },
            ShapedInlineBox {
                id: 8,
                rect: LogicalRect::new(px(20.0), px(0.0), px(12.0), px(12.0)),
                baseline: None,
            },
        ];
        let mut h = Harness::new();
        let node = spawn_text(
            &mut h,
            content,
            shaped_line(
                vec![
                    ShapedTextItem::InlineBox(boxes[0]),
                    ShapedTextItem::InlineBox(boxes[1]),
                ],
                Vec::new(),
            ),
        );
        let origin = text_origin(&h, node);
        let _ = h.paint();
        let quads: Vec<Primitive> = lane_primitives(&h, node)
            .into_iter()
            .filter(|primitive| matches!(primitive, Primitive::Quad(_)))
            .collect();
        assert_eq!(quads.len(), 1, "只有 paint = Some 的行内盒绘制：{quads:?}");
        let Primitive::Quad(quad) = &quads[0] else {
            unreachable!()
        };
        assert_eq!(quad.bounds, boxes[0].rect.translate(origin));
        assert_eq!(quad.corner_radius, [px(2.0), px(3.0), px(4.0), px(5.0)]);
    }

    #[test]
    fn inline_box_image_paint_resolves_tiles_and_skips_missing_resources() {
        let mut resources = crate::resources::ResourceCache::with_page_for_test((16, 16));
        let image = resources
            .insert(ONE_PIXEL_PNG)
            .expect("1×1 PNG 必须解码成功");
        let spec = |id: u64, paint: InlineBoxPaint| InlineBoxSpec {
            id,
            index: 1,
            size: crate::math::LogicalSize::new(px(8.0), px(8.0)),
            baseline: None,
            align: InlineAlign::Baseline,
            paint: Some(paint),
        };
        let content = TextContent::new("a b", TextStyle::new(px(16.0))).with_inline_boxes([
            spec(
                5,
                InlineBoxPaint::Image {
                    resource: image.id(),
                    modulation: Color::WHITE,
                },
            ),
            spec(
                6,
                InlineBoxPaint::Image {
                    resource: crate::resources::ResourceId::for_test(99, 1),
                    modulation: Color::WHITE,
                },
            ),
        ]);
        let boxes = [
            ShapedInlineBox {
                id: 5,
                rect: LogicalRect::new(px(2.0), px(1.0), px(8.0), px(8.0)),
                baseline: None,
            },
            ShapedInlineBox {
                id: 6,
                rect: LogicalRect::new(px(14.0), px(1.0), px(8.0), px(8.0)),
                baseline: None,
            },
        ];
        let mut h = Harness::new();
        let expected = image
            .variant_for(&resources, crate::math::DeviceSize::new(dp(8), dp(8)))
            .expect("位图的原生变体恒可用");
        h.resources = Some(resources);
        let node = spawn_text(
            &mut h,
            content,
            shaped_line(
                vec![
                    ShapedTextItem::GlyphRun(glyph_run(Color::BLACK, 0..2, 0.0)),
                    ShapedTextItem::InlineBox(boxes[0]),
                    ShapedTextItem::InlineBox(boxes[1]),
                ],
                Vec::new(),
            ),
        );
        let origin = text_origin(&h, node);
        let report = h.paint();
        let images: Vec<Primitive> = lane_primitives(&h, node)
            .into_iter()
            .filter(|primitive| matches!(primitive, Primitive::Image(_)))
            .collect();
        assert_eq!(images.len(), 1, "只有已加载的资源绘制：{images:?}");
        let Primitive::Image(primed) = &images[0] else {
            unreachable!()
        };
        assert_eq!(primed.tile, expected, "tile 必须来自 ResourceImage");
        assert_eq!(primed.bounds, boxes[0].rect.translate(origin));
        assert_eq!(primed.modulation, Color::WHITE);
        assert_eq!(report.skipped_inline_images.as_slice(), &[6]);

        let mut cache = h.resources.take().expect("resources");
        assert!(cache.unload(image.id()));
        h.resources = Some(cache);
        h.mark_paint(node);
        let report = h.paint();
        assert!(
            lane_primitives(&h, node)
                .iter()
                .all(|primitive| !matches!(primitive, Primitive::Image(_))),
            "资源卸载后不得编码旧 tile"
        );
        assert_eq!(
            report.skipped_inline_images.as_slice(),
            &[5, 6],
            "已卸载 + 未加载的图片都要可观测"
        );
    }

    #[test]
    fn geometry_node_transform_reaches_the_primitive() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let geometry = NodeBuilder::geometry(GeometryId::for_test(0, 1))
            .style(fixed_style(10.0, 10.0))
            .build(&mut h.tree, root)
            .expect("挂载");
        h.tree
            .set_geometry_transform(
                geometry,
                crate::math::Transform::translate(px(4.0), px(6.0)),
                crate::math::LogicalRect::default(),
            )
            .expect("几何节点");
        layout_all(&mut h.tree);
        let _ = h.paint();
        let Some(Primitive::Geometry(draw)) = lane_primitives(&h, geometry).first().cloned() else {
            panic!("必须产生一条几何绘制");
        };
        assert_eq!(
            draw.transform,
            crate::math::Transform::translate(px(4.0), px(6.0)),
            "节点级几何的 transform 必须是 setter 写入的那个"
        );
        let boxed = NodeBuilder::boxed()
            .style(fixed_style(10.0, 10.0))
            .build(&mut h.tree, root)
            .expect("挂载");
        assert!(matches!(
            h.tree.set_geometry_transform(
                boxed,
                crate::math::Transform::IDENTITY,
                crate::math::LogicalRect::default()
            ),
            Err(crate::error::EngineError::StaleNode { .. })
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

        let resize = crate::node::tree::LayoutBox {
            bounds: LogicalRect::new(px(0.0), px(0.0), px(60.0), px(60.0)),
            border: crate::math::LogicalEdges::ZERO,
        };
        let _ = h.tree.set_layout_for_test(clipped, resize);
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
            Some((
                1,
                Some(DevicePixelRatio::ONE.to_covering_device_rect(second_rect))
            ))
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
        let empty = crate::node::tree::LayoutBox {
            bounds: LogicalRect::default(),
            border: crate::math::LogicalEdges::ZERO,
        };
        let _ = h.tree.set_layout_for_test(clipped, empty);
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
        let _ = h.paint();
        assert!(
            h.tree
                .clip_key(deepest)
                .is_some_and(|(depth, _)| depth == 7)
        );
    }

    #[test]
    fn empty_clipping_containers_do_not_consume_clip_depth() {
        let mut h = Harness::new();
        let _ = nest_clips(&mut h, usize::from(crate::paint::scene::MAX_CLIP_DEPTH));
        let _ = h.paint();
    }

    #[test]
    fn clip_depth_over_limit_folds_instead_of_failing() {
        let mut h = Harness::new();
        let deepest = nest_clips(&mut h, usize::from(crate::paint::scene::MAX_CLIP_DEPTH) + 1);
        let report = h.paint();
        assert!(report.rebuilt > 0, "折叠后必须仍能绘制");
        assert!(
            h.tree
                .clip_key(deepest)
                .is_some_and(|(depth, _)| usize::from(depth)
                    <= usize::from(crate::paint::scene::MAX_CLIP_DEPTH)),
            "折叠后每层的 clip 深度不得超过上限"
        );
    }

    fn lane_version(h: &Harness, lane: LaneId) -> u32 {
        h.scene.lane_state(lane).expect("lane 活着").0
    }

    fn rendered_state(
        h: &Harness,
    ) -> (
        Vec<(NodeId, Vec<Primitive>, u32)>,
        Vec<NodeId>,
        Vec<(LogicalPoint, DeviceRect)>,
    ) {
        let ids = h.tree.dfs_ids();
        let mut lanes = Vec::new();
        let mut lane_nodes = ahash::HashMap::default();
        for id in ids {
            let Some(lane) = h.tree.lane_of(id) else {
                continue;
            };
            lane_nodes.insert(lane, id);
            let primitives = h
                .scene
                .lane_primitives(lane)
                .map_or_else(Vec::new, <[Primitive]>::to_vec);
            lanes.push((id, primitives, h.scene.lane_scroll(lane)));
        }
        let drawn_order: Vec<NodeId> = h
            .order
            .iter()
            .filter(|lane| {
                h.scene
                    .lane_primitives(**lane)
                    .is_some_and(|primitives| !primitives.is_empty())
            })
            .filter_map(|lane| lane_nodes.get(lane).copied())
            .collect();
        (lanes, drawn_order, h.scroll_slots.clone())
    }

    #[test]
    fn incremental_only_visits_the_dirty_closure() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let container = NodeBuilder::boxed()
            .style(fixed_style(200.0, 100.0))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("container");
        let left = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(40.0), px(20.0)))
            .paint(with_white())
            .build(&mut h.tree, container)
            .expect("left");
        let right = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(60.0), px(0.0), px(40.0), px(20.0)))
            .paint(with_white())
            .build(&mut h.tree, container)
            .expect("right");
        let sibling = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(40.0), px(40.0), px(20.0)))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("sibling");
        layout_all(&mut h.tree);

        let first = h.paint();
        assert_eq!(first.rebuilt, 4, "四个有图元的节点首帧都要建 lane");
        let order_before = h.order.clone();
        let left_lane = h.tree.lane_of(left).expect("left lane");
        let right_lane = h.tree.lane_of(right).expect("right lane");
        let sibling_lane = h.tree.lane_of(sibling).expect("sibling lane");
        let (left_v, right_v, sibling_v) = (
            lane_version(&h, left_lane),
            lane_version(&h, right_lane),
            lane_version(&h, sibling_lane),
        );
        let counters_before = h.tree.paint_counters();

        h.mark_paint(left);
        let report = h.paint_incremental(&[left]);
        assert_eq!(report.rebuilt, 1, "只有变化节点重建 lane");
        let counters = h.tree.paint_counters();
        assert_eq!(
            counters.visited_nodes, 3,
            "访问数 = left + container + root"
        );
        assert!(
            counters.visited_nodes < h.tree.len(),
            "访问数必须小于树规模（真实负载下远小于）：{} vs {}",
            counters.visited_nodes,
            h.tree.len()
        );
        assert_eq!(
            counters.full_frames, counters_before.full_frames,
            "增量帧不得退化为全量"
        );
        assert_eq!(
            counters.incremental_frames,
            counters_before.incremental_frames + 1
        );
        assert_eq!(
            counters.order_rebuilds, counters_before.order_rebuilds,
            "没有 lane 成员变化 ⇒ 不得重建绘制序"
        );
        assert_eq!(h.order, order_before, "增量路径不得改绘制序");
        assert_eq!(h.scene.order(), order_before.as_slice());
        assert!(
            lane_version(&h, left_lane) > left_v,
            "被改 lane 的内容版本必须推进"
        );
        assert_eq!(
            lane_version(&h, right_lane),
            right_v,
            "兄弟 lane 不得被触碰"
        );
        assert_eq!(
            lane_version(&h, sibling_lane),
            sibling_v,
            "无关子树 lane 不得被触碰"
        );
        #[cfg(feature = "test-support")]
        {
            let log = &report.log.lanes;
            assert!(log.iter().any(|entry| entry.node == left));
            assert!(
                !log.iter().any(|entry| entry.node == right),
                "未变化的兄弟不得被访问：{log:?}"
            );
            assert!(
                !log.iter().any(|entry| entry.node == sibling),
                "无关子树不得被访问：{log:?}"
            );
        }
    }

    #[test]
    fn incremental_expands_the_subtree_when_the_clip_context_changes() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let container = NodeBuilder::boxed()
            .style(fixed_style(100.0, 100.0))
            .build(&mut h.tree, root)
            .expect("container");
        let child = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(10.0), px(10.0)))
            .paint(with_white())
            .build(&mut h.tree, container)
            .expect("child");
        layout_all(&mut h.tree);
        let _ = h.paint();
        let before = lane_primitives(&h, child);
        assert!(
            !before
                .iter()
                .any(|primitive| matches!(primitive, Primitive::Clip(_))),
            "初始没有 clip 前缀"
        );

        let mut clipped = fixed_style(100.0, 100.0);
        clipped.clip = Clip::Bounds;
        h.tree.set_style_for_test(container, clipped);
        h.mark_paint(container);
        let report = h.paint_incremental(&[container]);
        assert!(report.rebuilt >= 1, "clip 变化必须重建子树 lane");
        let after = lane_primitives(&h, child);
        assert!(
            matches!(after.first(), Some(Primitive::Clip(ClipOp::Push(_)))),
            "clip 变化必须传播到子树：{after:?}"
        );
        assert!(
            h.tree.clip_key(child).is_some_and(|(depth, _)| depth == 1),
            "子树的 clip_key 必须写回新上下文"
        );
    }

    #[test]
    fn incremental_hides_the_whole_subtree() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let container = NodeBuilder::boxed()
            .style(fixed_style(100.0, 100.0))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("container");
        let child = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(10.0), px(10.0)))
            .paint(with_white())
            .build(&mut h.tree, container)
            .expect("child");
        layout_all(&mut h.tree);
        let _ = h.paint();
        assert!(!h.scene.is_empty());
        let container_lane = h.tree.lane_of(container).expect("lane");
        let child_lane = h.tree.lane_of(child).expect("lane");
        let order_before = h.order.clone();

        h.tree.set_style_patch_for_test(
            container,
            StylePatch {
                visible: Some(false),
                ..StylePatch::default()
            },
        );
        h.mark_paint(container);
        let report = h.paint_incremental(&[container]);
        assert!(report.rebuilt >= 2, "整棵子树都要清空");
        assert!(report.damage.rects > 0, "隐藏必须产生重绘 damage");
        assert!(h.scene.is_empty(), "隐藏之后场景里不应还有任何内容");
        assert!(
            h.scene
                .lane_primitives(container_lane)
                .is_some_and(<[Primitive]>::is_empty),
            "容器 lane 必须被清空（保留分配）"
        );
        assert!(
            h.scene
                .lane_primitives(child_lane)
                .is_some_and(<[Primitive]>::is_empty),
            "子节点 lane 也必须被清空（hide_subtree 递归）"
        );
        assert_eq!(h.order, order_before, "隐藏不改变绘制序成员");
    }

    #[test]
    fn incremental_new_lane_rebuilds_the_order() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let node = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(10.0), px(10.0)))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("node");
        layout_all(&mut h.tree);
        h.tree.set_style_patch_for_test(
            node,
            StylePatch {
                visible: Some(false),
                ..StylePatch::default()
            },
        );
        let _ = h.paint();
        assert!(
            h.tree.lane_of(node).is_none(),
            "隐藏节点不得在首帧分配 lane"
        );
        assert!(h.order.is_empty(), "隐藏节点不进绘制序");

        h.tree.set_style_patch_for_test(
            node,
            StylePatch {
                visible: Some(true),
                ..StylePatch::default()
            },
        );
        h.mark_paint(node);
        let counters_before = h.tree.paint_counters();
        let report = h.paint_incremental(&[node]);
        assert_eq!(report.rebuilt, 1);
        let lane = h.tree.lane_of(node).expect("重新显示必须新建 lane");
        assert_eq!(h.order, vec![lane], "兜底重建的绘制序必须包含新 lane");
        assert_eq!(h.scene.order(), h.order.as_slice());
        assert_eq!(
            h.tree.paint_counters().order_rebuilds,
            counters_before.order_rebuilds + 1,
            "新建 lane 必须记一次绘制序兜底"
        );
    }

    #[test]
    fn incremental_reuses_the_cached_scroll_slot() {
        use crate::{
            interaction::{ScrollAxis, ScrollSlot},
            math::LogicalSize,
        };

        let mut h = Harness::new();
        let root = h.tree.root();
        let mut style = fixed_style(100.0, 100.0);
        style.clip = Clip::Bounds;
        let container = NodeBuilder::boxed()
            .style(style)
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("container");
        let child = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(100.0), px(20.0)))
            .paint(with_white())
            .build(&mut h.tree, container)
            .expect("child");
        layout_all(&mut h.tree);
        h.tree
            .ensure_interaction(container)
            .expect("interaction")
            .scroll = Some(ScrollSlot {
            offset: LogicalPoint::ZERO,
            content: LogicalSize::new(px(100.0), px(400.0)),
            axis: ScrollAxis::Vertical,
            line_height: px(48.0),
        });
        h.tree.note_scroll_container(container);
        let first = h.paint();
        assert!(first.rebuilt >= 2);
        assert_eq!(h.scroll_slots.len(), 2, "哨兵 + 一个滚动槽");
        let child_lane = h.tree.lane_of(child).expect("child lane");
        assert_eq!(h.scene.lane_scroll(child_lane), 1, "lane 登记槽位 1");

        h.tree.scroll_slot_mut(container).expect("slot").offset =
            LogicalPoint::new(px(0.0), px(30.0));
        h.mark_paint(child);
        let report = h.paint_incremental(&[child]);
        assert_eq!(report.rebuilt, 1, "只有脏节点重建");
        assert_eq!(h.scroll_slots.len(), 2, "增量路径不得重排 / 追加槽位");
        assert_eq!(
            h.scroll_slots[1].0,
            LogicalPoint::new(px(0.0), px(30.0)),
            "偏移必须写回缓存的槽位"
        );
        assert_eq!(h.scene.lane_scroll(child_lane), 1, "槽号不得改变");
    }

    #[test]
    fn incremental_and_full_paths_produce_the_same_scene() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let mut clip_style = fixed_style(100.0, 100.0);
        clip_style.clip = Clip::Bounds;
        let clip_container = NodeBuilder::boxed()
            .style(clip_style)
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("clip container");
        let inner = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(10.0), px(10.0)))
            .paint(with_white())
            .build(&mut h.tree, clip_container)
            .expect("inner");
        let _sibling = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(50.0), px(40.0), px(20.0)))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("sibling");
        let hider = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(60.0), px(50.0), px(40.0), px(20.0)))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("hider");
        layout_all(&mut h.tree);
        let mut first = h.paint();
        assert!(first.rebuilt >= 4);

        let resize = crate::node::tree::LayoutBox {
            bounds: LogicalRect::new(px(0.0), px(0.0), px(60.0), px(60.0)),
            border: crate::math::LogicalEdges::ZERO,
        };
        let _ = h.tree.set_layout_for_test(clip_container, resize);
        h.tree.set_style_patch_for_test(
            hider,
            StylePatch {
                visible: Some(false),
                ..StylePatch::default()
            },
        );
        h.mark_dirty(&[clip_container, hider, inner]);
        let _ = h.paint_incremental(&[clip_container, hider, inner]);
        let incremental = rendered_state(&h);

        let ids = h.tree.dfs_ids();
        h.mark_dirty(&ids);
        first = h.paint();
        assert!(first.rebuilt >= 4, "全量重放必须重建全部有图元的节点");
        let full = rendered_state(&h);

        assert_eq!(
            incremental.0.len(),
            full.0.len(),
            "lane 集合（按节点归一化）必须一致"
        );
        assert_eq!(incremental, full, "增量与全量产物必须逐字段一致");
    }

    fn long_geometry(triangles: u32) -> crate::paint::geometry::GeometryDesc {
        use crate::paint::geometry::{GeometryDesc, GeometryPipeline, IndexType, Topology};
        let mut vertex_bytes = Vec::with_capacity(triangles as usize * 36);
        let mut index_bytes = Vec::with_capacity(triangles as usize * 6);
        for triangle in 0..triangles {
            let base = (triangle * 3) as u16;
            let x = (triangle % 64) as f32;
            let y = (triangle / 64) as f32;
            for (dx, dy) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)] {
                vertex_bytes.extend_from_slice(&(x + dx).to_ne_bytes());
                vertex_bytes.extend_from_slice(&(y + dy).to_ne_bytes());
                vertex_bytes.extend_from_slice(&(-1.0f32).to_ne_bytes());
            }
            for index in [base, base + 1, base + 2] {
                index_bytes.extend_from_slice(&index.to_ne_bytes());
            }
        }
        GeometryDesc {
            vertex_bytes: bytes::Bytes::from(vertex_bytes),
            index_bytes: bytes::Bytes::from(index_bytes),
            vertex_stride: 12,
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: LogicalRect::new(px(0.0), px(0.0), px(64.0), px(100.0)),
            pipeline: GeometryPipeline::Ui,
        }
    }

    fn mount_geometry(h: &mut Harness, id: GeometryId, clip: Clip) -> NodeId {
        let root = h.tree.root();
        let mut style = fixed_style(100.0, 100.0);
        style.clip = clip;
        let node = NodeBuilder::geometry(id)
            .style(style)
            .build(&mut h.tree, root)
            .expect("挂载几何节点");
        layout_all(&mut h.tree);
        node
    }

    fn append_triangle(h: &mut Harness, id: GeometryId, x: f32, y: f32) {
        let snapshot = h.geometry.snapshot_arc(id).expect("已注册");
        let vertex_at = snapshot.vertex_len();
        let mut vertices = Vec::with_capacity(36);
        for (dx, dy) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)] {
            vertices.extend_from_slice(&(x + dx).to_ne_bytes());
            vertices.extend_from_slice(&(y + dy).to_ne_bytes());
            vertices.extend_from_slice(&(-1.0f32).to_ne_bytes());
        }
        h.geometry
            .update_vertices(id, vertex_at..vertex_at + 36, &vertices)
            .expect("追加顶点");
        let index_at = snapshot.index_len();
        let base = (snapshot.vertex_len() / 12) as u16;
        let indices = [base, base + 1, base + 2];
        let mut bytes = Vec::with_capacity(6);
        for index in indices {
            bytes.extend_from_slice(&index.to_ne_bytes());
        }
        h.geometry
            .update_indices(id, index_at..index_at + 6, &bytes)
            .expect("追加索引");
    }

    #[test]
    fn geometry_node_gets_one_clipped_lane_per_chunk_in_order() {
        let mut h = Harness::new();
        let id = h
            .geometry
            .insert(long_geometry(4096 + 10))
            .expect("注册两块几何");
        let node = mount_geometry(&mut h, id, Clip::Bounds);
        h.paint();

        assert_eq!(h.tree.lane_count(node), 2, "两块 = 两条块 lane");
        let first = h.tree.lane_at(node, 0).expect("块 0 lane");
        let second = h.tree.lane_at(node, 1).expect("块 1 lane");
        assert_eq!(h.scene.lane_geometry_chunk(first), Some((id, 0)));
        assert_eq!(h.scene.lane_geometry_chunk(second), Some((id, 1)));
        assert_eq!(
            h.order,
            vec![first, second],
            "块间绘制序 = 块序，两条 lane 相邻"
        );

        for lane in [first, second] {
            let primitives = h.scene.lane_primitives(lane).expect("lane 活着");
            assert!(
                matches!(primitives.first(), Some(Primitive::Clip(ClipOp::Push(_)))),
                "块 lane 必须以内容 clip 前缀开始"
            );
            assert!(
                matches!(primitives.last(), Some(Primitive::Clip(ClipOp::Pop))),
                "块 lane 必须以内容 clip 后缀结束"
            );
            assert_eq!(
                primitives
                    .iter()
                    .filter(|primitive| matches!(primitive, Primitive::Geometry(_)))
                    .count(),
                1,
                "每条块 lane 恰好一条几何绘制"
            );
        }
        let ranges: Vec<std::ops::Range<u32>> = [first, second]
            .iter()
            .map(|lane| {
                let primitives = h.scene.lane_primitives(*lane).expect("lane 活着");
                match primitives
                    .iter()
                    .find(|primitive| matches!(primitive, Primitive::Geometry(_)))
                {
                    Some(Primitive::Geometry(draw)) => draw.range.clone(),
                    _ => panic!("块 lane 必须有几何绘制"),
                }
            })
            .collect();
        assert_eq!(
            ranges,
            vec![
                0..crate::paint::geometry::GEOMETRY_CHUNK_INDICES,
                12288..12318
            ],
            "块 lane 只携带本块的索引区间（尾块按实际索引数）"
        );
    }

    #[test]
    fn geometry_truncation_damages_only_the_removed_chunk_bounds() {
        let mut h = Harness::new();
        let id = h
            .geometry
            .insert(long_geometry(4096 + 10))
            .expect("注册两块几何");
        let node = mount_geometry(&mut h, id, Clip::None);
        h.paint();
        let old_tail = h
            .geometry
            .snapshot_arc(id)
            .expect("快照")
            .chunk(1)
            .expect("块 1")
            .bounds;
        assert!(
            old_tail.origin.y >= px(64.0),
            "尾块的旧 AABB 必须在块的 y 段上：{old_tail:?}"
        );

        let update = h
            .geometry
            .truncate(id, 4096 * 3, crate::paint::geometry::GEOMETRY_CHUNK_INDICES)
            .expect("截断成一块");
        assert!(
            !update.damaged.is_empty(),
            "被移除的块必须有 store 级 damage"
        );
        h.tree.mark_geometry_updated(id);
        let report = h.paint_incremental(&[node]);

        assert!(
            !report.damage.full,
            "块级 range 收缩是局部 damage，不得退化为整帧"
        );
        assert_eq!(h.tree.lane_count(node), 1, "截断后只剩一条块 lane");
        let rects = h.damage.rects();
        let hits = |x: i32, y: i32| {
            rects.iter().any(|rect| {
                rect.x0.value() <= x
                    && x < rect.x1.value()
                    && rect.y0.value() <= y
                    && y < rect.y1.value()
            })
        };
        let probe_y = old_tail.origin.y.value() as i32;
        assert!(
            hits(1, probe_y),
            "被移除块的旧像素必须并入 damage（否则留残影）：{rects:?}"
        );
        let lane = h.tree.lane_at(node, 0).expect("块 0 lane");
        let Some(Primitive::Geometry(draw)) = h
            .scene
            .lane_primitives(lane)
            .expect("lane 活着")
            .iter()
            .find(|primitive| matches!(primitive, Primitive::Geometry(_)))
        else {
            panic!("块 0 lane 必须有几何绘制")
        };
        assert_eq!(
            draw.range,
            0..crate::paint::geometry::GEOMETRY_CHUNK_INDICES,
            "保留下来的块仍画整块"
        );
    }

    #[test]
    fn geometry_data_update_rebuilds_only_the_tail_chunk_lane() {
        let mut h = Harness::new();
        let id = h
            .geometry
            .insert(long_geometry(4096 + 2))
            .expect("注册两块几何");
        let node = mount_geometry(&mut h, id, Clip::None);
        h.paint();
        let first = h.tree.lane_at(node, 0).expect("块 0 lane");
        let second = h.tree.lane_at(node, 1).expect("块 1 lane");
        let first_version = h.scene.lane_state(first).expect("lane").0;
        let second_version = h.scene.lane_state(second).expect("lane").0;

        append_triangle(&mut h, id, 100.0, 100.0);
        assert_eq!(h.tree.mark_geometry_updated(id), 1, "绑定节点被标脏");
        let report = h.paint_incremental(&[node]);

        assert_eq!(report.rebuilt, 1, "只重建尾块 lane");
        assert_eq!(
            h.scene.lane_state(first).expect("lane").0,
            first_version,
            "第一块 lane 内容版本不动（编码器整段复用）"
        );
        assert_ne!(
            h.scene.lane_state(second).expect("lane").0,
            second_version,
            "尾块 lane 必须重建"
        );
        assert_eq!(h.order, vec![first, second], "增量帧不重排绘制序");
        assert!(
            report.damage.rects > 0 && !report.damage.full,
            "尾块新范围产生局部 damage（不是整帧）"
        );
    }

    #[test]
    fn geometry_data_update_damage_covers_old_and_new_block_bounds() {
        use crate::paint::geometry::{GeometryDesc, GeometryPipeline, IndexType, Topology};

        fn triangle(x: f32, y: f32) -> Vec<u8> {
            let mut bytes = Vec::with_capacity(36);
            for (dx, dy) in [(0.0f32, 0.0f32), (1.0, 0.0), (0.0, 1.0)] {
                bytes.extend_from_slice(&(x + dx).to_ne_bytes());
                bytes.extend_from_slice(&(y + dy).to_ne_bytes());
                bytes.extend_from_slice(&(-1.0f32).to_ne_bytes());
            }
            bytes
        }

        let mut h = Harness::new();
        let id = h
            .geometry
            .insert(GeometryDesc {
                vertex_bytes: bytes::Bytes::from(triangle(10.0, 10.0)),
                index_bytes: bytes::Bytes::from_static(&[0, 0, 1, 0, 2, 0]),
                vertex_stride: 12,
                index_type: IndexType::U16,
                topology: Topology::TriangleList,
                bounds: LogicalRect::new(px(0.0), px(0.0), px(400.0), px(400.0)),
                pipeline: GeometryPipeline::Ui,
            })
            .expect("注册单三角形几何");
        let node = mount_geometry(&mut h, id, Clip::None);
        h.paint();
        assert_eq!(h.tree.lane_count(node), 1, "单块几何只有一条块 lane");

        h.geometry
            .update_vertices(id, 0..36, &triangle(100.0, 60.0))
            .expect("整体移动");
        assert_eq!(h.tree.mark_geometry_updated(id), 1);
        let report = h.paint_incremental(&[node]);
        assert!(!report.damage.full, "块移动是局部 damage，不得退化为整帧");
        assert_eq!(report.rebuilt, 1, "唯一块 lane 重建");

        let rects = h.damage.rects();
        let hits = |x: i32, y: i32| {
            rects.iter().any(|rect| {
                rect.x0.value() <= x
                    && x < rect.x1.value()
                    && rect.y0.value() <= y
                    && y < rect.y1.value()
            })
        };
        assert!(hits(10, 10), "旧位置必须重画（否则留残影）：{rects:?}");
        assert!(hits(100, 60), "新位置必须重画：{rects:?}");
    }

    #[test]
    fn appending_across_the_chunk_boundary_creates_a_new_lane_and_rebuilds_order() {
        let mut h = Harness::new();
        let id = h
            .geometry
            .insert(long_geometry(4096))
            .expect("注册单块几何");
        let node = mount_geometry(&mut h, id, Clip::None);
        h.paint();
        assert_eq!(h.tree.lane_count(node), 1);
        let first = h.tree.lane_at(node, 0).expect("块 0 lane");
        let first_version = h.scene.lane_state(first).expect("lane").0;

        append_triangle(&mut h, id, 100.0, 100.0);
        h.tree.mark_geometry_updated(id);
        let report = h.paint_incremental(&[node]);

        assert_eq!(report.rebuilt, 1);
        assert_eq!(h.tree.lane_count(node), 2, "满块后追加必须新建块 lane");
        let second = h.tree.lane_at(node, 1).expect("新块 lane");
        assert_eq!(h.scene.lane_geometry_chunk(second), Some((id, 1)));
        assert_eq!(h.order, vec![first, second], "绘制序兜底重建后包含新 lane");
        assert_eq!(
            h.scene.lane_state(first).expect("lane").0,
            first_version,
            "旧块 lane 不动"
        );
    }

    #[test]
    fn unchanged_geometry_reuses_every_block_lane() {
        let mut h = Harness::new();
        let id = h
            .geometry
            .insert(long_geometry(4096 + 10))
            .expect("注册两块几何");
        let node = mount_geometry(&mut h, id, Clip::None);
        h.paint();
        let versions: Vec<u32> = (0..2)
            .map(|chunk| {
                let lane = h.tree.lane_at(node, chunk).expect("lane");
                h.scene.lane_state(lane).expect("lane").0
            })
            .collect();

        let report = h.paint_incremental(&[]);
        assert_eq!(report.rebuilt, 0, "没有变化 ⇒ 零重建");
        for (chunk, version) in versions.iter().enumerate() {
            let lane = h.tree.lane_at(node, chunk).expect("lane");
            assert_eq!(h.scene.lane_state(lane).expect("lane").0, *version);
        }
    }

    #[test]
    fn geometry_transform_change_rebuilds_every_block_lane() {
        let mut h = Harness::new();
        let id = h
            .geometry
            .insert(long_geometry(4096 + 10))
            .expect("注册两块几何");
        let node = mount_geometry(&mut h, id, Clip::None);
        h.paint();
        let transform = crate::math::Transform::translate(px(7.0), px(9.0));
        h.tree
            .set_geometry_transform(node, transform, LogicalRect::default())
            .expect("几何节点");
        let report = h.paint_incremental(&[node]);
        assert_eq!(report.rebuilt, 2, "变换烘在块 lane 里 ⇒ 两块都要重建");
        for chunk in 0..2 {
            let lane = h.tree.lane_at(node, chunk).expect("lane");
            let primitives = h.scene.lane_primitives(lane).expect("lane");
            let Some(Primitive::Geometry(draw)) = primitives
                .iter()
                .find(|primitive| matches!(primitive, Primitive::Geometry(_)))
            else {
                panic!("块 {chunk} 必须有几何绘制");
            };
            assert_eq!(draw.transform, transform, "变换必须进每条块 lane");
        }
    }

    #[test]
    fn group_opacity_cascades_to_decorations_and_children() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let mut group_style = fixed_style(100.0, 60.0);
        group_style.clip = Clip::Bounds;
        let group = NodeBuilder::boxed()
            .style(group_style)
            .paint(PaintSpec {
                corner_radius: [px(8.0); 4],
                border: Some(Border {
                    width: px(1.0),
                    color: Color::BLACK,
                }),
                gradient: Some(Gradient {
                    from: Color::WHITE,
                    to: Color::BLACK,
                    kind: GradientKind::Horizontal,
                }),
                shadows: Shadows::from(ShadowSpec {
                    offset: LogicalPoint::new(px(0.0), px(3.0)),
                    blur: px(5.0),
                    spread: px(0.0),
                    color: Color::BLACK,
                }),
                ..PaintSpec::default()
            })
            .build(&mut h.tree, root)
            .expect("group");
        let child = NodeBuilder::boxed()
            .style(fixed_style(40.0, 20.0))
            .paint(with_white())
            .build(&mut h.tree, group)
            .expect("child");
        layout_all(&mut h.tree);

        let _ = h.paint();
        let baseline = lane_primitives(&h, group);
        let shadow_of = |primitives: &[Primitive]| {
            primitives
                .iter()
                .find_map(|primitive| match primitive {
                    Primitive::Shadow(shadow) => Some(*shadow),
                    _ => None,
                })
                .expect("group 必须有一层阴影")
        };
        assert_eq!(shadow_of(&baseline).color, Color::BLACK);

        let _ = h.tree.set_style_patch_for_test(
            group,
            StylePatch {
                opacity: Some(0.5),
                ..StylePatch::default()
            },
        );
        let _ = h.tree.set_style_patch_for_test(
            child,
            StylePatch {
                opacity: Some(0.5),
                ..StylePatch::default()
            },
        );
        h.mark_paint(group);
        let _ = h.paint_incremental(&[group]);

        let primitives = lane_primitives(&h, group);
        assert_eq!(
            shadow_of(&primitives).color,
            Color::BLACK.with_opacity(0.5),
            "阴影色乘父的有效不透明度"
        );
        let gradient = primitives
            .iter()
            .find_map(|primitive| match primitive {
                Primitive::Quad(quad) => match &quad.paint {
                    QuadPaint::Gradient(gradient) => Some(*gradient),
                    QuadPaint::Solid { .. } => None,
                },
                _ => None,
            })
            .expect("渐变 quad");
        assert_eq!(gradient.from, Color::WHITE.with_opacity(0.5));
        assert_eq!(gradient.to, Color::BLACK.with_opacity(0.5));
        let border = primitives
            .iter()
            .find_map(|primitive| match primitive {
                Primitive::Quad(quad) => match &quad.paint {
                    QuadPaint::Solid { border, .. } => *border,
                    QuadPaint::Gradient(_) => None,
                },
                _ => None,
            })
            .expect("边框 quad");
        assert_eq!(border.color, Color::BLACK.with_opacity(0.5));

        let child_primitives = lane_primitives(&h, child);
        assert!(
            child_primitives
                .iter()
                .any(|primitive| matches!(primitive, Primitive::Clip(ClipOp::Push(_)))),
            "`Clip::Bounds` 的裁剪前缀照旧压在子 lane 里（级联不改 clip 链）"
        );
        let fill = child_primitives
            .iter()
            .find_map(|primitive| match primitive {
                Primitive::Quad(quad) => match &quad.paint {
                    QuadPaint::Solid { fill, .. } => Some(*fill),
                    QuadPaint::Gradient(_) => None,
                },
                _ => None,
            })
            .expect("子节点的背景 quad");
        assert_eq!(
            fill,
            Color::WHITE.with_opacity(0.25),
            "子图元 = 父 0.5 × 子 0.5 = 0.25"
        );
    }

    #[test]
    fn group_opacity_cascades_to_text_image_focus_ring_and_geometry_colors() {
        let mut resources = crate::resources::ResourceCache::with_page_for_test((16, 16));
        let image = resources.insert(ONE_PIXEL_PNG).expect("1×1 PNG");
        let mut h = Harness::new();
        h.resources = Some(resources);
        let root = h.tree.root();
        let group = NodeBuilder::boxed()
            .style(fixed_style(100.0, 100.0))
            .build(&mut h.tree, root)
            .expect("group");
        let text = NodeBuilder::text("ab", TextStyle::new(px(16.0)))
            .style(fixed_style(50.0, 20.0))
            .build(&mut h.tree, group)
            .expect("text");
        let content = TextContent::new("ab", TextStyle::new(px(16.0)));
        assert!(h.tree.set_text_content(text, content.clone()));
        assert!(
            h.tree.install_text_line_for_test(
                text,
                &content,
                Rc::new(shaped_line(
                    vec![ShapedTextItem::GlyphRun(glyph_run(Color::BLACK, 0..2, 0.0))],
                    Vec::new(),
                )),
                px(20.0),
            ),
            "文本行注入"
        );
        let image_node = NodeBuilder::image(image.id(), crate::node::image::ImageFit::Fill)
            .style(fixed_style(40.0, 40.0))
            .build(&mut h.tree, group)
            .expect("image");
        let geometry_id = h.geometry.insert(long_geometry(3)).expect("几何");
        let geometry = NodeBuilder::geometry(geometry_id)
            .style(fixed_style(30.0, 30.0))
            .build(&mut h.tree, group)
            .expect("geometry");
        let focus = NodeBuilder::boxed()
            .style(fixed_style(20.0, 20.0))
            .focus_ring(FocusRing::new(px(2.0), px(1.0), Color::BLACK))
            .build(&mut h.tree, group)
            .expect("focus ring");
        layout_all(&mut h.tree);
        h.focused = Some(focus);

        let _ = h.paint();
        let text_color_of = |h: &Harness| {
            lane_primitives(h, text)
                .iter()
                .find_map(|primitive| match primitive {
                    Primitive::GlyphRun(run) => Some(run.color),
                    _ => None,
                })
                .expect("文本 run")
        };
        let modulation_of = |h: &Harness| {
            lane_primitives(h, image_node)
                .iter()
                .find_map(|primitive| match primitive {
                    Primitive::Image(image) => Some(image.modulation),
                    _ => None,
                })
                .expect("图片图元")
        };
        let geometry_color_of = |h: &Harness| {
            lane_primitives(h, geometry)
                .iter()
                .find_map(|primitive| match primitive {
                    Primitive::Geometry(draw) => Some(draw.color),
                    _ => None,
                })
                .expect("几何图元")
        };
        let ring_border_of = |h: &Harness| {
            lane_primitives(h, focus)
                .iter()
                .find_map(|primitive| match primitive {
                    Primitive::Quad(quad) => match &quad.paint {
                        QuadPaint::Solid {
                            border: Some(border),
                            ..
                        } => Some(*border),
                        _ => None,
                    },
                    _ => None,
                })
                .expect("焦点环 quad")
        };
        assert_eq!(text_color_of(&h), Color::BLACK);
        assert_eq!(modulation_of(&h), Color::WHITE);
        assert_eq!(geometry_color_of(&h), Color::WHITE);
        assert_eq!(ring_border_of(&h).color, Color::BLACK);

        let _ = h.tree.set_style_patch_for_test(
            group,
            StylePatch {
                opacity: Some(0.5),
                ..StylePatch::default()
            },
        );
        h.mark_paint(group);
        let report = h.paint_incremental(&[group]);
        assert!(
            report.rebuilt >= 4,
            "文本/图片/几何/焦点环四条 lane 全部重建（实际 {}）",
            report.rebuilt
        );

        assert_eq!(
            text_color_of(&h),
            Color::BLACK.with_opacity(0.5),
            "文本色级联"
        );
        assert_eq!(
            modulation_of(&h),
            Color::WHITE.with_opacity(0.5),
            "图片调制色级联"
        );
        assert_eq!(
            geometry_color_of(&h),
            Color::WHITE.with_opacity(0.5),
            "`GeometryDraw.color` 级联（与 Canvas 不同，几何色由引擎给定）"
        );
        assert_eq!(
            ring_border_of(&h).color,
            Color::BLACK.with_opacity(0.5),
            "焦点环边框色级联"
        );

        let settled = h.paint_incremental(&[]);
        assert_eq!(settled.rebuilt, 0, "有效不透明度不变 ⇒ 零重建");
    }

    #[test]
    fn a_group_opacity_change_rebuilds_only_that_subtree_then_settles() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let group_a = NodeBuilder::boxed()
            .style(fixed_style(50.0, 50.0))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("group a");
        let child_a = NodeBuilder::boxed()
            .style(fixed_style(20.0, 20.0))
            .paint(with_white())
            .build(&mut h.tree, group_a)
            .expect("child a");
        let group_b = NodeBuilder::boxed()
            .style(fixed_style(50.0, 50.0))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("group b");
        let child_b = NodeBuilder::boxed()
            .style(fixed_style(20.0, 20.0))
            .paint(with_white())
            .build(&mut h.tree, group_b)
            .expect("child b");
        layout_all(&mut h.tree);

        let first = h.paint();
        assert!(first.rebuilt >= 4, "四棵都产生像素");
        let sibling_lane = h.tree.lane_of(group_b).expect("group b lane");
        let sibling_version = h.scene.lane_state(sibling_lane).expect("lane").0;
        let settled = h.paint();
        assert_eq!(settled.rebuilt, 0, "无变化 ⇒ 稳定帧零重建（I3）");

        let _ = h.tree.set_style_patch_for_test(
            group_a,
            StylePatch {
                opacity: Some(0.25),
                ..StylePatch::default()
            },
        );
        h.mark_paint(group_a);
        let report = h.paint_incremental(&[group_a]);
        assert_eq!(report.rebuilt, 2, "只重建 group_a + child_a 两条 lane");
        assert_eq!(
            h.scene.lane_state(sibling_lane).expect("lane").0,
            sibling_version,
            "兄弟子树整条复用（不因别人的调光重建）"
        );

        let again = h.paint_incremental(&[]);
        assert_eq!(again.rebuilt, 0, "有效不透明度不变 ⇒ 零重建");

        let sibling_fill = lane_primitives(&h, child_b)
            .iter()
            .find_map(|primitive| match primitive {
                Primitive::Quad(quad) => match &quad.paint {
                    QuadPaint::Solid { fill, .. } => Some(*fill),
                    QuadPaint::Gradient(_) => None,
                },
                _ => None,
            })
            .expect("child b quad");
        assert_eq!(sibling_fill, Color::WHITE, "兄弟子树保持不透明");

        let group_fill = lane_primitives(&h, group_a)
            .iter()
            .find_map(|primitive| match primitive {
                Primitive::Quad(quad) => match &quad.paint {
                    QuadPaint::Solid { fill, .. } => Some(*fill),
                    QuadPaint::Gradient(_) => None,
                },
                _ => None,
            })
            .expect("group a quad");
        assert_eq!(group_fill, Color::WHITE.with_opacity(0.25));
        let child_fill = lane_primitives(&h, child_a)
            .iter()
            .find_map(|primitive| match primitive {
                Primitive::Quad(quad) => match &quad.paint {
                    QuadPaint::Solid { fill, .. } => Some(*fill),
                    QuadPaint::Gradient(_) => None,
                },
                _ => None,
            })
            .expect("child a quad");
        assert_eq!(
            child_fill,
            Color::WHITE.with_opacity(0.25),
            "子节点继承 0.25（父调光在增量路径上同样级联）"
        );

        let _ = h
            .tree
            .set_style_patch_for_test(group_a, StylePatch::default());
        h.mark_paint(group_a);
        let restored = h.paint_incremental(&[group_a]);
        assert_eq!(
            restored.rebuilt, 2,
            "恢复不透明度同样必须重建 group_a + child_a"
        );
        let restored_group_fill = lane_primitives(&h, group_a)
            .iter()
            .find_map(|primitive| match primitive {
                Primitive::Quad(quad) => match &quad.paint {
                    QuadPaint::Solid { fill, .. } => Some(*fill),
                    QuadPaint::Gradient(_) => None,
                },
                _ => None,
            })
            .expect("恢复后的 group a quad");
        assert_eq!(restored_group_fill, Color::WHITE, "组根恢复到 1.0");
        let restored_child_fill = lane_primitives(&h, child_a)
            .iter()
            .find_map(|primitive| match primitive {
                Primitive::Quad(quad) => match &quad.paint {
                    QuadPaint::Solid { fill, .. } => Some(*fill),
                    QuadPaint::Gradient(_) => None,
                },
                _ => None,
            })
            .expect("恢复后的 child a quad");
        assert_eq!(restored_child_fill, Color::WHITE, "子节点恢复到 1.0");
        let settled_after_restore = h.paint_incremental(&[]);
        assert_eq!(
            settled_after_restore.rebuilt, 0,
            "恢复后的稳定帧零重建（I3）"
        );
    }

    #[test]
    fn a_group_fade_to_zero_damages_the_old_shadow_spill_and_the_overflowing_child() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let group = NodeBuilder::boxed()
            .style(fixed_style(100.0, 60.0))
            .paint(PaintSpec {
                background: Color::WHITE,
                shadows: Shadows::from(ShadowSpec {
                    offset: LogicalPoint::new(px(0.0), px(25.0)),
                    blur: px(20.0),
                    spread: px(0.0),
                    color: Color::BLACK,
                }),
                ..PaintSpec::default()
            })
            .build(&mut h.tree, root)
            .expect("group");
        let child = NodeBuilder::boxed()
            .style(fixed_style(42.0, 20.0))
            .paint(with_white())
            .build(&mut h.tree, group)
            .expect("child");
        set_layout(
            &mut h.tree,
            group,
            LogicalRect::new(px(0.0), px(0.0), px(100.0), px(60.0)),
        );
        set_layout(
            &mut h.tree,
            child,
            LogicalRect::new(px(149.0), px(0.0), px(42.0), px(20.0)),
        );
        let _ = h.paint();

        let first = h.damage.rects().to_vec();
        let shadow_spill = DeviceRect::new(dp(0), dp(61), dp(100), dp(116));
        let overflow = DeviceRect::new(dp(149), dp(0), dp(191), dp(20));
        assert!(
            damage_covers(&first, shadow_spill),
            "首帧旧阴影必须被画到：{first:?}"
        );

        let _ = h.tree.set_style_patch_for_test(
            group,
            StylePatch {
                opacity: Some(0.0),
                ..StylePatch::default()
            },
        );
        h.mark_paint(group);
        let report = h.paint_incremental(&[group]);
        assert!(!report.damage.full, "局部残影修复不得退化为整帧");
        let damage = h.damage.rects().to_vec();
        assert!(
            damage_covers(&damage, shadow_spill),
            "组淡出到 0 后旧阴影外溢区必须进 damage（否则屏幕残影）：{damage:?}"
        );
        assert!(
            damage_covers(&damage, overflow),
            "父盒外的子节点背景必须进 damage：{damage:?}"
        );
        assert!(h.tree.lane_of(group).is_none(), "组 lane 应被释放");
        assert!(h.tree.lane_of(child).is_none(), "子 lane 应被释放");
    }

    #[test]
    fn a_geometry_node_under_a_group_fade_stays_reusable_when_only_the_group_repaints() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let group = NodeBuilder::boxed()
            .style(fixed_style(100.0, 60.0))
            .paint(with_white())
            .build(&mut h.tree, root)
            .expect("group");
        let geometry_id = h.geometry.insert(long_geometry(3)).expect("几何");
        let geometry = NodeBuilder::geometry(geometry_id)
            .style(fixed_style(30.0, 30.0))
            .build(&mut h.tree, group)
            .expect("geometry");
        layout_all(&mut h.tree);
        let _ = h.tree.set_style_patch_for_test(
            group,
            StylePatch {
                opacity: Some(0.5),
                ..StylePatch::default()
            },
        );
        let _ = h.paint();
        let geometry_color = |h: &Harness| {
            lane_primitives(h, geometry)
                .iter()
                .find_map(|primitive| match primitive {
                    Primitive::Geometry(draw) => Some(draw.color),
                    _ => None,
                })
                .expect("几何图元")
        };
        assert_eq!(geometry_color(&h), Color::WHITE.with_opacity(0.5));
        let lane = h.tree.lane_at(geometry, 0).expect("块 lane");
        let version = h.scene.lane_state(lane).expect("lane").0;

        h.mark_paint(group);
        let report = h.paint_incremental(&[group]);
        assert_eq!(
            report.rebuilt, 1,
            "只有组根 lane 重建；几何块 lane 必须整块复用"
        );
        assert_eq!(
            h.scene.lane_state(lane).expect("lane").0,
            version,
            "几何块 lane 版本不变（I3；没有写回戳时这里会被重建）"
        );
        assert_eq!(geometry_color(&h), Color::WHITE.with_opacity(0.5));
    }

    #[test]
    fn shadow_spread_expands_the_projection_box_and_the_corner_radii() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let node = NodeBuilder::boxed()
            .style(fixed_style(50.0, 20.0))
            .paint(PaintSpec {
                corner_radius: [px(8.0); 4],
                shadows: Shadows::from(ShadowSpec {
                    offset: LogicalPoint::new(px(1.0), px(2.0)),
                    blur: px(6.0),
                    spread: px(3.0),
                    color: Color::BLACK,
                }),
                ..PaintSpec::default()
            })
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();
        let bounds = h.tree.get(node).expect("node").layout().bounds;
        let shadow = lane_primitives(&h, node)
            .iter()
            .find_map(|primitive| match primitive {
                Primitive::Shadow(shadow) => Some(*shadow),
                _ => None,
            })
            .expect("阴影");
        assert_eq!(
            shadow.rect,
            bounds.outset(px(3.0)),
            "投影盒 = 布局盒四边外扩 spread"
        );
        assert_eq!(
            shadow.corner_radius,
            [px(11.0); 4],
            "圆角 = 原半径 + spread"
        );
        assert_eq!(shadow.blur, px(6.0), "σ 不随 spread 变");

        let negative = NodeBuilder::boxed()
            .style(fixed_style(50.0, 20.0))
            .paint(PaintSpec {
                corner_radius: [px(8.0); 4],
                shadows: Shadows::from(ShadowSpec {
                    offset: LogicalPoint::ZERO,
                    blur: px(2.0),
                    spread: px(-5.0),
                    color: Color::BLACK,
                }),
                ..PaintSpec::default()
            })
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();
        let bounds = h.tree.get(negative).expect("node").layout().bounds;
        let shadow = lane_primitives(&h, negative)
            .iter()
            .find_map(|primitive| match primitive {
                Primitive::Shadow(shadow) => Some(*shadow),
                _ => None,
            })
            .expect("阴影");
        assert_eq!(shadow.rect, bounds.outset(px(-5.0)), "负 spread 收缩投影盒");
        assert_eq!(shadow.corner_radius, [px(3.0); 4], "半径随负 spread 减小");
    }

    #[test]
    fn shadow_damage_pad_is_computed_in_physical_space() {
        use crate::paint::instance::{SHADOW_PAD_SIGMAS, SHADOW_SIGMA_MIN, SHADOW_SIGMA_SCALE};

        for (factor, blur) in [(1.5f32, 0.2f32), (2.0, 0.2), (1.5, 0.0), (2.0, 0.0)] {
            let mut h = Harness::with_scale(factor);
            let root = h.tree.root();
            let node = NodeBuilder::boxed()
                .style(fixed_style(100.0, 20.0))
                .paint(PaintSpec {
                    background: Color::WHITE,
                    shadows: Shadows::from(ShadowSpec {
                        offset: LogicalPoint::ZERO,
                        blur: px(blur),
                        spread: px(0.0),
                        color: Color::BLACK,
                    }),
                    ..PaintSpec::default()
                })
                .build(&mut h.tree, root)
                .expect("挂载");
            layout_all(&mut h.tree);
            let _ = h.paint();

            let scale = DevicePixelRatio::new(factor).expect("scale");
            let physical_blur = blur * factor;
            let sigma = (physical_blur * SHADOW_SIGMA_SCALE).max(SHADOW_SIGMA_MIN);
            let pad_physical = SHADOW_PAD_SIGMAS * sigma + 1.0;

            let painted = super::painted_content(&h.tree, &h.scene, node, scale);
            let (shadow_rect, painted_pad) =
                painted.shadow.expect("painted_content 必须记录阴影外溢");
            assert_eq!(
                painted_pad,
                physical_px(pad_physical),
                "factor {factor} blur {blur}：shadow pad 必须是纯物理像素"
            );
            let legacy_sigma = (blur * SHADOW_SIGMA_SCALE).max(SHADOW_SIGMA_MIN);
            let legacy_pad = SHADOW_PAD_SIGMAS * legacy_sigma * factor + 1.0;
            assert_ne!(
                pad_physical, legacy_pad,
                "factor {factor} blur {blur}：新旧公式必须给出不同 pad（否则测试无法区分）"
            );

            let physical = scale.to_covering_device_rect(shadow_rect);
            let pad = pad_physical.ceil() as i32;
            let expected = DeviceRect::new(
                physical.x0 - pad,
                physical.y0 - pad,
                physical.x1 + pad,
                physical.y1 + pad,
            );
            assert_eq!(
                h.damage.rects(),
                &[expected],
                "factor {factor} blur {blur}：damage 必须覆盖 emit_shadow 的 pad 扩张范围"
            );
        }
    }

    #[test]
    fn shadow_layers_are_emitted_in_order_and_capped() {
        let mut h = Harness::new();
        let root = h.tree.root();
        let layers: Vec<ShadowSpec> = (0..5)
            .map(|index| ShadowSpec {
                offset: LogicalPoint::new(px(0.0), px(index as f32)),
                blur: px(2.0),
                spread: px(0.0),
                color: Color::BLACK,
            })
            .collect();
        let node = NodeBuilder::boxed()
            .style(fixed_style(50.0, 20.0))
            .paint(PaintSpec {
                background: Color::WHITE,
                shadows: Shadows::new(layers),
                ..PaintSpec::default()
            })
            .build(&mut h.tree, root)
            .expect("挂载");
        layout_all(&mut h.tree);
        let _ = h.paint();
        let primitives = lane_primitives(&h, node);
        let shadows: Vec<crate::paint::primitive::Shadow> = primitives
            .iter()
            .filter_map(|primitive| match primitive {
                Primitive::Shadow(shadow) => Some(*shadow),
                _ => None,
            })
            .collect();
        assert_eq!(shadows.len(), 4, "上限 4 层（第 5 层被构造期截断）");
        assert_eq!(
            shadows
                .iter()
                .map(|shadow| shadow.offset.y)
                .collect::<Vec<_>>(),
            vec![px(0.0), px(1.0), px(2.0), px(3.0)],
            "绘制序 = 数组序（后画的在上；截断丢弃尾部）"
        );
        let last_shadow = primitives
            .iter()
            .rposition(|primitive| matches!(primitive, Primitive::Shadow(_)))
            .expect("有阴影");
        let background = primitives
            .iter()
            .position(|primitive| matches!(primitive, Primitive::Quad(_)))
            .expect("背景 quad");
        assert!(last_shadow < background, "全部阴影层都在背景之前绘制");
    }

    #[cfg(feature = "test-support")]
    mod group_fade {
        use std::time::Duration;

        use crate::{
            interaction::{AnimatedValue, AnimationSpec, Easing},
            math::{DeviceRect, LogicalPoint, px},
            node::{builder::NodeBuilder, dependency::ReadCtx},
            paint::{
                color::Color,
                style::{PaintSpec, ShadowSpec, Shadows, Style, StylePatch},
                style_builder::StyleBuilder,
            },
            test_support::app::TestApp,
        };

        fn sized(width: f32, height: f32) -> Style {
            StyleBuilder::new().size(px(width), px(height)).build()
        }

        fn white() -> PaintSpec {
            PaintSpec {
                background: Color::WHITE,
                ..PaintSpec::default()
            }
        }

        fn floating_shell() -> PaintSpec {
            PaintSpec {
                background: Color::WHITE,
                shadows: Shadows::from(ShadowSpec {
                    offset: LogicalPoint::new(px(0.0), px(25.0)),
                    blur: px(20.0),
                    spread: px(0.0),
                    color: Color::BLACK,
                }),
                ..PaintSpec::default()
            }
        }

        fn damage_has_point(damage: &[DeviceRect], x: i32, y: i32) -> bool {
            damage.iter().any(|rect| {
                rect.x0.value() <= x
                    && x < rect.x1.value()
                    && rect.y0.value() <= y
                    && y < rect.y1.value()
            })
        }

        fn alpha_of(
            record: &crate::test_support::record::FrameRecord,
            node: crate::node::id::NodeId,
        ) -> Option<f32> {
            let frame = record.frame.as_ref()?;
            frame
                .quads
                .iter()
                .find(|quad| quad.node == node)
                .map(|quad| quad.fill.to_array()[3])
        }

        #[test]
        fn a_group_fade_dims_the_whole_subtree_while_the_sibling_stays_opaque() {
            let mut app = TestApp::new();
            let window = app.window_id();
            let fade = app.app_mut().transient(1.0_f32);
            let root = app.app().window(window).expect("窗口").tree().root();
            let (group, child, sibling) = {
                let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
                let binder = fade;
                let group = NodeBuilder::boxed()
                    .style(sized(100.0, 100.0))
                    .paint(floating_shell())
                    .bind_style(move |cx: &mut ReadCtx<'_>| StylePatch {
                        opacity: Some(*cx.read_transient(&binder)),
                        ..StylePatch::default()
                    })
                    .id("group")
                    .build(tree, root)
                    .expect("group");
                let child = NodeBuilder::boxed()
                    .style(sized(40.0, 40.0))
                    .paint(white())
                    .id("child")
                    .build(tree, group)
                    .expect("child");
                let sibling = NodeBuilder::boxed()
                    .style(sized(60.0, 60.0))
                    .paint(white())
                    .id("sibling")
                    .build(tree, root)
                    .expect("sibling");
                (group, child, sibling)
            };

            let first = app.drain().expect("首帧").clone();
            assert_eq!(alpha_of(&first, group), Some(1.0), "初始不透明");
            assert_eq!(alpha_of(&first, child), Some(1.0));
            assert_eq!(alpha_of(&first, sibling), Some(1.0));

            app.app_mut()
                .animate(
                    window,
                    AnimatedValue::Scalar {
                        target: fade,
                        to: 0.0,
                    },
                    &[group],
                    AnimationSpec {
                        duration: Duration::from_millis(100),
                        easing: Easing::Linear,
                    },
                )
                .expect("动画注册");
            app.advance(Duration::from_millis(50));
            let middle = app.drain().expect("淡出中帧").clone();
            let group_alpha = alpha_of(&middle, group).expect("组 quad 仍在");
            let child_alpha = alpha_of(&middle, child).expect("子 quad 仍在");
            assert!(
                (group_alpha - 0.5).abs() < 0.01,
                "50% 进度 ⇒ 组根 quad 半透明（实测 {group_alpha}）"
            );
            assert!(
                (child_alpha - 0.5).abs() < 0.01,
                "子节点继承组的不透明度（实测 {child_alpha}）"
            );
            assert_eq!(alpha_of(&middle, sibling), Some(1.0), "兄弟组不受影响");

            app.advance(Duration::from_millis(60));
            let done = app.drain().expect("淡出结束帧").clone();
            assert_eq!(alpha_of(&done, group), None, "淡到 0 后组不再产生图元");
            assert_eq!(alpha_of(&done, child), None, "子树同上");
            assert_eq!(alpha_of(&done, sibling), Some(1.0), "兄弟组仍然不透明");

            let group_bounds = app
                .app()
                .window(window)
                .expect("窗口")
                .tree()
                .get(group)
                .expect("group")
                .layout()
                .bounds;
            let spill = LogicalPoint::new(
                group_bounds.origin.x + group_bounds.size.width * 0.5,
                group_bounds.origin.y + group_bounds.size.height + 10.0,
            );
            let (spill_x, spill_y) = (
                spill.x.value().floor() as i32,
                spill.y.value().floor() as i32,
            );
            assert!(
                !done.damage_full,
                "淡出帧不得退化为整帧（否则下面的覆盖断言没有意义）"
            );
            assert!(
                damage_has_point(&done.damage, spill_x, spill_y),
                "淡出到 0 后旧阴影外溢点 ({spill_x},{spill_y}) 必须被 damage 覆盖（否则屏幕残影）：{:?}",
                done.damage
            );
        }
    }
}

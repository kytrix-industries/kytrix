use std::{rc::Rc, sync::Arc};

use smallvec::SmallVec;
use smol_str::SmolStr;

use super::{BakedGeometryChunk, NodeKind, NodeTree};
#[cfg(test)]
use crate::paint::style::Style;
use crate::{
    error::EngineError,
    interaction::{NodeInteraction, OverlaySlot, ScrollSlot, VirtualSlot},
    math::{DeviceRect, LogicalPoint, LogicalPx, LogicalRect, LogicalSize, Transform, px},
    node::{dependency::ReadCtx, dirty::DirtyFlags, id::NodeId, signal::SignalId},
    paint::{geometry::GeometryId, scene::LaneId, style::StylePatch},
    text::{ShapedLine, TextContent, shape::ShapedText},
};

pub(crate) fn text_content(tree: &NodeTree, node: NodeId) -> Option<&TextContent> {
    tree.arena.get(node).and_then(|target| target.kind.text())
}

pub(crate) fn editable(tree: &NodeTree, node: NodeId) -> Option<&crate::text_edit::EditableText> {
    tree.get(node).ok()?;
    tree.editable_slots
        .get(node.index() as usize)
        .and_then(|slot| slot.as_deref())
}

pub(crate) fn editable_mut(
    tree: &mut NodeTree,
    node: NodeId,
) -> Option<&mut crate::text_edit::EditableText> {
    tree.get(node).ok()?;
    tree.editable_slots
        .get_mut(node.index() as usize)
        .and_then(|slot| slot.as_deref_mut())
}

pub(crate) fn set_canvas(
    tree: &mut NodeTree,
    node: NodeId,
    canvas: crate::paint::canvas::Canvas,
) -> bool {
    let Some(target) = tree.arena.get_mut(node) else {
        return false;
    };
    let NodeKind::Canvas(existing) = &mut target.kind else {
        return false;
    };
    let fresh = Arc::new(canvas);
    let previous = std::mem::replace(existing, Arc::clone(&fresh));
    target.canvas_generation = target.canvas_generation.saturating_add(1);
    for geometry in canvas_geometry_ids(&previous) {
        unbind_geometry_node(tree, geometry, node);
    }
    for geometry in canvas_geometry_ids(&fresh) {
        bind_geometry_node(tree, geometry, node);
    }
    tree.mark_subtree_paint(node);
    true
}

pub(crate) fn canvas(tree: &NodeTree, node: NodeId) -> Option<&Arc<crate::paint::canvas::Canvas>> {
    tree.arena.get(node).and_then(|target| target.canvas())
}

pub(crate) fn canvas_geometry_ids(
    canvas: &crate::paint::canvas::Canvas,
) -> SmallVec<[GeometryId; 2]> {
    let mut ids: SmallVec<[GeometryId; 2]> = SmallVec::new();
    for primitive in canvas.primitives() {
        let id = match primitive {
            crate::paint::primitive::Primitive::Geometry(draw) => draw.id,
            crate::paint::primitive::Primitive::Path(path) => path.geometry,
            _ => continue,
        };
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

pub(crate) fn set_text_content(tree: &mut NodeTree, node: NodeId, content: TextContent) -> bool {
    match tree.arena.get_mut(node) {
        Some(target) => match &mut target.kind {
            NodeKind::Text(existing) => {
                **existing = content;
                tree.mark_text_dirty(node);
                true
            }
            _ => false,
        },
        None => false,
    }
}

pub(crate) fn mark_text_dirty(tree: &mut NodeTree, node: NodeId) {
    tree.text_dirty.push(node);
}

pub(crate) fn take_text_dirty(tree: &mut NodeTree, out: &mut Vec<NodeId>) {
    tree.text_dirty.take_into(out);
}

pub(crate) fn text_dirty_pending(tree: &NodeTree, node: NodeId) -> bool {
    tree.text_dirty.contains(node)
}

#[cfg(test)]
pub(crate) fn text_dirty_len(tree: &NodeTree) -> usize {
    tree.text_dirty.len()
}

pub(crate) fn mark_text_layout_dirty(tree: &mut NodeTree, node: NodeId) {
    tree.text_layout_dirty.push(node);
}

pub(crate) fn take_text_layout_dirty(tree: &mut NodeTree, out: &mut Vec<NodeId>) {
    tree.text_layout_dirty.take_into(out);
}

#[cfg(test)]
pub(crate) fn text_layout_dirty_len(tree: &NodeTree) -> usize {
    tree.text_layout_dirty.len()
}

pub(crate) fn mark_canvas_nodes_paint(tree: &mut NodeTree) -> usize {
    let nodes: Vec<NodeId> = tree.canvas_nodes().to_vec();
    let marked = nodes.len();
    for node in nodes {
        tree.mark_paint(node);
    }
    marked
}

pub(crate) fn invalidate_text_lines(tree: &mut NodeTree) -> usize {
    let mut nodes: Vec<NodeId> = Vec::new();
    tree.for_each_dfs(|node| {
        if tree
            .arena
            .get(node)
            .is_some_and(|target| target.kind.text().is_some())
        {
            nodes.push(node);
        }
    });
    let mut invalidated = 0;
    for node in nodes {
        let index = node.index() as usize;
        let Some(slot) = tree.text_slots.get_mut(index).and_then(Option::as_mut) else {
            continue;
        };
        if slot.line.take().is_some() {
            invalidated += 1;
        }
        slot.laid_width_bits = 0;
        slot.laid_generation = 0;
        if let Some(editable) = tree.editable_slots.get_mut(index).and_then(Option::as_mut) {
            editable.clear_placeholder_line();
        }
        tree.mark_text_layout_dirty(node);
        tree.mark_paint(node);
    }
    invalidated
}

pub(crate) fn set_text_shaped(
    tree: &mut NodeTree,
    node: NodeId,
    shaped: Rc<crate::text::shape::ShapedText>,
    key: crate::text::cache::ShapeKey,
    dpi_epoch: u32,
    measured: LogicalSize,
    size_changed: bool,
    editable: bool,
) -> bool {
    let Some(target) = tree.arena.get_mut(node) else {
        return false;
    };
    let rc = Rc::clone(&shaped);
    target.measure = Some(if editable {
        Box::new(move |known, available| rc.measure_editable(known, available))
    } else {
        Box::new(move |known, available| rc.measure(known, available))
    });
    target.text_measured_size = measured;

    let index = node.index() as usize;
    if tree.text_slots.len() <= index {
        tree.text_slots.resize_with(index + 1, || None);
    }
    match tree.text_slots.get_mut(index).and_then(Option::as_mut) {
        Some(slot) => {
            slot.shaped = shaped;
            slot.key = key;
            slot.dpi_epoch = dpi_epoch;
            slot.line = None;
            slot.laid_width_bits = 0;
            slot.laid_generation = 0;
            slot.stale_request = None;
        }
        None => {
            if let Some(slot) = tree.text_slots.get_mut(index) {
                *slot = Some(TextSlot {
                    shaped,
                    key,
                    dpi_epoch,
                    line: None,
                    laid_width_bits: 0,
                    laid_generation: 0,
                    stale_request: None,
                });
            }
        }
    }
    tree.mark_text_layout_dirty(node);

    tree.invalidate_caches_upwards(node);
    if size_changed {
        tree.mark_layout(node)
    } else {
        tree.mark_paint(node)
    }
}

#[cfg(test)]
pub(crate) fn install_text_line_for_test(
    tree: &mut NodeTree,
    node: NodeId,
    content: &TextContent,
    line: Rc<ShapedLine>,
    width: LogicalPx,
) -> bool {
    let shaped = Rc::new(crate::text::shape::ShapedText::blank_for_text_test(
        content.text.clone(),
        content.style.clone(),
    ));
    let key = crate::text::cache::ShapeKey::new(content, 1);
    let _ = set_text_shaped(tree, node, shaped, key, 1, LogicalSize::ZERO, false, false);
    set_text_line(tree, node, line, width, 0)
}

pub(crate) fn set_text_line(
    tree: &mut NodeTree,
    node: NodeId,
    line: Rc<ShapedLine>,
    width: LogicalPx,
    generation: u32,
) -> bool {
    let index = node.index() as usize;
    let Some(slot) = tree.text_slots.get_mut(index).and_then(Option::as_mut) else {
        return false;
    };
    slot.line = Some(line);
    slot.laid_width_bits = width.value().to_bits();
    slot.laid_generation = generation;
    tree.mark_paint(node);
    true
}

pub(crate) fn text_content_width(tree: &NodeTree, node: NodeId) -> LogicalPx {
    tree.arena
        .get(node)
        .map_or(LogicalPx::ZERO, |target| target.text_content_width)
}

pub(crate) fn text_line(tree: &NodeTree, node: NodeId) -> Option<&Rc<ShapedLine>> {
    tree.text_slots
        .get(node.index() as usize)
        .and_then(Option::as_ref)
        .and_then(TextSlot::line)
}

pub(crate) fn text_measured_size(tree: &NodeTree, node: NodeId) -> Option<LogicalSize> {
    tree.arena.get(node).map(|target| target.text_measured_size)
}

pub(crate) fn text_slot(tree: &NodeTree, node: NodeId) -> Option<&TextSlot> {
    tree.text_slots
        .get(node.index() as usize)
        .and_then(Option::as_ref)
}

pub(crate) fn text_selection_rects(
    tree: &NodeTree,
    node: NodeId,
    anchor: usize,
    focus: usize,
) -> Result<Vec<LogicalRect>, EngineError> {
    let Some(target) = tree.arena.get(node) else {
        return Err(EngineError::StaleNode { node });
    };
    if target.kind.text().is_none() {
        return Err(EngineError::TextShape {
            node,
            detail: SmolStr::new_static("node is not a text node"),
        });
    }
    let Some(slot) = tree.text_slot(node) else {
        return Err(EngineError::TextShape {
            node,
            detail: SmolStr::new_static("text has not been shaped yet"),
        });
    };
    let shaped = slot.shaped();
    let Some(content) = tree.text_content(node) else {
        return Err(EngineError::StaleNode { node });
    };
    let width = expected_layout_width(tree, shaped, node);
    Ok(shaped.selection_rects(width, content.style.alignment, anchor, focus))
}

pub(crate) fn text_caret_rect(
    tree: &NodeTree,
    node: NodeId,
    byte_index: usize,
) -> Result<LogicalRect, EngineError> {
    let Some(target) = tree.arena.get(node) else {
        return Err(EngineError::StaleNode { node });
    };
    if target.kind.text().is_none() {
        return Err(EngineError::TextShape {
            node,
            detail: SmolStr::new_static("node is not a text node"),
        });
    }
    let Some(slot) = tree.text_slot(node) else {
        return Err(EngineError::TextShape {
            node,
            detail: SmolStr::new_static("text has not been shaped yet"),
        });
    };
    let shaped = slot.shaped();
    let Some(content) = tree.text_content(node) else {
        return Err(EngineError::StaleNode { node });
    };
    let text = content.text.as_str();
    if shaped.text() != text
        || !shaped.same_shaping_inputs(&content.style, content.runs())
        || shaped.inline_boxes() != content.inline_boxes.as_ref()
    {
        return Err(EngineError::TextShape {
            node,
            detail: SmolStr::new_static(
                "text content/style changed since the last shape; wait for the text pass",
            ),
        });
    }
    if byte_index > text.len() {
        return Err(EngineError::TextShape {
            node,
            detail: SmolStr::new_static("byte index is past the end of the text"),
        });
    }
    if !text.is_char_boundary(byte_index) {
        return Err(EngineError::TextShape {
            node,
            detail: SmolStr::new_static("byte index is not a char boundary"),
        });
    }
    let expected_width = expected_layout_width(tree, shaped, node);
    if slot.laid_width_bits() != expected_width.value().to_bits() {
        return Err(EngineError::TextShape {
            node,
            detail: SmolStr::new_static(
                "text layout width changed since the last text pass; wait for the text pass",
            ),
        });
    }

    let mut rect = shaped.caret_rect(expected_width, content.style.alignment, byte_index);
    if rect.size.width <= LogicalPx::ZERO {
        rect.size.width = px(1.0);
    }
    if rect.size.height <= LogicalPx::ZERO {
        let style_size = content.style.size.max(px(1.0));
        let line_height = content.style.line_height.unwrap_or(1.0).max(0.0);
        rect.size.height = (style_size * line_height).max(px(1.0));
    }
    Ok(rect)
}

pub(crate) fn expected_layout_width(
    tree: &NodeTree,
    shaped: &ShapedText,
    node: NodeId,
) -> LogicalPx {
    let width = tree.text_content_width(node);
    if width > LogicalPx::ZERO {
        width
    } else {
        shaped.max_content_size().width
    }
}

pub(crate) fn laid_width(tree: &NodeTree, node: NodeId) -> Option<LogicalPx> {
    let slot = tree.text_slot(node)?;
    let width = expected_layout_width(tree, slot.shaped(), node);
    (slot.laid_width_bits() == width.value().to_bits()).then_some(width)
}

pub(crate) fn text_byte_index_at(
    tree: &NodeTree,
    node: NodeId,
    local_point: LogicalPoint,
) -> Result<usize, EngineError> {
    let Some(target) = tree.arena.get(node) else {
        return Err(EngineError::StaleNode { node });
    };
    if target.kind.text().is_none() {
        return Err(EngineError::StaleNode { node });
    }
    let Some(slot) = tree.text_slot(node) else {
        return Err(EngineError::StaleNode { node });
    };
    let Some(content) = tree.text_content(node) else {
        return Err(EngineError::StaleNode { node });
    };
    let shaped = slot.shaped();
    if tree.text_line(node).is_none()
        || shaped.text() != content.text.as_str()
        || !shaped.same_shaping_inputs(&content.style, content.runs())
        || shaped.inline_boxes() != content.inline_boxes.as_ref()
    {
        return Err(EngineError::StaleNode { node });
    }
    let width = expected_layout_width(tree, shaped, node);
    if slot.laid_width_bits() != width.value().to_bits() {
        return Err(EngineError::StaleNode { node });
    }
    Ok(shaped.byte_index_at(width, content.style.alignment, local_point))
}

pub(crate) fn invalidate_text_line(tree: &mut NodeTree, node: NodeId) {
    let index = node.index() as usize;
    let Some(slot) = tree.text_slots.get_mut(index).and_then(Option::as_mut) else {
        return;
    };
    slot.line = None;
    slot.laid_width_bits = 0;
    slot.laid_generation = 0;
    if let Some(editable) = tree.editable_slots.get_mut(index).and_then(Option::as_mut) {
        editable.clear_placeholder_line();
    }
    tree.mark_text_layout_dirty(node);
    tree.mark_paint(node);
}

pub(crate) fn request_text_line_relayout(
    tree: &mut NodeTree,
    node: NodeId,
    atlas_generations: (u32, u32),
) -> bool {
    let index = node.index() as usize;
    let Some(slot) = tree.text_slots.get_mut(index).and_then(Option::as_mut) else {
        return false;
    };
    if slot.stale_request == Some(atlas_generations) {
        return false;
    }
    slot.stale_request = Some(atlas_generations);
    true
}

pub(crate) fn invalidate_text_layout(tree: &mut NodeTree, node: NodeId) -> bool {
    if tree.invalidate_measure(node).is_err() {
        return false;
    }
    tree.mark_paint(node)
}

pub(crate) fn mark_all_text_dirty(tree: &mut NodeTree) {
    let mut nodes: Vec<NodeId> = Vec::new();
    tree.for_each_dfs(|node| {
        if tree
            .arena
            .get(node)
            .is_some_and(|target| target.kind.text().is_some())
        {
            nodes.push(node);
        }
    });
    for node in nodes {
        tree.mark_text_dirty(node);
        tree.mark_text_layout_dirty(node);
    }
}

pub(crate) fn take_bind_text(
    tree: &mut NodeTree,
    node: NodeId,
) -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>> {
    tree.arena.get_mut(node)?.text_bind.take()
}

pub(crate) fn put_bind_text(
    tree: &mut NodeTree,
    node: NodeId,
    bind_text: Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>,
) -> bool {
    match tree.arena.get_mut(node) {
        Some(target) => {
            target.text_bind = Some(bind_text);
            true
        }
        None => false,
    }
}

pub(crate) fn take_geometry_damage_escalation(tree: &mut NodeTree) -> bool {
    std::mem::take(&mut tree.geometry_damage_escalation)
}

pub(crate) fn geometry_transform(tree: &NodeTree, node: NodeId) -> Transform {
    tree.geometry_transforms
        .get(&node)
        .copied()
        .unwrap_or(Transform::IDENTITY)
}

pub(crate) fn has_geometry_transforms(tree: &NodeTree) -> bool {
    !tree.geometry_transforms.is_empty()
}

pub(crate) fn set_geometry_transform(
    tree: &mut NodeTree,
    node: NodeId,
    transform: Transform,
    extra_damage: LogicalRect,
) -> Result<(), EngineError> {
    match tree.arena.get(node).map(|target| target.kind()) {
        Some(NodeKind::Geometry(_)) => {}
        _ => return Err(EngineError::StaleNode { node }),
    }
    if transform == Transform::IDENTITY {
        tree.geometry_transforms.remove(&node);
    } else {
        tree.geometry_transforms.insert(node, transform);
    }
    tree.mark_paint(node);
    if let Some(target) = tree.arena.get_mut(node) {
        target.damage = target.damage.union(extra_damage);
    }
    Ok(())
}

pub(crate) fn content_offset(tree: &NodeTree, node: NodeId) -> LogicalPoint {
    tree.arena
        .get(node)
        .map_or(LogicalPoint::ZERO, |target| target.content_offset)
}

pub(crate) fn lane_of(tree: &NodeTree, node: NodeId) -> Option<LaneId> {
    tree.arena
        .get(node)
        .and_then(|target| target.lanes.first().copied())
}

pub(crate) fn lane_count(tree: &NodeTree, node: NodeId) -> usize {
    tree.arena.get(node).map_or(0, |target| target.lanes.len())
}

pub(crate) fn lane_at(tree: &NodeTree, node: NodeId, index: usize) -> Option<LaneId> {
    tree.arena
        .get(node)
        .and_then(|target| target.lanes.get(index).copied())
}

pub(crate) fn set_lane(tree: &mut NodeTree, node: NodeId, lane: LaneId) {
    if let Some(target) = tree.arena.get_mut(node) {
        target.lanes.clear();
        target.lanes.push(lane);
    }
}

pub(crate) fn push_lane(tree: &mut NodeTree, node: NodeId, lane: LaneId) {
    if let Some(target) = tree.arena.get_mut(node) {
        target.lanes.push(lane);
    }
}

pub(crate) fn truncate_lanes(tree: &mut NodeTree, node: NodeId, len: usize) {
    if let Some(target) = tree.arena.get_mut(node) {
        target.lanes.truncate(len);
    }
}

pub(crate) fn baked_geometry_chunks(
    tree: &NodeTree,
    node: NodeId,
) -> Option<&SmallVec<[BakedGeometryChunk; 2]>> {
    tree.geometry_chunks.get(&node)
}

pub(crate) fn resize_baked_geometry_chunks(tree: &mut NodeTree, node: NodeId, len: usize) {
    if len == 0 {
        tree.geometry_chunks.remove(&node);
        return;
    }
    let entry = tree
        .geometry_chunks
        .entry(node)
        .or_insert_with(|| SmallVec::with_capacity(len));
    entry.resize(
        len,
        BakedGeometryChunk {
            revision: 0,
            bounds: LogicalRect::default(),
        },
    );
}

pub(crate) fn set_baked_geometry_chunk(
    tree: &mut NodeTree,
    node: NodeId,
    chunk: usize,
    baked: BakedGeometryChunk,
) {
    if let Some(entry) = tree.geometry_chunks.get_mut(&node)
        && let Some(slot) = entry.get_mut(chunk)
    {
        *slot = baked;
    }
}

pub(crate) fn mark_geometry_updated(tree: &mut NodeTree, geometry: GeometryId) -> usize {
    let Some(nodes) = tree.geometry_bindings.get(&geometry) else {
        return 0;
    };
    let nodes: SmallVec<[NodeId; 2]> = nodes.iter().copied().collect();
    let mut touched = 0;
    for node in nodes {
        let Some(target) = tree.arena.get(node) else {
            continue;
        };
        let is_canvas = matches!(target.kind(), NodeKind::Canvas(_));
        if is_canvas {
            tree.mark_paint(node);
        } else {
            tree.dirty_paint.insert(node);
            tree.dirty_settle.insert(node);
        }
        touched += 1;
    }
    touched
}

pub(crate) fn bind_geometry_node(tree: &mut NodeTree, geometry: GeometryId, node: NodeId) {
    let entry = tree.geometry_bindings.entry(geometry).or_default();
    if !entry.contains(&node) {
        entry.push(node);
    }
}

pub(crate) fn unbind_geometry_node(tree: &mut NodeTree, geometry: GeometryId, node: NodeId) {
    if let Some(nodes) = tree.geometry_bindings.get_mut(&geometry) {
        nodes.retain(|existing| *existing != node);
        if nodes.is_empty() {
            tree.geometry_bindings.remove(&geometry);
        }
    }
}

pub(crate) fn lane_offset(tree: &NodeTree, node: NodeId) -> LogicalPoint {
    tree.arena
        .get(node)
        .map_or(LogicalPoint::ZERO, |target| target.lane_offset)
}

pub(crate) fn set_lane_offset(tree: &mut NodeTree, node: NodeId, offset: LogicalPoint) {
    if let Some(target) = tree.arena.get_mut(node) {
        target.lane_offset = offset;
    }
}

pub(crate) fn paint_state(
    tree: &NodeTree,
    node: NodeId,
    screen_offset: LogicalPoint,
    bake_offset: LogicalPoint,
    clip_key: (u8, Option<DeviceRect>),
    inherited_opacity: f32,
) -> Option<super::PaintNodeState> {
    let target = tree.arena.get(node)?;
    let hidden = matches!(target.style.inner().display, taffy::style::Display::None)
        || target.style_patch().visible == Some(false);
    let screen_baked = matches!(
        target.kind(),
        super::NodeKind::Geometry(_) | super::NodeKind::Canvas(_)
    );
    let effective_bake = if screen_baked {
        screen_offset
    } else {
        bake_offset
    };
    let opacity = inherited_opacity
        * match target.style_patch().opacity {
            Some(value) if value.is_nan() => 0.0,
            Some(value) => value.clamp(0.0, 1.0),
            None => 1.0,
        };
    let reusable = target.dirty.contains(DirtyFlags::REUSABLE)
        && tree.clip_key(node) == Some(clip_key)
        && target.lane_offset == effective_bake
        && target.paint_opacity == opacity;
    let clip_bounds = (target.style.clip == crate::paint::style::Clip::Bounds).then(|| {
        target
            .layout
            .bounds
            .translate(LogicalPoint::new(-screen_offset.x, -screen_offset.y))
            .inset(target.layout.border)
    });
    let scroll = target
        .interaction
        .as_deref()
        .and_then(|slot| slot.scroll)
        .map(|slot| slot.offset);
    let geometry = match target.kind() {
        NodeKind::Geometry(id) => Some(*id),
        _ => None,
    };
    Some(super::PaintNodeState {
        hidden,
        reusable,
        lane: target.lanes.first().copied(),
        geometry,
        children: target.children.len(),
        clip_bounds,
        scroll,
        screen_baked,
        opacity,
    })
}

pub(crate) fn set_paint_opacity(tree: &mut NodeTree, node: NodeId, opacity: f32) {
    if let Some(target) = tree.arena.get_mut(node) {
        target.paint_opacity = opacity;
    }
}

pub(crate) fn clip_key(tree: &NodeTree, node: NodeId) -> Option<(u8, Option<DeviceRect>)> {
    tree.clip_keys.get(node.index() as usize).copied().flatten()
}

pub(crate) fn set_clip_key(tree: &mut NodeTree, node: NodeId, key: (u8, Option<DeviceRect>)) {
    let index = node.index() as usize;
    if tree.clip_keys.len() <= index {
        tree.clip_keys.resize_with(index + 1, || None);
    }
    tree.clip_keys[index] = Some(key);
}

pub(crate) fn clear_clip_key(tree: &mut NodeTree, node: NodeId) {
    let index = node.index() as usize;
    if let Some(slot) = tree.clip_keys.get_mut(index) {
        *slot = None;
    }
}

pub(crate) fn set_reusable(tree: &mut NodeTree, node: NodeId) {
    if let Some(target) = tree.arena.get_mut(node) {
        target.dirty.insert(DirtyFlags::REUSABLE);
    }
}

#[cfg(feature = "test-support")]
pub(crate) fn force_repaint_all(tree: &mut NodeTree) {
    tree.force_full_paint();
    let mut ids: Vec<NodeId> = Vec::new();
    tree.for_each_dfs(|id| ids.push(id));
    for id in ids {
        let _ = tree.mark_paint(id);
        if let Some(target) = tree.arena.get_mut(id) {
            target.dirty.remove(DirtyFlags::REUSABLE);
        }
    }
}

pub(crate) fn clear_lane(tree: &mut NodeTree, node: NodeId) {
    if let Some(target) = tree.arena.get_mut(node) {
        target.lanes.clear();
    }
}

pub(crate) fn drain_removed_lanes(tree: &mut NodeTree) -> std::vec::Drain<'_, LaneId> {
    tree.removed_lanes.drain(..)
}

pub(crate) fn a11y_of(tree: &NodeTree, node: NodeId) -> Option<&crate::a11y::tree::NodeA11y> {
    tree.a11y_slots
        .get(node.index() as usize)
        .and_then(|slot| slot.as_deref())
}

pub(crate) fn take_bind_a11y(
    tree: &mut NodeTree,
    node: NodeId,
) -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> crate::a11y::A11yPatch>> {
    tree.a11y_slots
        .get_mut(node.index() as usize)?
        .as_mut()?
        .bind
        .take()
}

pub(crate) fn put_bind_a11y(
    tree: &mut NodeTree,
    node: NodeId,
    bind: Box<dyn Fn(&mut ReadCtx<'_>) -> crate::a11y::A11yPatch>,
) -> bool {
    match tree
        .a11y_slots
        .get_mut(node.index() as usize)
        .and_then(|slot| slot.as_mut())
    {
        Some(slot) => {
            slot.bind = Some(bind);
            true
        }
        None => false,
    }
}

pub(crate) fn set_a11y_patch(
    tree: &mut NodeTree,
    node: NodeId,
    patch: crate::a11y::A11yPatch,
) -> bool {
    if tree.arena.get(node).is_none() {
        return false;
    }
    let index = node.index() as usize;
    if tree.a11y_slots.len() <= index {
        tree.a11y_slots.resize_with(index + 1, || None);
    }
    let slot = &mut tree.a11y_slots[index];
    let slot = slot.get_or_insert_with(|| Box::new(crate::a11y::tree::NodeA11y::new(None, None)));
    slot.patch = patch;
    true
}

pub(crate) fn has_a11y_bind(tree: &NodeTree, node: NodeId) -> bool {
    tree.a11y_slots
        .get(node.index() as usize)
        .and_then(|slot| slot.as_ref())
        .is_some_and(|slot| slot.bind.is_some())
}

pub(crate) fn take_bind_style(
    tree: &mut NodeTree,
    node: NodeId,
) -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>> {
    tree.arena.get_mut(node)?.bind_style.take()
}

pub(crate) fn put_bind_style(
    tree: &mut NodeTree,
    node: NodeId,
    bind_style: Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>,
) -> bool {
    match tree.arena.get_mut(node) {
        Some(target) => {
            target.bind_style = Some(bind_style);
            true
        }
        None => false,
    }
}

pub(crate) fn set_deps(tree: &mut NodeTree, node: NodeId, deps: SmallVec<[SignalId; 8]>) -> bool {
    match tree.arena.get_mut(node) {
        Some(target) => match &mut target.deps {
            Some(existing) => {
                existing.clear();
                existing.extend_from_slice(&deps);
            }
            None => target.deps = Some(Box::new(deps)),
        },
        None => return false,
    }
    true
}

pub(crate) fn set_style_patch(tree: &mut NodeTree, node: NodeId, patch: StylePatch) -> bool {
    match tree.arena.get_mut(node) {
        Some(target) => match &mut target.style_patch {
            Some(existing) => **existing = patch,
            None => target.style_patch = Some(Box::new(patch)),
        },
        None => return false,
    }
    true
}

pub(crate) fn has_visual_bind(tree: &NodeTree, node: NodeId) -> bool {
    tree.arena
        .get(node)
        .is_some_and(|target| target.bind_style.is_some() || target.text_bind.is_some())
}

pub(crate) fn interaction(tree: &NodeTree, node: NodeId) -> Option<&NodeInteraction> {
    tree.arena
        .get(node)
        .and_then(|target| target.interaction.as_deref())
}

pub(crate) fn ensure_interaction(
    tree: &mut NodeTree,
    node: NodeId,
) -> Option<&mut NodeInteraction> {
    tree.arena
        .get_mut(node)
        .map(|target| {
            target
                .interaction
                .get_or_insert_with(|| Box::new(NodeInteraction::new()))
        })
        .map(Box::as_mut)
}

pub(crate) fn is_hittable(tree: &NodeTree, node: NodeId) -> bool {
    tree.arena
        .get(node)
        .is_some_and(|target| target.style_patch().interactive != Some(false))
}

pub(crate) fn is_effectively_interactive(tree: &NodeTree, node: NodeId) -> bool {
    let mut cursor = Some(node);
    while let Some(current) = cursor {
        if !is_hittable(tree, current) {
            return false;
        }
        cursor = tree.arena.get(current).and_then(|target| target.parent);
    }
    true
}

pub(crate) fn note_hit_test_skip(tree: &NodeTree) {
    tree.hit_test_skips
        .set(tree.hit_test_skips.get().saturating_add(1));
}

pub(crate) fn hit_test_skips(tree: &NodeTree) -> u64 {
    tree.hit_test_skips.get()
}

pub(crate) fn focus_ring(tree: &NodeTree, node: NodeId) -> Option<crate::paint::style::FocusRing> {
    tree.interaction(node).and_then(|slot| slot.focus_ring)
}

pub(crate) fn is_explicitly_interactive(tree: &NodeTree, node: NodeId) -> bool {
    tree.arena
        .get(node)
        .is_some_and(|target| target.style_patch().interactive == Some(true))
}

pub(crate) fn scroll_slot(tree: &NodeTree, node: NodeId) -> Option<ScrollSlot> {
    tree.interaction(node).and_then(|slot| slot.scroll)
}

pub(crate) fn scroll_slot_mut(tree: &mut NodeTree, node: NodeId) -> Option<&mut ScrollSlot> {
    tree.ensure_interaction(node)
        .and_then(|slot| slot.scroll.as_mut())
}

pub(crate) fn scroll_proxy(tree: &NodeTree, node: NodeId) -> Option<NodeId> {
    tree.interaction(node).and_then(|slot| slot.scroll_proxy)
}

pub(crate) fn set_scroll_proxy(tree: &mut NodeTree, node: NodeId, proxy: Option<NodeId>) -> bool {
    let Some(interaction) = tree.ensure_interaction(node) else {
        return false;
    };
    interaction.scroll_proxy = proxy;
    true
}

pub(crate) fn overlay_slot(tree: &NodeTree, node: NodeId) -> Option<OverlaySlot> {
    tree.interaction(node).and_then(|slot| slot.overlay)
}

pub(crate) fn virtual_slot(tree: &NodeTree, node: NodeId) -> Option<&VirtualSlot> {
    tree.interaction(node)
        .and_then(|slot| slot.virtual_list.as_ref())
}

pub(crate) fn virtual_slot_mut(tree: &mut NodeTree, node: NodeId) -> Option<&mut VirtualSlot> {
    tree.ensure_interaction(node)
        .and_then(|slot| slot.virtual_list.as_mut())
}

#[cfg(test)]
pub(crate) fn set_style_for_test(tree: &mut NodeTree, node: NodeId, style: Style) -> bool {
    match tree.arena.get_mut(node) {
        Some(target) => {
            *target.style = style;
            true
        }
        None => false,
    }
}

#[cfg(test)]
pub(crate) fn set_style_patch_for_test(
    tree: &mut NodeTree,
    node: NodeId,
    patch: StylePatch,
) -> bool {
    tree.set_style_patch(node, patch)
}

#[cfg(test)]
pub(crate) fn edit_style_for_test(
    tree: &mut NodeTree,
    node: NodeId,
    edit: impl FnOnce(&mut Style),
) {
    if let Some(target) = tree.arena.get_mut(node) {
        edit(&mut target.style);
    }
}

pub(crate) struct TextDirtyQueue {
    nodes: Vec<NodeId>,
    seen: ahash::HashSet<NodeId>,
}

impl TextDirtyQueue {
    pub(crate) fn new() -> Self {
        Self {
            nodes: Vec::new(),
            seen: ahash::HashSet::default(),
        }
    }

    pub(crate) fn push(&mut self, node: NodeId) {
        if self.seen.insert(node) {
            self.nodes.push(node);
        }
    }

    #[must_use]
    pub(crate) fn contains(&self, node: NodeId) -> bool {
        self.seen.contains(&node)
    }

    pub(crate) fn take_into(&mut self, out: &mut Vec<NodeId>) {
        self.seen.clear();
        out.clear();
        out.append(&mut self.nodes);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }
}

impl Default for TextDirtyQueue {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) struct TextSlot {
    shaped: Rc<crate::text::shape::ShapedText>,
    key: crate::text::cache::ShapeKey,
    dpi_epoch: u32,
    line: Option<Rc<ShapedLine>>,
    laid_width_bits: u32,
    laid_generation: u32,
    stale_request: Option<(u32, u32)>,
}

impl TextSlot {
    pub(crate) fn shaped(&self) -> &Rc<crate::text::shape::ShapedText> {
        &self.shaped
    }

    pub(crate) fn key(&self) -> crate::text::cache::ShapeKey {
        self.key
    }

    pub(crate) fn dpi_epoch(&self) -> u32 {
        self.dpi_epoch
    }

    pub(crate) fn line(&self) -> Option<&Rc<ShapedLine>> {
        self.line.as_ref()
    }

    pub(crate) fn laid_width_bits(&self) -> u32 {
        self.laid_width_bits
    }

    pub(crate) fn laid_generation(&self) -> u32 {
        self.laid_generation
    }
}

#[cfg(test)]
mod tests {
    use super::TextDirtyQueue;
    use crate::{
        math::px,
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
        text::{TextContent, TextStyle},
    };

    fn text_tree(count: usize) -> (NodeTree, Vec<NodeId>) {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut nodes = Vec::with_capacity(count);
        for index in 0..count {
            let node = NodeBuilder::text(format!("t{index}"), TextStyle::new(px(12.0)))
                .build(&mut tree, root)
                .expect("text node");
            nodes.push(node);
        }
        (tree, nodes)
    }

    #[test]
    fn text_dirty_queue_dedupes_in_constant_time() {
        let (tree, nodes) = text_tree(10);
        let mut queue = TextDirtyQueue::new();
        for _ in 0..100 {
            for node in &nodes {
                queue.push(*node);
            }
        }
        assert_eq!(queue.len(), nodes.len(), "去重后恒为节点数");
        let mut out = Vec::new();
        queue.take_into(&mut out);
        assert_eq!(out.len(), nodes.len());
        assert_eq!(queue.len(), 0, "take 之后队列为空");
        queue.push(nodes[0]);
        assert_eq!(queue.len(), 1);
        let _ = tree;
    }

    #[test]
    fn text_content_and_layout_queues_are_separate() {
        let (mut tree, nodes) = text_tree(2);
        assert_eq!(tree.text_dirty_len(), 2);
        assert_eq!(tree.text_layout_dirty_len(), 0);

        let node = nodes[0];
        let content = TextContent::new("changed", TextStyle::new(px(12.0)));
        assert!(tree.set_text_content(node, content));
        assert_eq!(
            tree.text_dirty_len(),
            2,
            "同一节点重复标脏被 O(1) 去重（R12-13）"
        );

        let mut shaped = Vec::new();
        tree.take_text_dirty(&mut shaped);
        assert_eq!(tree.text_dirty_len(), 0);
        let mut layout = Vec::new();
        tree.take_text_layout_dirty(&mut layout);
        assert!(layout.is_empty(), "还没有快照 ⇒ 没有待重排的节点");
        assert_eq!(tree.invalidate_text_lines(), 0, "还没有 line 可失效");
    }
}

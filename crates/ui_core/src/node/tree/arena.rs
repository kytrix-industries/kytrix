use smallvec::SmallVec;
use smol_str::SmolStr;

use super::{HitFocusSkip, LayoutBox, Node, NodeKind, NodeSpec, NodeTree, layout::LayoutStats};
use crate::{
    error::EngineError,
    event::Action,
    interaction::NodeInteraction,
    layout::{LayoutOverride, MeasureFn},
    math::{LogicalEdges, LogicalPoint, LogicalPx, LogicalRect, LogicalSize},
    node::{
        dependency::ReadCtx,
        dirty::{DirtyFlags, DirtySet},
        id::{Arena, NodeId},
        signal::SignalId,
    },
    paint::style::{PaintSpec, Style, StylePatch},
    text_edit::EditableText,
};

pub(crate) fn new() -> NodeTree {
    let mut root_style = Style::default();
    root_style.size_percent(1.0, 1.0);
    let mut arena: Arena<Node> = Arena::new();
    let root = arena.insert_with(|id| Node {
        id,
        parent: None,
        children: Vec::new(),
        kind: NodeKind::Box,
        canvas_generation: 0,
        style: Box::new(root_style),
        style_patch: None,
        bind_style: None,
        measure: None,
        hit_shape: None,
        layout_override: None,
        tab_stop: true,
        paint: None,
        content_offset: LogicalPoint::ZERO,
        text_bind: None,
        text_measured_size: LogicalSize::ZERO,
        text_content_width: LogicalPx::ZERO,
        layout: LayoutBox::default(),
        local: LogicalRect::default(),
        local_border: LogicalEdges::ZERO,
        laid_out_epoch: 0,
        damage: LogicalRect::default(),
        lanes: SmallVec::new(),
        deps: None,
        dirty: DirtyFlags::LAYOUT | DirtyFlags::PAINT | DirtyFlags::A11Y,
        interaction: None,
        lane_offset: LogicalPoint::ZERO,
        paint_opacity: 1.0,
    });
    let mut dirty_layout = DirtySet::new();
    dirty_layout.insert(root);
    let mut dirty_paint = DirtySet::new();
    dirty_paint.insert(root);
    let mut dirty_a11y = DirtySet::new();
    dirty_a11y.insert(root);
    let mut dirty_settle = DirtySet::new();
    dirty_settle.insert(root);
    NodeTree {
        arena,
        root,
        window: None,
        hit_test_skips: std::cell::Cell::new(0),
        layout_cache: ahash::HashMap::default(),
        text_slots: vec![None],
        editable_slots: vec![None],
        debug_ids: vec![None],
        clip_keys: vec![None],
        baked_screen_bounds: vec![None],
        painted_extents: vec![None],
        popup_transforms: vec![None],
        paint_scales: vec![(1.0, LogicalPoint::ZERO)],
        popup_pointer_transparent: vec![false],
        subtree_bounds: vec![LogicalRect::default()],
        a11y_slots: vec![None],
        images_need_refresh: false,
        image_nodes: Vec::new(),
        canvas_nodes: Vec::new(),
        dirty_bind: DirtySet::new(),
        dirty_layout,
        dirty_paint,
        dirty_a11y,
        dirty_settle,
        moved_nodes: DirtySet::new(),
        scroll_containers: Vec::new(),
        virtual_containers: Vec::new(),
        text_dirty: crate::node::tree::content::TextDirtyQueue::new(),
        text_layout_dirty: crate::node::tree::content::TextDirtyQueue::new(),
        removed_lanes: Vec::new(),
        hoisted: ahash::HashMap::default(),
        layout_epoch: 0,
        layout_stats: LayoutStats::default(),
        invalid_measure: None,
        geometry_damage_escalation: false,
        geometry_transforms: ahash::HashMap::default(),
        geometry_chunks: ahash::HashMap::default(),
        geometry_bindings: ahash::HashMap::default(),
        paint_marks: vec![0],
        paint_epoch: 0,
        scroll_slot_of: vec![0],
        structure_epoch: 0,
        painted_structure_epoch: 0,
        scroll_reg_epoch: 0,
        painted_scroll_reg_epoch: 0,
        force_full_paint: false,
        paint_visited: 0,
        paint_order_rebuilds: 0,
        paint_full_frames: 0,
        paint_incremental_frames: 0,
        dirty_scratch: Vec::new(),
        paint_tail: SmallVec::new(),
        order_dirty: false,
        popup_root: None,
        #[cfg(feature = "test-support")]
        scanned: std::cell::RefCell::new(ahash::HashSet::default()),
        bound_nodes: 0,
    }
}

pub(crate) fn window_tag(tree: &NodeTree) -> u32 {
    tree.arena.scope()
}

pub(crate) fn root(tree: &NodeTree) -> NodeId {
    tree.root
}

pub(crate) fn bind_window(tree: &mut NodeTree, window: crate::window::WindowId) {
    debug_assert!(
        tree.window.is_none(),
        "窗口只能绑定一次（NodeTree::bind_window）"
    );
    debug_assert_eq!(
        tree.arena.len(),
        1,
        "窗口必须在挂载任何子节点之前绑定（只有根节点存在）"
    );
    tree.arena.set_scope(window.to_bits());
    let root = tree.root.with_window_tag(window.to_bits());
    if let Some(node) = tree.arena.get_mut(tree.root) {
        node.retag(root);
    }
    tree.root = root;
    tree.window = Some(window);
    for set in [
        &mut tree.dirty_layout,
        &mut tree.dirty_paint,
        &mut tree.dirty_a11y,
        &mut tree.dirty_settle,
    ] {
        set.clear();
        set.insert(root);
    }
}

pub(crate) fn get(tree: &NodeTree, node: NodeId) -> Result<&Node, EngineError> {
    tree.arena.get(node).ok_or(EngineError::StaleNode { node })
}

pub(crate) fn len(tree: &NodeTree) -> usize {
    tree.arena.len()
}

pub(crate) fn insert_child(
    tree: &mut NodeTree,
    parent: NodeId,
    spec: NodeSpec,
) -> Result<NodeId, EngineError> {
    tree.get(parent)?;
    let id = insert_prepared(tree, parent, spec);
    if let Some(editable) = tree.editable(id)
        && let Err(detail) = editable.spec().validate()
    {
        let _ = remove(tree, id);
        return Err(EngineError::TextEditSpecInvalid { node: id, detail });
    }
    Ok(id)
}

pub(crate) fn install_popup_root(tree: &mut NodeTree) -> NodeId {
    let parent = tree.root;
    let spec = NodeSpec::new(NodeKind::Box, super::popup_root_style(), None)
        .with_hit_focus_skip(HitFocusSkip::Container)
        .with_debug_id("popup-root");
    let id = insert_prepared(tree, parent, spec);
    pin_tail_child(tree, id);
    tree.popup_root = Some(id);
    id
}

fn insert_prepared(tree: &mut NodeTree, parent: NodeId, spec: NodeSpec) -> NodeId {
    let NodeSpec {
        kind,
        style,
        bind_style,
        measure,
        hit_shape,
        layout_override,
        bind_text,
        editable,
        paint,
        debug_id,
        action,
        drag,
        focus_ring,
        a11y,
        bind_a11y,
        tab_stop,
        hit_focus_skip,
    } = spec;
    let bound = bind_style.is_some() || bind_text.is_some() || bind_a11y.is_some();
    let interaction = (action.is_some()
        || drag.is_some()
        || focus_ring.is_some()
        || hit_focus_skip != HitFocusSkip::None)
        .then(|| {
            let mut slot = NodeInteraction::new();
            slot.action = action;
            slot.drag = drag;
            slot.focus_ring = focus_ring;
            slot.hit_focus_skip = hit_focus_skip;
            Box::new(slot)
        });
    let id = tree.arena.insert_with(move |id| Node {
        id,
        parent: None,
        children: Vec::new(),
        kind,
        canvas_generation: 0,
        style: Box::new(style),
        style_patch: None,
        bind_style,
        measure,
        hit_shape: hit_shape.map(Box::new),
        layout_override,
        tab_stop,
        paint,
        content_offset: LogicalPoint::ZERO,
        text_bind: bind_text,
        text_measured_size: LogicalSize::ZERO,
        text_content_width: LogicalPx::ZERO,
        layout: LayoutBox::default(),
        local: LogicalRect::default(),
        local_border: LogicalEdges::ZERO,
        laid_out_epoch: 0,
        damage: LogicalRect::default(),
        lanes: SmallVec::new(),
        deps: None,
        dirty: DirtyFlags::LAYOUT | DirtyFlags::PAINT | DirtyFlags::A11Y,
        interaction,
        lane_offset: LogicalPoint::ZERO,
        paint_opacity: 1.0,
    });
    if tree
        .arena
        .get(id)
        .is_some_and(|target| target.kind.image().is_some())
    {
        tree.images_need_refresh = true;
        tree.image_nodes.push(id);
    }
    if tree
        .arena
        .get(id)
        .is_some_and(|target| matches!(target.kind, NodeKind::Canvas(_)))
    {
        tree.canvas_nodes.push(id);
    }
    tree.layout_cache.remove(&id);
    let index = id.index() as usize;
    if tree.text_slots.len() <= index {
        tree.text_slots.resize_with(index + 1, || None);
    }
    tree.text_slots[index] = None;
    if tree.editable_slots.len() <= index {
        tree.editable_slots.resize_with(index + 1, || None);
    }
    tree.editable_slots[index] = editable.map(|spec| Box::new(EditableText::new(spec)));
    if tree
        .arena
        .get(id)
        .is_some_and(|target| target.kind.text().is_some())
    {
        tree.mark_text_dirty(id);
    }
    if bound {
        tree.bound_nodes += 1;
        let _ = tree.touch(id, DirtyFlags::BIND);
    }
    if tree.debug_ids.len() <= index {
        tree.debug_ids.resize_with(index + 1, || None);
    }
    tree.debug_ids[index] = debug_id;
    if tree.clip_keys.len() <= index {
        tree.clip_keys.resize_with(index + 1, || None);
    }
    tree.clip_keys[index] = None;
    if tree.baked_screen_bounds.len() <= index {
        tree.baked_screen_bounds.resize_with(index + 1, || None);
    }
    tree.baked_screen_bounds[index] = None;
    if tree.painted_extents.len() <= index {
        tree.painted_extents.resize_with(index + 1, || None);
    }
    tree.painted_extents[index] = None;
    if tree.paint_marks.len() <= index {
        tree.paint_marks.resize(index + 1, 0);
    }
    tree.paint_marks[index] = 0;
    if tree.scroll_slot_of.len() <= index {
        tree.scroll_slot_of.resize(index + 1, 0);
    }
    tree.scroll_slot_of[index] = 0;
    if tree.subtree_bounds.len() <= index {
        tree.subtree_bounds
            .resize_with(index + 1, LogicalRect::default);
    }
    tree.subtree_bounds[index] = LogicalRect::default();
    if tree.popup_transforms.len() <= index {
        tree.popup_transforms.resize_with(index + 1, || None);
    }

    if tree.a11y_slots.len() <= index {
        tree.a11y_slots.resize_with(index + 1, || None);
    }
    tree.a11y_slots[index] = (a11y.is_some() || bind_a11y.is_some())
        .then(|| Box::new(crate::a11y::tree::NodeA11y::new(a11y, bind_a11y)));
    if tree.editable(id).is_some() {
        crate::text_edit::sync_a11y_patch(tree, id);
    }
    tree.dirty_layout.insert(id);
    tree.dirty_paint.insert(id);
    tree.dirty_a11y.insert(id);
    tree.dirty_settle.insert(id);
    tree.hoisted.remove(&id);
    if let Some(node) = tree.arena.get_mut(id) {
        node.parent = Some(parent);
    }
    let position = child_insert_position(tree, parent);
    if let Some(node) = tree.arena.get_mut(parent) {
        node.children.insert(position, id);
    }
    match tree.arena.get(id).map(Node::kind) {
        Some(NodeKind::Geometry(geometry)) => tree.bind_geometry_node(*geometry, id),
        Some(NodeKind::Canvas(canvas)) => {
            for geometry in super::content::canvas_geometry_ids(canvas) {
                tree.bind_geometry_node(geometry, id);
            }
        }
        _ => {}
    }
    tree.mark_structure(parent);
    id
}

pub(crate) fn append_child(
    tree: &mut NodeTree,
    parent: NodeId,
    child: NodeId,
) -> Result<(), EngineError> {
    tree.get(parent)?;
    tree.get(child)?;
    if parent == child || tree.is_ancestor(child, parent) {
        debug_assert!(
            false,
            "structural cycle: cannot append an ancestor as a child"
        );
        return Ok(());
    }

    if let Some(old_parent) = tree.get(child)?.parent {
        if let Some(p) = tree.arena.get_mut(old_parent) {
            p.children.retain(|existing| *existing != child);
        }
        tree.mark_structure(old_parent);
        tree.mark_subtree_paint(child);
    }

    if let Some(node) = tree.arena.get_mut(child) {
        node.parent = Some(parent);
    }
    let position = child_insert_position(tree, parent);
    if let Some(node) = tree.arena.get_mut(parent) {
        node.children.insert(position, child);
    }
    if !tree.scroll_containers.is_empty()
        && tree
            .scroll_containers
            .iter()
            .any(|container| *container == child || tree.is_ancestor(child, *container))
    {
        tree.scroll_reg_epoch = tree.scroll_reg_epoch.wrapping_add(1);
    }
    tree.mark_structure(parent);
    Ok(())
}

fn child_insert_position(tree: &NodeTree, parent: NodeId) -> usize {
    let Some(parent_node) = tree.arena.get(parent) else {
        return 0;
    };
    if parent != tree.root() || tree.paint_tail.is_empty() {
        return parent_node.children.len();
    }
    let mut position = parent_node.children.len();
    while position > 0
        && tree
            .paint_tail
            .iter()
            .any(|tail| *tail == parent_node.children[position - 1])
    {
        position -= 1;
    }
    position
}

pub(crate) fn pin_tail_child(tree: &mut NodeTree, node: NodeId) {
    debug_assert!(
        tree.get(node)
            .is_ok_and(|target| target.parent() == Some(tree.root)),
        "only root children can be pinned to the paint tail"
    );
    if !tree.paint_tail.contains(&node) {
        tree.paint_tail.push(node);
    }
    if let Some(root) = tree.arena.get_mut(tree.root) {
        root.children.retain(|existing| *existing != node);
        root.children.push(node);
    }
    tree.mark_structure(tree.root);
    tree.mark_order_dirty();
    tree.mark_a11y(tree.root);
    let _ = tree.mark_paint_without_damage(tree.root);
}

pub(crate) fn reorder_children(
    tree: &mut NodeTree,
    parent: NodeId,
    order: &[NodeId],
) -> Result<(), EngineError> {
    let current = tree.get(parent)?.children().to_vec();
    if order.len() != current.len() {
        return Err(EngineError::NodeSpecInvalid {
            parent,
            detail: SmolStr::new_static(
                "reorder_children requires a full permutation of the parent's children",
            ),
        });
    }
    let mut seen: Vec<NodeId> = Vec::with_capacity(order.len());
    for child in order {
        if !current.contains(child) || seen.contains(child) {
            return Err(EngineError::NodeSpecInvalid {
                parent,
                detail: SmolStr::new_static(
                    "reorder_children requires a full permutation of the parent's children",
                ),
            });
        }
        seen.push(*child);
    }
    if current == order {
        return Ok(());
    }
    let mut damage = LogicalRect::default();
    let mut nodes: Vec<NodeId> = Vec::with_capacity(current.len());
    for child in &current {
        damage = damage.union(tree.subtree_bounds(*child));
        nodes.clear();
        tree.for_each_subtree(*child, |node| nodes.push(node));
        for node in &nodes {
            if let Some(extent) = tree.painted_extent(*node) {
                damage = damage.union(extent.layout_bounds());
            }
        }
    }
    if let Some(node) = tree.arena.get_mut(parent) {
        node.damage = node.damage.union(damage);
        node.children.clear();
        node.children.extend_from_slice(order);
    }
    if order.iter().any(|child| !tree.is_out_of_flow(*child)) {
        tree.invalidate_layout(parent);
    }
    tree.mark_order_dirty();
    mark_order_a11y(tree, parent);
    let _ = tree.mark_paint_without_damage(parent);
    Ok(())
}

fn mark_order_a11y(tree: &mut NodeTree, parent: NodeId) {
    tree.mark_a11y(parent);
    if tree.hit_focus_skip(parent) != HitFocusSkip::Container {
        return;
    }
    let mut cursor = tree.get(parent).ok().and_then(|node| node.parent());
    while let Some(current) = cursor {
        tree.mark_a11y(current);
        if tree.hit_focus_skip(current) != HitFocusSkip::Container {
            return;
        }
        cursor = tree.get(current).ok().and_then(|node| node.parent());
    }
}

pub(crate) fn remove(
    tree: &mut NodeTree,
    node: NodeId,
) -> Result<Vec<(NodeId, SmallVec<[SignalId; 8]>)>, EngineError> {
    if node == tree.root {
        return Err(EngineError::StaleNode { node });
    }
    let parent = tree
        .get(node)?
        .parent
        .expect("non-root node always has a parent");
    if let Some(p) = tree.arena.get_mut(parent) {
        p.children.retain(|existing| *existing != node);
    }
    let parent_own = tree
        .scroll_slot(parent)
        .map_or(LogicalPoint::ZERO, |slot| slot.offset);
    let mut block: Vec<NodeId> = Vec::with_capacity(16);
    let mut scroll_extras: Vec<LogicalPoint> = Vec::with_capacity(16);
    let mut stack: SmallVec<[(NodeId, LogicalPoint); 16]> = SmallVec::new();
    stack.push((node, parent_own));
    while let Some((current, extra)) = stack.pop() {
        block.push(current);
        scroll_extras.push(extra);
        #[cfg(feature = "test-support")]
        tree.note_scanned(current);
        if let Some(target) = tree.arena.get(current) {
            let child_extra = extra
                + tree
                    .scroll_slot(current)
                    .map_or(LogicalPoint::ZERO, |slot| slot.offset);
            for child in target.children.iter().rev() {
                stack.push((*child, child_extra));
            }
        }
    }
    let removed: ahash::HashSet<NodeId> = block.iter().copied().collect();
    if block.iter().any(|id| {
        tree.arena
            .get(*id)
            .is_some_and(|target| matches!(target.kind, NodeKind::Geometry(_)))
    }) {
        tree.geometry_damage_escalation = true;
    }
    if removed.iter().any(|id| {
        tree.scroll_containers
            .iter()
            .any(|container| container == id)
    }) {
        tree.scroll_reg_epoch = tree.scroll_reg_epoch.wrapping_add(1);
    }
    let mut subtree_damage = LogicalRect::default();
    for (id, extra) in block.iter().zip(&scroll_extras) {
        if let Some(target) = tree.arena.get(*id) {
            let painted = tree
                .painted_extent(*id)
                .map_or_else(LogicalRect::default, |extent| extent.layout_bounds());
            let paint = target.damage.union(target.layout.bounds).union(painted);
            subtree_damage =
                subtree_damage.union(paint.translate(LogicalPoint::new(-extra.x, -extra.y)));
            if !target.lanes.is_empty() {
                tree.removed_lanes.extend(target.lanes.iter().copied());
            }
        }
    }
    if let Some(p) = tree.arena.get_mut(parent) {
        p.damage = p.damage.union(subtree_damage);
    }
    let mut detached: Vec<(NodeId, SmallVec<[SignalId; 8]>)> = Vec::with_capacity(block.len());
    for id in block.iter().rev() {
        let index = id.index() as usize;
        tree.layout_cache.remove(id);
        if let Some(slot) = tree.text_slots.get_mut(index) {
            *slot = None;
        }
        if let Some(slot) = tree.editable_slots.get_mut(index) {
            *slot = None;
        }
        if let Some(slot) = tree.debug_ids.get_mut(index) {
            *slot = None;
        }
        if let Some(slot) = tree.clip_keys.get_mut(index) {
            *slot = None;
        }
        if let Some(slot) = tree.baked_screen_bounds.get_mut(index) {
            *slot = None;
        }
        if let Some(slot) = tree.painted_extents.get_mut(index) {
            *slot = None;
        }
        if let Some(slot) = tree.paint_marks.get_mut(index) {
            *slot = 0;
        }
        if let Some(slot) = tree.scroll_slot_of.get_mut(index) {
            *slot = 0;
        }

        if let Some(slot) = tree.popup_transforms.get_mut(index) {
            *slot = None;
        }
        if let Some(slot) = tree.paint_scales.get_mut(index) {
            *slot = (1.0, LogicalPoint::ZERO);
        }
        if let Some(slot) = tree.popup_pointer_transparent.get_mut(index) {
            *slot = false;
        }
        if let Some(slot) = tree.a11y_slots.get_mut(index) {
            *slot = None;
        }
        tree.dirty_bind.remove(*id);
        tree.dirty_layout.remove(*id);
        tree.dirty_paint.remove(*id);
        tree.dirty_a11y.remove(*id);
        tree.dirty_settle.remove(*id);
        tree.moved_nodes.remove(*id);
        tree.hoisted.remove(id);
        tree.geometry_transforms.remove(id);
        tree.geometry_chunks.remove(id);
        let geometry_ids: SmallVec<[crate::paint::geometry::GeometryId; 2]> =
            match tree.arena.get(*id).map(Node::kind) {
                Some(NodeKind::Geometry(geometry)) => SmallVec::from_slice(&[*geometry]),
                Some(NodeKind::Canvas(canvas)) => super::content::canvas_geometry_ids(canvas),
                _ => SmallVec::new(),
            };
        for geometry in geometry_ids {
            super::content::unbind_geometry_node(tree, geometry, *id);
        }
        let had_bind = tree
            .arena
            .get(*id)
            .is_some_and(|target| target.bind_style.is_some() || target.text_bind.is_some())
            || tree
                .a11y_slots
                .get(id.index() as usize)
                .and_then(|slot| slot.as_deref())
                .is_some_and(|slot| slot.bind.is_some());
        if had_bind {
            tree.bound_nodes = tree.bound_nodes.saturating_sub(1);
        }
        if let Some(target) = tree.arena.remove(*id)
            && let Some(deps) = target.deps
            && !deps.is_empty()
        {
            detached.push((*id, *deps));
        }
    }
    if !tree.hoisted.is_empty() {
        tree.hoisted.retain(|_, list| {
            list.retain(|child| !removed.contains(child));
            !list.is_empty()
        });
    }
    if !tree.scroll_containers.is_empty() {
        tree.scroll_containers.retain(|id| !removed.contains(id));
    }
    if !tree.virtual_containers.is_empty() {
        tree.virtual_containers.retain(|id| !removed.contains(id));
    }
    if !tree.image_nodes.is_empty() {
        tree.image_nodes.retain(|id| !removed.contains(id));
    }
    if !tree.canvas_nodes.is_empty() {
        tree.canvas_nodes.retain(|id| !removed.contains(id));
    }
    if !tree.paint_tail.is_empty() {
        tree.paint_tail.retain(|id| !removed.contains(id));
    }
    tree.dirty_bind.shrink_stale();
    tree.dirty_layout.shrink_stale();
    tree.dirty_paint.shrink_stale();
    tree.dirty_a11y.shrink_stale();
    tree.dirty_settle.shrink_stale();
    tree.moved_nodes.shrink_stale();
    tree.mark_structure(parent);
    super::layout::recompute_bounds_upwards(tree, parent);
    Ok(detached)
}

pub(crate) fn set_press_focus_target(
    tree: &mut NodeTree,
    host: NodeId,
    target: NodeId,
) -> Result<(), EngineError> {
    tree.get(host)?;
    tree.get(target)?;
    let node = tree
        .ensure_interaction(host)
        .ok_or(EngineError::StaleNode { node: host })?;
    node.press_focus_target = Some(target);
    Ok(())
}

pub(crate) fn debug_id(tree: &NodeTree, node: NodeId) -> Option<&str> {
    tree.debug_ids
        .get(node.index() as usize)
        .and_then(|slot| slot.as_ref())
        .map(SmolStr::as_str)
}

impl NodeSpec {
    pub(crate) fn new(
        kind: NodeKind,
        style: Style,
        bind_style: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>>,
    ) -> Self {
        Self {
            kind,
            style,
            bind_style,
            measure: None,
            hit_shape: None,
            layout_override: None,
            bind_text: None,
            editable: None,
            paint: None,
            debug_id: None,
            action: None,
            drag: None,
            focus_ring: None,
            a11y: None,
            bind_a11y: None,
            tab_stop: true,
            hit_focus_skip: HitFocusSkip::None,
        }
    }

    pub(crate) fn with_a11y(mut self, over: crate::a11y::A11yOverride) -> Self {
        self.a11y = Some(over);
        self
    }

    pub(crate) fn with_bind_a11y(
        mut self,
        bind: Box<dyn Fn(&mut ReadCtx<'_>) -> crate::a11y::A11yPatch>,
    ) -> Self {
        self.bind_a11y = Some(bind);
        self
    }

    pub(crate) fn with_tab_stop(mut self, tab_stop: bool) -> Self {
        self.tab_stop = tab_stop;
        self
    }

    pub(crate) fn with_hit_focus_skip(mut self, hit_focus_skip: HitFocusSkip) -> Self {
        self.hit_focus_skip = hit_focus_skip;
        self
    }

    pub(crate) fn with_focus_ring(mut self, ring: crate::paint::style::FocusRing) -> Self {
        self.focus_ring = Some(ring);
        self
    }

    pub(crate) fn with_action(mut self, action: Action) -> Self {
        self.action = Some(action);
        self
    }

    pub(crate) fn with_drag(mut self, action: Action) -> Self {
        self.drag = Some(action);
        self
    }

    pub(crate) fn with_paint(mut self, paint: PaintSpec) -> Self {
        self.paint = Some(Box::new(paint));
        self
    }

    pub(crate) fn with_debug_id(mut self, id: impl Into<SmolStr>) -> Self {
        self.debug_id = Some(id.into());
        self
    }

    pub(crate) fn with_measure(mut self, measure: MeasureFn) -> Self {
        self.measure = Some(measure);
        self
    }

    pub(crate) fn with_hit_shape(mut self, hit_shape: crate::layout::hit_test::HitShape) -> Self {
        self.hit_shape = Some(hit_shape);
        self
    }

    pub(crate) fn with_layout_override(mut self, layout_override: LayoutOverride) -> Self {
        self.layout_override = Some(layout_override);
        self
    }

    pub(crate) fn with_bind_text(
        mut self,
        bind_text: Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>,
    ) -> Self {
        self.bind_text = Some(bind_text);
        self
    }

    pub(crate) fn with_editable(mut self, editable: crate::text_edit::EditableSpec) -> Self {
        self.editable = Some(editable);
        self
    }
}

impl Default for NodeSpec {
    fn default() -> Self {
        Self::new(NodeKind::Box, Style::default(), None)
    }
}

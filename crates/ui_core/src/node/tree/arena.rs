use smallvec::SmallVec;
use smol_str::SmolStr;

use super::{LayoutBox, Node, NodeKind, NodeSpec, NodeTree, layout::LayoutStats};
use crate::{
    error::EngineError,
    event::Action,
    interaction::NodeInteraction,
    layout::{LayoutOverride, MeasureFn},
    math::{Edges, Point, Rect, Size},
    node::{
        dependency::ReadCtx,
        dirty::{DirtyFlags, DirtySet},
        id::{Arena, NodeId},
        signal::SignalId,
    },
    paint::style::{PaintSpec, Style, StylePatch},
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
        style: Box::new(root_style),
        style_patch: None,
        bind_style: None,
        measure: None,
        hit_shape: None,
        layout_override: None,
        tab_stop: true,
        paint: None,
        content_offset: Point::ZERO,
        text_bind: None,
        text_measured_size: Size::ZERO,
        text_content_width: 0.0,
        layout: LayoutBox::default(),
        local: Rect::default(),
        local_border: Edges::ZERO,
        laid_out_epoch: 0,
        damage: Rect::default(),
        lanes: SmallVec::new(),
        deps: None,
        dirty: DirtyFlags::LAYOUT | DirtyFlags::PAINT | DirtyFlags::A11Y,
        interaction: None,
        lane_offset: Point::ZERO,
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
        debug_ids: vec![None],
        clip_keys: vec![None],
        subtree_bounds: vec![Rect::default()],
        a11y_slots: vec![None],
        images_need_refresh: false,
        dirty_bind: DirtySet::new(),
        dirty_layout,
        dirty_paint,
        dirty_a11y,
        dirty_settle,
        moved_nodes: DirtySet::new(),
        scroll_containers: Vec::new(),
        virtual_containers: Vec::new(),
        overlay_containers: Vec::new(),
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
    let NodeSpec {
        kind,
        style,
        bind_style,
        measure,
        hit_shape,
        layout_override,
        bind_text,
        paint,
        debug_id,
        action,
        drag,
        focus_ring,
        a11y,
        bind_a11y,
        tab_stop,
    } = spec;
    let bound = bind_style.is_some() || bind_text.is_some() || bind_a11y.is_some();
    let interaction = (action.is_some() || drag.is_some() || focus_ring.is_some()).then(|| {
        let mut slot = NodeInteraction::new();
        slot.action = action;
        slot.drag = drag;
        slot.focus_ring = focus_ring;
        Box::new(slot)
    });
    let id = tree.arena.insert_with(move |id| Node {
        id,
        parent: None,
        children: Vec::new(),
        kind,
        style: Box::new(style),
        style_patch: None,
        bind_style,
        measure,
        hit_shape: hit_shape.map(Box::new),
        layout_override,
        tab_stop,
        paint,
        content_offset: Point::ZERO,
        text_bind: bind_text,
        text_measured_size: Size::ZERO,
        text_content_width: 0.0,
        layout: LayoutBox::default(),
        local: Rect::default(),
        local_border: Edges::ZERO,
        laid_out_epoch: 0,
        damage: Rect::default(),
        lanes: SmallVec::new(),
        deps: None,
        dirty: DirtyFlags::LAYOUT | DirtyFlags::PAINT | DirtyFlags::A11Y,
        interaction,
        lane_offset: Point::ZERO,
        paint_opacity: 1.0,
    });
    if tree
        .arena
        .get(id)
        .is_some_and(|target| target.kind.image().is_some())
    {
        tree.images_need_refresh = true;
    }
    tree.layout_cache.remove(&id);
    let index = id.index() as usize;
    if tree.text_slots.len() <= index {
        tree.text_slots.resize_with(index + 1, || None);
    }
    tree.text_slots[index] = None;
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
    if tree.paint_marks.len() <= index {
        tree.paint_marks.resize(index + 1, 0);
    }
    tree.paint_marks[index] = 0;
    if tree.scroll_slot_of.len() <= index {
        tree.scroll_slot_of.resize(index + 1, 0);
    }
    tree.scroll_slot_of[index] = 0;
    if tree.subtree_bounds.len() <= index {
        tree.subtree_bounds.resize_with(index + 1, Rect::default);
    }
    tree.subtree_bounds[index] = Rect::default();
    if tree.a11y_slots.len() <= index {
        tree.a11y_slots.resize_with(index + 1, || None);
    }
    tree.a11y_slots[index] = (a11y.is_some() || bind_a11y.is_some())
        .then(|| Box::new(crate::a11y::tree::NodeA11y::new(a11y, bind_a11y)));
    tree.dirty_layout.insert(id);
    tree.dirty_paint.insert(id);
    tree.dirty_a11y.insert(id);
    tree.dirty_settle.insert(id);
    tree.hoisted.remove(&id);
    if let Some(node) = tree.arena.get_mut(id) {
        node.parent = Some(parent);
    }
    if let Some(node) = tree.arena.get_mut(parent) {
        node.children.push(id);
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
    Ok(id)
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
    if let Some(node) = tree.arena.get_mut(parent) {
        node.children.push(child);
    }
    tree.mark_structure(parent);
    Ok(())
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
    let mut block: Vec<NodeId> = Vec::with_capacity(16);
    tree.for_each_subtree(node, |id| block.push(id));
    let removed: ahash::HashSet<NodeId> = block.iter().copied().collect();
    if block.iter().any(|id| {
        tree.arena
            .get(*id)
            .is_some_and(|target| matches!(target.kind, NodeKind::Geometry(_)))
    }) {
        tree.geometry_damage_escalation = true;
    }
    let mut subtree_damage = Rect::default();
    for id in &block {
        if let Some(target) = tree.arena.get(*id) {
            subtree_damage = subtree_damage
                .union(target.damage)
                .union(target.layout.bounds);
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
        if let Some(slot) = tree.debug_ids.get_mut(index) {
            *slot = None;
        }
        if let Some(slot) = tree.clip_keys.get_mut(index) {
            *slot = None;
        }
        if let Some(slot) = tree.paint_marks.get_mut(index) {
            *slot = 0;
        }
        if let Some(slot) = tree.scroll_slot_of.get_mut(index) {
            *slot = 0;
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
    if !tree.overlay_containers.is_empty() {
        tree.overlay_containers.retain(|id| !removed.contains(id));
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
            paint: None,
            debug_id: None,
            action: None,
            drag: None,
            focus_ring: None,
            a11y: None,
            bind_a11y: None,
            tab_stop: true,
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
}

impl Default for NodeSpec {
    fn default() -> Self {
        Self::new(NodeKind::Box, Style::default(), None)
    }
}

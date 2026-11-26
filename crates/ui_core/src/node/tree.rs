mod arena;
mod content;
mod dirty;
mod image;
mod layout;
#[cfg(test)]
mod tests;
mod traverse;

#[cfg(feature = "test-support")]
use std::cell::RefCell;
use std::{fmt, rc::Rc, sync::Arc};

use smallvec::SmallVec;
use smol_str::SmolStr;

use self::{content::TextSlot, layout::LayoutStats};
use crate::{
    error::EngineError,
    event::Action,
    interaction::{NodeInteraction, OverlaySlot, ScrollSlot, VirtualSlot},
    layout::{
        Axis, LayoutOverride, LayoutReport, MeasureFn,
        hit_test::{HitResult, HitShape},
    },
    math::{
        DeviceRect, LogicalEdges, LogicalPoint, LogicalPx, LogicalRect, LogicalSize, Transform,
    },
    node::{
        dependency::ReadCtx,
        dirty::{DirtyFlags, DirtySet},
        id::{Arena, NodeId},
        image::ImageContent,
        signal::SignalId,
    },
    paint::{
        canvas::Canvas,
        extent::PaintedExtent,
        geometry::GeometryId,
        scene::LaneId,
        style::{Clip, PaintSpec, Style, StylePatch},
    },
    resources::ResourceId,
    text::{ShapedLine, TextContent},
    text_edit::{EditableSpec, EditableText},
};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LayoutBox {
    pub bounds: LogicalRect,
    pub border: LogicalEdges,
}

#[derive(Clone, Debug, PartialEq)]
pub enum NodeKind {
    Box,
    Canvas(Arc<Canvas>),
    Geometry(GeometryId),
    Text(Box<TextContent>),
    Image(Box<ImageContent>),
}

impl NodeKind {
    #[must_use]
    pub(crate) fn is_leaf(&self) -> bool {
        matches!(
            self,
            Self::Canvas(_) | Self::Geometry(_) | Self::Text(_) | Self::Image(_)
        )
    }

    #[must_use]
    pub(crate) fn is_geometry(&self) -> bool {
        matches!(self, Self::Geometry(_))
    }
    #[must_use]
    pub(crate) fn text(&self) -> Option<&TextContent> {
        match self {
            Self::Text(content) => Some(content),
            _ => None,
        }
    }

    #[must_use]
    pub(crate) fn image(&self) -> Option<&ImageContent> {
        match self {
            Self::Image(content) => Some(content),
            _ => None,
        }
    }
}

pub(crate) struct NodeSpec {
    kind: NodeKind,
    style: Style,
    bind_style: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>>,
    measure: Option<MeasureFn>,
    hit_shape: Option<HitShape>,
    layout_override: Option<LayoutOverride>,
    bind_text: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>>,
    editable: Option<EditableSpec>,
    paint: Option<Box<PaintSpec>>,
    debug_id: Option<SmolStr>,
    action: Option<Action>,
    drag: Option<Action>,
    focus_ring: Option<crate::paint::style::FocusRing>,
    a11y: Option<crate::a11y::A11yOverride>,
    bind_a11y: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> crate::a11y::A11yPatch>>,
    tab_stop: bool,
}

static EMPTY_STYLE_PATCH: StylePatch = StylePatch {
    background: None,
    foreground: None,
    border: None,
    opacity: None,
    visible: None,
    interactive: None,
    cursor: None,
    affects_layout: false,
};

pub struct Node {
    id: NodeId,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    kind: NodeKind,
    canvas_generation: u32,
    style: Box<Style>,
    style_patch: Option<Box<StylePatch>>,
    bind_style: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>>,
    measure: Option<MeasureFn>,
    hit_shape: Option<Box<HitShape>>,
    layout_override: Option<LayoutOverride>,
    tab_stop: bool,
    paint: Option<Box<PaintSpec>>,
    content_offset: LogicalPoint,
    text_bind: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>>,
    text_measured_size: LogicalSize,
    text_content_width: LogicalPx,
    layout: LayoutBox,
    local: LogicalRect,
    local_border: LogicalEdges,
    laid_out_epoch: u32,
    damage: LogicalRect,
    lanes: SmallVec<[LaneId; 2]>,
    deps: Option<Box<SmallVec<[SignalId; 8]>>>,
    dirty: DirtyFlags,
    interaction: Option<Box<NodeInteraction>>,
    lane_offset: LogicalPoint,
    paint_opacity: f32,
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("id", &self.id)
            .field("parent", &self.parent)
            .field("children", &self.children)
            .field("kind", &self.kind)
            .field("style", &self.style)
            .field("style_patch", &self.style_patch())
            .field("bind_style", &self.bind_style.as_ref().map(|_| "<closure>"))
            .field("measure", &self.measure.as_ref().map(|_| "<closure>"))
            .field("hit_shape", &self.hit_shape)
            .field("layout_override", &self.layout_override)
            .field("paint", &self.paint)
            .field("content_offset", &self.content_offset)
            .field("text_bind", &self.text_bind.as_ref().map(|_| "<closure>"))
            .field("text_measured_size", &self.text_measured_size)
            .field("text_content_width", &self.text_content_width)
            .field("layout", &self.layout)
            .field("local", &self.local)
            .field("local_border", &self.local_border)
            .field("laid_out_epoch", &self.laid_out_epoch)
            .field("damage", &self.damage)
            .field("lanes", &self.lanes)
            .field("deps", &self.deps())
            .field("dirty", &self.dirty)
            .field("interaction", &self.interaction)
            .field("lane_offset", &self.lane_offset)
            .field("paint_opacity", &self.paint_opacity)
            .finish()
    }
}

impl Node {
    #[must_use]
    pub fn kind(&self) -> &NodeKind {
        &self.kind
    }

    #[must_use]
    pub(crate) fn canvas(&self) -> Option<&Arc<Canvas>> {
        match &self.kind {
            NodeKind::Canvas(canvas) => Some(canvas),
            _ => None,
        }
    }

    #[must_use]
    pub(crate) fn canvas_generation(&self) -> u32 {
        self.canvas_generation
    }

    #[must_use]
    pub fn style(&self) -> &Style {
        &self.style
    }

    #[must_use]
    pub(crate) fn tab_stop(&self) -> bool {
        self.tab_stop
    }

    #[must_use]
    pub(crate) fn press_focus_target(&self) -> Option<NodeId> {
        self.interaction
            .as_deref()
            .and_then(|slot| slot.press_focus_target)
    }

    #[must_use]
    pub fn style_patch(&self) -> &StylePatch {
        self.style_patch.as_deref().unwrap_or(&EMPTY_STYLE_PATCH)
    }

    #[must_use]
    pub fn dirty(&self) -> DirtyFlags {
        self.dirty
    }

    #[must_use]
    pub fn id(&self) -> NodeId {
        self.id
    }

    pub(crate) fn retag(&mut self, id: NodeId) {
        self.id = id;
    }

    #[must_use]
    pub fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    #[must_use]
    pub fn children(&self) -> &[NodeId] {
        &self.children
    }

    #[must_use]
    pub fn damage(&self) -> LogicalRect {
        self.damage
    }

    #[must_use]
    pub fn layout(&self) -> LayoutBox {
        self.layout
    }

    #[must_use]
    pub fn lane(&self) -> Option<LaneId> {
        self.lanes.first().copied()
    }

    #[must_use]
    pub fn lanes(&self) -> &[LaneId] {
        &self.lanes
    }

    #[must_use]
    pub fn clip(&self) -> Clip {
        self.style.clip
    }

    #[must_use]
    pub fn hit_shape(&self) -> Option<&HitShape> {
        self.hit_shape.as_deref()
    }

    #[must_use]
    pub fn layout_override(&self) -> Option<LayoutOverride> {
        self.layout_override
    }

    #[must_use]
    pub(crate) fn has_bindings(&self) -> bool {
        self.bind_style.is_some() || self.text_bind.is_some()
    }

    #[must_use]
    pub(crate) fn paint_spec(&self) -> Option<&PaintSpec> {
        self.paint.as_deref()
    }

    #[must_use]
    pub(crate) fn deps(&self) -> Option<&[SignalId]> {
        self.deps.as_deref().map(SmallVec::as_slice)
    }

    fn accumulate_damage(&mut self) {
        self.damage = self.damage.union(self.layout.bounds);
    }
}

pub(crate) struct PaintNodeState {
    pub hidden: bool,
    pub reusable: bool,
    pub lane: Option<LaneId>,

    pub geometry: Option<GeometryId>,
    pub children: usize,
    pub clip_bounds: Option<LogicalRect>,
    pub scroll: Option<LogicalPoint>,
    pub needs_screen_bake: bool,
    pub opacity: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BakedGeometryChunk {
    pub revision: u64,
    pub bounds: LogicalRect,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaintCounters {
    pub visited_nodes: usize,
    pub order_rebuilds: u64,
    pub full_frames: u64,
    pub incremental_frames: u64,
}

pub struct NodeTree {
    arena: Arena<Node>,
    root: NodeId,
    window: Option<crate::window::WindowId>,
    hit_test_skips: std::cell::Cell<u64>,
    layout_cache: ahash::HashMap<NodeId, taffy::Cache>,
    text_slots: Vec<Option<TextSlot>>,
    editable_slots: Vec<Option<Box<EditableText>>>,
    debug_ids: Vec<Option<SmolStr>>,
    clip_keys: Vec<Option<(u8, Option<DeviceRect>)>>,
    baked_screen_bounds: Vec<Option<LogicalRect>>,
    painted_extents: Vec<Option<PaintedExtent>>,
    subtree_bounds: Vec<LogicalRect>,
    a11y_slots: Vec<Option<Box<crate::a11y::tree::NodeA11y>>>,
    images_need_refresh: bool,
    image_nodes: Vec<NodeId>,
    canvas_nodes: Vec<NodeId>,
    dirty_bind: DirtySet,
    dirty_layout: DirtySet,
    dirty_paint: DirtySet,
    dirty_a11y: DirtySet,
    dirty_settle: DirtySet,
    moved_nodes: DirtySet,
    scroll_containers: Vec<NodeId>,
    virtual_containers: Vec<NodeId>,
    overlay_containers: Vec<NodeId>,
    text_dirty: content::TextDirtyQueue,
    text_layout_dirty: content::TextDirtyQueue,
    removed_lanes: Vec<LaneId>,
    hoisted: ahash::HashMap<NodeId, SmallVec<[NodeId; 2]>>,
    layout_epoch: u32,
    layout_stats: LayoutStats,
    invalid_measure: Option<EngineError>,
    geometry_damage_escalation: bool,
    geometry_transforms: ahash::HashMap<NodeId, Transform>,
    geometry_chunks: ahash::HashMap<NodeId, SmallVec<[BakedGeometryChunk; 2]>>,
    geometry_bindings: ahash::HashMap<GeometryId, SmallVec<[NodeId; 1]>>,
    paint_marks: Vec<u32>,
    paint_epoch: u32,
    scroll_slot_of: Vec<u32>,
    structure_epoch: u32,
    painted_structure_epoch: u32,
    scroll_reg_epoch: u32,
    painted_scroll_reg_epoch: u32,
    force_full_paint: bool,
    paint_visited: usize,
    paint_order_rebuilds: u64,
    paint_full_frames: u64,
    paint_incremental_frames: u64,
    dirty_scratch: Vec<NodeId>,
    #[cfg(feature = "test-support")]
    scanned: RefCell<ahash::HashSet<NodeId>>,
    bound_nodes: usize,
}

impl Default for NodeTree {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeTree {
    #[must_use]
    pub fn new() -> Self {
        arena::new()
    }
    #[must_use]
    pub fn root(&self) -> NodeId {
        arena::root(self)
    }
    pub(crate) fn bind_window(&mut self, window: crate::window::WindowId) {
        arena::bind_window(self, window)
    }
    #[must_use]
    pub(crate) fn window(&self) -> Option<crate::window::WindowId> {
        self.window
    }
    #[must_use]
    pub(crate) fn text_dirty_pending(&self, node: NodeId) -> bool {
        content::text_dirty_pending(self, node)
    }
    #[must_use]
    pub(crate) fn window_tag(&self) -> u32 {
        arena::window_tag(self)
    }

    pub fn get(&self, node: NodeId) -> Result<&Node, EngineError> {
        arena::get(self, node)
    }
    pub(crate) fn insert_child(
        &mut self,
        parent: NodeId,
        spec: NodeSpec,
    ) -> Result<NodeId, EngineError> {
        arena::insert_child(self, parent, spec)
    }
    pub fn set_press_focus_target(
        &mut self,
        host: NodeId,
        target: NodeId,
    ) -> Result<(), EngineError> {
        arena::set_press_focus_target(self, host, target)
    }
    pub fn remove(
        &mut self,
        node: NodeId,
    ) -> Result<Vec<(NodeId, SmallVec<[SignalId; 8]>)>, EngineError> {
        arena::remove(self, node)
    }
    #[must_use]
    pub fn debug_id(&self, node: NodeId) -> Option<&str> {
        arena::debug_id(self, node)
    }
    pub fn append_child(&mut self, parent: NodeId, child: NodeId) -> Result<(), EngineError> {
        arena::append_child(self, parent, child)
    }
    #[must_use]
    pub fn len(&self) -> usize {
        arena::len(self)
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() <= 1
    }

    pub(crate) fn for_each_dfs(&self, visit: impl FnMut(NodeId)) {
        traverse::for_each_dfs(self, visit)
    }
    #[must_use]
    pub(crate) fn dfs_ids(&self) -> Vec<NodeId> {
        traverse::dfs_ids(self)
    }
    pub(crate) fn for_each_subtree(&self, root: NodeId, visit: impl FnMut(NodeId)) {
        traverse::for_each_subtree(self, root, visit)
    }
    #[must_use]
    pub(crate) fn is_visible_locally(&self, node: NodeId) -> bool {
        traverse::is_visible_locally(self, node)
    }
    #[must_use]
    pub(crate) fn is_effectively_visible(&self, node: NodeId) -> bool {
        traverse::is_effectively_visible(self, node)
    }
    #[must_use]
    pub(crate) fn is_effectively_interactive(&self, node: NodeId) -> bool {
        content::is_effectively_interactive(self, node)
    }
    pub(crate) fn note_hit_test_skip(&self) {
        content::note_hit_test_skip(self)
    }
    #[must_use]
    pub fn hit_test_skips(&self) -> u64 {
        content::hit_test_skips(self)
    }
    pub(crate) fn is_ancestor(&self, ancestor: NodeId, node: NodeId) -> bool {
        traverse::is_ancestor(self, ancestor, node)
    }
    pub(crate) fn hoisted_clear(&mut self, node: NodeId) {
        traverse::hoisted_clear(self, node)
    }
    pub(crate) fn hoisted_push(&mut self, node: NodeId, hoisted: &[NodeId]) {
        traverse::hoisted_push(self, node, hoisted)
    }
    pub(crate) fn note_scroll_container(&mut self, node: NodeId) {
        traverse::note_scroll_container(self, node)
    }
    pub(crate) fn note_virtual_container(&mut self, node: NodeId) {
        traverse::note_virtual_container(self, node)
    }
    pub(crate) fn note_overlay_container(&mut self, node: NodeId) {
        traverse::note_overlay_container(self, node)
    }
    #[must_use]
    pub(crate) fn scroll_containers(&self) -> &[NodeId] {
        &self.scroll_containers
    }
    #[must_use]
    pub(crate) fn virtual_containers(&self) -> &[NodeId] {
        &self.virtual_containers
    }
    #[must_use]
    pub(crate) fn overlay_containers(&self) -> &[NodeId] {
        &self.overlay_containers
    }

    pub(crate) fn compute_layout(
        &mut self,
        available: taffy::Size<Option<f32>>,
    ) -> Result<LayoutReport, EngineError> {
        layout::compute_layout(self, available)
    }
    pub(crate) fn invalidate_layout(&mut self, node: NodeId) -> bool {
        layout::invalidate_layout(self, node)
    }
    pub fn invalidate_measure(&mut self, node: NodeId) -> Result<(), EngineError> {
        layout::invalidate_measure(self, node)
    }
    pub fn set_style(&mut self, node: NodeId, style: Style) -> Result<(), EngineError> {
        layout::set_style(self, node, style)
    }
    pub fn set_layout_override(
        &mut self,
        node: NodeId,
        layout_override: Option<LayoutOverride>,
    ) -> Result<(), EngineError> {
        layout::set_layout_override(self, node, layout_override)
    }
    pub(crate) fn set_virtual_row(
        &mut self,
        node: NodeId,
        top: LogicalPx,
        height: LogicalPx,
    ) -> bool {
        layout::set_virtual_row(self, node, top, height)
    }
    pub(crate) fn set_absolute_inset(&mut self, node: NodeId, origin: LogicalPoint) -> bool {
        layout::set_absolute_inset(self, node, origin)
    }
    pub(crate) fn taffy_style(&self, node: NodeId) -> &taffy::Style<SmolStr> {
        layout::taffy_style(self, node)
    }
    pub(crate) fn taffy_children(&self, node: NodeId) -> &[NodeId] {
        layout::taffy_children(self, node)
    }
    pub(crate) fn taffy_cache(&mut self, node: NodeId) -> &mut taffy::Cache {
        layout::taffy_cache(self, node)
    }
    pub(crate) fn set_local_layout(&mut self, node: NodeId, layout: &taffy::Layout) {
        layout::set_local_layout(self, node, layout)
    }
    pub(crate) fn measure_of(&self, node: NodeId) -> Option<&MeasureFn> {
        layout::measure_of(self, node)
    }
    pub(crate) fn layout_override_of(&self, node: NodeId) -> Option<LayoutOverride> {
        layout::layout_override_of(self, node)
    }
    pub(crate) fn is_out_of_flow(&self, node: NodeId) -> bool {
        layout::is_out_of_flow(self, node)
    }
    pub(crate) fn begin_layout(&mut self) {
        layout::begin_layout(self)
    }
    pub(crate) fn take_report(&mut self) -> LayoutReport {
        layout::take_report(self)
    }
    pub(crate) fn stats_mut(&mut self) -> &mut LayoutStats {
        layout::stats_mut(self)
    }
    pub(crate) fn note_invalid_measure(&mut self, node: NodeId, axis: Axis, value: LogicalPx) {
        layout::note_invalid_measure(self, node, axis, value)
    }
    pub(crate) fn take_invalid_measure(&mut self) -> Option<EngineError> {
        layout::take_invalid_measure(self)
    }
    pub(crate) fn absolutize(&mut self) {
        layout::absolutize(self)
    }
    fn place_subtree(
        &mut self,
        node: NodeId,
        reference: LogicalPoint,
        epoch: u32,
        force: bool,
        parent_hidden: bool,
    ) {
        layout::place_subtree(self, node, reference, epoch, force, parent_hidden)
    }
    #[cfg(test)]
    pub(crate) fn recompute_subtree_bounds(&mut self) {
        layout::recompute_subtree_bounds(self)
    }
    pub(crate) fn subtree_bounds(&self, node: NodeId) -> LogicalRect {
        layout::subtree_bounds(self, node)
    }
    fn invalidate_caches_upwards(&mut self, node: NodeId) {
        layout::invalidate_caches_upwards(self, node)
    }
    pub(crate) fn settle_frame(&mut self) {
        layout::settle_frame(self)
    }
    fn bubble_layout(&mut self, from: NodeId) {
        layout::bubble_layout(self, from)
    }
    #[must_use]
    pub fn hit_test(&self, point: LogicalPoint) -> Option<HitResult> {
        layout::hit_test(self, point)
    }
    #[cfg(test)]
    pub(crate) fn set_layout_for_test(&mut self, node: NodeId, layout: LayoutBox) -> bool {
        layout::set_layout_for_test(self, node, layout)
    }
    #[cfg(test)]
    pub(crate) fn layout_cache(&self, node: NodeId) -> Option<&taffy::Cache> {
        layout::layout_cache(self, node)
    }

    pub(crate) fn mark_bind(&mut self, node: NodeId) -> bool {
        dirty::mark_bind(self, node)
    }
    pub(crate) fn mark_paint(&mut self, node: NodeId) -> bool {
        dirty::mark_paint(self, node)
    }
    pub(crate) fn mark_paint_without_damage(&mut self, node: NodeId) -> bool {
        dirty::mark_paint_without_damage(self, node)
    }
    pub(crate) fn mark_layout(&mut self, node: NodeId) -> bool {
        dirty::mark_layout(self, node)
    }
    pub(crate) fn mark_a11y(&mut self, node: NodeId) -> bool {
        dirty::mark_a11y(self, node)
    }
    pub(crate) fn mark_structure(&mut self, node: NodeId) -> bool {
        dirty::mark_structure(self, node)
    }
    pub(crate) fn mark_subtree_paint(&mut self, node: NodeId) -> usize {
        dirty::mark_subtree_paint(self, node)
    }
    pub(crate) fn mark_subtree_a11y(&mut self, node: NodeId) -> usize {
        dirty::mark_subtree_a11y(self, node)
    }
    pub(crate) fn clear_bind(&mut self, node: NodeId) -> bool {
        dirty::clear_bind(self, node)
    }
    pub(crate) fn clear_paint(&mut self, node: NodeId) -> bool {
        dirty::clear_paint(self, node)
    }
    pub(crate) fn mark_moved_a11y(&mut self) -> usize {
        dirty::mark_moved_a11y(self)
    }
    pub(crate) fn clear_a11y(&mut self, node: NodeId) -> bool {
        dirty::clear_a11y(self, node)
    }
    #[must_use]
    pub(crate) fn a11y_dirty(&self) -> usize {
        dirty::a11y_dirty(self)
    }
    fn touch(&mut self, node: NodeId, flags: DirtyFlags) -> bool {
        dirty::touch(self, node, flags)
    }
    fn touch_layout_only(&mut self, node: NodeId) -> bool {
        dirty::touch_layout_only(self, node)
    }
    #[must_use]
    pub(crate) fn has_layout_work(&self) -> bool {
        !self.dirty_layout.is_empty()
    }
    #[must_use]
    pub(crate) fn has_paint_work(&self) -> bool {
        !self.dirty_paint.is_empty()
    }

    #[must_use]
    pub fn paint_counters(&self) -> PaintCounters {
        PaintCounters {
            visited_nodes: self.paint_visited,
            order_rebuilds: self.paint_order_rebuilds,
            full_frames: self.paint_full_frames,
            incremental_frames: self.paint_incremental_frames,
        }
    }

    pub(crate) fn take_force_full_paint(&mut self) -> bool {
        std::mem::take(&mut self.force_full_paint)
    }

    #[must_use]
    pub(crate) fn structure_changed(&self) -> bool {
        self.structure_epoch != self.painted_structure_epoch
    }

    #[must_use]
    pub(crate) fn scroll_registry_changed(&self) -> bool {
        self.scroll_reg_epoch != self.painted_scroll_reg_epoch
    }

    pub(crate) fn note_paint_frame(&mut self, full_path: bool, visited: usize) {
        self.paint_visited = visited;
        self.painted_structure_epoch = self.structure_epoch;
        if full_path {
            self.paint_full_frames = self.paint_full_frames.saturating_add(1);
            self.painted_scroll_reg_epoch = self.scroll_reg_epoch;
        } else {
            self.paint_incremental_frames = self.paint_incremental_frames.saturating_add(1);
        }
    }

    pub(crate) fn reset_paint_visited(&mut self) {
        self.paint_visited = 0;
    }

    pub(crate) fn note_order_rebuild(&mut self) {
        self.paint_order_rebuilds = self.paint_order_rebuilds.saturating_add(1);
    }

    pub(crate) fn take_paint_work(&mut self) -> Vec<NodeId> {
        let NodeTree {
            dirty_scratch,
            dirty_paint,
            ..
        } = self;
        let mut out = std::mem::take(dirty_scratch);
        dirty_paint.snapshot_into(&mut out);
        #[cfg(feature = "test-support")]
        for node in &out {
            self.note_scanned(*node);
        }
        out
    }

    pub(crate) fn mark_paint_closure(&mut self, dirty: &[NodeId]) {
        self.paint_epoch = self.paint_epoch.wrapping_add(1);
        if self.paint_epoch == 0 {
            self.paint_marks.fill(0);
            self.paint_epoch = 1;
        }
        let epoch = self.paint_epoch;
        for start in dirty.iter().copied() {
            let mut cursor = Some(start);
            while let Some(node) = cursor {
                let index = node.index() as usize;
                if self.paint_marks.len() <= index {
                    self.paint_marks.resize(index + 1, 0);
                }
                if self.paint_marks[index] == epoch {
                    break;
                }
                self.paint_marks[index] = epoch;
                cursor = self.arena.get(node).and_then(|target| target.parent);
            }
        }
    }

    #[must_use]
    pub(crate) fn paint_marked(&self, node: NodeId) -> bool {
        self.paint_epoch != 0
            && self
                .paint_marks
                .get(node.index() as usize)
                .is_some_and(|stamp| *stamp == self.paint_epoch)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn force_full_paint(&mut self) {
        self.force_full_paint = true;
    }

    pub(crate) fn begin_full_scroll_slots(&mut self) {
        self.scroll_slot_of.fill(0);
    }

    #[must_use]
    pub(crate) fn scroll_slot_cached(&self, node: NodeId) -> u32 {
        self.scroll_slot_of
            .get(node.index() as usize)
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn set_scroll_slot(&mut self, node: NodeId, slot: u32) {
        let index = node.index() as usize;
        if self.scroll_slot_of.len() <= index {
            self.scroll_slot_of.resize(index + 1, 0);
        }
        self.scroll_slot_of[index] = slot;
    }

    #[must_use]
    pub(crate) fn has_bind_work(&self) -> bool {
        !self.dirty_bind.is_empty()
    }

    pub(crate) fn take_dirty_bind(&mut self) -> Vec<NodeId> {
        dirty::take_dirty_bind(self)
    }
    pub(crate) fn take_settle(&mut self) -> Vec<NodeId> {
        dirty::take_settle(self)
    }
    pub(crate) fn take_a11y(&mut self) -> Vec<NodeId> {
        dirty::take_a11y(self)
    }
    pub(crate) fn take_moved(&mut self) -> Vec<NodeId> {
        dirty::take_moved(self)
    }
    pub(crate) fn recycle(&mut self, mut buffer: Vec<NodeId>) {
        buffer.clear();
        self.dirty_scratch = buffer;
    }
    #[must_use]
    pub(crate) fn has_bound_nodes(&self) -> bool {
        self.bound_nodes > 0
    }
    #[cfg(test)]
    pub(crate) fn clear_all_dirty_for_test(&mut self) {
        dirty::clear_all_dirty_for_test(self)
    }
    #[cfg(test)]
    pub(crate) fn set_dirty_for_test(&mut self, node: NodeId, flags: DirtyFlags) -> bool {
        dirty::set_dirty_for_test(self, node, flags)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn note_scanned(&self, node: NodeId) {
        self.scanned.borrow_mut().insert(node);
    }
    #[cfg(feature = "test-support")]
    #[must_use]
    pub(crate) fn scanned_nodes(&self) -> usize {
        self.scanned.borrow().len()
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn reset_scanned_nodes(&self) {
        self.scanned.borrow_mut().clear();
    }

    pub(crate) fn text_content(&self, node: NodeId) -> Option<&TextContent> {
        content::text_content(self, node)
    }

    #[must_use]
    pub(crate) fn canvas(&self, node: NodeId) -> Option<&Arc<Canvas>> {
        content::canvas(self, node)
    }

    #[must_use]
    pub(crate) fn canvas_generation(&self, node: NodeId) -> Option<u32> {
        self.arena.get(node).map(Node::canvas_generation)
    }
    pub(crate) fn set_canvas(
        &mut self,
        node: NodeId,
        canvas: crate::paint::canvas::Canvas,
    ) -> bool {
        content::set_canvas(self, node, canvas)
    }

    pub(crate) fn take_images_need_refresh(&mut self) -> bool {
        std::mem::take(&mut self.images_need_refresh)
    }
    pub(crate) fn image_nodes(&self) -> &[NodeId] {
        &self.image_nodes
    }

    pub(crate) fn canvas_nodes(&self) -> &[NodeId] {
        &self.canvas_nodes
    }
    pub(crate) fn refresh_image_intrinsics(
        &mut self,
        resources: &crate::resources::ResourceCache,
        dpr: crate::math::DevicePixelRatio,
    ) -> usize {
        image::refresh_intrinsics(self, resources, dpr)
    }
    pub(crate) fn prepare_image_variants(
        &mut self,
        resources: &mut crate::resources::ResourceCache,
        dpr: crate::math::DevicePixelRatio,
    ) -> image::PrepareImageVariants {
        image::prepare_variants(self, resources, dpr)
    }
    pub(crate) fn mark_bitmap_images_paint(
        &mut self,
        resources: &crate::resources::ResourceCache,
    ) -> usize {
        image::mark_bitmap_paint(self, resources)
    }
    pub(crate) fn resolve_pending_images(
        &mut self,
        resolved: &ahash::HashMap<
            crate::resources::PendingResource,
            crate::resources::ResourceImage,
        >,
        dpr: crate::math::DevicePixelRatio,
    ) -> usize {
        image::resolve_pending(self, resolved, dpr)
    }
    pub(crate) fn invalidate_images_with_resource(&mut self, resource: ResourceId) -> usize {
        image::invalidate_with_resource(self, resource)
    }
    pub(crate) fn invalidate_dead_images(
        &mut self,
        resources: &crate::resources::ResourceCache,
    ) -> usize {
        image::invalidate_dead(self, resources)
    }
    pub(crate) fn set_image_resource(&mut self, node: NodeId, resource: ResourceId) -> bool {
        image::set_resource(self, node, resource)
    }

    pub(crate) fn set_text_content(&mut self, node: NodeId, content: TextContent) -> bool {
        content::set_text_content(self, node, content)
    }
    fn mark_text_dirty(&mut self, node: NodeId) {
        content::mark_text_dirty(self, node)
    }
    pub(crate) fn take_text_dirty(&mut self, out: &mut Vec<NodeId>) {
        content::take_text_dirty(self, out)
    }
    #[cfg(test)]
    pub(crate) fn text_dirty_len(&self) -> usize {
        content::text_dirty_len(self)
    }
    pub(crate) fn mark_text_layout_dirty(&mut self, node: NodeId) {
        content::mark_text_layout_dirty(self, node)
    }
    pub(crate) fn take_text_layout_dirty(&mut self, out: &mut Vec<NodeId>) {
        content::take_text_layout_dirty(self, out)
    }
    #[cfg(test)]
    pub(crate) fn text_layout_dirty_len(&self) -> usize {
        content::text_layout_dirty_len(self)
    }
    pub(crate) fn invalidate_text_lines(&mut self) -> usize {
        content::invalidate_text_lines(self)
    }
    pub(crate) fn mark_canvas_nodes_paint(&mut self) -> usize {
        content::mark_canvas_nodes_paint(self)
    }
    pub(crate) fn set_text_shaped(
        &mut self,
        node: NodeId,
        shaped: Rc<crate::text::shape::ShapedText>,
        key: crate::text::cache::ShapeKey,
        dpi_epoch: u32,
        measured: LogicalSize,
        size_changed: bool,
        editable: bool,
    ) -> bool {
        content::set_text_shaped(
            self,
            node,
            shaped,
            key,
            dpi_epoch,
            measured,
            size_changed,
            editable,
        )
    }
    #[cfg(test)]
    pub(crate) fn install_text_line_for_test(
        &mut self,
        node: NodeId,
        content: &crate::text::TextContent,
        line: Rc<ShapedLine>,
        width: LogicalPx,
    ) -> bool {
        content::install_text_line_for_test(self, node, content, line, width)
    }
    pub(crate) fn set_text_line(
        &mut self,
        node: NodeId,
        line: Rc<ShapedLine>,
        width: LogicalPx,
        generation: u32,
    ) -> bool {
        content::set_text_line(self, node, line, width, generation)
    }
    pub(crate) fn text_content_width(&self, node: NodeId) -> LogicalPx {
        content::text_content_width(self, node)
    }
    pub(crate) fn text_line(&self, node: NodeId) -> Option<&Rc<ShapedLine>> {
        content::text_line(self, node)
    }
    pub(crate) fn text_measured_size(&self, node: NodeId) -> Option<LogicalSize> {
        content::text_measured_size(self, node)
    }
    pub(crate) fn text_slot(&self, node: NodeId) -> Option<&TextSlot> {
        content::text_slot(self, node)
    }
    #[must_use]
    pub(crate) fn editable(&self, node: NodeId) -> Option<&EditableText> {
        content::editable(self, node)
    }
    pub(crate) fn editable_mut(&mut self, node: NodeId) -> Option<&mut EditableText> {
        content::editable_mut(self, node)
    }
    #[must_use]
    pub(crate) fn text_layout_width(&self, node: NodeId) -> Option<LogicalPx> {
        let slot = self.text_slot(node)?;
        Some(content::expected_layout_width(self, slot.shaped(), node))
    }
    #[must_use]
    pub(crate) fn text_laid_width(&self, node: NodeId) -> Option<LogicalPx> {
        content::laid_width(self, node)
    }
    #[cfg(test)]
    pub(crate) fn editable_slot_for_test(&self, node: NodeId) -> Option<&EditableText> {
        self.editable_slots
            .get(node.index() as usize)
            .and_then(|slot| slot.as_deref())
    }
    pub fn text_selection_rects(
        &self,
        node: NodeId,
        anchor: usize,
        focus: usize,
    ) -> Result<Vec<LogicalRect>, EngineError> {
        content::text_selection_rects(self, node, anchor, focus)
    }
    pub fn text_caret_rect(
        &self,
        node: NodeId,
        byte_index: usize,
    ) -> Result<LogicalRect, EngineError> {
        content::text_caret_rect(self, node, byte_index)
    }
    pub(crate) fn text_byte_index_at(
        &self,
        node: NodeId,
        local_point: LogicalPoint,
    ) -> Result<usize, EngineError> {
        content::text_byte_index_at(self, node, local_point)
    }
    pub(crate) fn invalidate_text_line(&mut self, node: NodeId) {
        content::invalidate_text_line(self, node)
    }

    pub(crate) fn request_text_line_relayout(
        &mut self,
        node: NodeId,
        atlas_generations: (u32, u32),
    ) -> bool {
        content::request_text_line_relayout(self, node, atlas_generations)
    }

    pub(crate) fn invalidate_text_layout(&mut self, node: NodeId) -> bool {
        content::invalidate_text_layout(self, node)
    }
    pub(crate) fn mark_all_text_dirty(&mut self) {
        content::mark_all_text_dirty(self)
    }
    pub(crate) fn take_bind_text(
        &mut self,
        node: NodeId,
    ) -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>> {
        content::take_bind_text(self, node)
    }
    pub(crate) fn put_bind_text(
        &mut self,
        node: NodeId,
        bind_text: Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>,
    ) -> bool {
        content::put_bind_text(self, node, bind_text)
    }
    pub(crate) fn take_geometry_damage_escalation(&mut self) -> bool {
        content::take_geometry_damage_escalation(self)
    }
    #[must_use]
    pub(crate) fn geometry_transform(&self, node: NodeId) -> Transform {
        content::geometry_transform(self, node)
    }
    #[must_use]
    pub(crate) fn has_geometry_transforms(&self) -> bool {
        content::has_geometry_transforms(self)
    }
    pub(crate) fn set_geometry_transform(
        &mut self,
        node: NodeId,
        transform: Transform,
        extra_damage: LogicalRect,
    ) -> Result<(), EngineError> {
        content::set_geometry_transform(self, node, transform, extra_damage)
    }
    pub(crate) fn content_offset(&self, node: NodeId) -> LogicalPoint {
        content::content_offset(self, node)
    }
    pub(crate) fn lane_of(&self, node: NodeId) -> Option<LaneId> {
        content::lane_of(self, node)
    }
    pub(crate) fn lane_count(&self, node: NodeId) -> usize {
        content::lane_count(self, node)
    }
    pub(crate) fn lane_at(&self, node: NodeId, index: usize) -> Option<LaneId> {
        content::lane_at(self, node, index)
    }
    pub(crate) fn set_lane(&mut self, node: NodeId, lane: LaneId) {
        content::set_lane(self, node, lane)
    }
    pub(crate) fn push_lane(&mut self, node: NodeId, lane: LaneId) {
        content::push_lane(self, node, lane)
    }
    pub(crate) fn baked_geometry_chunks(
        &self,
        node: NodeId,
    ) -> Option<&SmallVec<[BakedGeometryChunk; 2]>> {
        content::baked_geometry_chunks(self, node)
    }
    pub(crate) fn resize_baked_geometry_chunks(&mut self, node: NodeId, len: usize) {
        content::resize_baked_geometry_chunks(self, node, len)
    }
    pub(crate) fn set_baked_geometry_chunk(
        &mut self,
        node: NodeId,
        chunk: usize,
        baked: BakedGeometryChunk,
    ) {
        content::set_baked_geometry_chunk(self, node, chunk, baked)
    }

    pub(crate) fn truncate_lanes(&mut self, node: NodeId, len: usize) {
        content::truncate_lanes(self, node, len)
    }
    pub(crate) fn mark_geometry_updated(&mut self, geometry: GeometryId) -> usize {
        content::mark_geometry_updated(self, geometry)
    }
    pub(crate) fn bind_geometry_node(&mut self, geometry: GeometryId, node: NodeId) {
        content::bind_geometry_node(self, geometry, node)
    }
    pub(crate) fn lane_offset(&self, node: NodeId) -> LogicalPoint {
        content::lane_offset(self, node)
    }
    pub(crate) fn set_lane_offset(&mut self, node: NodeId, offset: LogicalPoint) {
        content::set_lane_offset(self, node, offset)
    }
    pub(crate) fn set_baked_screen_bounds(&mut self, node: NodeId, bounds: LogicalRect) {
        content::set_baked_screen_bounds(self, node, bounds)
    }
    #[must_use]
    pub(crate) fn painted_extent(&self, node: NodeId) -> Option<PaintedExtent> {
        content::painted_extent(self, node)
    }
    pub(crate) fn set_painted_extent(&mut self, node: NodeId, extent: Option<PaintedExtent>) {
        content::set_painted_extent(self, node, extent)
    }
    pub(crate) fn clip_key(&self, node: NodeId) -> Option<(u8, Option<DeviceRect>)> {
        content::clip_key(self, node)
    }
    pub(crate) fn set_paint_opacity(&mut self, node: NodeId, opacity: f32) {
        content::set_paint_opacity(self, node, opacity)
    }
    pub(crate) fn paint_state(
        &self,
        node: NodeId,
        screen_offset: LogicalPoint,
        bake_offset: LogicalPoint,
        clip_key: (u8, Option<DeviceRect>),
        inherited_opacity: f32,
    ) -> Option<PaintNodeState> {
        content::paint_state(
            self,
            node,
            screen_offset,
            bake_offset,
            clip_key,
            inherited_opacity,
        )
    }
    pub(crate) fn set_clip_key(&mut self, node: NodeId, key: (u8, Option<DeviceRect>)) {
        content::set_clip_key(self, node, key)
    }
    pub(crate) fn clear_clip_key(&mut self, node: NodeId) {
        content::clear_clip_key(self, node)
    }
    pub(crate) fn set_reusable(&mut self, node: NodeId) {
        content::set_reusable(self, node)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn force_repaint_all(&mut self) {
        content::force_repaint_all(self)
    }
    pub(crate) fn clear_lane(&mut self, node: NodeId) {
        content::clear_lane(self, node)
    }
    pub(crate) fn drain_removed_lanes(&mut self) -> std::vec::Drain<'_, LaneId> {
        content::drain_removed_lanes(self)
    }
    pub(crate) fn a11y_of(&self, node: NodeId) -> Option<&crate::a11y::tree::NodeA11y> {
        content::a11y_of(self, node)
    }
    pub(crate) fn take_bind_a11y(
        &mut self,
        node: NodeId,
    ) -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> crate::a11y::A11yPatch>> {
        content::take_bind_a11y(self, node)
    }
    pub(crate) fn put_bind_a11y(
        &mut self,
        node: NodeId,
        bind: Box<dyn Fn(&mut ReadCtx<'_>) -> crate::a11y::A11yPatch>,
    ) -> bool {
        content::put_bind_a11y(self, node, bind)
    }
    pub(crate) fn set_a11y_patch(&mut self, node: NodeId, patch: crate::a11y::A11yPatch) -> bool {
        content::set_a11y_patch(self, node, patch)
    }
    pub(crate) fn has_a11y_bind(&self, node: NodeId) -> bool {
        content::has_a11y_bind(self, node)
    }
    pub(crate) fn take_bind_style(
        &mut self,
        node: NodeId,
    ) -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>> {
        content::take_bind_style(self, node)
    }
    pub(crate) fn put_bind_style(
        &mut self,
        node: NodeId,
        bind_style: Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>,
    ) -> bool {
        content::put_bind_style(self, node, bind_style)
    }
    pub(crate) fn deps(&self, node: NodeId) -> Option<&[SignalId]> {
        self.arena.get(node).and_then(Node::deps)
    }
    pub(crate) fn set_deps(&mut self, node: NodeId, deps: SmallVec<[SignalId; 8]>) -> bool {
        content::set_deps(self, node, deps)
    }
    pub(crate) fn set_style_patch(&mut self, node: NodeId, patch: StylePatch) -> bool {
        content::set_style_patch(self, node, patch)
    }
    pub(crate) fn has_visual_bind(&self, node: NodeId) -> bool {
        content::has_visual_bind(self, node)
    }
    #[must_use]
    pub(crate) fn interaction(&self, node: NodeId) -> Option<&NodeInteraction> {
        content::interaction(self, node)
    }
    pub(crate) fn ensure_interaction(&mut self, node: NodeId) -> Option<&mut NodeInteraction> {
        content::ensure_interaction(self, node)
    }
    #[must_use]
    pub(crate) fn is_hittable(&self, node: NodeId) -> bool {
        content::is_hittable(self, node)
    }
    #[must_use]
    pub(crate) fn focus_ring(&self, node: NodeId) -> Option<crate::paint::style::FocusRing> {
        content::focus_ring(self, node)
    }
    #[must_use]
    pub(crate) fn is_explicitly_interactive(&self, node: NodeId) -> bool {
        content::is_explicitly_interactive(self, node)
    }
    #[must_use]
    pub(crate) fn scroll_slot(&self, node: NodeId) -> Option<ScrollSlot> {
        content::scroll_slot(self, node)
    }
    pub(crate) fn scroll_slot_mut(&mut self, node: NodeId) -> Option<&mut ScrollSlot> {
        content::scroll_slot_mut(self, node)
    }
    #[must_use]
    pub(crate) fn overlay_slot(&self, node: NodeId) -> Option<OverlaySlot> {
        content::overlay_slot(self, node)
    }
    #[must_use]
    pub(crate) fn virtual_slot(&self, node: NodeId) -> Option<&VirtualSlot> {
        content::virtual_slot(self, node)
    }
    pub(crate) fn virtual_slot_mut(&mut self, node: NodeId) -> Option<&mut VirtualSlot> {
        content::virtual_slot_mut(self, node)
    }
    #[cfg(test)]
    pub(crate) fn set_style_for_test(&mut self, node: NodeId, style: Style) -> bool {
        content::set_style_for_test(self, node, style)
    }
    #[cfg(test)]
    pub(crate) fn set_style_patch_for_test(&mut self, node: NodeId, patch: StylePatch) -> bool {
        content::set_style_patch_for_test(self, node, patch)
    }
    #[cfg(test)]
    pub(crate) fn edit_style_for_test(&mut self, node: NodeId, edit: impl FnOnce(&mut Style)) {
        content::edit_style_for_test(self, node, edit)
    }
}

mod a11y;
mod focus;
mod frame;
mod frame_budget;
mod paint;
mod record;
mod scale;
mod surface;
#[cfg(test)]
mod tests;

use std::{
    cell::Cell,
    collections::VecDeque,
    rc::Rc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use smol_str::SmolStr;

use self::{
    frame::{FrameScheduler, FrameState},
    surface::covering_surface,
};
#[cfg(feature = "test-support")]
use crate::paint::instance::InstanceBuffer;
#[cfg(any(target_os = "linux", test))]
use crate::render::present_evidence::clamp_presented_at;
#[cfg(feature = "test-support")]
use crate::test_support::record::{FrameRecord, FrameStats};
use crate::{
    error::EngineError,
    interaction::{FocusScope, PointerState},
    layout::LayoutReport,
    math::{PhysicalRect, Point, Rect, Scale, Size},
    node::{id::NodeId, tree::NodeTree},
    paint::{
        atlas::{AtlasKind, MAX_IMAGE_PAGES},
        color::{Color, ColorResolver},
        pass::PaintReport,
        scene::Scene,
    },
    render::{
        backend::Renderer, compose::WindowComposer, compositor::Plan, damage::DamageAccumulator,
        delta::SceneDelta, timer::FrameTimings,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WindowId(u32);

impl WindowId {
    pub(crate) fn from_raw(value: u32) -> Self {
        debug_assert!(value != 0, "WindowId 0 是非法句柄");
        Self(value)
    }

    #[must_use]
    pub const fn to_bits(self) -> u32 {
        self.0
    }

    #[cfg(test)]
    pub(crate) const fn for_test(value: u32) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Decorations {
    #[default]
    Native,
    None,
}

pub(crate) const MAX_WINDOW_ICON_DIM: u32 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowControl {
    Minimize,
    ToggleMaximize,
    ToggleFullscreen,
    BeginMove,
    BeginResize(WindowResizeEdge),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowState {
    pub client_decoration: bool,
    pub maximized: bool,
    pub fullscreen: bool,
    pub can_maximize: bool,
    pub can_minimize: bool,
    pub can_fullscreen: bool,
}

impl WindowState {
    #[must_use]
    pub(crate) const fn for_decorations(decorations: Decorations) -> Self {
        Self {
            client_decoration: matches!(decorations, Decorations::None)
                && !cfg!(target_os = "macos"),
            maximized: false,
            fullscreen: false,
            can_maximize: true,
            can_minimize: true,
            can_fullscreen: !cfg!(target_os = "windows"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowResizeEdge {
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ImeContentType {
    #[default]
    Text,
    Password,
    Pin,
    Number,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WindowOptions {
    pub title: SmolStr,
    pub size: Size,
    pub position: Option<Point>,
    pub min_size: Option<Size>,
    pub max_size: Option<Size>,
    pub owner: Option<WindowId>,
    pub modal: bool,
    pub always_on_top: bool,
    pub decorations: Decorations,
    pub icon: Option<Bytes>,
    pub scale: Scale,
    pub background: Color,
}

impl Default for WindowOptions {
    fn default() -> Self {
        Self {
            title: SmolStr::new_static(""),
            size: Size::new(800.0, 600.0),
            position: None,
            min_size: None,
            max_size: None,
            owner: None,
            modal: false,
            always_on_top: false,
            decorations: Decorations::Native,
            icon: None,
            scale: Scale::ONE,
            background: Color::BLACK,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameIndex(u32);

impl FrameIndex {
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) const fn from_raw(value: u32) -> Self {
        debug_assert!(value >= 1, "FrameIndex 0 从未提交");
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FramePresentation {
    pub index: FrameIndex,
    pub published_at: Instant,
    pub completed_at: Instant,
    pub presented_at: Option<Instant>,
    pub latency: Duration,
}

impl FramePresentation {
    #[must_use]
    pub fn publish_to_presented(&self) -> Option<Duration> {
        self.presented_at
            .map(|presented| presented.saturating_duration_since(self.published_at))
    }

    #[must_use]
    pub fn completion_to_presented(&self) -> Option<Duration> {
        self.presented_at
            .map(|presented| presented.saturating_duration_since(self.completed_at))
    }
}

const FRAME_PRESENTATION_HISTORY: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameOutcome {
    Idle,
    Committed(FrameIndex),
}

pub(crate) struct FrameTicket {
    state: Rc<Cell<FrameState>>,
    consumed: bool,
}

impl Drop for FrameTicket {
    fn drop(&mut self) {
        if self.consumed {
            return;
        }
        if self.state.get() == FrameState::Draining {
            self.state.set(FrameState::Idle);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ImeCursorRequest {
    node: NodeId,
    byte_index: usize,
}

pub struct Window {
    id: WindowId,
    options: WindowOptions,
    tree: NodeTree,
    scene: Scene,
    scale: Scale,
    surface: (u32, u32),
    surface_from_platform: bool,
    #[cfg(target_os = "linux")]
    buffer_scale: i32,
    scale_epoch: u32,
    frame_budget: frame_budget::FrameBudget,
    scheduler: FrameScheduler,
    composer: Option<WindowComposer>,
    pending_delta: Option<SceneDelta>,
    pending_payload: Option<crate::render::compose::ComposedFrame>,
    delta_builder: crate::render::delta::DeltaBuilder,
    scroll_slots: Vec<(crate::math::Point, PhysicalRect)>,
    scroll_slot_nodes: Vec<crate::node::id::NodeId>,
    resync: crate::render::frame::ResyncGate,
    compositor_ticket: Option<crate::render::compositor_thread::CompositorTicket>,
    damage: DamageAccumulator,
    resolver: ColorResolver,
    renderer: Option<Renderer>,
    presentation: VecDeque<FramePresentation>,
    glyph_page_received: Option<u32>,
    image_page_received: Option<crate::render::AtlasArraySpec>,
    image_layers_received: [bool; MAX_IMAGE_PAGES as usize],
    image_drawable_layers: u32,
    #[cfg(test)]
    atlas_participating_override: Option<bool>,
    #[cfg(all(test, windows))]
    atlas_upload_failure_from: Option<u16>,
    glyph_atlas_rects: crate::render::DirtyRects,
    image_atlas_rects: [crate::render::DirtyRects; MAX_IMAGE_PAGES as usize],
    atlas_deferrals: u8,
    stale_canvas_followup_generation: Option<u32>,
    timings: FrameTimings,
    paint_order: Vec<crate::paint::scene::LaneId>,
    full_damage_pending: bool,
    #[cfg(feature = "test-support")]
    stats: FrameStats,
    #[cfg(feature = "test-support")]
    record: Option<FrameRecord>,
    inline_box_image_diagnostics: Vec<crate::text::TextDiagnostic>,
    focus: FocusScope,
    pointer: PointerState,
    pointer_position: Option<Point>,
    pending_damage: Option<Rect>,
    a11y: crate::a11y::adapter::A11yBridge,
    ime_cursor: Option<ImeCursorRequest>,
    titlebar_drag_regions: Vec<Rect>,
    #[cfg(target_os = "linux")]
    ime_content: ImeContentType,
    state: WindowState,
    #[cfg(target_os = "macos")]
    traffic_light_inset: Option<Point>,
    #[cfg(target_os = "macos")]
    traffic_light_position: Option<Point>,
    #[cfg(feature = "hud")]
    render_stats: crate::render::RenderStats,
    #[cfg(feature = "hud")]
    last_frame_latency: Option<std::time::Duration>,
    #[cfg(feature = "hud")]
    completed_frames: u64,
    #[cfg(feature = "hud")]
    frame_cpu: Option<std::time::Duration>,
    #[cfg(feature = "hud")]
    frame_started_at: Option<std::time::Instant>,
}

impl Window {
    pub(crate) fn new(id: WindowId, options: WindowOptions) -> Self {
        let scale = options.scale;
        let surface = covering_surface(options.size, scale);
        let state = WindowState::for_decorations(options.decorations);
        let tree = NodeTree::new();
        Self {
            id,
            options,
            tree,
            scene: Scene::empty(),
            scale,
            surface,
            surface_from_platform: false,
            #[cfg(target_os = "linux")]
            buffer_scale: 1,
            scale_epoch: 0,
            frame_budget: frame_budget::FrameBudget::default(),
            scheduler: FrameScheduler::new(),
            composer: Some(WindowComposer::new()),
            pending_delta: None,
            pending_payload: None,
            delta_builder: crate::render::delta::DeltaBuilder::new(),
            scroll_slots: Vec::new(),
            scroll_slot_nodes: Vec::new(),
            resync: crate::render::frame::ResyncGate::default(),
            compositor_ticket: None,
            damage: DamageAccumulator::new(scale),
            resolver: ColorResolver,
            renderer: None,
            presentation: VecDeque::with_capacity(FRAME_PRESENTATION_HISTORY),
            glyph_page_received: None,
            image_page_received: None,
            image_layers_received: [false; MAX_IMAGE_PAGES as usize],
            image_drawable_layers: u32::MAX,
            #[cfg(test)]
            atlas_participating_override: None,
            #[cfg(all(test, windows))]
            atlas_upload_failure_from: None,
            glyph_atlas_rects: crate::render::DirtyRects::default(),
            image_atlas_rects: std::array::from_fn(|_| crate::render::DirtyRects::default()),
            atlas_deferrals: 0,
            stale_canvas_followup_generation: None,
            timings: FrameTimings::new(),
            paint_order: Vec::new(),
            full_damage_pending: false,
            #[cfg(feature = "test-support")]
            stats: FrameStats::default(),
            #[cfg(feature = "test-support")]
            record: None,
            inline_box_image_diagnostics: Vec::new(),
            focus: FocusScope::default(),
            pointer: PointerState::new(),
            pointer_position: None,
            pending_damage: None,
            a11y: crate::a11y::adapter::A11yBridge::new(),
            ime_cursor: None,
            titlebar_drag_regions: Vec::new(),
            #[cfg(target_os = "linux")]
            ime_content: ImeContentType::default(),
            state,
            #[cfg(target_os = "macos")]
            traffic_light_inset: None,
            #[cfg(target_os = "macos")]
            traffic_light_position: None,
            #[cfg(feature = "hud")]
            render_stats: crate::render::RenderStats::default(),
            #[cfg(feature = "hud")]
            last_frame_latency: None,
            #[cfg(feature = "hud")]
            completed_frames: 0,
            #[cfg(feature = "hud")]
            frame_cpu: None,
            #[cfg(feature = "hud")]
            frame_started_at: None,
        }
    }

    #[must_use]
    pub fn id(&self) -> WindowId {
        self.id
    }

    #[must_use]
    pub fn last_frame_presentation(&self) -> Option<FramePresentation> {
        self.presentation.back().copied()
    }

    pub fn recent_frame_presentations(&self) -> impl Iterator<Item = FramePresentation> + '_ {
        self.presentation.iter().copied()
    }

    pub(crate) fn note_frame_presentation(&mut self, presentation: FramePresentation) {
        if self.presentation.len() >= FRAME_PRESENTATION_HISTORY {
            let _ = self.presentation.pop_front();
        }
        self.presentation.push_back(presentation);
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn note_frame_presented(
        &mut self,
        index: FrameIndex,
        presented_at: Option<Instant>,
    ) {
        if let Some(entry) = self
            .presentation
            .iter_mut()
            .find(|entry| entry.index == index)
        {
            entry.presented_at = clamp_presented_at(presented_at, entry.completed_at);
        }
    }

    #[must_use]
    pub fn tree(&self) -> &NodeTree {
        &self.tree
    }

    pub fn tree_mut(&mut self) -> &mut NodeTree {
        &mut self.tree
    }

    pub fn set_canvas(
        &mut self,
        node: NodeId,
        canvas: crate::paint::canvas::Canvas,
    ) -> Result<bool, EngineError> {
        if self.tree.get(node).is_err() {
            return Err(EngineError::StaleNode { node });
        }
        Ok(self.tree.set_canvas(node, canvas))
    }

    pub fn set_image_resource(
        &mut self,
        node: NodeId,
        resource: crate::resources::ResourceId,
    ) -> Result<bool, EngineError> {
        if self.tree.get(node).is_err() {
            return Err(EngineError::StaleNode { node });
        }
        Ok(self.tree.set_image_resource(node, resource))
    }

    #[must_use]
    pub fn pointer_position(&self) -> Option<Point> {
        self.pointer_position
    }

    pub(crate) fn note_pointer_position(&mut self, position: Point) {
        self.pointer_position = Some(position);
    }

    pub(crate) fn pointer(&self) -> &PointerState {
        &self.pointer
    }

    pub(crate) fn pointer_mut(&mut self) -> &mut PointerState {
        &mut self.pointer
    }

    #[must_use]
    pub(crate) fn a11y_pending(&self) -> bool {
        a11y::a11y_pending(self)
    }
    pub(crate) fn a11y_take(
        &mut self,
        diagnostics: &mut Vec<crate::a11y::A11yDiagnostic>,
    ) -> Option<crate::a11y::A11yTreeSnapshot> {
        a11y::a11y_take(self, diagnostics)
    }
    pub(crate) fn a11y_note_scroll(&mut self, container: crate::node::id::NodeId) {
        a11y::a11y_note_scroll(self, container)
    }
    #[must_use]
    pub(crate) fn a11y_focus_pending(&self) -> bool {
        a11y::a11y_focus_pending(self)
    }
    #[must_use]
    pub(crate) fn a11y_consumer_epoch(&self) -> crate::a11y::adapter::A11yConsumerEpoch {
        a11y::a11y_consumer_epoch(self)
    }
    #[must_use]
    pub(crate) fn a11y_consumer_epoch_pending(&self) -> bool {
        a11y::a11y_consumer_epoch_pending(self)
    }
    #[must_use]
    pub(crate) fn a11y_backlog_pending(&self) -> bool {
        a11y::a11y_backlog_pending(self)
    }
    #[must_use]
    pub(crate) fn a11y_rebuild_pending(&self) -> bool {
        a11y::a11y_rebuild_pending(self)
    }
    #[must_use]
    pub(crate) fn a11y_engine_of(
        &self,
        id: crate::a11y::A11yNodeId,
    ) -> Option<crate::node::id::NodeId> {
        a11y::a11y_engine_of(self, id)
    }
    #[cfg(test)]
    #[must_use]
    pub(crate) fn a11y_stable_id(
        &self,
        node: crate::node::id::NodeId,
    ) -> Option<crate::a11y::A11yNodeId> {
        a11y::a11y_stable_id(self, node)
    }
    pub(crate) fn a11y_force_full(&mut self) {
        a11y::a11y_force_full(self);
    }
    pub(crate) fn a11y_note_binding(&mut self, node: crate::node::id::NodeId) -> bool {
        a11y::a11y_note_binding(self, node)
    }
    #[must_use]
    pub(crate) fn a11y_binding_work(&self) -> bool {
        a11y::a11y_binding_work(self)
    }
    pub(crate) fn a11y_take_bindings(&mut self) -> Vec<crate::node::id::NodeId> {
        a11y::a11y_take_bindings(self)
    }
    pub(crate) fn a11y_recycle_bindings(&mut self, buffer: Vec<crate::node::id::NodeId>) {
        a11y::a11y_recycle_bindings(self, buffer)
    }
    #[must_use]
    pub(crate) fn a11y_only_frame(&self) -> bool {
        a11y::a11y_only_frame(self)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn record_a11y_only_frame(&mut self) {
        a11y::record_a11y_only_frame(self)
    }
    #[cfg(feature = "test-support")]
    #[must_use]
    pub(crate) fn a11y_only_frames(&self) -> usize {
        a11y::a11y_only_frames(self)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn set_a11y_capture(&mut self, on: bool) {
        a11y::set_a11y_capture(self, on)
    }

    pub(crate) fn needs_frame(&self, wake: bool) -> bool {
        frame::needs_frame(self, wake)
    }

    pub(crate) fn set_frame_refresh_hz(&mut self, hz: f32) {
        self.frame_budget.set_refresh_hz(hz);
    }
    #[must_use]
    pub(crate) fn frame_refresh_hz(&self) -> Option<f32> {
        self.frame_budget.refresh_hz()
    }
    pub(crate) fn frame_budget_admits(&self, now: std::time::Instant, forced: bool) -> bool {
        self.frame_budget.admits(now, forced)
    }
    pub(crate) fn note_frame_start(&mut self, now: std::time::Instant) {
        self.frame_budget.note_start(now);
    }
    pub(crate) fn note_frame_budget_deferred(&mut self) -> bool {
        self.frame_budget.note_deferred()
    }
    #[must_use]
    pub(crate) fn frame_budget_deferred(&self) -> bool {
        self.frame_budget.is_deferred()
    }
    pub(crate) fn clear_frame_budget_deferred(&mut self) {
        self.frame_budget.clear_deferred();
    }
    #[cfg(target_os = "linux")]
    #[must_use]
    pub(crate) fn frame_budget_deadline(&self) -> Option<std::time::Instant> {
        self.frame_budget.deadline()
    }
    #[must_use]
    pub(crate) fn frame_budget_deferrals(&self) -> u64 {
        self.frame_budget.deferrals()
    }
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn set_frame_budget_test_interval(&mut self, interval: std::time::Duration) {
        self.frame_budget.set_test_interval(Some(interval));
    }
    pub(crate) fn begin_frame(&mut self) -> Option<FrameTicket> {
        frame::begin_frame(self)
    }
    pub(crate) fn force_full_republish(&mut self) {
        frame::force_full_republish(self);
    }
    pub(crate) fn layout_if_dirty(&mut self) -> Result<Option<LayoutReport>, EngineError> {
        frame::layout_if_dirty(self)
    }
    pub(crate) fn commit_frame(
        &mut self,
        ticket: FrameTicket,
        report: PaintReport,
    ) -> FrameOutcome {
        frame::commit_frame(self, ticket, report)
    }
    fn submit_gpu(
        &mut self,
        renderer: &mut Renderer,
        index: FrameIndex,
        plan: Plan,
    ) -> Option<crate::render::RenderStats> {
        frame::submit_gpu(self, renderer, index, plan)
    }
    pub(crate) fn abort_frame(&mut self, ticket: FrameTicket) {
        frame::abort_frame(self, ticket)
    }
    pub fn frame(&mut self) -> Result<FrameOutcome, EngineError> {
        frame::frame(self)
    }
    #[must_use]
    pub fn inline_box_image_diagnostics(&self) -> &[crate::text::TextDiagnostic] {
        &self.inline_box_image_diagnostics
    }
    pub(crate) fn note_skipped_inline_images(&mut self, ids: &[u64]) {
        self.inline_box_image_diagnostics.clear();
        self.inline_box_image_diagnostics.extend(
            ids.iter()
                .map(|id| crate::text::TextDiagnostic::InlineBoxImageMissing { id: *id }),
        );
    }
    #[must_use]
    pub fn frame_index(&self) -> FrameIndex {
        frame::frame_index(self)
    }
    pub fn compute_layout(&mut self) -> Result<LayoutReport, EngineError> {
        frame::compute_layout(self)
    }

    pub(crate) fn paint_frame(
        &mut self,
        geometry: &crate::paint::geometry_store::GeometryStore,
        atlas: Option<&crate::render::AtlasAllocator>,
        resources: Option<&crate::resources::ResourceCache>,
    ) -> Result<PaintReport, EngineError> {
        paint::paint_frame(self, geometry, atlas, resources)
    }
    fn needs_node_repaint(&self) -> bool {
        paint::needs_node_repaint(self)
    }
    #[cfg(feature = "test-support")]
    pub fn detach_composer(&mut self) {
        self.composer = None;
        self.pending_payload = None;
    }

    #[cfg(feature = "test-support")]
    #[must_use]
    pub(crate) fn instance_buffer(&self) -> Option<&InstanceBuffer> {
        paint::instance_buffer(self)
    }
    #[must_use]
    pub fn scene(&self) -> &Scene {
        paint::scene(self)
    }
    #[cfg(all(windows, test, feature = "test-support"))]
    #[must_use]
    pub(crate) fn pending_delta(&self) -> Option<&crate::render::delta::SceneDelta> {
        self.pending_delta.as_ref()
    }
    #[cfg(all(windows, test, feature = "test-support"))]
    #[must_use]
    pub(crate) fn has_pending_payload(&self) -> bool {
        self.pending_payload.is_some()
    }
    #[cfg(test)]
    pub(crate) fn encoded_geometry(&self) -> &[crate::paint::instance::GeometryDrawGpu] {
        paint::encoded_geometry(self)
    }
    #[cfg(test)]
    #[must_use]
    pub(crate) fn scroll_slots(&self) -> &[(crate::math::Point, PhysicalRect)] {
        &self.scroll_slots
    }
    pub(crate) fn request_damage(&mut self, rect: Rect) {
        paint::request_damage(self, rect)
    }
    pub(crate) fn request_repaint(&mut self) {
        paint::request_repaint(self)
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn request_composition_rebuild(&mut self) {
        surface::request_composition_rebuild(self)
    }
    #[must_use]
    pub fn options(&self) -> &WindowOptions {
        surface::options(self)
    }
    pub fn set_background_color(&mut self, color: Color) {
        surface::set_background_color(self, color)
    }
    #[must_use]
    pub fn size(&self) -> Size {
        surface::size(self)
    }
    pub fn set_size(&mut self, size: Size) {
        surface::set_size(self, size)
    }
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    pub(crate) fn set_surface(&mut self, surface: (u32, u32)) {
        surface::set_surface(self, surface)
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn set_max_frame_rate(&mut self, max_frame_rate: f32) {
        surface::set_max_frame_rate(self, max_frame_rate)
    }
    #[cfg(target_os = "linux")]
    pub(crate) fn set_buffer_scale(&mut self, scale: i32) {
        surface::set_buffer_scale(self, scale)
    }
    pub(crate) fn has_renderer(&self) -> bool {
        surface::has_renderer(self)
    }
    #[cfg(test)]
    pub(crate) fn glyph_page_received(&self) -> bool {
        surface::glyph_page_received(self)
    }
    #[cfg(test)]
    pub(crate) fn image_page_received(&self) -> bool {
        surface::image_page_received(self)
    }
    pub(crate) fn image_array_spec(&self) -> Option<crate::render::AtlasArraySpec> {
        self.image_page_received
    }
    pub(crate) fn image_layer_received(&self, layer: u16) -> bool {
        surface::image_layer_received(self, layer)
    }
    #[cfg(all(test, windows))]
    pub(crate) fn image_drawable_layers(&self) -> u32 {
        self.image_drawable_layers
    }
    pub(crate) fn set_image_drawable_layers(&mut self, layers: u32) {
        self.image_drawable_layers = layers;
    }
    pub(crate) fn atlas_participating(&self) -> bool {
        #[cfg(test)]
        if let Some(participating) = self.atlas_participating_override {
            return participating;
        }
        self.renderer
            .as_ref()
            .is_some_and(|renderer| renderer.atlas_participating())
    }
    #[cfg(all(test, windows))]
    pub(crate) fn set_atlas_participating_for_test(&mut self, participating: bool) {
        self.atlas_participating_override = Some(participating);
    }

    #[cfg(all(test, windows))]
    pub(crate) fn set_atlas_upload_failure_for_test(&mut self, from: Option<u16>) {
        self.atlas_upload_failure_from = from;
    }
    pub(crate) fn atlas_page_matches(
        &self,
        kind: AtlasKind,
        spec: crate::render::AtlasArraySpec,
    ) -> bool {
        surface::atlas_page_matches(self, kind, spec)
    }
    pub(crate) fn mark_atlas_page_received(
        &mut self,
        kind: AtlasKind,
        spec: crate::render::AtlasArraySpec,
    ) {
        surface::mark_atlas_page_received(self, kind, spec)
    }
    pub(crate) fn mark_image_layer_received(&mut self, layer: u16) {
        surface::mark_image_layer_received(self, layer)
    }
    pub(crate) fn mark_atlas_page_stale(&mut self, kind: AtlasKind) {
        surface::mark_atlas_page_stale(self, kind)
    }
    pub(crate) fn atlas_rects(&self, kind: AtlasKind, layer: u16) -> &crate::render::DirtyRects {
        surface::atlas_rects(self, kind, layer)
    }
    pub(crate) fn atlas_rects_mut(
        &mut self,
        kind: AtlasKind,
        layer: u16,
    ) -> &mut crate::render::DirtyRects {
        surface::atlas_rects_mut(self, kind, layer)
    }
    pub(crate) fn clear_atlas_rects(&mut self, kind: AtlasKind, layer: u16) {
        surface::clear_atlas_rects(self, kind, layer)
    }
    pub(crate) fn clear_all_atlas_rects(&mut self, kind: AtlasKind) {
        surface::clear_all_atlas_rects(self, kind)
    }
    pub(crate) fn atlas_rects_pending(&self) -> bool {
        surface::atlas_rects_pending(self)
    }
    pub(crate) fn note_atlas_deferral(&mut self) -> bool {
        surface::note_atlas_deferral(self)
    }
    pub(crate) fn reset_atlas_deferrals(&mut self) {
        surface::reset_atlas_deferrals(self)
    }
    pub(crate) fn note_stale_canvas_followup(&mut self, generation: u32) -> bool {
        surface::note_stale_canvas_followup(self, generation)
    }
    pub(crate) fn queue_atlas_upload(
        &mut self,
        kind: AtlasKind,
        layer: u16,
        spec: crate::render::AtlasArraySpec,
        pixels: &[u8],
        rects: crate::render::DirtyRects,
    ) -> bool {
        surface::queue_atlas_upload(self, kind, layer, spec, pixels, rects)
    }
    pub(crate) fn atlas_idle(&self, kind: AtlasKind) -> bool {
        surface::atlas_idle(self, kind)
    }
    #[cfg(all(test, windows))]
    pub(crate) fn atlas_pending_layers(&self, kind: AtlasKind) -> Option<Vec<u16>> {
        surface::atlas_pending_layers(self, kind)
    }
    pub(crate) fn set_renderer(&mut self, renderer: Renderer) {
        surface::set_renderer(self, renderer)
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn take_renderer(&mut self) -> Option<Renderer> {
        surface::take_renderer(self)
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn has_compositor_ticket(&self) -> bool {
        surface::has_compositor_ticket(self)
    }
    pub(crate) fn poll_renderer(&mut self) -> Result<(), EngineError> {
        surface::poll_renderer(self)
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn sync_pacing(&mut self, continuous: bool) {
        surface::sync_pacing(self, continuous)
    }
    pub fn enable_offscreen_renderer(&mut self) -> Result<(), EngineError> {
        surface::enable_offscreen_renderer(self)
    }
    fn physical_surface(&self) -> (u32, u32) {
        surface::physical_surface(self)
    }
    #[cfg(windows)]
    pub(crate) fn presented_surface(&self) -> (u32, u32) {
        surface::presented_surface(self)
    }
    #[cfg(windows)]
    pub(crate) fn take_composition_pending(&self) -> bool {
        surface::take_composition_pending(self)
    }
    fn frame_rect(&self) -> PhysicalRect {
        surface::frame_rect(self)
    }

    #[must_use]
    pub fn scale(&self) -> Scale {
        scale::scale(self)
    }
    pub fn set_scale(&mut self, scale: Scale) {
        scale::set_scale(self, scale)
    }
    #[must_use]
    pub fn scale_epoch(&self) -> u32 {
        scale::scale_epoch(self)
    }

    #[must_use]
    pub fn focus(&self) -> &FocusScope {
        focus::focus(self)
    }
    pub(crate) fn focus_mut(&mut self) -> &mut FocusScope {
        focus::focus_mut(self)
    }
    pub fn text_caret_rect(&self, node: NodeId, byte_index: usize) -> Result<Rect, EngineError> {
        focus::text_caret_rect(self, node, byte_index)
    }
    pub fn text_byte_index_at(
        &self,
        node: NodeId,
        local_point: Point,
    ) -> Result<usize, EngineError> {
        focus::text_byte_index_at(self, node, local_point)
    }
    pub(crate) fn set_ime_cursor_request(&mut self, node: NodeId, byte_index: usize) -> bool {
        focus::set_ime_cursor_request(self, node, byte_index)
    }
    pub(crate) fn clear_ime_cursor_request(&mut self) -> bool {
        focus::clear_ime_cursor_request(self)
    }
    pub(crate) fn ime_cursor_area(&self) -> Result<Option<Rect>, EngineError> {
        focus::ime_cursor_area(self)
    }

    pub fn set_drag_regions(&mut self, regions: &[Rect]) {
        if self.titlebar_drag_regions == regions {
            return;
        }
        self.titlebar_drag_regions.clear();
        self.titlebar_drag_regions.extend_from_slice(regions);
    }

    #[must_use]
    pub(crate) fn drag_regions(&self) -> &[Rect] {
        &self.titlebar_drag_regions
    }

    #[cfg(target_os = "linux")]
    pub fn set_ime_content_type(&mut self, content: ImeContentType) {
        self.ime_content = content;
    }

    #[cfg(target_os = "linux")]
    #[must_use]
    pub(crate) fn ime_content_type(&self) -> ImeContentType {
        self.ime_content
    }

    #[must_use]
    pub fn state(&self) -> WindowState {
        self.state
    }

    pub(crate) fn set_state(&mut self, state: WindowState) -> bool {
        if self.state == state {
            return false;
        }
        self.state = state;
        let root = self.tree.root();
        self.tree.mark_paint(root);
        true
    }

    pub(crate) fn set_title(&mut self, title: SmolStr) {
        self.options.title = title;
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn set_traffic_light_inset(&mut self, inset: Option<Point>) -> bool {
        if self.traffic_light_inset == inset {
            return false;
        }
        self.traffic_light_inset = inset;
        true
    }

    #[cfg(target_os = "macos")]
    #[must_use]
    pub(crate) fn traffic_light_inset(&self) -> Option<Point> {
        self.traffic_light_inset
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn set_traffic_light_position(&mut self, inset: Point) {
        self.traffic_light_position = Some(inset);
    }

    #[cfg(target_os = "macos")]
    #[must_use]
    pub(crate) fn traffic_light_position(&self) -> Option<Point> {
        self.traffic_light_position
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn record(&self) -> Option<&FrameRecord> {
        record::record(self)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn record_idle(&mut self) {
        record::record_idle(self)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn record_budget_deferred(&mut self) {
        record::record_budget_deferred(self)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn attach_a11y(&mut self, snapshot: crate::a11y::A11yTreeSnapshot) {
        record::attach_a11y(self, snapshot)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn set_frame_stats(&mut self, stats: FrameStats) {
        record::set_frame_stats(self, stats)
    }
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn frame_stats(&self) -> Option<&FrameStats> {
        self.record.as_ref().map(|record| &record.stats)
    }
    #[cfg(feature = "hud")]
    pub(crate) fn reset_debug_hud_timing(&mut self) {
        record::reset_debug_hud_timing(self)
    }
    #[cfg(feature = "hud")]
    pub(crate) fn debug_hud_metrics(&self) -> crate::debug_hud::DebugHudMetrics {
        record::debug_hud_metrics(self)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn force_full_repaint(&mut self) {
        record::force_full_repaint(self)
    }
}

mod a11y;
mod animation;
#[cfg(feature = "automation")]
mod automation;
#[cfg(feature = "hud")]
mod debug;
mod event_queue;
mod focus;
mod frame;
mod geometry;
mod input;
mod invalidate;
mod overlay;
mod resources;
mod scroll;
mod signal_api;
mod style_pass;
mod text_pass;
mod timer;
mod virtual_list;
mod window_set;

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[cfg(feature = "hud")]
use crate::debug_hud::DebugHudState;
use crate::{
    error::EngineError,
    event::{
        Action, ActionHandler, ActionSource, CloseHandler, Event, InputEvent, InputOutcome,
        KeyBinding, KeyCode, KeyState, Modifiers, PointerButton,
    },
    interaction::{
        ActionCtx, AnimatedValue, AnimationId, AnimationSpec, DragInfo, DragState, Easing,
        InputThresholds, ItemAlign, Overlay, OverlayDismiss, OverlayOptions, OverlayPlacement,
        ScrollConfig, ScrollHandle, ScrollState, VirtualList, VirtualListConfig, VirtualListState,
    },
    layout::hit_test::HitResult,
    math::{DevicePixelRatio, LogicalPoint, LogicalPx, LogicalRect, LogicalSize},
    node::{
        builder::NodeBuilder,
        dependency::{DependencyGraph, EvalStack, ReadCtx},
        id::NodeId,
        signal::{Derived, Signal, SignalId, SignalRef, SignalScope, StateStore, Transient},
    },
    paint::{color::Color, style::Cursor, style_builder::StyleBuilder},
    platform::dispatcher::{Dispatcher, UserTask},
    render::{AtlasAllocator, MIN_ATLAS_PAGE},
    resources::{DecodeState, PendingResource, ResourceCache},
    text::{
        FontStack, FontStackStatus, InlineBoxSpec, ShapedInlineBox, ShapedTextHandle,
        TextDiagnostic, TextLayoutReport, TextPassReport, TextStyle, atlas::GlyphAtlas,
        context::TextContext, raster::GlyphRasterizer,
    },
    window::{FrameOutcome, FramePresentation, FrameTicket, Window, WindowId, WindowOptions},
};

const A11Y_DIAGNOSTIC_CAPACITY: usize = 64;

const A11Y_THROTTLE: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StylePassReport {
    pub evaluated: usize,
    pub layout_dirty: bool,
    pub paint_dirty: bool,
    pub wake_requested: bool,
    pub text_evaluated: usize,
    pub text_changed: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RemoveOutcome {
    pub removed_nodes: usize,
    pub scoped_key_bindings: usize,
    pub timers: usize,
    pub animations: usize,
    pub released_values: usize,
}

pub struct App {
    store: StateStore,
    graph: DependencyGraph,
    windows: ahash::HashMap<WindowId, Window>,
    eval_stack: EvalStack,
    wake_requested: bool,
    next_window_id: u32,
    runtime: tokio::runtime::Handle,
    text: TextContext,
    raster: GlyphRasterizer,
    atlas: AtlasAllocator,
    resources: ResourceCache,
    pending_resource_results: Vec<(PendingResource, DecodeState)>,
    geometry: crate::paint::geometry_store::GeometryStore,
    glyph_atlas: GlyphAtlas,
    text_dirty_scratch: Vec<crate::node::id::NodeId>,
    text_layout_scratch: Vec<crate::node::id::NodeId>,
    scale_epochs: ahash::HashMap<WindowId, u32>,
    image_scale_epochs: ahash::HashMap<WindowId, u32>,
    atlas_generation_seen: u32,
    atlas_generation_lease: Arc<AtomicU64>,
    default_text_scale: f32,
    text_scale_override: Option<f32>,
    focused_window: Option<WindowId>,
    pending_dirty: crate::render::DirtyRects,
    glyph_page_extent: Option<u32>,
    image_array_established: Option<crate::render::AtlasArraySpec>,
    image_growth_deferrals: u32,
    glyph_page: Vec<u8>,
    packed: Vec<u8>,
    dispatcher: Arc<Dispatcher>,
    events: event_queue::EventQueue,
    held_keys: ahash::HashMap<WindowId, ahash::HashSet<KeyCode>>,
    frame_hook: Option<Box<dyn FnMut(&mut App)>>,
    handlers: ahash::HashMap<Action, ActionHandler>,
    close_handlers: ahash::HashMap<WindowId, CloseHandler>,
    bindings: Vec<KeyBinding>,
    scoped_bindings: Vec<ScopedKeyBinding>,
    thresholds: InputThresholds,
    clock: Instant,
    clock_live: bool,
    a11y_delivered: ahash::HashMap<WindowId, Instant>,
    a11y_delivered_round: bool,
    last_click: Option<ClickRecord>,
    animations: Vec<AnimationSlot>,
    next_animation: u32,
    animation_completions: std::collections::VecDeque<AnimationCompletion>,
    animation_complete_handler: Option<Box<dyn FnMut(&mut App, AnimationCompletion)>>,
    dispatching_animation_completions: bool,
    timers: Vec<timer::TimerEntry>,
    next_timer: u32,
    cursor_pending: Option<(WindowId, Cursor)>,
    capture_requests: ahash::HashMap<WindowId, bool>,
    ime_disabled: ahash::HashSet<WindowId>,
    window_control: Option<(WindowId, crate::window::WindowControl)>,
    a11y_handlers: ahash::HashMap<
        crate::a11y::A11yAction,
        Box<dyn FnMut(&mut crate::a11y::A11yActionCtx<'_>)>,
    >,
    a11y_diagnostics: Vec<crate::a11y::A11yDiagnostic>,
    #[cfg(feature = "hud")]
    debug_hud: ahash::HashMap<WindowId, DebugHudState>,
    window_scopes: ahash::HashMap<WindowId, SignalScope>,
    batch_depth: u32,
    batch_wake: bool,
    frame_requests: u64,
}

#[derive(Clone, Copy, Debug)]
struct ClickRecord {
    window: WindowId,
    node: Option<NodeId>,
    button: PointerButton,
    position: LogicalPoint,
    at: Instant,
    count: u8,
}

struct ScopedKeyBinding {
    window: WindowId,
    scope: NodeId,
    binding: KeyBinding,
}

struct AnimationSlot {
    id: u32,
    window: WindowId,
    target: AnimationTarget,
    start: Instant,
    duration: Duration,
    easing: Easing,
    nodes: Vec<NodeId>,
}

enum AnimationTarget {
    Scalar {
        transient: Transient<f32>,
        from: f32,
        to: f32,
    },
    Color {
        transient: Transient<Color>,
        from: Color,
        to: Color,
    },
    Scroll {
        handle: ScrollHandle,
        from: LogicalPoint,
        to: LogicalPoint,
    },
}

impl AnimationTarget {
    fn same_target(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Scalar {
                    transient: left, ..
                },
                Self::Scalar {
                    transient: right, ..
                },
            ) => left.id() == right.id(),
            (
                Self::Color {
                    transient: left, ..
                },
                Self::Color {
                    transient: right, ..
                },
            ) => left.id() == right.id(),
            (Self::Scroll { handle: left, .. }, Self::Scroll { handle: right, .. }) => {
                *left == *right
            }
            _ => false,
        }
    }

    fn targets_any(&self, ids: &[SignalId]) -> bool {
        match self {
            Self::Scalar { transient, .. } => ids.contains(&transient.id()),
            Self::Color { transient, .. } => ids.contains(&transient.id()),
            Self::Scroll { .. } => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AnimationEndKind {
    Finished,
    Cancelled,
    Replaced,
    Released,
    Removed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AnimationCompletion {
    pub id: AnimationId,
    pub kind: AnimationEndKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimerId {
    window: WindowId,
    generation: u32,
}

impl TimerId {
    pub(crate) fn new(window: WindowId, generation: u32) -> Self {
        Self { window, generation }
    }

    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    pub(crate) fn generation(&self) -> u32 {
        self.generation
    }
}

fn default_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding {
            key: KeyCode::Tab,
            modifiers: Modifiers::empty(),
            action: Action::FocusNext,
        },
        KeyBinding {
            key: KeyCode::Tab,
            modifiers: Modifiers::SHIFT,
            action: Action::FocusPrev,
        },
        KeyBinding {
            key: KeyCode::Escape,
            modifiers: Modifiers::empty(),
            action: Action::Dismiss,
        },
    ]
}

fn overlay_style(origin: LogicalPoint) -> crate::paint::style::Style {
    StyleBuilder::new()
        .absolute()
        .left(origin.x)
        .top(origin.y)
        .build()
}

#[derive(Clone)]
pub struct AppHandle {
    dispatcher: Arc<Dispatcher>,
    runtime: tokio::runtime::Handle,
}

impl AppHandle {
    pub fn wake(&self) {
        self.dispatcher.wake();
    }

    pub fn send(&self, task: impl FnOnce(&mut App) + Send + 'static) {
        self.dispatcher.push(UserTask::new(task));
    }

    pub fn spawn<F>(&self, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.runtime.spawn(future);
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Application;

impl Application {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    pub fn run(self, app: App) -> Result<(), EngineError> {
        crate::platform::native::run(app)
    }
}

impl App {
    #[must_use]
    pub fn new(runtime: tokio::runtime::Handle) -> Self {
        let dispatcher = Dispatcher::new();
        let atlas = AtlasAllocator::with_page(
            crate::paint::atlas::AtlasKind::Glyph,
            (MIN_ATLAS_PAGE, MIN_ATLAS_PAGE),
        );
        let atlas_generation_lease = Arc::new(AtomicU64::new(crate::text::pack_atlas_lease(
            atlas.generation(),
            0,
        )));
        Self {
            store: StateStore::with_capacity(64, 16),
            graph: DependencyGraph::new(),
            windows: ahash::HashMap::default(),
            eval_stack: EvalStack::new(),
            wake_requested: false,
            next_window_id: 1,
            runtime: runtime.clone(),
            text: TextContext::new(),
            raster: GlyphRasterizer::new(),
            atlas,
            resources: ResourceCache::new(runtime, Arc::clone(&dispatcher)),
            pending_resource_results: Vec::new(),
            geometry: crate::paint::geometry_store::GeometryStore::new(),
            glyph_atlas: GlyphAtlas::new(),
            text_dirty_scratch: Vec::new(),
            text_layout_scratch: Vec::new(),
            scale_epochs: ahash::HashMap::default(),
            image_scale_epochs: ahash::HashMap::default(),
            atlas_generation_seen: 0,
            atlas_generation_lease,
            default_text_scale: 1.0,
            text_scale_override: None,
            focused_window: None,
            pending_dirty: crate::render::DirtyRects::default(),
            glyph_page_extent: None,
            image_array_established: None,
            image_growth_deferrals: 0,
            glyph_page: Vec::new(),
            packed: Vec::new(),
            dispatcher,
            events: event_queue::EventQueue::new(),
            held_keys: ahash::HashMap::default(),
            frame_hook: None,
            handlers: ahash::HashMap::default(),
            close_handlers: ahash::HashMap::default(),
            bindings: default_bindings(),
            scoped_bindings: Vec::new(),
            thresholds: InputThresholds::platform_default(),
            clock: Instant::now(),
            clock_live: true,
            a11y_delivered: ahash::HashMap::default(),
            a11y_delivered_round: false,
            last_click: None,
            animations: Vec::new(),
            next_animation: 1,
            animation_completions: std::collections::VecDeque::new(),
            animation_complete_handler: None,
            dispatching_animation_completions: false,
            timers: Vec::new(),
            next_timer: 1,
            cursor_pending: None,
            capture_requests: ahash::HashMap::default(),
            ime_disabled: ahash::HashSet::default(),
            window_control: None,
            a11y_handlers: ahash::HashMap::default(),
            a11y_diagnostics: Vec::new(),
            #[cfg(feature = "hud")]
            debug_hud: ahash::HashMap::default(),
            window_scopes: ahash::HashMap::default(),
            batch_depth: 0,
            batch_wake: false,
            frame_requests: 0,
        }
    }

    pub fn on_frame(&mut self, hook: impl FnMut(&mut App) + 'static) {
        resources::on_frame(self, hook)
    }

    #[must_use]
    pub fn frame_hook_installed(&self) -> bool {
        self.frame_hook.is_some()
    }

    pub(crate) fn run_frame_hook(&mut self) {
        timer::run_due(self);
        animation::dispatch_completions(self);
        resources::run_frame_hook(self)
    }

    pub(crate) fn poll_resources(&mut self) {
        resources::poll_resources(self)
    }

    pub fn take_resource_results(&mut self) -> Vec<(PendingResource, DecodeState)> {
        resources::take_resource_results(self)
    }

    pub fn unload_resource(&mut self, id: crate::resources::ResourceId) -> bool {
        resources::unload_resource(self, id)
    }

    pub(crate) fn refresh_images(&mut self, window: WindowId) -> usize {
        resources::refresh_images(self, window)
    }

    #[must_use]
    pub fn handle(&self) -> AppHandle {
        resources::handle(self)
    }

    pub(crate) fn dispatcher(&self) -> Arc<Dispatcher> {
        resources::dispatcher(self)
    }

    #[must_use]
    pub fn drain_events(&mut self) -> Vec<(WindowId, Event)> {
        resources::drain_events(self)
    }

    #[must_use]
    pub fn drain_events_where(
        &mut self,
        keep: impl FnMut(WindowId, &Event) -> bool,
    ) -> Vec<(WindowId, Event)> {
        resources::drain_events_where(self, keep)
    }

    #[must_use]
    pub fn events_dropped(&self) -> u64 {
        self.events.dropped()
    }

    pub(crate) fn dispatch(&mut self, window: WindowId, event: Event) {
        resources::dispatch(self, window, event)
    }

    pub fn set_clipboard_text(&self, text: &str) -> Result<(), EngineError> {
        resources::set_clipboard_text(self, text)
    }

    pub fn clipboard_text(&self) -> Result<smol_str::SmolStr, EngineError> {
        resources::clipboard_text(self)
    }

    pub(crate) fn upload_atlas_pages(&mut self) {
        text_pass::upload_atlas_pages(self)
    }

    fn upload_glyph_page(&mut self) {
        text_pass::upload_glyph_page(self)
    }

    fn upload_image_page(&mut self) {
        text_pass::upload_image_page(self)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn glyph_page_pixels(&self) -> ((u32, u32), Vec<u8>) {
        text_pass::glyph_page_pixels(self)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn resource_page_pixels_all(&self) -> Vec<((u32, u32), &[u8])> {
        text_pass::resource_page_pixels_all(self)
    }

    #[must_use]
    pub fn resources(&self) -> &ResourceCache {
        resources::resources(self)
    }

    pub fn resources_mut(&mut self) -> &mut ResourceCache {
        resources::resources_mut(self)
    }

    #[must_use]
    pub fn geometry(&self) -> &crate::paint::geometry_store::GeometryStore {
        resources::geometry(self)
    }

    pub fn geometry_mut(&mut self) -> &mut crate::paint::geometry_store::GeometryStore {
        resources::geometry_mut(self)
    }

    pub fn set_geometry_transform(
        &mut self,
        window: WindowId,
        node: NodeId,
        transform: crate::math::Transform,
    ) -> Result<(), EngineError> {
        geometry::set_geometry_transform(self, window, node, transform)
    }

    pub fn update_geometry_vertices(
        &mut self,
        geometry: crate::paint::geometry::GeometryId,
        range: std::ops::Range<u32>,
        bytes: &[u8],
    ) -> Result<crate::paint::geometry::GeometryUpdate, EngineError> {
        geometry::update_vertices(self, geometry, range, bytes)
    }

    pub fn update_geometry_indices(
        &mut self,
        geometry: crate::paint::geometry::GeometryId,
        range: std::ops::Range<u32>,
        bytes: &[u8],
    ) -> Result<crate::paint::geometry::GeometryUpdate, EngineError> {
        geometry::update_indices(self, geometry, range, bytes)
    }

    pub fn truncate_geometry(
        &mut self,
        geometry: crate::paint::geometry::GeometryId,
        vertices: u32,
        indices: u32,
    ) -> Result<crate::paint::geometry::GeometryUpdate, EngineError> {
        geometry::truncate(self, geometry, vertices, indices)
    }

    #[cfg(feature = "test-support")]
    #[must_use]
    pub(crate) fn signal_slot_counts_for_test(&self) -> (usize, usize, usize) {
        (
            self.store.slot_count_for_test(),
            self.store.live_slot_count_for_test(),
            self.store.free_slot_count_for_test(),
        )
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn dependency_graph(&self) -> &DependencyGraph {
        signal_api::dependency_graph(self)
    }

    #[cfg(test)]
    pub(crate) fn dependency_edge_count_for_test(&self) -> usize {
        signal_api::dependency_edge_count(self)
    }

    #[cfg(all(test, windows))]
    pub(crate) fn image_array_established_for_test(&self) -> Option<crate::render::AtlasArraySpec> {
        self.image_array_established
    }

    #[cfg(test)]
    pub(crate) fn set_next_window_id_for_test(&mut self, next: u32) {
        self.next_window_id = next;
    }

    #[must_use]
    pub fn runtime_handle(&self) -> &tokio::runtime::Handle {
        resources::runtime_handle(self)
    }

    pub fn signal<T: Clone + 'static>(&mut self, initial: T) -> Signal<T> {
        signal_api::signal(self, initial)
    }

    #[must_use]
    pub fn signal_scope(&mut self) -> SignalScope {
        signal_api::signal_scope(self)
    }

    pub fn signal_scoped<T: Clone + 'static>(
        &mut self,
        scope: SignalScope,
        initial: T,
    ) -> Signal<T> {
        signal_api::signal_scoped(self, scope, initial)
    }

    pub fn window_signal<T: Clone + 'static>(&mut self, window: WindowId, initial: T) -> Signal<T> {
        let scope = *self
            .window_scopes
            .get(&window)
            .expect("window signal scope: window does not exist");
        signal_api::signal_scoped(self, scope, initial)
    }

    pub fn release_scope(&mut self, scope: SignalScope) -> usize {
        signal_api::release_scope(self, scope)
    }

    pub fn transient<T: Clone + 'static>(&mut self, initial: T) -> Transient<T> {
        signal_api::transient(self, initial)
    }

    pub fn transient_scoped<T: Clone + 'static>(
        &mut self,
        scope: SignalScope,
        initial: T,
    ) -> Transient<T> {
        signal_api::transient_scoped(self, scope, initial)
    }

    pub fn derived<T: Clone + 'static>(
        &mut self,
        compute: impl Fn(&mut ReadCtx<'_>) -> T + 'static,
    ) -> Derived<T> {
        signal_api::derived(self, compute)
    }

    pub fn write<T: Clone + PartialEq + 'static>(&mut self, signal: &Signal<T>, value: T) -> bool {
        signal_api::write(self, signal, value)
    }

    pub fn write_with<T: Clone + PartialEq + 'static>(
        &mut self,
        signal: &Signal<T>,
        body: impl FnOnce(&T) -> T,
    ) -> bool {
        signal_api::write_with(self, signal, body)
    }

    pub fn update<T: 'static>(&mut self, signal: &Signal<T>, update: impl FnOnce(&mut T)) -> bool {
        signal_api::update(self, signal, update)
    }

    pub fn update_with<T: 'static, R>(
        &mut self,
        signal: &Signal<T>,
        body: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        signal_api::update_with(self, signal, body)
    }

    pub fn write_transient<T: 'static>(&mut self, transient: &Transient<T>, value: T) {
        signal_api::write_transient(self, transient, value)
    }

    pub fn batch<R>(&mut self, body: impl FnOnce(&mut App) -> R) -> R {
        self.batch_depth += 1;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(self)));
        self.batch_depth = self.batch_depth.saturating_sub(1);
        if self.batch_depth == 0 && self.batch_wake {
            self.batch_wake = false;
            self.request_frame();
        }
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    #[must_use]
    pub fn batching(&self) -> bool {
        self.batch_depth > 0
    }

    #[must_use]
    pub fn frame_requests(&self) -> u64 {
        self.frame_requests
    }

    pub(crate) fn take_wake_request(&mut self) -> bool {
        frame::take_wake_request(self)
    }

    pub fn request_repaint(&mut self, window: WindowId) -> Result<(), EngineError> {
        invalidate::request_repaint(self, window)
    }

    pub fn request_node_repaint(
        &mut self,
        window: WindowId,
        node: NodeId,
    ) -> Result<bool, EngineError> {
        invalidate::request_node_repaint(self, window, node)
    }

    pub(crate) fn request_frame(&mut self) {
        if self.batch_depth > 0 {
            self.batch_wake = true;
            return;
        }
        self.frame_requests = self.frame_requests.saturating_add(1);
        frame::request_frame(self)
    }

    pub fn on_action(&mut self, action: Action, handler: impl FnMut(&mut ActionCtx<'_>) + 'static) {
        style_pass::on_action(self, action, handler)
    }

    pub fn off_action(&mut self, action: &Action) -> bool {
        style_pass::off_action(self, action)
    }

    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn has_action_handler(&self, action: &Action) -> bool {
        style_pass::has_action_handler(self, action)
    }

    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn scoped_key_binding_count(&self, window: WindowId, scope: NodeId) -> usize {
        style_pass::scoped_binding_count(self, window, scope)
    }

    pub fn bind_key(&mut self, binding: KeyBinding) {
        style_pass::bind_key(self, binding)
    }

    pub fn bind_key_scoped(
        &mut self,
        window: WindowId,
        scope: NodeId,
        binding: KeyBinding,
    ) -> Result<(), EngineError> {
        style_pass::bind_key_scoped(self, window, scope, binding)
    }

    pub fn unbind_key_scoped(
        &mut self,
        window: WindowId,
        scope: NodeId,
        key: KeyCode,
        modifiers: Modifiers,
    ) -> bool {
        style_pass::unbind_key_scoped(self, window, scope, key, modifiers)
    }

    pub fn unbind_scope_keys(&mut self, window: WindowId, scope: NodeId) -> usize {
        style_pass::unbind_scope_keys(self, window, scope)
    }

    pub fn unbind_key(&mut self, key: KeyCode, modifiers: Modifiers) -> bool {
        style_pass::unbind_key(self, key, modifiers)
    }

    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn edit_menu_availability(
        &self,
        text_editing: bool,
    ) -> crate::platform::menu::EditAvailability {
        crate::platform::menu::EditAvailability::resolve(text_editing, |key, modifiers| {
            self.bindings.iter().any(|binding| {
                binding.key == key
                    && binding.modifiers == modifiers
                    && (matches!(
                        binding.action,
                        Action::FocusNext | Action::FocusPrev | Action::Dismiss
                    ) || self.handlers.contains_key(&binding.action))
            })
        })
    }

    #[must_use]
    pub fn read<T: 'static>(&self, signal: &Signal<T>) -> SignalRef<T> {
        signal_api::read(self, signal)
    }

    pub fn read_with<T: 'static, R>(&self, signal: &Signal<T>, body: impl FnOnce(&T) -> R) -> R {
        signal_api::read_with(self, signal, body)
    }

    pub fn set_input_thresholds(&mut self, thresholds: InputThresholds) {
        input::set_input_thresholds(self, thresholds)
    }

    #[must_use]
    pub fn read_transient<T: 'static>(&self, transient: &Transient<T>) -> SignalRef<T> {
        signal_api::read_transient(self, transient)
    }

    #[must_use]
    pub fn scroll_handle(&self, window: WindowId, node: NodeId) -> Option<ScrollHandle> {
        scroll::scroll_handle(self, window, node)
    }

    pub fn dispatch_action(
        &mut self,
        window: WindowId,
        node: NodeId,
        action: Action,
        source: ActionSource,
    ) -> bool {
        input::dispatch_action(self, window, node, action, source)
    }

    pub fn dispatch_action_focused(
        &mut self,
        window: WindowId,
        action: Action,
        source: ActionSource,
    ) -> bool {
        input::dispatch_action_focused(self, window, action, source)
    }

    pub(crate) fn dispatch_action_with_hit(
        &mut self,
        window: WindowId,
        node: NodeId,
        action: Action,
        source: ActionSource,
        hit: Option<HitResult>,
        modifiers: Modifiers,
    ) -> bool {
        input::dispatch_action_with_hit(self, window, node, action, source, hit, modifiers)
    }

    pub(crate) fn dispatch_input(&mut self, window: WindowId, event: InputEvent) -> InputOutcome {
        input::dispatch_input(self, window, event)
    }

    fn hit_at(&self, window: WindowId, position: LogicalPoint) -> Option<HitResult> {
        input::hit_at(self, window, position)
    }

    fn on_pointer_down(
        &mut self,
        window: WindowId,
        position: LogicalPoint,
        button: PointerButton,
        modifiers: Modifiers,
        timestamp: Instant,
        outcome: &mut InputOutcome,
    ) {
        input::on_pointer_down(
            self, window, position, button, modifiers, timestamp, outcome,
        )
    }

    fn on_pointer_move(
        &mut self,
        window: WindowId,
        position: LogicalPoint,
        modifiers: Modifiers,
        outcome: &mut InputOutcome,
    ) {
        input::on_pointer_move(self, window, position, modifiers, outcome)
    }

    fn on_pointer_up(
        &mut self,
        window: WindowId,
        position: LogicalPoint,
        button: PointerButton,
        modifiers: Modifiers,
        timestamp: Instant,
        outcome: &mut InputOutcome,
    ) {
        input::on_pointer_up(
            self, window, position, button, modifiers, timestamp, outcome,
        )
    }

    fn on_pointer_leave(&mut self, window: WindowId, modifiers: Modifiers) {
        input::on_pointer_leave(self, window, modifiers)
    }

    fn on_scroll(
        &mut self,
        window: WindowId,
        position: LogicalPoint,
        delta: LogicalPoint,
        unit: crate::event::ScrollUnit,
        phase: crate::event::ScrollPhase,
        modifiers: Modifiers,
        outcome: &mut InputOutcome,
    ) {
        input::on_scroll(
            self, window, position, delta, unit, phase, modifiers, outcome,
        )
    }

    fn scroll_animation_target(&self, node: NodeId, window: WindowId) -> Option<LogicalPoint> {
        animation::scroll_animation_target(self, node, window)
    }

    fn on_key(
        &mut self,
        window: WindowId,
        code: KeyCode,
        state: KeyState,
        repeat: bool,
        modifiers: Modifiers,
        outcome: &mut InputOutcome,
    ) {
        input::on_key(self, window, code, state, repeat, modifiers, outcome)
    }

    fn click_count(
        &mut self,
        window: WindowId,
        node: NodeId,
        position: LogicalPoint,
        button: PointerButton,
        timestamp: Instant,
    ) -> u8 {
        input::click_count(self, window, node, position, button, timestamp)
    }

    fn on_window_focus_lost(&mut self, window: WindowId) {
        input::on_window_focus_lost(self, window)
    }

    fn cancel_pointer_session(&mut self, window: WindowId) {
        input::cancel_pointer_session(self, window)
    }

    #[must_use]
    pub fn focused(&self, window: WindowId) -> Option<NodeId> {
        focus::focused(self, window)
    }

    fn validate_focus(&mut self, window: WindowId) {
        focus::validate_focus(self, window)
    }

    #[must_use]
    pub fn modal_scope(&self, window: WindowId) -> Option<NodeId> {
        focus::focus_scope(self, window)
    }

    #[must_use]
    pub fn hovered(&self, window: WindowId) -> Option<NodeId> {
        focus::hovered(self, window)
    }

    #[must_use]
    pub fn pointer_position(&self, window: WindowId) -> Option<LogicalPoint> {
        self.window(window).and_then(Window::pointer_position)
    }

    pub fn capture_pointer(&mut self, window: WindowId, node: NodeId) -> bool {
        input::capture_pointer(self, window, node)
    }

    pub fn release_pointer(&mut self, window: WindowId) -> bool {
        input::release_pointer(self, window)
    }

    #[must_use]
    pub fn pointer_captured(&self, window: WindowId) -> Option<NodeId> {
        input::pointer_captured(self, window)
    }

    #[must_use]
    pub fn key_down(&self, window: WindowId, code: KeyCode) -> bool {
        self.held_keys
            .get(&window)
            .is_some_and(|held| held.contains(&code))
    }

    pub fn set_focus(&mut self, window: WindowId, node: Option<NodeId>) -> bool {
        focus::set_focus(self, window, node)
    }

    pub fn set_ime_cursor_area(
        &mut self,
        window: WindowId,
        node: NodeId,
        byte_index: usize,
    ) -> Result<(), EngineError> {
        focus::set_ime_cursor_area(self, window, node, byte_index)
    }

    pub fn clear_ime_cursor_area(&mut self, window: WindowId) {
        focus::clear_ime_cursor_area(self, window)
    }

    pub fn set_ime_enabled(&mut self, window: WindowId, enabled: bool) {
        focus::set_ime_enabled(self, window, enabled)
    }

    pub fn ime_cursor_area(&self, window: WindowId) -> Result<Option<LogicalRect>, EngineError> {
        focus::ime_cursor_area(self, window)
    }

    pub fn focus_next(&mut self, window: WindowId) -> Option<NodeId> {
        focus::focus_next(self, window)
    }

    pub fn focus_prev(&mut self, window: WindowId) -> Option<NodeId> {
        focus::focus_prev(self, window)
    }

    pub fn make_scroll(
        &mut self,
        window: WindowId,
        node: NodeId,
        config: ScrollConfig,
    ) -> Result<ScrollHandle, EngineError> {
        scroll::make_scroll(self, window, node, config)
    }

    pub fn set_scroll_content(
        &mut self,
        handle: &ScrollHandle,
        content: LogicalSize,
    ) -> Result<(), EngineError> {
        scroll::set_scroll_content(self, handle, content)
    }

    pub fn scroll_to(
        &mut self,
        handle: &ScrollHandle,
        offset: LogicalPoint,
    ) -> Result<(), EngineError> {
        scroll::scroll_to(self, handle, offset)
    }

    fn normalize_scroll_offsets(&mut self, window: WindowId) {
        scroll::normalize_scroll_offsets(self, window)
    }

    fn clamp_scroll_offsets(&mut self, window: WindowId) -> bool {
        scroll::clamp_scroll_offsets(self, window)
    }

    pub fn scroll_by(
        &mut self,
        handle: &ScrollHandle,
        delta: LogicalPoint,
    ) -> Result<(), EngineError> {
        scroll::scroll_by(self, handle, delta)
    }

    #[must_use]
    pub fn scroll_state(&self, handle: &ScrollHandle) -> Option<ScrollState> {
        scroll::scroll_state(self, handle)
    }

    fn scroll_to_animated(
        &mut self,
        handle: &ScrollHandle,
        target: LogicalPoint,
    ) -> Result<(), EngineError> {
        scroll::scroll_to_animated(self, handle, target)
    }

    pub fn make_virtual_list(
        &mut self,
        window: WindowId,
        container: NodeId,
        config: VirtualListConfig,
    ) -> Result<VirtualList, EngineError> {
        virtual_list::make_virtual_list(self, window, container, config)
    }

    pub fn set_virtual_items(
        &mut self,
        list: &VirtualList,
        count: usize,
        build: impl FnMut(usize) -> NodeBuilder + 'static,
    ) -> Result<(), EngineError> {
        virtual_list::set_virtual_items(self, list, count, build)
    }

    #[must_use]
    pub fn virtual_list_state(&self, list: &VirtualList) -> Option<VirtualListState> {
        virtual_list::virtual_list_state(self, list)
    }

    #[must_use]
    pub fn virtual_row_index(&self, list: &VirtualList, row: NodeId) -> Option<usize> {
        virtual_list::virtual_row_index(self, list, row)
    }

    pub fn set_virtual_extents(
        &mut self,
        list: &VirtualList,
        extents: &[LogicalPx],
    ) -> Result<(), EngineError> {
        virtual_list::set_virtual_extents(self, list, extents)
    }

    pub fn scroll_to_item(
        &mut self,
        list: &VirtualList,
        index: usize,
        align: ItemAlign,
    ) -> Result<(), EngineError> {
        virtual_list::scroll_to_item(self, list, index, align)
    }

    fn rewindow_virtuals(&mut self, window: WindowId) {
        virtual_list::rewindow_virtuals(self, window)
    }

    fn rewindow_virtual(&mut self, window: WindowId, node: NodeId) {
        virtual_list::rewindow_virtual(self, window, node)
    }

    pub fn create_overlay(
        &mut self,
        window: WindowId,
        options: OverlayOptions,
    ) -> Result<Overlay, EngineError> {
        overlay::create_overlay(self, window, options)
    }

    pub fn close_overlay(&mut self, overlay: &Overlay) -> bool {
        overlay::close_overlay(self, overlay)
    }

    fn resolve_overlay_placements(&mut self, window: WindowId) {
        overlay::resolve_overlay_placements(self, window)
    }

    pub fn place_overlay(&mut self, overlay: &Overlay, placement: OverlayPlacement) -> bool {
        overlay::place_overlay(self, overlay, placement)
    }

    pub fn bring_overlay_to_front(&mut self, overlay: &Overlay) -> bool {
        overlay::bring_overlay_to_front(self, overlay)
    }

    pub fn overlays(&self, window: WindowId) -> Vec<(NodeId, OverlayDismiss)> {
        overlay::overlays(self, window)
    }

    fn dismiss_overlay(&mut self, window: WindowId) -> bool {
        overlay::dismiss_overlay(self, window)
    }

    pub fn remove_node(
        &mut self,
        window: WindowId,
        node: NodeId,
    ) -> Result<RemoveOutcome, EngineError> {
        window_set::remove_node(self, window, node)
    }

    #[must_use = "scope 错配会不可恢复地释放其它组件的值（引擎只能拒绝可证明的错配），必须检查返回的 RemoveOutcome"]
    pub fn remove_node_with_scope(
        &mut self,
        window: WindowId,
        node: NodeId,
        scope: SignalScope,
    ) -> Result<RemoveOutcome, EngineError> {
        window_set::remove_node_with_scope(self, window, node, scope)
    }

    #[must_use]
    pub fn drag_state(&self, window: WindowId) -> Option<DragState> {
        input::drag_state(self, window)
    }

    #[must_use]
    pub fn drag(&self, state: &DragState) -> Option<DragInfo> {
        input::drag(self, state)
    }

    pub fn cancel_drag(&mut self, state: &DragState) -> bool {
        input::cancel_drag(self, state)
    }

    pub fn animate(
        &mut self,
        window: WindowId,
        value: AnimatedValue,
        nodes: &[NodeId],
        spec: AnimationSpec,
    ) -> Result<AnimationId, EngineError> {
        animation::animate(self, window, value, nodes, spec)
    }

    pub fn on_animation_complete(
        &mut self,
        handler: impl FnMut(&mut App, AnimationCompletion) + 'static,
    ) {
        self.animation_complete_handler = Some(Box::new(handler));
    }

    pub fn after(
        &mut self,
        window: WindowId,
        delay: Duration,
        callback: impl FnOnce(&mut App) + 'static,
    ) -> Result<TimerId, EngineError> {
        timer::after(self, window, None, delay, callback)
    }

    pub fn after_scoped(
        &mut self,
        window: WindowId,
        scope: NodeId,
        delay: Duration,
        callback: impl FnOnce(&mut App) + 'static,
    ) -> Result<TimerId, EngineError> {
        timer::after(self, window, Some(scope), delay, callback)
    }

    pub fn cancel_timer(&mut self, id: &TimerId) -> bool {
        timer::cancel(self, id)
    }

    pub(crate) fn next_wake_deadline(&self) -> Option<Instant> {
        #[cfg(target_os = "linux")]
        {
            let frames = self
                .windows()
                .filter_map(|(_, window)| window.frame_budget_deadline())
                .min();
            match (timer::next_deadline(self), frames) {
                (Some(timer), Some(frame)) => Some(timer.min(frame)),
                (Some(timer), None) => Some(timer),
                (None, Some(frame)) => Some(frame),
                (None, None) => None,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            timer::next_deadline(self)
        }
    }

    #[cfg(all(test, feature = "test-support"))]
    #[must_use]
    pub(crate) fn timer_count_for_test(&self) -> usize {
        timer::count(self)
    }

    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn take_pump_wake_for_test(&self) -> bool {
        self.dispatcher.take_frame_wake()
    }

    #[cfg(all(test, feature = "test-support"))]
    #[must_use]
    pub(crate) fn pending_animation_completions_for_test(&self) -> usize {
        self.animation_completions.len()
    }

    pub fn cancel_animation(&mut self, id: &AnimationId) -> bool {
        animation::cancel_animation(self, id)
    }

    #[must_use]
    pub fn animations_running(&self, window: WindowId) -> bool {
        animation::animations_running(self, window)
    }

    pub(crate) fn has_animations(&self) -> bool {
        animation::has_animations(self)
    }

    pub(crate) fn needs_continuous_frames(&self) -> bool {
        animation::needs_continuous_frames(self)
    }

    #[cfg(any(windows, target_os = "macos"))]
    pub(crate) fn has_deferred_frames(&self) -> bool {
        self.windows()
            .any(|(_, window)| window.frame_budget_deferred())
    }

    fn mark_nodes_paint(&mut self, window: WindowId, nodes: &[NodeId]) {
        animation::mark_nodes_paint(self, window, nodes)
    }

    fn tick_animations(&mut self, window: WindowId) {
        animation::tick_animations(self, window)
    }

    pub(crate) fn sync_clock(&mut self) {
        animation::sync_clock(self)
    }

    pub(crate) fn sync_clock_live(&mut self) {
        animation::sync_clock_live(self)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn use_engine_clock(&mut self) {
        self.clock_live = false;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn advance_clock(&mut self, by: Duration) {
        animation::advance_clock(self, by)
    }

    pub(crate) fn take_cursor_change(&mut self) -> Option<(WindowId, Cursor)> {
        input::take_cursor_change(self)
    }

    pub(crate) fn take_capture_requests(&mut self) -> Vec<(WindowId, bool)> {
        input::take_capture_requests(self)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn clock(&self) -> Instant {
        self.clock
    }

    pub(crate) fn ime_enabled(&self, window: WindowId) -> bool {
        focus::ime_enabled(self, window)
    }

    pub fn create_window(&mut self, options: WindowOptions) -> WindowId {
        window_set::create_window(self, options)
    }

    #[must_use]
    pub fn window(&self, id: WindowId) -> Option<&Window> {
        window_set::window(self, id)
    }

    #[must_use]
    pub fn last_frame_presentation(&self, window: WindowId) -> Option<FramePresentation> {
        self.window(window)
            .and_then(Window::last_frame_presentation)
    }

    pub fn window_mut(&mut self, id: WindowId) -> Option<&mut Window> {
        window_set::window_mut(self, id)
    }

    pub fn set_drag_regions(&mut self, window: WindowId, regions: &[LogicalRect]) {
        window_set::set_drag_regions(self, window, regions)
    }

    pub fn on_close(&mut self, id: WindowId, handler: impl FnMut(&mut Self) -> bool + 'static) {
        window_set::on_close(self, id, handler)
    }

    pub(crate) fn request_close(&mut self, id: WindowId) -> bool {
        window_set::request_close(self, id)
    }

    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn request_close_all(&mut self, ids: &[WindowId]) -> bool {
        window_set::request_close_all(self, ids)
    }

    pub fn close_window(&mut self, id: WindowId) -> bool {
        window_set::close_window(self, id)
    }

    pub fn request_window_control(
        &mut self,
        window: WindowId,
        control: crate::window::WindowControl,
    ) -> Result<(), EngineError> {
        window_set::request_window_control(self, window, control)
    }

    pub(crate) fn take_window_control(
        &mut self,
    ) -> Option<(WindowId, crate::window::WindowControl)> {
        window_set::take_window_control(self)
    }

    pub fn set_title(
        &mut self,
        window: WindowId,
        title: smol_str::SmolStr,
    ) -> Result<(), EngineError> {
        window_set::set_title(self, window, title)
    }

    pub fn set_traffic_light_position(
        &mut self,
        window: WindowId,
        inset: crate::math::LogicalPoint,
    ) -> Result<(), EngineError> {
        window_set::set_traffic_light_position(self, window, inset)
    }

    #[must_use]
    pub fn traffic_light_inset(&self, window: WindowId) -> Option<crate::math::LogicalPoint> {
        window_set::traffic_light_inset(self, window)
    }

    pub fn windows(&self) -> impl Iterator<Item = (WindowId, &Window)> {
        window_set::windows(self)
    }

    #[cfg(feature = "hud")]
    pub fn set_debug_hud(&mut self, window: WindowId, enabled: bool) -> Result<(), EngineError> {
        debug::set_debug_hud(self, window, enabled)
    }

    #[cfg(feature = "hud")]
    #[must_use]
    pub fn debug_hud_enabled(&self, window: WindowId) -> bool {
        debug::debug_hud_enabled(self, window)
    }

    #[cfg(feature = "hud")]
    fn prepare_debug_hud(&mut self, window: WindowId) {
        debug::prepare_debug_hud(self, window)
    }

    #[cfg(feature = "hud")]
    fn apply_debug_hud(&mut self, window: WindowId) {
        debug::apply_debug_hud(self, window)
    }

    #[cfg(feature = "hud")]
    fn raise_debug_hud(&mut self, window: WindowId) {
        debug::raise_debug_hud(self, window)
    }

    pub fn refresh_styles(&mut self, window: WindowId) -> Option<StylePassReport> {
        let report = style_pass::refresh_styles(self, window);
        if report.is_some() {
            self.refresh_a11y_bindings(window);
        }
        report
    }

    pub fn on_a11y_action(
        &mut self,
        action: crate::a11y::A11yAction,
        handler: impl FnMut(&mut crate::a11y::A11yActionCtx<'_>) + 'static,
    ) -> bool {
        a11y::on_a11y_action(self, action, handler)
    }

    pub fn off_a11y_action(&mut self, action: crate::a11y::A11yAction) -> bool {
        a11y::off_a11y_action(self, action)
    }

    pub fn set_a11y_patch(
        &mut self,
        window: WindowId,
        node: NodeId,
        patch: crate::a11y::A11yPatch,
    ) -> Result<(), EngineError> {
        a11y::set_a11y_patch(self, window, node, patch)
    }

    #[must_use]
    pub fn a11y_diagnostics(&self) -> &[crate::a11y::A11yDiagnostic] {
        a11y::a11y_diagnostics(self)
    }

    pub(crate) fn a11y_note(&mut self, diagnostic: crate::a11y::A11yDiagnostic) {
        a11y::a11y_note(self, diagnostic)
    }

    pub(crate) fn a11y_has_handler(&self, action: crate::a11y::A11yAction) -> bool {
        a11y::a11y_has_handler(self, action)
    }

    pub(crate) fn a11y_take_handler(
        &mut self,
        action: crate::a11y::A11yAction,
    ) -> Option<Box<dyn FnMut(&mut crate::a11y::A11yActionCtx<'_>)>> {
        a11y::a11y_take_handler(self, action)
    }

    pub(crate) fn a11y_put_handler(
        &mut self,
        action: crate::a11y::A11yAction,
        handler: Box<dyn FnMut(&mut crate::a11y::A11yActionCtx<'_>)>,
    ) {
        a11y::a11y_put_handler(self, action, handler)
    }

    #[must_use]
    pub(crate) fn a11y_pending(&self, window: WindowId) -> bool {
        a11y::a11y_pending(self, window)
    }

    #[must_use]
    pub(crate) fn window_a11y_epoch(
        &self,
        window: WindowId,
    ) -> Option<crate::a11y::adapter::A11yConsumerEpoch> {
        a11y::window_a11y_epoch(self, window)
    }

    pub(crate) fn a11y_retry_pending(&self) -> bool {
        a11y::a11y_retry_pending(self)
    }

    #[must_use]
    pub(crate) fn a11y_backlog_pending(&self) -> bool {
        a11y::a11y_backlog_pending(self)
    }

    pub(crate) fn wake_for_a11y_backlog(
        &mut self,
        dispatcher: &crate::platform::dispatcher::Dispatcher,
    ) {
        a11y::wake_for_a11y_backlog(self, dispatcher)
    }
    pub(crate) fn a11y_take(&mut self, window: WindowId) -> Option<crate::a11y::A11yTreeSnapshot> {
        a11y::a11y_take(self, window)
    }

    pub(crate) fn refresh_a11y_bindings(&mut self, window: WindowId) {
        a11y::refresh_a11y_bindings(self, window)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn set_a11y_capture(&mut self, window: WindowId, on: bool) {
        a11y::set_a11y_capture(self, window, on)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn attach_a11y(
        &mut self,
        window: WindowId,
        snapshot: crate::a11y::A11yTreeSnapshot,
    ) {
        a11y::attach_a11y(self, window, snapshot)
    }

    pub(crate) fn poll_renderers(&mut self) -> Result<(), EngineError> {
        resources::poll_renderers(self)
    }

    pub fn frame(&mut self, window: WindowId) -> Option<Result<FrameOutcome, EngineError>> {
        frame::frame(self, window, false)
    }

    #[must_use]
    pub fn frame_refresh_hz(&self, window: WindowId) -> Option<f32> {
        self.window(window).and_then(Window::frame_refresh_hz)
    }

    #[must_use]
    pub fn frame_budget_deferrals(&self, window: WindowId) -> u64 {
        self.window(window)
            .map_or(0, Window::frame_budget_deferrals)
    }

    #[cfg(any(windows, target_os = "macos"))]
    pub(crate) fn frame_forced(
        &mut self,
        window: WindowId,
    ) -> Option<Result<FrameOutcome, EngineError>> {
        frame::frame(self, window, true)
    }

    fn abort_frame(&mut self, window: WindowId, ticket: FrameTicket) {
        frame::abort_frame(self, window, ticket)
    }

    fn record_idle(&mut self, window: WindowId) {
        frame::record_idle(self, window)
    }

    pub fn refresh_text(
        &mut self,
        window: WindowId,
    ) -> Option<Result<TextPassReport, EngineError>> {
        text_pass::refresh_text(self, window)
    }

    pub fn layout_text(
        &mut self,
        window: WindowId,
    ) -> Option<Result<TextLayoutReport, EngineError>> {
        let result = text_pass::layout_text(self, window);
        self.sync_atlas_generation_lease();
        result
    }

    pub fn shape_text(
        &mut self,
        text: &str,
        style: &TextStyle,
        max_width: Option<LogicalPx>,
        inline_boxes: &[InlineBoxSpec],
    ) -> Result<ShapedTextHandle, EngineError> {
        let App {
            text: text_context,
            raster,
            atlas,
            glyph_atlas,
            pending_dirty,
            atlas_generation_lease,
            default_text_scale,
            windows,
            ..
        } = self;
        let scale = *default_text_scale;
        let result = crate::text::standalone::shape(
            text_context,
            raster,
            atlas,
            glyph_atlas,
            pending_dirty,
            atlas_generation_lease,
            text,
            style,
            max_width,
            inline_boxes,
            scale,
        );
        let evicted = glyph_atlas.take_evicted();
        if evicted > 0 {
            for window in windows.values_mut() {
                window.tree_mut().invalidate_text_lines();
                window.tree_mut().mark_canvas_nodes_paint();
            }
        }
        self.sync_atlas_generation_lease();
        result
    }

    #[cfg(test)]
    pub(crate) fn shape_count_for_test(&self) -> u32 {
        self.text.shape_count()
    }

    #[must_use]
    pub fn shaped_inline_boxes<'a>(&self, handle: &'a ShapedTextHandle) -> &'a [ShapedInlineBox] {
        handle.inline_boxes()
    }

    fn sync_atlas_generation_lease(&self) {
        self.atlas_generation_lease.store(
            crate::text::pack_atlas_lease(
                self.atlas.generation(),
                self.glyph_atlas.eviction_epoch(),
            ),
            Ordering::Relaxed,
        );
    }

    #[must_use]
    pub fn text_scale(&self) -> f32 {
        self.default_text_scale
    }

    pub fn set_text_scale(&mut self, scale: f32) -> Result<(), EngineError> {
        if !scale.is_finite() || scale <= 0.0 {
            return Err(EngineError::InvalidTextScale { scale });
        }
        self.text_scale_override = Some(scale);
        self.default_text_scale = scale;
        Ok(())
    }

    pub fn clear_text_scale_override(&mut self) {
        self.text_scale_override = None;
        self.follow_focused_window_scale();
    }

    pub(crate) fn note_window_focus(&mut self, window: WindowId, focused: bool) {
        if focused {
            self.focused_window = Some(window);
            self.follow_focused_window_scale();
        } else if self.focused_window == Some(window) {
            self.focused_window = None;
        }
    }

    pub(crate) fn note_window_dpi(&mut self, window: WindowId, scale: DevicePixelRatio) {
        if self.focused_window == Some(window) {
            self.apply_text_scale(scale.factor());
        }
    }

    fn apply_text_scale(&mut self, scale: f32) {
        if self.text_scale_override.is_some() {
            return;
        }
        self.default_text_scale = scale;
    }

    fn follow_focused_window_scale(&mut self) {
        let Some(window) = self.focused_window else {
            return;
        };
        let scale = self
            .windows
            .get(&window)
            .map(|target| target.scale().factor());
        if let Some(scale) = scale {
            self.apply_text_scale(scale);
        }
    }

    #[cfg(test)]
    pub(crate) fn cap_glyph_atlas_page_for_test(&mut self, page: u32) {
        self.atlas.cap_page_for_test(page);
        self.glyph_atlas.clear();
        self.sync_atlas_generation_lease();
    }

    #[cfg(test)]
    pub(crate) fn glyph_atlas_generation_for_test(&self) -> u32 {
        self.atlas.generation()
    }

    #[cfg(test)]
    pub(crate) fn glyph_atlas_eviction_epoch_for_test(&self) -> u32 {
        self.glyph_atlas.eviction_epoch()
    }

    #[must_use]
    pub fn font_stack(&self) -> &FontStack {
        text_pass::font_stack(self)
    }

    #[must_use]
    pub fn font_stack_version(&self) -> u32 {
        text_pass::font_stack_version(self)
    }

    #[must_use]
    pub fn text_diagnostics(&self) -> &[TextDiagnostic] {
        text_pass::text_diagnostics(self)
    }

    pub fn font_stack_status(&mut self) -> FontStackStatus {
        text_pass::font_stack_status(self)
    }

    pub fn set_font_stack(&mut self, stack: FontStack) -> u32 {
        let version = text_pass::set_font_stack(self, stack);
        self.sync_atlas_generation_lease();
        version
    }

    pub fn reload_system_fonts(&mut self) -> u32 {
        let version = text_pass::reload_system_fonts(self);
        self.sync_atlas_generation_lease();
        version
    }

    pub fn set_digit_fast_path(&mut self, enabled: bool) {
        text_pass::set_digit_fast_path(self, enabled);
    }

    #[must_use]
    pub fn digit_fast_path_enabled(&self) -> bool {
        text_pass::digit_fast_path_enabled(self)
    }

    fn repaint_text_if_atlas_changed(&mut self) {
        text_pass::repaint_text_if_atlas_changed(self);
        self.sync_atlas_generation_lease();
    }

    fn invalidate_all_text(&mut self) {
        text_pass::invalidate_all_text(self)
    }

    fn invalidate(&mut self, signal: SignalId) {
        style_pass::invalidate(self, signal)
    }

    #[cfg(test)]
    pub(crate) fn eval_stack_is_empty_for_test(&self) -> bool {
        signal_api::eval_stack_is_empty_for_test(self)
    }

    #[cfg(feature = "automation")]
    pub fn inject_input(
        &mut self,
        window: WindowId,
        event: crate::event::InputEvent,
    ) -> crate::event::InputOutcome {
        automation::inject_input(self, window, event)
    }

    #[cfg(feature = "automation")]
    #[must_use]
    pub fn frame_index(&self, window: WindowId) -> Option<crate::window::FrameIndex> {
        automation::frame_index(self, window)
    }

    #[cfg(feature = "automation")]
    #[must_use]
    pub fn painted_primitives(&self, window: WindowId) -> usize {
        automation::painted_primitives(self, window)
    }

    #[cfg(feature = "automation")]
    #[must_use]
    pub fn texts(&self, window: WindowId) -> Vec<smol_str::SmolStr> {
        automation::texts(self, window)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod interaction_tests;

#[cfg(test)]
mod resources_tests;

#[cfg(test)]
mod invalidate_tests;

#[cfg(all(test, feature = "test-support"))]
mod image_tests;
#[cfg(test)]
mod scoped_tests;

#[cfg(all(test, feature = "test-support"))]
mod e10_tests;

#[cfg(all(test, feature = "test-support"))]
mod e17_tests;

#[cfg(all(test, feature = "test-support"))]
mod removal_tests;

#[cfg(all(test, feature = "automation"))]
mod automation_tests;

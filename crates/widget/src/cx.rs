//! The one context a widget is handed: services, window, tree, and engine state.
//!
//! A [`WidgetCx`] is a borrowed view of everything a widget needs to build nodes and
//! to drive its own state, assembled by the framework from the four things it holds:
//! the application context, the window the widget lives in, the injected
//! [`WidgetServices`], and the widget's own [`SignalScope`] plus [`FrameRouter`].
//! Nothing here is a thread-local, a global, or a type-lookup: a test composes the
//! same context the application does.
//!
//! # Borrow rules
//!
//! The node tree lives inside [`Window`], which lives inside [`App`], so a
//! signature that took `&mut App` and `&mut NodeTree` at once could not be called.
//! The context therefore owns its borrows and hands out one access at a time:
//!
//! ```text
//! cx.app()          // &App        — reads, window queries, hovered/focused
//! cx.app_mut()      // &mut App    — action handlers, signals outside the widget scope
//! cx.tree()         // &NodeTree   — read nodes this widget built
//! cx.tree_mut()     // &mut NodeTree — build nodes
//! ```
//!
//! A widget builds with `cx.tree_mut()`, and calls an `app`-form kit component with
//! `cx.app_mut()` plus [`WidgetCx::window`]. Inside a widget method the context is
//! borrowed mutably, so the two are naturally taken one after the other.
//!
//! # Language dependency
//!
//! The engine registers a dependency only for reads that happen inside a
//! `bind_*` closure. A widget that draws localized text therefore reads the
//! language signal **inside** the closure that draws it:
//!
//! ```text
//! let language = cx.language();
//! let translate = cx.translate().clone();
//! let node = NodeBuilder::text("", TextStyle::new(13.0))
//!     .bind_text(move |rcx| {
//!         let _language = rcx.read(&language); // ← registers the dependency
//!         t_smol!(translate, "widget.key")
//!     })
//!     .build(cx.tree_mut(), parent)?;
//! ```
//!
//! Writing the language signal then invalidates exactly the nodes whose closure
//! read it, and the next frame draws the new language. A read outside a `bind_*`
//! closure — for example in [`WidgetView::load`](crate::view::WidgetView::load) —
//! is not tracked, which is correct: loading does not draw.

use app_config::settings::Config;
use base_ui::theme::handle::ThemeHandle;
use i18n::{language::codes::LanguageCode, translate::Translate};
use smol_str::SmolStr;
use tokio::runtime::Handle;
use ui_core::{
    a11y::A11yPatch,
    app::{App, AppHandle},
    error::EngineError,
    node::{
        id::NodeId,
        signal::{Signal, SignalRef, SignalScope, Transient},
        tree::NodeTree,
    },
    window::WindowId,
};

use super::{frame::FrameRouter, services::WidgetServices};

/// Everything one widget gets to work with, for one call.
///
/// See the module documentation for the borrow rules. The context is created by the
/// framework — a widget never constructs one — and it is only valid for the call it
/// was passed to; a widget that needs state across calls keeps `Signal` handles,
/// not the context.
pub struct WidgetCx<'a> {
    app: &'a mut App,
    window: WindowId,
    services: &'a WidgetServices,
    scope: SignalScope,
    router: FrameRouter,
}

impl<'a> WidgetCx<'a> {
    /// Assembles a context. The framework is the only caller; tests reach it
    /// through [`WidgetTestApp`](crate::test_support::WidgetTestApp).
    pub fn new(
        app: &'a mut App,
        window: WindowId,
        services: &'a WidgetServices,
        scope: SignalScope,
        router: FrameRouter,
    ) -> Self {
        Self {
            app,
            window,
            services,
            scope,
            router,
        }
    }

    /// The application context, for reads.
    pub fn app(&self) -> &App {
        self.app
    }

    /// The application context, for work only a widget can route (its own action
    /// handlers, window-level bindings).
    pub fn app_mut(&mut self) -> &mut App {
        self.app
    }

    /// The window this widget's nodes belong to.
    pub const fn window(&self) -> WindowId {
        self.window
    }

    /// The node tree of that window.
    ///
    /// # Panics
    /// The widget's window is gone. The framework releases a widget's surface
    /// before its window closes, so a live widget always has a window.
    pub fn tree(&self) -> &NodeTree {
        self.app
            .window(self.window)
            .expect("a live widget's window exists")
            .tree()
    }

    /// The node tree of that window, for building nodes.
    ///
    /// # Panics
    /// The widget's window is gone; see [`WidgetCx::tree`].
    pub fn tree_mut(&mut self) -> &mut NodeTree {
        self.app
            .window_mut(self.window)
            .expect("a live widget's window exists")
            .tree_mut()
    }

    /// The injected services.
    pub const fn services(&self) -> &WidgetServices {
        self.services
    }

    /// The translation context: the widget's only source of interface text.
    pub const fn translate(&self) -> &Translate {
        &self.services.translate
    }

    /// The selected language as an engine signal.
    ///
    /// Read it inside a `bind_*` closure to follow language changes; see the module
    /// documentation.
    pub const fn language(&self) -> Signal<LanguageCode> {
        self.services.language
    }

    /// The theme handle. Colours must be read through it inside a `bind_style`
    /// closure (the engine's G3 rule), not captured at build time.
    pub const fn theme(&self) -> &ThemeHandle {
        &self.services.theme
    }

    /// Runs `read` with a borrowed view of the current settings snapshot.
    ///
    /// A read here does not create a dependency — it is for decisions taken in
    /// `sync`/`load`. A widget that must redraw when a setting moves reads
    /// [`WidgetServices::config`](crate::services::WidgetServices::config) inside
    /// the closure that draws. The framework's surface root also reads that signal,
    /// so a settings write schedules a frame even while the widget has no body — its
    /// `sync` then sees the new snapshot. The snapshot is not cloned: a per-frame or
    /// per-row read (the display time zone, the account) borrows it for the duration
    /// of the call.
    pub fn config_ref<R>(&self, read: impl FnOnce(&Config) -> R) -> R {
        self.app.read_with(&self.services.config, read)
    }

    /// The account the settings currently scope the widget to, if any.
    ///
    /// It is derived from the settings snapshot rather than kept as a second
    /// signal: two sources of "which account" could disagree.
    pub fn account(&self) -> Option<SmolStr> {
        self.config_ref(crate::account::account_of)
    }

    /// The display time zone the settings currently select.
    pub fn data_time_zone(&self) -> time::UtcOffset {
        self.config_ref(crate::time_zone::data_time_zone)
    }

    /// The engine window the widget's nodes live in, for the drawing paths the
    /// context does not wrap.
    ///
    /// # Panics
    /// The widget's window is gone; see [`WidgetCx::tree`].
    pub fn window_mut(&mut self) -> &mut ui_core::window::Window {
        self.app
            .window_mut(self.window)
            .expect("a live widget's window exists")
    }

    /// Replaces the canvas of a node the widget built as a canvas (the drawing
    /// widget's path).
    ///
    /// Returns whether the canvas changed — the same value
    /// [`Window::set_canvas`](ui_core::window::Window::set_canvas) answers. A node
    /// built as anything but a canvas is not a drawing target and answers `false`.
    pub fn set_canvas(
        &mut self,
        node: NodeId,
        canvas: ui_core::paint::canvas::Canvas,
    ) -> Result<bool, EngineError> {
        self.window_mut().set_canvas(node, canvas)
    }

    /// The widget's signal scope. Every signal the widget mints through this context
    /// belongs to it, and the framework releases the scope on teardown.
    pub const fn scope(&self) -> SignalScope {
        self.scope
    }

    /// The frame router, so a widget can register the components it drives.
    pub const fn router(&self) -> &FrameRouter {
        &self.router
    }

    /// Mints a signal in the widget's scope.
    pub fn signal<T: Clone + 'static>(&mut self, initial: T) -> Signal<T> {
        self.app.signal_scoped(self.scope, initial)
    }

    /// Mints a transient value in the widget's scope.
    pub fn transient<T: Clone + 'static>(&mut self, initial: T) -> Transient<T> {
        self.app.transient_scoped(self.scope, initial)
    }

    /// Reads a signal (no dependency: this is not a `bind_*` closure).
    pub fn read<T: 'static>(&self, signal: &Signal<T>) -> SignalRef<T> {
        self.app.read(signal)
    }

    /// Writes a signal; the return value says whether the value changed.
    pub fn write<T: Clone + PartialEq + 'static>(&mut self, signal: &Signal<T>, value: T) -> bool {
        self.app.write(signal, value)
    }

    /// Updates a signal in place; the return value says whether the value changed.
    pub fn update<T: 'static>(&mut self, signal: &Signal<T>, update: impl FnOnce(&mut T)) -> bool {
        self.app.update(signal, update)
    }

    /// Requests a repaint.
    ///
    /// This replaces the toolkit's `cx.notify()`: a widget that coordinates several
    /// signals in one step, or repaints a canvas, asks for the frame explicitly
    /// instead of relying on a write.
    pub fn request_frame(&mut self) -> Result<(), EngineError> {
        self.app.request_repaint(self.window)
    }

    /// Requests a repaint of one node's lane.
    pub fn request_node_repaint(&mut self, node: NodeId) -> Result<bool, EngineError> {
        self.app.request_node_repaint(self.window, node)
    }

    /// Writes one a11y delta (role/label/state) on a node the widget built.
    pub fn set_a11y_patch(&mut self, node: NodeId, patch: A11yPatch) -> Result<(), EngineError> {
        self.app.set_a11y_patch(self.window, node, patch)
    }

    /// The application handle: the only way work leaves the UI thread and comes
    /// back.
    pub fn handle(&self) -> AppHandle {
        self.app.handle()
    }

    /// Runs a future on the shared runtime, with no result channel. Work that needs
    /// to come back to the interface uses [`AppHandle::send`].
    pub fn spawn(&mut self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        self.app.handle().spawn(future);
    }

    /// The Tokio runtime the widget's streams and RPC calls are driven on.
    pub const fn runtime(&self) -> &Handle {
        &self.services.runtime
    }
}

impl std::fmt::Debug for WidgetCx<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WidgetCx")
            .field("window", &self.window)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use app_config::settings::{AccountConfig, AccountWalletKind, Config};
    use ui_core::{
        math::Rect,
        node::{builder::NodeBuilder, tree::NodeKind},
        paint::canvas::Canvas,
    };

    use crate::test_support::WidgetTestApp;

    #[test]
    fn a_context_builds_nodes_in_the_widget_s_own_scope() {
        let mut harness = WidgetTestApp::new();
        let signal = harness.with_cx(|cx| cx.signal(7_u32));

        assert_eq!(*harness.app().read(&signal), 7);

        let node = harness.with_cx(|cx| {
            let parent = cx.tree().root();
            NodeBuilder::boxed()
                .build(cx.tree_mut(), parent)
                .expect("the root is alive")
        });
        let tree = harness.tree();
        assert_eq!(
            tree.get(node).expect("the node exists").kind(),
            &NodeKind::Box
        );
    }

    #[test]
    fn releasing_the_widget_scope_releases_the_signals_it_minted() {
        let mut harness = WidgetTestApp::new();
        let released = harness.with_cx(|cx| {
            let scope = cx.scope();
            cx.app_mut().release_scope(scope)
        });

        assert!(released > 0, "the harness's widget scope owns its signals");
    }

    /// The settings view is borrowed, not cloned: the accessors read the snapshot the
    /// services signal holds, including the account and the display time zone.
    #[test]
    fn the_settings_accessors_read_the_snapshot_without_cloning_it() {
        let mut harness = WidgetTestApp::new();
        let mut config = Config::default();
        config.account = Some(AccountConfig {
            user_id: String::from("11111111-1111-4111-8111-111111111111"),
            wallet_address: String::from("0x0000000000000000000000000000000000000001"),
            wallet_kind: AccountWalletKind::Ethereum,
        });
        config.data_time_zone =
            app_config::time_zone::DataTimeZone::from_hours(8).expect("UTC+8 is offered");
        let config_signal = harness.services().config;
        assert!(harness.app_mut().write(&config_signal, config));

        harness.with_cx(|cx| {
            assert_eq!(
                cx.account().as_deref(),
                Some("11111111-1111-4111-8111-111111111111")
            );
            assert_eq!(cx.data_time_zone().whole_hours(), 8);
            assert_eq!(
                cx.config_ref(|config| crate::time_zone::data_time_zone(config).whole_hours()),
                8,
                "the same snapshot is visible through the borrowed read"
            );
        });
    }

    /// The drawing paths the context does not wrap are one call away: the window and
    /// the canvas of a node the widget built as a canvas.
    #[test]
    fn the_drawing_accessors_replace_a_node_canvas() {
        let mut harness = WidgetTestApp::new();
        let node = harness.with_cx(|cx| {
            let parent = cx.tree().root();
            NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 8.0, 8.0)))
                .build(cx.tree_mut(), parent)
                .expect("the root is alive")
        });

        let changed = harness
            .with_cx(|cx| {
                cx.window_mut();
                cx.set_canvas(node, Canvas::new(Rect::new(0.0, 0.0, 16.0, 16.0)))
            })
            .expect("the node is alive");

        assert!(changed);
        assert!(matches!(
            harness.tree().get(node).expect("the node exists").kind(),
            NodeKind::Canvas(_)
        ));
    }
}

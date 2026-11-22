//! The one context a widget is handed: services, window, tree, and engine state.
//!
//! A [`WidgetCx`] is a borrowed view of everything a widget needs to build nodes and
//! to drive its own state, assembled by the framework from the four things it holds:
//! the application context, the window the widget lives in, the injected
//! [`WidgetServices`], and the widget's own [`SignalScope`]. The frame router the
//! widget's routes belong to is the one in the services
//! ([`WidgetCx::frame_router`]), next to the layer stack ([`WidgetCx::layer`]) and the
//! animation router ([`WidgetCx::animation`]) the composition root created. Nothing
//! here is a thread-local, a global, or a type-lookup: a test composes the same
//! context the application does.
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
//! # The composition-root singletons
//!
//! Three collaborators are application-level instances the composition root creates
//! exactly once and injects through [`WidgetServices`]:
//!
//! - [`WidgetCx::layer`] — the one layer stack that decides which overlay is on top;
//! - [`WidgetCx::frame_router`] — the one owner of the engine's frame hook;
//! - [`WidgetCx::animation`] — the one owner of the engine's animation-completion
//!   hook.
//!
//! A widget **must not build its own**. A second layer stack splits the z-order (two
//! overlays opened through different stacks disagree about who is on top); a second
//! animation router silently takes the engine's single completion slot over and the
//! overlays that registered with the first one never finish fading out; a second
//! frame router cannot install itself at all (the slot is taken, `FrameRouter::new`
//! reports it) and its routes would never run. The context hands out the instances it
//! was given — clone them into the kit component being built (an `Overlay`, a dialog,
//! a selector host), never construct replacements.
//!
//! A host that owns the aggregate itself (a selector host, a dialog's presenter) takes
//! it through [`WidgetCx::services_rc`]: the `Rc` the composition root built, cloned —
//! never a copy of the collaborator set. The widget and its host therefore share one
//! aggregate, just as they share the three singletons inside it.
//!
//! # Component keys
//!
//! A kit component's `key` is an **application-level** action identity: a
//! `Button::on_click` registers `"<key>.click"` with [`App::on_action`], and a second
//! live component that passes the same key silently replaces the first one's handler —
//! then releasing either one unregisters it for both. A widget that builds controls
//! into two panes must therefore never pass a constant key; it asks the context for
//! one that carries the instance's identity:
//!
//! ```text
//! let key = cx.unique_key("market_trade.header.sound"); // "market_trade.header.sound@7"
//! ```
//!
//! [`WidgetCx::unique_key`] suffixes the widget-local name with the instance's signal
//! scope, which the engine mints once per widget mount and never recycles: two panes
//! cannot collide, and one instance mints the same key on every rebuild.
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

use std::rc::Rc;

use app_config::settings::Config;
use base_ui::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::handle::ThemeHandle,
};
use i18n::{language::codes::LanguageCode, translate::Translate};
use smol_str::{SmolStr, format_smolstr};
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
    services: &'a Rc<WidgetServices>,
    scope: SignalScope,
}

impl<'a> WidgetCx<'a> {
    /// Assembles a context. The framework is the only caller; tests reach it
    /// through [`WidgetTestApp`](crate::test_support::WidgetTestApp).
    ///
    /// The aggregate is taken as the `Rc` the composition root owns, not as a bare
    /// reference: a widget that builds a host owning services takes the handle from
    /// [`WidgetCx::services_rc`] and clones it, so the host works over the one
    /// aggregate the application built.
    pub fn new(
        app: &'a mut App,
        window: WindowId,
        services: &'a Rc<WidgetServices>,
        scope: SignalScope,
    ) -> Self {
        Self {
            app,
            window,
            services,
            scope,
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
    pub fn services(&self) -> &WidgetServices {
        self.services
    }

    /// The injected services as the shared handle the composition root handed over.
    ///
    /// A widget that builds a **host owning services** (a selector host, a dialog's
    /// presenter) needs the `Rc` itself: clone this handle into the host
    /// (`Rc::clone(cx.services_rc())`) so the two work over the application's one
    /// aggregate. Assembling a second `WidgetServices` — or copying its fields — would
    /// give the host a collaborator set that drifts from the application's (the three
    /// singletons inside are the same instances, the signals are the same slots, but
    /// the aggregate identity is what a later field would be missing from).
    pub fn services_rc(&self) -> &Rc<WidgetServices> {
        self.services
    }

    /// The translation context: the widget's only source of interface text.
    pub fn translate(&self) -> &Translate {
        &self.services.translate
    }

    /// The selected language as an engine signal.
    ///
    /// Read it inside a `bind_*` closure to follow language changes; see the module
    /// documentation.
    pub fn language(&self) -> Signal<LanguageCode> {
        self.services.language
    }

    /// The theme handle. Colours must be read through it inside a `bind_style`
    /// closure (the engine's G3 rule), not captured at build time.
    pub fn theme(&self) -> &ThemeHandle {
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

    /// Mints the stable, per-instance key a kit component's action identity and debug
    /// id are built from.
    ///
    /// A kit component's `key` is **application-level**: `Button::on_click` registers
    /// the action `"<key>.click"` with [`App::on_action`], and [`App::on_action`]
    /// replaces an existing handler of the same action. Two live panes that build the
    /// same constant key therefore share one handler (the later pane wins), and the
    /// first pane to rebuild or release unregisters it for both — a click dispatches
    /// into the wrong pane, or into nothing. This method is the only valid source of
    /// such a key.
    ///
    /// The format is `"{suffix}@{scope}"`, where `scope` is this context's
    /// [`WidgetCx::scope`] index and `suffix` is the caller's local name:
    ///
    /// ```text
    /// cx.unique_key("market_trade.header.sound") // "market_trade.header.sound@7"
    /// ```
    ///
    /// # Stability and uniqueness
    ///
    /// - **Same instance, same suffix → the same key.** The prefix is the widget's
    ///   signal scope, fixed for the widget's lifetime, so a control rebuilt in
    ///   `sync`/`header` (a static-text rebuild, a language switch) mints the key it
    ///   already registered and can pair the new registration with the old release.
    /// - **Different instances → different keys.** Every widget mount mints its own
    ///   scope, and the engine appends a scope table entry per mount and never
    ///   recycles an index, so no two widget instances of one application share a
    ///   prefix — not even a released instance and one mounted later.
    /// - **The suffix is the widget's namespace.** It names the component site *inside*
    ///   the widget: two different controls need two different suffixes, and a repeated
    ///   control (a table row's button) puts its row identity into the suffix
    ///   (`format_smolstr!("row.{index}.sound")`).
    ///
    /// # Release pairing
    ///
    /// Keep the key (or the component handle, or the `Action` the component derived
    /// from it) for as long as the component lives and release with that same identity:
    /// a kit component's `release` unregisters exactly the handler its `build`
    /// registered. A widget that unregisters by hand must use the same `Action`;
    /// `off_action` alone fixes the handler but leaves the component's signal scope
    /// behind.
    pub fn unique_key(&self, suffix: &str) -> SmolStr {
        format_smolstr!("{suffix}@{}", self.scope.index())
    }

    /// The application's one layer stack, for the overlays the widget builds.
    ///
    /// Clone it into the kit component (the component keeps its own `Layer` handle;
    /// every handle shares the same stack). Never build a `Layer::new()` here: a
    /// second stack would disagree with the composition root about the z-order (see
    /// the module documentation).
    pub fn layer(&self) -> &Layer {
        &self.services.layer
    }

    /// The application's one frame router.
    ///
    /// The route the framework registered for this widget lives in this router, next
    /// to the routes of the kit components the widget ticks (overlays, lists,
    /// tables). A widget that needs its own per-frame work registers it here and keeps
    /// the [`RouteId`](crate::frame::RouteId); building a second `FrameRouter` cannot
    /// install itself and the route would never run.
    pub fn frame_router(&self) -> &FrameRouter {
        &self.services.frame_router
    }

    /// The application's one animation-completion router.
    ///
    /// Every `Overlay` / `Tooltip` the widget builds is handed a clone of this router;
    /// the engine's completion slot belongs to it. A second router would replace the
    /// callback and the fade-outs the first one tracks would never finish (see the
    /// module documentation).
    pub fn animation(&self) -> &AnimationRouter {
        &self.services.animation_router
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
    pub fn runtime(&self) -> &Handle {
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
    use std::rc::Rc;

    use app_config::settings::{AccountConfig, AccountWalletKind, Config};
    use base_ui::kit::button::Button;
    use smol_str::format_smolstr;
    use ui_core::{
        event::Action,
        math::Rect,
        node::{builder::NodeBuilder, id::NodeId, tree::NodeKind},
        paint::canvas::Canvas,
    };

    use crate::{cx::WidgetCx, test_support::WidgetTestApp};

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

    /// A widget that builds a host owning services takes the composition root's
    /// aggregate, not a copy: the context hands out the one `Rc` the application built,
    /// and cloning it reaches the same singletons the context reads.
    #[test]
    fn the_context_hands_over_the_composition_root_s_services() {
        let mut harness = WidgetTestApp::new();

        let same_handle = harness.with_cx(|cx| std::ptr::eq(cx.services_rc(), cx.services_rc()));
        assert!(
            same_handle,
            "the context hands out one handle, not a fresh one per call"
        );

        let from_context = harness.with_cx(|cx| Rc::clone(cx.services_rc()));
        assert!(
            Rc::ptr_eq(&from_context, harness.services_rc()),
            "the handle is the composition root's instance, not a copy of it"
        );

        let layer = Rc::clone(&from_context.layer);
        assert!(
            std::ptr::eq(&*layer, harness.layer()),
            "a host built over that handle shares the application's layer stack"
        );
    }

    /// Runs `body` with a context over a **second** widget instance of the same
    /// application: a fresh signal scope (what `AnyWidget::mount` mints per pane), the
    /// host's services and window. The contract's multi-pane assertions build their
    /// peer context through this, since `WidgetTestApp::with_cx` is the first instance.
    fn with_peer_cx<R>(
        harness: &mut WidgetTestApp,
        body: impl FnOnce(&mut WidgetCx<'_>) -> R,
    ) -> R {
        let services = Rc::clone(harness.services_rc());
        let window = harness.window();
        let scope = harness.app_mut().signal_scope();
        let mut cx = WidgetCx::new(harness.app_mut(), window, &services, scope);
        body(&mut cx)
    }

    /// Builds a control the way a widget's header builds one: the key comes from the
    /// instance's own context, not from a constant.
    fn build_keyed_button(cx: &mut WidgetCx<'_>, parent: NodeId, suffix: &str) -> (Button, Action) {
        let key = cx.unique_key(suffix);
        let theme = cx.theme().clone();
        let window = cx.window();
        let button = Button::new(cx.app_mut(), theme, key).on_click(|_| {});
        button
            .build(cx.app_mut(), window, parent)
            .expect("the parent is alive");
        let action = button
            .action_identity()
            .expect("a click callback registered the component's action");
        (button, action)
    }

    /// A rebuilt or re-synced control mints the key it already used: the prefix is the
    /// instance's scope, not a counter, so release/registration pairing stays exact.
    #[test]
    fn the_same_suffix_in_one_instance_keeps_one_key() {
        let mut harness = WidgetTestApp::new();

        let (within_one_call, again_in_one_call) =
            harness.with_cx(|cx| (cx.unique_key("sound"), cx.unique_key("sound")));
        assert_eq!(
            within_one_call, again_in_one_call,
            "two controls of one instance mint their own keys, not a fresh one per call"
        );

        let next_call = harness.with_cx(|cx| cx.unique_key("sound"));
        assert_eq!(
            within_one_call, next_call,
            "a control rebuilt on a later frame must reclaim the key it registered"
        );
    }

    /// The key documents its own origin: the widget-local suffix, then the instance's
    /// scope index.
    #[test]
    fn the_key_carries_the_instance_scope_and_the_widget_local_suffix() {
        let mut harness = WidgetTestApp::new();

        let (key, scope) =
            harness.with_cx(|cx| (cx.unique_key("market_trade.header.sound"), cx.scope()));

        assert_eq!(
            key,
            format_smolstr!("market_trade.header.sound@{}", scope.index())
        );
        assert!(
            key.starts_with("market_trade.header.sound@"),
            "the suffix stays the prefix so a test can locate the control's debug id"
        );
    }

    /// Two panes never share a key: the application's action registry is keyed by it.
    #[test]
    fn two_instances_in_one_application_mint_different_keys() {
        let mut harness = WidgetTestApp::new();

        let first = harness.with_cx(|cx| (cx.scope(), cx.unique_key("sound")));
        let second = with_peer_cx(&mut harness, |cx| (cx.scope(), cx.unique_key("sound")));

        assert_ne!(
            first.0.index(),
            second.0.index(),
            "a widget mount mints its own signal scope"
        );
        assert_ne!(
            first.1, second.1,
            "two panes built the same suffix and must still get different keys"
        );
        assert_eq!(
            second.1,
            format_smolstr!("sound@{}", second.0.index()),
            "each key is stamped with its own instance's scope"
        );
    }

    /// The suffix is the widget's namespace: two controls of one instance have to
    /// choose two names, and the keys stay distinct.
    #[test]
    fn different_suffixes_in_one_instance_stay_distinct() {
        let mut harness = WidgetTestApp::new();

        let (sound, confirm) =
            harness.with_cx(|cx| (cx.unique_key("header.sound"), cx.unique_key("confirm")));

        assert_ne!(sound, confirm);
        assert_eq!(sound, harness.with_cx(|cx| cx.unique_key("header.sound")));
    }

    /// **The W2a F1 regression.** Two panes build the same header control; closing one
    /// pane releases only its own handler. With a constant key the second pane's
    /// handler would be unregistered by the first pane's release.
    #[test]
    fn releasing_one_pane_leaves_the_other_pane_s_action_handler() {
        let mut harness = WidgetTestApp::new();
        let root = harness.root();

        let (first_button, first_action) =
            harness.with_cx(|cx| build_keyed_button(cx, root, "market_trade.header.sound"));
        let (second_button, second_action) = with_peer_cx(&mut harness, |cx| {
            build_keyed_button(cx, root, "market_trade.header.sound")
        });

        assert_ne!(
            first_action, second_action,
            "each pane's control registered its own action identity"
        );
        assert!(harness.app().has_action_handler(&first_action));
        assert!(harness.app().has_action_handler(&second_action));

        // The widget's `release` hook releases its controls; the peer pane is still
        // open and must keep its handler.
        first_button.release(harness.app_mut());
        assert!(
            !harness.app().has_action_handler(&first_action),
            "the closed pane's handler is unregistered"
        );
        assert!(
            harness.app().has_action_handler(&second_action),
            "releasing one pane removed another pane's handler — every instance uses the \
             same action key"
        );

        second_button.release(harness.app_mut());
        assert!(!harness.app().has_action_handler(&second_action));
    }
}

//! The framework's test host: an engine application, the injected services, one
//! mounted widget, and the assertions the contract's tests are written in.
//!
//! The previous contract's tests drove its own application context and asserted through
//! its element tree. This host is the engine's equivalent and the migration carrier for
//! the twenty widget crates: it builds the same [`WidgetServices`] the composition
//! root builds through [`WidgetServices::builder`] (minus the market-participants
//! directory, which still needs a toolkit context to construct — see
//! [`WidgetServices`]), creates and injects the three composition-root singletons, and
//! drains frames through [`ui_core::test_support::app::TestApp`].
//!
//! # What the headless host does that the platform pump does
//!
//! [`TestApp::drain`](ui_core::test_support::app::TestApp::drain) runs the frame
//! pipeline but **not** the engine's `App::on_frame` hook (that hook belongs to the
//! platform event loop). [`WidgetTestApp::drain`] therefore ticks the frame router
//! first and only then drains — the same order the production pump uses. A test that
//! wants one synchronized frame calls `drain`; a test that wants to inspect the
//! sync in isolation calls [`WidgetTestApp::tick`].
//!
//! # The composition-root singletons
//!
//! The host is the composition root of a test: it creates the one
//! [`Layer`](base_ui::kit::layer::Layer), the one [`FrameRouter`] and the one
//! [`AnimationRouter`](base_ui::kit::overlay::AnimationRouter), keeps its own handles
//! ([`WidgetTestApp::layer`], [`WidgetTestApp::router`],
//! [`WidgetTestApp::animation`]) and injects the same instances through
//! [`WidgetServices::builder`]. A test therefore asserts against the application-level
//! instances, not against a copy a widget might have built: an overlay opened through
//! the widget context shows up in [`WidgetTestApp::layer`], and the first overlay
//! installs [`WidgetTestApp::animation`] as the engine's completion dispatcher.
//!
//! # Driving the widget's async work
//!
//! A widget's service, its binding forwarders and its RPC calls spawn on
//! [`WidgetServices::runtime`]. The host owns that runtime as a **current-thread**
//! runtime, so nothing advances between `drain` calls: that is what makes a test
//! deterministic. [`WidgetTestApp::drain_runtime`] advances it by one turn (polling
//! every task that is ready, without sleeping and without moving any timer), and
//! [`WidgetTestApp::run_until_idle`] alternates runtime turns with frames until no
//! work is left. The engine's application handle keeps its own runtime (the one the
//! test engine built); a widget that uses `WidgetCx::spawn` rather than the injected
//! runtime is the composition root's business, and a test of such a widget drives a
//! runtime it owns.
//!
//! # Moving time (E10 timers and countdowns)
//!
//! The host's engine is on the **engine clock**: [`TestApp`] switches the live clock
//! off when it is built, so no wall-clock time passes however long a test runs.
//! [`WidgetTestApp::advance`] is the only thing that moves time: it advances the
//! engine clock by the duration the test asks for and runs one production-shaped
//! round, so an [`App::after`] timer becomes due at exactly its deadline and never
//! earlier. Nothing sleeps and nothing waits.
//!
//! One `advance` is one engine round: the frame routes run first, then one frame.
//! The engine dispatches the timers that are due at the frame's entry — before the
//! style / text / layout / paint passes — so a callback's writes are painted by the
//! same round. A timer that re-arms itself (the countdown pattern,
//! `crates/widgets/AGENTS.md` §11) fires once per call, not once per elapsed delay.
//!
//! The engine clock is not a widget's "now": a countdown formats the deadline the
//! widget holds, and a test moves that state itself. Work that waits on tokio time
//! belongs to a test-owned `#[tokio::test(start_paused = true)]` runtime, as for
//! [`WidgetTestApp::drain_runtime`].
//!
//! # Injecting the host's collaborators
//!
//! The default host composes the same aggregate the application root does, with the
//! headless absences: no market-participants directory, no Kytrix service connection,
//! a silent `sound` source and an `open_url` that records nothing. A test that
//! exercises one of those paths hands its own collaborator in with
//! [`WidgetTestApp::with_market_participants`],
//! [`WidgetTestApp::with_kytrix_service`], [`WidgetTestApp::with_open_url`] or
//! [`WidgetTestApp::with_sound`] — the framework's own crate does not need a private
//! seam per widget library to test a service path.
//!
//! # Selectors
//!
//! The contract fixes the debug ids widget surfaces are found by:
//!
//! | selector | node |
//! |---|---|
//! | `WIDGET_SURFACE` | the widget's root |
//! | `WIDGET_SLOT` | the content surface (background/border/clip) |
//! | `WIDGET_CONTENT` | the body slot the widget builds into |
//! | `WIDGET_PLACEHOLDER` | the stand-in overlay |
//! | `WIDGET_BANNER` | the failure banner (present only while it applies) |
//!
//! and the controls the framework builds carry the instance-derived keys
//! `widget.placeholder.action.<node>` / `widget.banner.action.<node>`.
//!
//! # Reading what a canvas drew (P3-1j)
//!
//! A self-drawn widget's canvas is **one opaque primitive** in the engine's paint, so the
//! old assertions could only see that a canvas node exists — deleting `set_canvas`,
//! breaking a steady-state short circuit, or dropping `request_repaint` stayed green. The
//! host now reads the canvas contents as paint:
//!
//! - [`WidgetTestApp::paint_entries`] — the node's own paint (its background/border/text/…
//!   and the opaque `Canvas` entry), for "this node was painted at all".
//! - [`WidgetTestApp::canvas_primitives`] — the canvas' contents as [`CanvasPrimitive`]s
//!   (quads, paths, clip brackets, text runs, geometry) with the colors/radii the widget
//!   wrote.
//! - [`WidgetTestApp::canvas_handle`] — the tree's canvas value, no frame needed, for
//!   `Arc::ptr_eq` steady-state assertions (an unchanged widget must not replace it).
//!
//! The first two read the **last drained frame**, so an idle round has no paint (assert
//! after a round where [`WidgetTestApp::rendered`] is true). The four test shapes the
//! widget crates write (steady state, theme switch, "canvas never written", geometry
//! handed back) are documented in `crates/widgets/AGENTS.md` §11 and design §13.8.
//!
//! # Compiling this module
//!
//! It is available to this crate's own tests (`cfg(test)`) and to other crates as
//! `widget::test_support` when the `test-support` feature is enabled. The feature is
//! the only thing that turns on `ui_core`'s own test support, so a release build of
//! `widget` never links the engine's test host.

use std::{rc::Rc, sync::Arc, time::Duration};

use base_ui::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::handle::ThemeHandle,
};
use bytes::Bytes;
use i18n::{language::codes::LanguageCode, translate::Translate, translator::types::I18n};
use kytrix_service_provider::service::connector::KytrixServiceConnector;
use market_participants_connector::connector::MarketParticipantsConnector;
use smol_str::SmolStr;
use ui_core::{
    a11y::A11yPatch,
    app::App,
    error::EngineError,
    event::{Action, ActionSource, Event},
    math::Rect,
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::SignalScope,
        tree::{NodeKind, NodeTree},
    },
    paint::{
        canvas::Canvas,
        color::Color,
        geometry::GeometryId,
        primitive::{ClipOp, GradientKind, Primitive, QuadPaint},
    },
    test_support::{
        app::TestApp,
        diagnostics::SignalSlotStats,
        record::{PrimitiveKind, PrimitiveRecord},
    },
    window::{FrameIndex, WindowId},
};

use crate::{
    any::AnyWidget,
    cx::WidgetCx,
    error::TranslatedError,
    frame::FrameRouter,
    identity::WidgetInstanceId,
    services::WidgetServices,
    status::{WidgetAction, WidgetState, WidgetStatus},
    surface::WidgetSurface,
    view::WidgetView,
};

/// How many runtime/frame turns [`WidgetTestApp::run_until_idle`] runs before it
/// gives up on settling.
///
/// A test whose subject is genuinely waiting (on a timer, or on work another host
/// drives) must not hang: the budget bounds it, and the returned turn count says
/// whether it settled or spent the budget.
const RUNTIME_TURNS: usize = 32;

/// The messages the framework itself resolves while a widget runs, and the copy its
/// tests report as a widget's own. `test.waiting` stands for a widget's own notice,
/// `test.rich` for one the catalogue authors as markdown/HTML (a `<p>` paragraph
/// with a `<kbd>` icon and an inline image), and `sample.failure` for a widget's
/// typed failure; the rest are the framework's own labels.
pub const MESSAGES_EN: &str = r#"
common:
  connecting: Connecting
  loading: Loading
  retry: Retry
sample:
  failure: the fixture failed
test:
  waiting: Waiting for orders
  rich: '<p align="center">Click the <kbd>![](file://icons/phosphoricons/puzzle-piece.svg)</kbd> button to pick a widget.</p>'
"#;

/// The same messages in Chinese, so a test can move the language.
pub const MESSAGES_ZH: &str = r#"
common:
  connecting: 正在连接
  loading: 正在加载
  retry: 重试
sample:
  failure: 示例失败
test:
  waiting: 正在等待订单
  rich: '<p align="center">点击 <kbd>![](file://icons/phosphoricons/puzzle-piece.svg)</kbd> 按钮选择一个部件</p>'
"#;

/// A typed failure the contract's tests report into a state cell.
pub struct FixtureFailure;

impl TranslatedError for FixtureFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        i18n::t_smol!(translate, "sample.failure")
    }
}

/// The fixture translator: the deterministic catalogue the contract tests render.
pub fn fixture_translate(language: LanguageCode) -> Translate {
    let i18n = I18n::builder()
        .locale_yaml(LanguageCode::En, MESSAGES_EN)
        .expect("the English fixture parses")
        .locale_yaml(LanguageCode::ZhCn, MESSAGES_ZH)
        .expect("the Chinese fixture parses")
        .fallback(LanguageCode::En)
        .build()
        .expect("the fixture catalogue builds");

    Translate::new(Rc::new(i18n), language)
}

/// A headless engine host with the framework's services and one widget scope.
///
/// ```text
/// let mut harness = WidgetTestApp::new();
/// let surface = harness.build_surface();
/// harness.report(WidgetState::Connecting);
/// harness.sync_surface(&mut surface, false);
/// assert!(harness.visible(surface.placeholder_node()));
/// ```
pub struct WidgetTestApp {
    app: TestApp,
    window: WindowId,
    /// The runtime the injected services were handed. The host owns it so a test
    /// can drive a widget's async work ([`WidgetTestApp::drain_runtime`]); it is a
    /// current-thread runtime, so nothing runs between drains.
    runtime: tokio::runtime::Runtime,
    /// The composition root's one layer stack, mirrored into `services`.
    layer: Rc<Layer>,
    /// The composition root's one frame router, mirrored into `services`.
    router: Rc<FrameRouter>,
    /// The composition root's one animation-completion router, mirrored into
    /// `services`.
    animation: Rc<AnimationRouter>,
    services: Rc<WidgetServices>,
    scope: SignalScope,
    status: WidgetStatus,
}

impl Default for WidgetTestApp {
    fn default() -> Self {
        Self::new()
    }
}

impl WidgetTestApp {
    /// Builds the host: one 800×600 window, the fixture translator, the default
    /// settings snapshot, a light theme, and a frame router already installed.
    #[must_use]
    pub fn new() -> Self {
        Self::with_translate(fixture_translate(LanguageCode::En))
    }

    /// Builds the host over an explicit translation context.
    ///
    /// A test that asserts catalogue-authored copy (a notice the product ships, say)
    /// needs the product's own catalogue rather than the fixture one; everything else
    /// is composed exactly as [`WidgetTestApp::new`] composes it.
    #[must_use]
    pub fn with_translate(translate: Translate) -> Self {
        let mut app = TestApp::new();
        let window = app.window_id();
        let theme = ThemeHandle::new(app.app_mut(), base_ui::theme::appearance::Appearance::Light);
        // The services' runtime is one the host owns (a current-thread runtime, so a
        // test drives its work explicitly) rather than the engine's: the engine's
        // test runtime is private to `TestApp` and cannot be pumped from outside.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the host's runtime builds");
        // The composition root's three singletons: the host keeps its own handles and
        // injects the same instances, exactly as the application root does.
        let layer = Rc::new(Layer::new());
        let animation = Rc::new(AnimationRouter::new());
        let router =
            Rc::new(FrameRouter::new(app.app_mut()).expect("the host owns the frame hook slot"));
        let services = Rc::new(
            WidgetServices::builder(
                app.app_mut(),
                runtime.handle().clone(),
                translate,
                theme,
                Rc::clone(&layer),
                Rc::clone(&router),
                Rc::clone(&animation),
            )
            .build(),
        );
        let scope = app.app_mut().signal_scope();
        let status = WidgetStatus::new(app.app_mut(), scope);

        Self {
            app,
            window,
            runtime,
            layer,
            router,
            animation,
            services,
            scope,
            status,
        }
    }

    /// Hands the host a market-participants directory.
    ///
    /// The slot is `Option` because the connector's construction still needs the
    /// application shell (`WidgetServices` documents the transition); a fixture that
    /// builds one headlessly passes it here, and every widget that reads the
    /// directory sees it through `services.market_participants()`. See
    /// `crates/widgets/AGENTS.md` §8 and §11.
    #[must_use]
    pub fn with_market_participants(
        mut self,
        market_participants: Option<MarketParticipantsConnector>,
    ) -> Self {
        self.services_mut().market_participants = market_participants;
        self
    }

    /// Hands the host a Kytrix service connection.
    ///
    /// Like the directory, the connector's `install` still takes the toolkit's
    /// application context, so a headless test cannot construct one until P4 adds the
    /// toolkit-free seam (design §13.9 R-1); the setter is what a fixture with one
    /// will use, and passing `None` is the headless absence the default host has.
    #[must_use]
    pub fn with_kytrix_service(mut self, kytrix_service: Option<KytrixServiceConnector>) -> Self {
        self.services_mut().kytrix_service = kytrix_service;
        self
    }

    /// Replaces the application command a widget opens external links through.
    #[must_use]
    pub fn with_open_url(mut self, open_url: impl Fn(&str) + 'static) -> Self {
        self.services_mut().open_url = Rc::new(open_url);
        self
    }

    /// Replaces the source of the application's audio assets.
    ///
    /// The default host has no audio (`None` for every name); a test that asserts a
    /// widget plays the sound it decided on hands in the bytes it should receive.
    #[must_use]
    pub fn with_sound(mut self, sound: impl Fn(&'static str) -> Option<Bytes> + 'static) -> Self {
        self.services_mut().sound = Rc::new(sound);
        self
    }

    /// The services aggregate, for a builder that replaces one collaborator.
    ///
    /// `WidgetTestApp` is the only owner of the aggregate (no context outlives a
    /// call), so the mutable borrow is always available.
    fn services_mut(&mut self) -> &mut WidgetServices {
        Rc::get_mut(&mut self.services).expect("the host is the only owner of its services")
    }

    /// The engine's test application.
    #[must_use]
    pub fn app(&self) -> &App {
        self.app.app()
    }

    /// The engine's test application, for writes.
    #[must_use]
    pub fn app_mut(&mut self) -> &mut App {
        self.app.app_mut()
    }

    /// The injected services.
    #[must_use]
    pub fn services(&self) -> &WidgetServices {
        &self.services
    }

    /// Delivers one platform event the way the pump does.
    ///
    /// Input goes through the engine's interaction dispatch, and every event lands in the
    /// observation queue [`App::drain_events_where`] reads — which is what a widget that
    /// owns the text input of its own window consumes (`WidgetView::sync` drains it, the
    /// way a dialog's fields do). A widget that only builds kit controls never needs this:
    /// the engine dispatches their actions without the observation queue.
    pub fn dispatch_event(&mut self, event: Event) {
        self.app.dispatch_event(event);
    }

    /// The services aggregate as the shared handle this host owns, like the
    /// composition root does.
    ///
    /// A widget that builds a host owning services takes the same handle through
    /// [`WidgetCx::services_rc`]; a test compares the two with `Rc::ptr_eq` to pin that
    /// the widget works over the application's one aggregate rather than a copy.
    #[must_use]
    pub fn services_rc(&self) -> &Rc<WidgetServices> {
        &self.services
    }

    /// The composition root's one layer stack the services carry.
    ///
    /// A test asserts an overlay opened through the widget context against this
    /// instance: it is the same one the services hand out, and a widget that built its
    /// own stack could not register there.
    #[must_use]
    pub fn layer(&self) -> &Layer {
        &self.layer
    }

    /// The installed frame router.
    #[must_use]
    pub fn router(&self) -> &FrameRouter {
        &self.router
    }

    /// The composition root's one animation-completion router the services carry.
    ///
    /// The engine's single completion slot stays with this instance: the first kit
    /// component built through the widget context installs it
    /// ([`AnimationRouter::installed`]).
    #[must_use]
    pub fn animation(&self) -> &AnimationRouter {
        &self.animation
    }

    /// The window every widget of this host lives in.
    #[must_use]
    pub const fn window(&self) -> WindowId {
        self.window
    }

    /// The state cell the framework minted for the fixture widget.
    #[must_use]
    pub const fn status(&self) -> WidgetStatus {
        self.status
    }

    /// The fixture widget's scope.
    #[must_use]
    pub const fn scope(&self) -> SignalScope {
        self.scope
    }

    /// Runs `body` with a [`WidgetCx`] over this host.
    pub fn with_cx<R>(&mut self, body: impl FnOnce(&mut WidgetCx<'_>) -> R) -> R {
        let services = Rc::clone(&self.services);
        let mut cx = WidgetCx::new(self.app.app_mut(), self.window, &services, self.scope);
        body(&mut cx)
    }

    /// Runs every registered frame route, as the engine hook would.
    pub fn tick(&mut self) {
        self.router
            .tick(self.app.app_mut())
            .expect("the widget frame routes succeed");
    }

    /// One production-shaped frame: the frame hook first, then the pipeline.
    pub fn drain(&mut self) {
        self.tick();
        self.app.drain().expect("the frame commits");
    }

    /// Advances the engine clock by `by` and runs one production-shaped round.
    ///
    /// This is the host's only time source: the engine is on the engine clock, so
    /// an E10 timer ([`App::after`]) becomes due exactly when the accumulated
    /// `advance` calls reach its deadline — and never because wall-clock time
    /// passed (see the module docs, "Moving time"). The round is
    /// [`WidgetTestApp::drain`]: the frame routes first, then one frame; the engine
    /// dispatches the due timers at that frame's entry, so a callback's writes are
    /// painted by the same round.
    ///
    /// Returns whether the round committed pixels. A callback that writes no render
    /// state leaves the round idle: `false`, and neither
    /// [`WidgetTestApp::frame_index`] nor [`WidgetTestApp::frame_requests`] moves
    /// (I3). A callback whose write a node reads — or one that asks for a repaint —
    /// has that request presented by this same round: `true`.
    ///
    /// One call is one engine round, so a timer that re-arms itself (the countdown
    /// pattern, `crates/widgets/AGENTS.md` §11) fires once per call, not once per
    /// elapsed `delay`.
    pub fn advance(&mut self, by: Duration) -> bool {
        self.app.advance(by);
        self.drain();
        self.app.frame_record().rendered
    }

    /// The host window's commit cursor (`0` = nothing committed yet).
    ///
    /// A round that paints nothing — an idle [`WidgetTestApp::advance`], say — leaves
    /// it unchanged (I3).
    #[must_use]
    pub fn frame_index(&self) -> FrameIndex {
        self.app
            .app()
            .window(self.window)
            .expect("the host's window exists")
            .frame_index()
    }

    /// How many frames the engine has been asked for.
    ///
    /// This is the I3 assertion surface: an idle round adds nothing. A signal write
    /// only asks for a frame when a node (or derived value) reads it; an explicit
    /// [`App::request_repaint`] always asks.
    #[must_use]
    pub fn frame_requests(&self) -> u64 {
        self.app.app().frame_requests()
    }

    /// The engine's value-slot table statistics (E21).
    ///
    /// A widget's signals and transient values live in its scope, and releasing the
    /// scope returns them to the free stack for the next mount to reuse. That is what
    /// makes a leak assertion expressible here: mount and release a widget (or open and
    /// close a component) many times and neither `slots` nor `live` may grow — a
    /// signal minted into the root scope instead is never reclaimed, and the `live`
    /// count after a release is where it shows up.
    ///
    /// The read is an engine test probe: it draws nothing and asks for no frame.
    #[must_use]
    pub fn signal_slot_stats(&self) -> SignalSlotStats {
        ui_core::test_support::diagnostics::signal_slot_stats(self.app.app())
    }

    /// Whether the last drained round committed pixels.
    ///
    /// [`WidgetTestApp::advance`] returns this same fact for the round it ran; this
    /// reads it after a plain [`WidgetTestApp::drain`].
    ///
    /// # Panics
    /// No round has been drained yet (there is no frame record to read).
    #[must_use]
    pub fn rendered(&self) -> bool {
        self.app.frame_record().rendered
    }

    /// Advances the runtime the widget services were handed by one turn.
    ///
    /// A widget's service, its binding forwarders and its RPC replies all spawn on
    /// [`WidgetServices::runtime`]; the host owns it as a current-thread runtime, so
    /// its tasks make no progress between calls. One turn polls every task that is
    /// ready — including the tasks those tasks wake — without sleeping and without
    /// advancing a timer. The loop goes on until nothing else is ready or the runtime
    /// has no more work to schedule.
    ///
    /// This drives the *services* runtime, which is the one `WidgetBinding`,
    /// `WidgetRuntime` and the RPC client use. Work a widget sends through
    /// `WidgetCx::spawn`/`AppHandle` runs on the engine's handle and belongs to the
    /// composition root; a test of it drives a runtime it owns.
    pub fn drain_runtime(&mut self) {
        self.runtime.block_on(tokio::task::yield_now());
    }

    /// Advances the runtime and the frame pipeline until neither has work left.
    ///
    /// Each turn is one [`WidgetTestApp::drain_runtime`] followed by one
    /// [`WidgetTestApp::drain`] — the production order: widget work first, then the
    /// frame that presents what it wrote (a binding delivery requests its frame from
    /// the runtime side, and the pump runs the request on the next frame). The loop
    /// stops after a turn that changed neither the frame-request counter nor the
    /// number of dispatched tasks, or after [`RUNTIME_TURNS`] turns; it returns how
    /// many turns it ran, so a test can assert that its subject settled rather than
    /// spending the budget.
    ///
    /// Nothing sleeps and no clock moves: a widget that waits on wall-clock or tokio
    /// time is not advanced here. A test of such work drives its own
    /// `#[tokio::test(start_paused = true)]` runtime (see `crates/widgets/AGENTS.md`
    /// §11).
    pub fn run_until_idle(&mut self) -> usize {
        let mut turns = 0;
        let mut settled = (self.app.app().frame_requests(), self.app.tasks_run());
        while turns < RUNTIME_TURNS {
            self.drain_runtime();
            self.drain();
            turns += 1;
            let after = (self.app.app().frame_requests(), self.app.tasks_run());
            if turns >= 2 && after == settled {
                break;
            }
            settled = after;
        }
        turns
    }

    /// The window's node tree.
    #[must_use]
    pub fn tree(&self) -> &NodeTree {
        self.app
            .app()
            .window(self.window)
            .expect("the host's window exists")
            .tree()
    }

    /// The root node of the host window.
    #[must_use]
    pub fn root(&self) -> NodeId {
        self.tree().root()
    }

    /// Reports a state through the fixture widget's cell.
    pub fn report(&mut self, state: WidgetState) {
        assert!(
            self.status.report(self.app.app_mut(), state),
            "the test's report is a change"
        );
    }

    /// Switches the language the way the composition root does: translator first,
    /// then the signal.
    pub fn switch_language(&mut self, language: LanguageCode) {
        self.services.translate.set_language(language);
        assert!(
            self.app.app_mut().write(&self.services.language, language),
            "the switch is a change"
        );
    }

    /// Builds one widget surface under the window root.
    pub fn build_surface(&mut self) -> WidgetSurface {
        let status = self.status;
        let services = Rc::clone(&self.services);
        let scope = self.scope;
        let root = self.root();
        let mut cx = WidgetCx::new(self.app.app_mut(), self.window, &services, scope);
        WidgetSurface::build(&mut cx, root, status).expect("the surface builds")
    }

    /// Runs the surface's sync for one frame and then drains it.
    pub fn sync_surface(&mut self, surface: &mut WidgetSurface, has_content: bool) {
        let services = Rc::clone(&self.services);
        let scope = self.scope;
        {
            let mut cx = WidgetCx::new(self.app.app_mut(), self.window, &services, scope);
            surface
                .sync(&mut cx, has_content)
                .expect("the surface syncs");
        }
        self.drain();
    }

    /// Builds a test body node under `parent` and returns it.
    pub fn build_body(&mut self, parent: NodeId) -> NodeId {
        self.with_cx(|cx| {
            NodeBuilder::boxed()
                .id("TEST_BODY")
                .build(cx.tree_mut(), parent)
                .expect("the body parent is alive")
        })
    }

    /// Builds a plain container under the root, for tests that exercise a
    /// placeholder outside a surface.
    pub fn placeholder_parent(&mut self) -> NodeId {
        self.with_cx(|cx| {
            let root = cx.tree().root();
            NodeBuilder::boxed()
                .id("TEST_PLACEHOLDER_HOST")
                .build(cx.tree_mut(), root)
                .expect("the root is alive")
        })
    }

    /// Mounts a widget through the real registration path and returns its handle.
    pub fn mount<W: WidgetView>(&mut self, config: W::Config) -> AnyWidget {
        self.try_mount::<W>(config).expect("the widget mounts")
    }

    /// Mounts a widget, keeping the error a refused construction reports.
    pub fn try_mount<W: WidgetView>(
        &mut self,
        config: W::Config,
    ) -> Result<AnyWidget, EngineError> {
        let services = Rc::clone(&self.services);
        let parent = self.root();
        AnyWidget::mount::<W>(
            self.app.app_mut(),
            self.window,
            parent,
            &services,
            WidgetInstanceId::new(),
            config,
            crate::config::WidgetConfigChange::default(),
        )
    }

    /// Mounts a widget under `parent` over a scope the caller minted.
    ///
    /// The failure path is what this seam is for: a stale `parent` refuses the
    /// surface, and the caller still holds the scope, so it can assert that the
    /// refused attempt released the state cell it minted (a leaked scope has no
    /// other observable).
    pub fn try_mount_in_scope<W: WidgetView>(
        &mut self,
        parent: NodeId,
        scope: SignalScope,
        config: W::Config,
    ) -> Result<AnyWidget, EngineError> {
        let services = Rc::clone(&self.services);
        AnyWidget::mount_in_scope::<W>(
            self.app.app_mut(),
            self.window,
            parent,
            &services,
            WidgetInstanceId::new(),
            config,
            crate::config::WidgetConfigChange::default(),
            scope,
        )
    }

    /// Whether the node draws (its evaluated style patch is visible).
    ///
    /// The patch is written by the style pass, so a test reads this after a frame.
    #[must_use]
    pub fn visible(&self, node: NodeId) -> bool {
        self.tree()
            .get(node)
            .expect("the node exists")
            .style_patch()
            .visible
            .unwrap_or(true)
    }

    /// The text the subtree under `node` draws, in document order, one line per text
    /// node.
    #[must_use]
    pub fn text_of(&self, node: NodeId) -> String {
        let mut texts = Vec::new();
        self.collect_text(node, &mut texts);
        texts.join("\n")
    }

    /// Whether any text node in the subtree draws exactly `text`.
    #[must_use]
    pub fn has_text(&self, node: NodeId, text: &str) -> bool {
        let mut texts = Vec::new();
        self.collect_text(node, &mut texts);
        texts.iter().any(|drawn| drawn == text)
    }

    /// Whether the subtree under `node` contains a canvas node (a spinner's shape).
    #[must_use]
    pub fn has_canvas(&self, node: NodeId) -> bool {
        let tree = self.tree();
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            if matches!(tree.get(current).expect("node").kind(), NodeKind::Canvas(_)) {
                return true;
            }
            stack.extend(tree.get(current).expect("node").children().iter().copied());
        }
        false
    }

    /// The primitives the last drained frame painted for `node`.
    ///
    /// This is the node's **own** paint in paint order — its background quad, border, text run,
    /// image, shadows and, for a self-drawn node, the single opaque
    /// [`PrimitiveKind::Canvas`] entry. A canvas node's contents are not flattened here: read
    /// them with [`WidgetTestApp::canvas_primitives`].
    ///
    /// The read is the last drained frame's paint, not the tree's inputs: a round that painted
    /// nothing (an idle round, I3) has no paint and answers empty. Assert after a round that
    /// committed ([`WidgetTestApp::rendered`]).
    ///
    /// # Panics
    /// No round has been drained yet (there is no frame record to read).
    #[must_use]
    pub fn paint_entries(&self, node: NodeId) -> Vec<PrimitiveRecord> {
        self.app
            .rendered()
            .map(|frame| {
                frame
                    .primitives
                    .iter()
                    .filter(|entry| entry.node == node)
                    .copied()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The primitives a canvas node's canvas holds in the last drained frame.
    ///
    /// A self-drawn widget (`NodeBuilder::canvas` + [`WidgetCx::set_canvas`](crate::cx::WidgetCx::set_canvas))
    /// is one opaque `Primitive::Canvas` in the engine's paint; what the widget drew — quads,
    /// paths, clip brackets, text runs, geometry — is read here, in the order the widget pushed
    /// them. Colors are the [`Color`]s the widget wrote (compare them against the theme role it
    /// read, not against resolved linear-premultiplied values), and every text run is one
    /// [`PrimitiveKind::GlyphRun`] entry whose `glyphs` is its positioned-glyph count.
    ///
    /// The read is the last drained frame's paint: a node with no canvas, a canvas that was not
    /// painted (hidden/removed), and an idle round all answer empty. Assert after a round that
    /// committed ([`WidgetTestApp::rendered`]).
    ///
    /// # Panics
    /// No round has been drained yet (there is no frame record to read).
    #[must_use]
    pub fn canvas_primitives(&self, node: NodeId) -> Vec<CanvasPrimitive> {
        self.app
            .rendered()
            .map(|frame| {
                frame
                    .canvases
                    .iter()
                    .filter(|entry| entry.node == node)
                    .flat_map(|entry| entry.canvas.primitives().iter().map(canvas_primitive))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The canvas the node tree currently holds for `node` (`None` = not a canvas node).
    ///
    /// This is the input side and needs no frame. A widget that repaints only when something it
    /// paints changed leaves the same [`Arc`] in place across steady-state syncs, so
    /// `Arc::ptr_eq` of two reads is the assertion that `set_canvas` was **not** called between
    /// them (a widget that rebuilds its canvas every frame replaces it, which is what a
    /// steady-state mutation breaks).
    #[must_use]
    pub fn canvas_handle(&self, node: NodeId) -> Option<Arc<Canvas>> {
        let NodeKind::Canvas(canvas) = self.tree().get(node).ok()?.kind() else {
            return None;
        };
        Some(Arc::clone(canvas))
    }

    /// Clicks the recovery control the framework built under `parent`.
    ///
    /// The click goes through the engine's real action dispatch — the control's
    /// handler is the framework's, and this is the same path a pointer click takes.
    pub fn click_recovery(&mut self, parent: NodeId) {
        let node = self
            .find_by_debug_prefix(parent, "widget.placeholder.action.")
            .or_else(|| self.find_by_debug_prefix(parent, "widget.banner.action."))
            .expect("the framework built a recovery control");
        let debug_id = self
            .tree()
            .debug_id(node)
            .expect("the control carries its debug id")
            .to_owned();
        let key = debug_id
            .strip_suffix(":interactive")
            .expect("a kit control's node id ends with its interactive suffix");
        let action = Action::Custom(smol_str::format_smolstr!("{key}.click"));
        let window = self.window;

        assert!(
            self.app
                .app_mut()
                .dispatch_action(window, node, action, ActionSource::Programmatic),
            "the recovery control is registered"
        );
    }

    /// The nearest node under `parent` whose debug id starts with `prefix`.
    #[must_use]
    pub fn find_by_debug_prefix(&self, parent: NodeId, prefix: &str) -> Option<NodeId> {
        let tree = self.tree();
        let mut stack = vec![parent];
        while let Some(current) = stack.pop() {
            if tree
                .debug_id(current)
                .is_some_and(|id| id.starts_with(prefix))
            {
                return Some(current);
            }
            stack.extend(
                tree.get(current)
                    .expect("node")
                    .children()
                    .iter()
                    .rev()
                    .copied(),
            );
        }
        None
    }

    /// Sets an a11y patch through the engine, as a widget's sync would.
    pub fn set_a11y_patch(&mut self, node: NodeId, patch: A11yPatch) {
        self.app
            .app_mut()
            .set_a11y_patch(self.window, node, patch)
            .expect("the node is alive");
    }

    /// Turns on a11y capture for the following frames.
    ///
    /// The first drained frame is the full tree; every later one is the incremental
    /// delivery. Assertions read [`WidgetTestApp::a11y_hidden`] against the last
    /// drained frame.
    pub fn enable_a11y_capture(&mut self) {
        self.app.enable_a11y_capture();
    }

    /// Whether the engine marked `node` hidden in the last drained a11y delivery.
    ///
    /// This is the engine's own hidden predicate (`A11yPatch.hidden` / `visible` /
    /// `display:none` resolved by the a11y tree), not the style patch: it is what an
    /// assistive technology receives.
    #[must_use]
    pub fn a11y_hidden(&self, node: NodeId) -> bool {
        let snapshot = self
            .app
            .a11y()
            .expect("a11y capture is on and a frame was drained");
        snapshot
            .node_for(node)
            .unwrap_or_else(|| panic!("{node:?} was not delivered in this a11y frame"))
            .hidden
    }

    /// Whether the subtree under `node` contains a text node with inline boxes
    /// (an inline image or a `<kbd>` span).
    #[must_use]
    pub fn has_inline_boxes(&self, node: NodeId) -> bool {
        let tree = self.tree();
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            let target = tree.get(current).expect("node");
            if let NodeKind::Text(content) = target.kind()
                && !content.inline_boxes.is_empty()
            {
                return true;
            }
            stack.extend(target.children().iter().copied());
        }
        false
    }

    /// How many nodes the host window's tree holds (mount-leak assertions).
    #[must_use]
    pub fn node_count(&self) -> usize {
        let tree = self.tree();
        let mut count = 0;
        let mut stack = vec![tree.root()];
        while let Some(current) = stack.pop() {
            count += 1;
            stack.extend(tree.get(current).expect("node").children().iter().copied());
        }
        count
    }

    fn collect_text(&self, node: NodeId, texts: &mut Vec<String>) {
        let tree = self.tree();
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            let target = tree.get(current).expect("node");
            if let NodeKind::Text(content) = target.kind() {
                texts.push(content.text.to_string());
            }
            stack.extend(target.children().iter().rev().copied());
        }
    }
}

/// One primitive inside a canvas, as the last drained frame painted it.
///
/// [`WidgetTestApp::canvas_primitives`] returns these in push order: one entry per
/// `Canvas::push_*` call, with the fields that call wrote. `ClipPush` / `ClipPop` are the
/// canvas' bracket instructions (a balanced pair is how a test asserts the widget closed what it
/// opened); `glyphs` is meaningful on `GlyphRun` entries only (one entry = one text run).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CanvasPrimitive {
    /// Which primitive this is.
    pub kind: PrimitiveKind,
    /// Logical-pixel bounds; `None` for instructions without geometry (`ClipPop`, `Geometry`).
    pub bounds: Option<Rect>,
    /// Four corner radii (logical px) for `Quad`, `Shadow` and `ClipPush`.
    pub corner_radius: Option<[f32; 4]>,
    /// Solid fill / path fill / text / shadow colour, or a gradient's **start** colour.
    pub color: Option<Color>,
    /// A quad's inner border (width, colour).
    pub border: Option<(f32, Color)>,
    /// A gradient quad's end colour and direction; its start colour is [`Self::color`].
    pub gradient: Option<(Color, GradientKind)>,
    /// Positioned-glyph count for a `GlyphRun` (0 for every other kind).
    pub glyphs: usize,
    /// The geometry a `Path` / `Geometry` primitive draws from.
    pub geometry: Option<GeometryId>,
}

/// Snapshots one canvas-inner primitive, keeping the app-written colors unresolved.
fn canvas_primitive(primitive: &Primitive) -> CanvasPrimitive {
    match primitive {
        Primitive::Quad(quad) => {
            let (color, border, gradient) = match &quad.paint {
                QuadPaint::Solid { fill, border } => (
                    Some(*fill),
                    border.map(|painted| (painted.width, painted.color)),
                    None,
                ),
                QuadPaint::Gradient(painted) => {
                    (Some(painted.from), None, Some((painted.to, painted.kind)))
                }
            };
            CanvasPrimitive {
                kind: PrimitiveKind::Quad,
                bounds: Some(quad.bounds),
                corner_radius: Some(quad.corner_radius),
                color,
                border,
                gradient,
                glyphs: 0,
                geometry: None,
            }
        }
        Primitive::GlyphRun(run) => {
            let mut bounds = Rect::default();
            for glyph in &run.glyphs {
                bounds = bounds.union(Rect::from_origin_size(glyph.position, glyph.size));
            }
            CanvasPrimitive {
                kind: PrimitiveKind::GlyphRun,
                bounds: Some(bounds),
                corner_radius: None,
                color: Some(run.color),
                border: None,
                gradient: None,
                glyphs: run.glyphs.len(),
                geometry: None,
            }
        }
        Primitive::Image(image) => CanvasPrimitive {
            kind: PrimitiveKind::Image,
            bounds: Some(image.bounds),
            corner_radius: None,
            color: None,
            border: None,
            gradient: None,
            glyphs: 0,
            geometry: None,
        },
        Primitive::Path(path) => CanvasPrimitive {
            kind: PrimitiveKind::Path,
            bounds: Some(path.bounds),
            corner_radius: None,
            color: Some(path.fill),
            border: None,
            gradient: None,
            glyphs: 0,
            geometry: Some(path.geometry),
        },
        Primitive::Shadow(shadow) => CanvasPrimitive {
            kind: PrimitiveKind::Shadow,
            bounds: Some(shadow.rect),
            corner_radius: Some(shadow.corner_radius),
            color: Some(shadow.color),
            border: None,
            gradient: None,
            glyphs: 0,
            geometry: None,
        },
        Primitive::Clip(ClipOp::Push(clip)) => CanvasPrimitive {
            kind: PrimitiveKind::ClipPush,
            bounds: Some(clip.rect),
            corner_radius: Some(clip.corner_radius),
            color: None,
            border: None,
            gradient: None,
            glyphs: 0,
            geometry: None,
        },
        Primitive::Clip(ClipOp::Pop) => CanvasPrimitive {
            kind: PrimitiveKind::ClipPop,
            bounds: None,
            corner_radius: None,
            color: None,
            border: None,
            gradient: None,
            glyphs: 0,
            geometry: None,
        },
        Primitive::Geometry(draw) => CanvasPrimitive {
            kind: PrimitiveKind::Geometry,
            bounds: None,
            corner_radius: None,
            color: Some(draw.color),
            border: None,
            gradient: None,
            glyphs: 0,
            geometry: Some(draw.id),
        },
        Primitive::Canvas(canvas) => CanvasPrimitive {
            kind: PrimitiveKind::Canvas,
            bounds: Some(canvas.bounds()),
            corner_radius: None,
            color: None,
            border: None,
            gradient: None,
            glyphs: 0,
            geometry: None,
        },
    }
}

/// A widget in the shape the framework drives, for the tests that need the whole
/// path rather than one piece of it.
///
/// It reports a [`Notice`](crate::status::Notice) rather than a sentence, draws its
/// own copy in the language showing now, and counts the states it writes: a language
/// change has to reach the surface without the widget being asked for anything, and
/// that count is how a test sees whether it did.
pub struct WaitingWidget {
    status: WidgetStatus,
    writes: Rc<std::cell::Cell<usize>>,
    loads: Rc<std::cell::Cell<usize>>,
    released: Rc<std::cell::Cell<bool>>,
}

impl WaitingWidget {
    /// How many states this fixture has written.
    #[must_use]
    pub fn writes(&self) -> usize {
        self.writes.get()
    }

    /// How many times this fixture has loaded.
    #[must_use]
    pub fn loads(&self) -> usize {
        self.loads.get()
    }

    /// Whether the framework ran the fixture's release hook.
    #[must_use]
    pub fn released(&self) -> bool {
        self.released.get()
    }
}

impl WidgetView for WaitingWidget {
    type Config = ();

    const ID: crate::identity::WidgetId = crate::identity::WidgetId::new_static("test.waiting");
    const DISPLAY_NAME: &'static str = "test.waiting";
    const CATEGORY: crate::category::WidgetCategory = crate::category::WidgetCategory::Market;

    fn new(
        _instance_id: WidgetInstanceId,
        _config: Self::Config,
        status: WidgetStatus,
        _reload: crate::view::ReloadHandle,
        _config_change: crate::config::WidgetConfigChange,
        _recovery: WidgetAction,
        _cx: &mut WidgetCx<'_>,
    ) -> Result<Self, ui_core::error::EngineError> {
        Ok(Self {
            status,
            writes: Rc::new(std::cell::Cell::new(0)),
            loads: Rc::new(std::cell::Cell::new(0)),
            released: Rc::new(std::cell::Cell::new(false)),
        })
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        self.loads.set(self.loads.get() + 1);
        self.writes.set(self.writes.get() + 1);
        self.status.report(
            cx.app_mut(),
            WidgetState::waiting(crate::status::Notice::message("test.waiting")),
        );
    }

    fn has_content(&self) -> bool {
        false
    }

    fn body(
        &mut self,
        _parent: NodeId,
        _cx: &mut WidgetCx<'_>,
    ) -> Result<(), ui_core::error::EngineError> {
        Ok(())
    }

    fn release(&mut self, _cx: &mut WidgetCx<'_>) {
        self.released.set(true);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use app_config_provider::notification::ConfigNotification;
    use base_ui::theme::{appearance::Appearance, role::Role};
    use bytes::Bytes;
    use i18n::language::codes::LanguageCode;
    use market_participants_connector::{
        connector::{MarketParticipantsConnector, MarketParticipantsConnectorInput},
        runtime::MarketParticipantNetworkSnapshot,
    };
    use network::transport::hub::{TransportHub, TransportPolicy};
    use smol_str::{SmolStr, format_smolstr};
    use ui_core::{
        app::App,
        error::EngineError,
        math::{Point, Rect},
        node::{builder::NodeBuilder, id::NodeId},
        paint::{
            canvas::Canvas,
            color::Color,
            geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology},
            primitive::{ClipRect, Path, Quad, QuadPaint},
            style::StylePatch,
        },
        test_support::record::PrimitiveKind,
        text::TextStyle,
    };

    use super::{CanvasPrimitive, FixtureFailure, WaitingWidget, WidgetTestApp};
    use crate::{
        any::AnyWidget,
        category::WidgetCategory,
        config::WidgetConfigChange,
        cx::WidgetCx,
        identity::{WidgetId, WidgetInstanceId},
        status::{WidgetAction, WidgetState, WidgetStatus},
        view::{ReloadHandle, WidgetView},
    };

    /// The contract's minimum end-to-end example: mount a widget, let the framework
    /// build its body, move through loading → ready → failed, switch the language,
    /// and release with nothing left behind.
    #[test]
    fn a_minimal_widget_runs_the_whole_lifecycle() {
        let mut harness = WidgetTestApp::new();
        let widget = harness.mount::<WaitingWidget>(());

        // Registration: the framework built the surface and ran the first load.
        assert_eq!(
            harness.tree().debug_id(widget.surface()),
            Some("WIDGET_SURFACE")
        );
        assert_eq!(
            widget.status().state(harness.app()),
            WidgetState::waiting(crate::status::Notice::message("test.waiting"))
        );

        // The surface presents the notice without any content.
        harness.drain();
        let surface_node = widget.surface();
        let placeholder = harness
            .find_by_debug_prefix(surface_node, "WIDGET_PLACEHOLDER")
            .expect("the placeholder exists");
        assert!(harness.visible(placeholder));
        assert!(harness.text_of(placeholder).contains("Waiting for orders"));

        // State switching: the widget reports through its own cell, and the surface
        // follows on the next frame.
        assert!(
            widget
                .status()
                .report(harness.app_mut(), WidgetState::Loading)
        );
        harness.drain();
        assert!(harness.has_canvas(placeholder), "a spinner stands in");
        assert!(
            widget
                .status()
                .report(harness.app_mut(), WidgetState::Ready)
        );

        // The language switch: the translator and the signal move together, and the
        // next synchronization redraws the sentence.
        harness.switch_language(LanguageCode::ZhCn);
        assert!(widget.status().report(
            harness.app_mut(),
            WidgetState::waiting(crate::status::Notice::message("test.waiting"))
        ));
        harness.drain();
        assert!(
            harness.text_of(placeholder).contains("正在等待订单"),
            "the stand-in follows the language: {}",
            harness.text_of(placeholder)
        );

        // The failure terminal state: the placeholder carries the typed failure and
        // its recovery, and the state cell reports it exactly like every other state.
        assert!(widget.status().report(
            harness.app_mut(),
            WidgetState::failed(
                std::sync::Arc::new(FixtureFailure),
                WidgetAction::new("common.retry", |_| {}),
            )
        ));
        harness.drain();
        assert!(
            harness.text_of(placeholder).contains("示例失败"),
            "the failure is drawn in the language showing now: {}",
            harness.text_of(placeholder)
        );
        assert!(
            !harness.visible(
                harness
                    .find_by_debug_prefix(surface_node, "WIDGET_CONTENT")
                    .expect("the content slot exists")
            ),
            "a failed widget with no content draws the failure, not the content"
        );

        // Release: the route is gone, the scope is released, the hook ran, and the
        // surfaces are no longer in the window's tree.
        assert_eq!(harness.router().len(), 1);
        let released = widget.release(harness.app_mut());
        assert!(released > 0, "the scope owned the widget's signals");
        assert_eq!(harness.router().len(), 0, "the frame route is unregistered");
        assert!(widget.is_released());
        assert!(
            harness.tree().get(widget.surface()).is_err(),
            "the nodes are gone"
        );
        assert!(widget.config().is_none());

        // Idempotent: a second release is a no-op.
        assert_eq!(widget.release(harness.app_mut()), 0);
    }

    /// A released widget's recovery route is gone: the router no longer holds it.
    #[test]
    fn a_released_widget_leaves_no_frame_route() {
        let mut harness = WidgetTestApp::new();
        let widget = harness.mount::<WaitingWidget>(());
        assert_eq!(harness.router().len(), 1);

        widget.release(harness.app_mut());

        assert!(harness.router().is_empty());
        harness.drain();
    }

    /// A mounted widget's values live in its own scope, and releasing it returns them:
    /// the value-slot table is the leak assertion surface (E21 / W2c-R9). Repeating the
    /// mount and release must leave neither the allocated slots nor the live count
    /// growing — an unreclaimed signal shows up in `live`, and a slot table that only
    /// ever appends shows up in `slots`.
    #[test]
    fn releasing_a_widget_reclaims_every_value_slot() {
        let mut harness = WidgetTestApp::new();
        let baseline = harness.signal_slot_stats();

        let widget = harness.mount::<WaitingWidget>(());
        let mounted = harness.signal_slot_stats();
        assert!(
            mounted.live > baseline.live,
            "the mounted widget mints its own values: {mounted:?}"
        );

        widget.release(harness.app_mut());
        let released = harness.signal_slot_stats();
        assert_eq!(
            released.live, baseline.live,
            "release returns every value the mounted widget minted: {released:?}"
        );

        for _ in 0..8 {
            let widget = harness.mount::<WaitingWidget>(());
            widget.release(harness.app_mut());
        }
        assert_eq!(
            harness.signal_slot_stats(),
            released,
            "eight more mount/release rounds reuse the slots instead of growing the table"
        );
    }

    /// Two widget instances live in one application, each with its own signal scope and
    /// surface: the host is a pane host, not a single-widget fixture. Closing one pane
    /// removes only its own nodes and frame route — what a multi-pane action key has to
    /// survive (P3-1h: keys are prefixed by the instance scope).
    #[test]
    fn two_widget_instances_live_side_by_side() {
        let mut harness = WidgetTestApp::new();
        let first = harness.mount::<WaitingWidget>(());
        let second = harness.mount::<WaitingWidget>(());
        harness.drain();

        assert_ne!(
            first.scope().index(),
            second.scope().index(),
            "each mount mints its own signal scope"
        );
        assert_ne!(first.surface(), second.surface());
        assert_eq!(harness.router().len(), 2);
        assert!(harness.tree().get(first.surface()).is_ok());
        assert!(harness.tree().get(second.surface()).is_ok());

        let released = first.release(harness.app_mut());
        assert!(released > 0);
        assert!(
            harness.tree().get(first.surface()).is_err(),
            "the closed pane's nodes are gone"
        );
        assert!(
            harness.tree().get(second.surface()).is_ok(),
            "the other pane is untouched"
        );
        assert_eq!(harness.router().len(), 1);
        assert!(!second.is_released());

        second.release(harness.app_mut());
        assert!(harness.tree().get(second.surface()).is_err());
    }

    /// The failure path: the fixture refuses its own recovery, and the framework's
    /// banner is shown above content.
    #[test]
    fn a_recovery_action_offers_the_reload_path() {
        let mut harness = WidgetTestApp::new();
        let retries = Rc::new(std::cell::Cell::new(0));
        let counted = retries.clone();
        let action = WidgetAction::new("common.retry", move |_| counted.set(counted.get() + 1));
        let state = WidgetState::failed(Arc::new(FixtureFailure), action);

        harness.report(state);
        let mut surface = harness.build_surface();
        harness.sync_surface(&mut surface, false);
        let placeholder = harness
            .find_by_debug_prefix(surface.root(), "WIDGET_PLACEHOLDER")
            .expect("the placeholder stands in for the missing content");
        harness.click_recovery(placeholder);

        assert_eq!(retries.get(), 1);
    }

    /// The erasure: `AnyWidget` answers the framework's questions without the
    /// caller knowing the widget type.
    #[test]
    fn any_widget_answers_through_the_vtable_only() {
        let mut harness = WidgetTestApp::new();
        let widget: AnyWidget = harness.mount::<WaitingWidget>(());

        assert_eq!(widget.display_name(), "test.waiting");
        assert!(!widget.status().is_failed(harness.app()));
        assert!(widget.config().is_none());
        assert!(widget.instrument_link().is_none());
        assert!(!widget.is_released());
    }

    /// The default host composes the headless absences: no market-participants
    /// directory, no Kytrix service connection, no audio. A widget maps each of them
    /// to its own business notice or failure.
    #[test]
    fn the_default_host_has_no_directory_service_or_audio() {
        let harness = WidgetTestApp::new();

        assert!(harness.services().market_participants().is_none());
        assert!(harness.services().kytrix_service().is_none());
        assert_eq!(
            (harness.services().sound)("sounds/news_notification.wav"),
            None,
            "a host without audio answers None instead of failing"
        );
    }

    /// The host composes the collaborators the application root does, and a test
    /// injects the one it exercises: the command a widget opens links through records
    /// what it was asked to open, and the sound source hands back the bytes it was
    /// asked to play. These are the two W1a had to work around with a crate-private
    /// seam.
    #[test]
    fn a_test_injects_the_application_collaborators_it_exercises() {
        let opened: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let recorded = Rc::clone(&opened);
        let harness = WidgetTestApp::new()
            .with_open_url(move |url| recorded.borrow_mut().push(url.to_owned()))
            .with_sound(|_| Some(Bytes::from_static(b"clip")));

        (harness.services().open_url)("https://example.com/news");
        assert_eq!(
            opened.borrow().as_slice(),
            ["https://example.com/news"],
            "the injected command is the one the widget calls"
        );
        assert_eq!(
            (harness.services().sound)("sounds/news_notification.wav"),
            Some(Bytes::from_static(b"clip")),
            "the injected audio source is the one the widget hears"
        );
    }

    /// A headless fixture builds the market-participants directory over the host's
    /// own runtime and hands it in, so a widget's directory path is reachable through
    /// `mount` instead of a crate-private source seam (W1b's gap). The Kytrix
    /// connection is the collaborator whose construction still needs the toolkit, so
    /// a headless host states its absence explicitly (design §13.9 R-1).
    #[test]
    fn a_headless_market_participants_directory_can_be_injected() {
        let harness = WidgetTestApp::new();
        let runtime = harness.services().runtime.clone();
        let notification =
            ConfigNotification::new(Arc::new(MarketParticipantNetworkSnapshot::default()));
        let participants = MarketParticipantsConnector::new(
            MarketParticipantsConnectorInput::new(
                Arc::new(TransportHub::new(TransportPolicy::default())),
                notification,
            )
            .with_runtime(runtime),
        )
        .expect("the headless directory builds");

        let harness = harness
            .with_market_participants(Some(participants))
            .with_kytrix_service(None);

        assert!(
            harness.services().market_participants().is_some(),
            "the widget sees the directory the test handed in"
        );
        assert!(harness.services().kytrix_service().is_none());
    }

    /// The host owns the runtime its services were handed as a current-thread
    /// runtime, so a widget's async work runs only when the test drives it — which is
    /// what makes a service test deterministic, and what lets the test drive it here
    /// instead of building a second host just to pump a runtime (W1a/W1b's gap).
    #[test]
    fn drain_runtime_advances_the_services_runtime() {
        let mut harness = WidgetTestApp::new();
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        harness.services().runtime.spawn(async move {
            flag.store(true, Ordering::Relaxed);
        });

        assert!(
            !ran.load(Ordering::Relaxed),
            "a current-thread runtime runs nothing until it is driven"
        );
        harness.drain_runtime();
        assert!(
            ran.load(Ordering::Relaxed),
            "one turn polled the ready task"
        );
    }

    /// A chain of service work is driven by `run_until_idle` with no sleep: the first
    /// task wakes the second, and the loop stops as soon as nothing moves.
    #[test]
    fn run_until_idle_drives_a_chain_of_service_work() {
        let mut harness = WidgetTestApp::new();
        let received = Arc::new(AtomicUsize::new(0));
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<u32>();
        harness.services().runtime.spawn(async move {
            let _ = sender.send(1);
        });
        let count = Arc::clone(&received);
        harness.services().runtime.spawn(async move {
            while receiver.recv().await.is_some() {
                count.fetch_add(1, Ordering::Relaxed);
            }
        });

        let turns = harness.run_until_idle();

        assert_eq!(
            received.load(Ordering::Relaxed),
            1,
            "the second task received the message the first sent"
        );
        assert!(
            turns >= 2,
            "the host settled instead of returning at once: {turns}"
        );
        assert!(
            turns < 32,
            "the host did not spend its whole budget: {turns}"
        );
    }

    /// A service that asks the interface for a frame is presented by the same loop:
    /// the request goes through the real application handle, and the frame pump runs
    /// it on the next turn — the production order the host's `drain` implements.
    #[test]
    fn run_until_idle_presents_the_frame_a_service_asked_for() {
        let mut harness = WidgetTestApp::new();
        let before = harness.frame_requests();
        let handle = harness.app().handle();
        let window = harness.window();
        harness.services().runtime.spawn(async move {
            handle.send(move |app| {
                app.request_repaint(window)
                    .expect("the host's window is alive");
            });
        });

        let turns = harness.run_until_idle();

        assert!(
            harness.frame_requests() > before,
            "the service's frame request was pumped and ran"
        );
        assert!(
            turns >= 2,
            "the request is pumped after a frame and presented on the next turn: {turns}"
        );
    }

    /// E10 in the host: an [`App::after`] timer fires when [`WidgetTestApp::advance`]
    /// reaches its deadline, and a callback that writes no render state leaves the
    /// round idle — no frame request, no commit (I3).
    #[test]
    fn advance_dispatches_a_due_timer_without_asking_for_a_frame() {
        let mut harness = WidgetTestApp::new();
        // The host's first frame paints the window; settle it before measuring.
        harness.drain();
        let fired = Rc::new(Cell::new(0));
        let counter = Rc::clone(&fired);
        let window = harness.window();
        harness
            .app_mut()
            .after(window, Duration::from_millis(250), move |_| {
                counter.set(counter.get() + 1);
            })
            .expect("the host's window takes the timer");
        let requests = harness.frame_requests();
        let index = harness.frame_index();

        assert!(
            !harness.advance(Duration::from_millis(250)),
            "a callback that writes no render state commits nothing"
        );
        assert_eq!(fired.get(), 1, "the timer fired at its deadline");
        assert_eq!(
            harness.frame_requests(),
            requests,
            "dispatching a due timer asks for no frame (I3)"
        );
        assert_eq!(harness.frame_index(), index, "no frame was committed");
        assert!(!harness.rendered());
    }

    /// The timer is never early: half the delay is an idle round, and the second half
    /// reaches the deadline exactly.
    #[test]
    fn advance_does_not_fire_a_timer_before_its_delay() {
        let mut harness = WidgetTestApp::new();
        // The host's first frame paints the window; settle it before measuring.
        harness.drain();
        let fired = Rc::new(Cell::new(0));
        let counter = Rc::clone(&fired);
        let window = harness.window();
        harness
            .app_mut()
            .after(window, Duration::from_millis(250), move |_| {
                counter.set(counter.get() + 1);
            })
            .expect("the host's window takes the timer");
        let requests = harness.frame_requests();

        assert!(!harness.advance(Duration::from_millis(125)));
        assert_eq!(fired.get(), 0, "half the delay is not enough");
        assert_eq!(harness.frame_requests(), requests);

        assert!(!harness.advance(Duration::from_millis(125)));
        assert_eq!(
            fired.get(),
            1,
            "the second half reaches the deadline exactly"
        );
        assert_eq!(
            harness.frame_requests(),
            requests,
            "the callback writes no render state"
        );
    }

    /// The countdown pattern: a timer's callback writes the state a label reads, and
    /// the same round presents it — the callback's write is the only frame request the
    /// advance adds.
    #[test]
    fn a_timer_callback_s_write_is_presented_by_the_same_advance() {
        let mut harness = WidgetTestApp::new();
        let countdown = harness.app_mut().signal(0_u32);
        let label = harness.with_cx(|cx| {
            let root = cx.tree().root();
            NodeBuilder::text(SmolStr::default(), TextStyle::new(12.0))
                .layout_override(Rect::new(0.0, 0.0, 80.0, 20.0))
                .bind_text(move |read| {
                    let value = *read.read(&countdown);
                    format_smolstr!("{value}")
                })
                .build(cx.tree_mut(), root)
                .expect("the countdown label builds")
        });
        // The first frame evaluates the binding, so the label depends on the signal
        // before the callback writes it.
        harness.drain();
        assert!(
            harness.has_text(label, "0"),
            "the label starts at the value the test wrote"
        );
        let window = harness.window();
        harness
            .app_mut()
            .after(window, Duration::from_millis(250), move |app| {
                assert!(app.write(&countdown, 1), "the countdown moves");
            })
            .expect("the host's window takes the timer");
        let requests = harness.frame_requests();
        let index = harness.frame_index();

        assert!(
            harness.advance(Duration::from_millis(250)),
            "the callback's write asks for a frame and this round presents it"
        );
        assert!(
            harness.has_text(label, "1"),
            "the label draws the state the callback wrote: {}",
            harness.text_of(label)
        );
        assert_eq!(
            harness.frame_requests(),
            requests + 1,
            "the callback's write is the only frame request"
        );
        assert_eq!(harness.frame_index().get(), index.get() + 1);
    }

    /// How far the canvas fixture animates before it settles.
    const CANVAS_TICKS: u8 = 3;

    /// The canvas box the fixture draws into.
    const CANVAS_BOX: Rect = Rect::new(0.0, 0.0, 120.0, 40.0);

    /// A widget whose body is one self-drawn canvas: it animates a few frames, draws a clipped
    /// quad + a path over registered geometry + one shaped text run, reads the theme on every
    /// repaint, and hands the geometry back on release.
    ///
    /// It is the subject the canvas observations are tested on (P3-1j): the same `sync → set_canvas
    /// → frame` path the migrated deep-bar / chart widgets take, without their domain code.
    struct CanvasWidget {
        progress: u8,
        painted: Option<(u8, Appearance)>,
        node: Option<NodeId>,
        mesh: Option<GeometryId>,
    }

    impl WidgetView for CanvasWidget {
        type Config = ();

        const ID: WidgetId = WidgetId::new_static("test.canvas");
        const DISPLAY_NAME: &'static str = "test.canvas";
        const CATEGORY: WidgetCategory = WidgetCategory::Market;

        fn new(
            _instance_id: WidgetInstanceId,
            _config: Self::Config,
            _status: WidgetStatus,
            _reload: ReloadHandle,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut WidgetCx<'_>,
        ) -> Result<Self, EngineError> {
            Ok(Self {
                progress: 0,
                painted: None,
                node: None,
                mesh: None,
            })
        }

        fn load(&mut self, _cx: &mut WidgetCx<'_>) {}

        fn has_content(&self) -> bool {
            true
        }

        fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
            // A plate that reads a theme role registers the dependency an appearance switch
            // invalidates, so switching the theme asks for the repaint the canvas follows.
            let theme = cx.theme().clone();
            NodeBuilder::boxed()
                .id("TEST_PLATE")
                .layout_override(Rect::new(0.0, 0.0, CANVAS_BOX.size.width, 10.0))
                .bind_style(move |read| StylePatch {
                    background: Some(theme.role(read, Role::MarketUp)),
                    ..StylePatch::default()
                })
                .build(cx.tree_mut(), parent)?;
            let node = NodeBuilder::canvas(Canvas::new(CANVAS_BOX))
                .id("TEST_CANVAS")
                .layout_override(CANVAS_BOX)
                .build(cx.tree_mut(), parent)?;
            self.node = Some(node);
            self.mesh = Some(register_triangle(cx.app_mut())?);
            Ok(())
        }

        fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
            let appearance = cx.theme().current(cx.app());
            let key = (self.progress, appearance);
            if self.painted == Some(key) {
                return Ok(());
            }
            let (Some(node), Some(mesh)) = (self.node, self.mesh) else {
                return Ok(());
            };
            let theme = cx.theme().clone();
            let fill = theme.theme_for(appearance).role(Role::MarketUp);
            let text_color = theme.theme_for(appearance).role(Role::FgDefault);
            let mut canvas = Canvas::new(CANVAS_BOX);
            assert!(
                canvas.push_clip(ClipRect {
                    rect: CANVAS_BOX,
                    corner_radius: [4.0; 4],
                }),
                "the fixture's clip is one rounded layer, under the depth limit"
            );
            canvas.push_quad(Quad {
                bounds: Rect::new(0.0, 0.0, 10.0 * f32::from(self.progress + 1), 20.0),
                corner_radius: [2.0; 4],
                paint: QuadPaint::Solid { fill, border: None },
            });
            canvas.push_path(Path {
                bounds: Rect::new(0.0, 0.0, 40.0, 20.0),
                geometry: mesh,
                fill,
            });
            let mut style = TextStyle::new(12.0);
            style.color = text_color;
            let line = cx.app_mut().shape_text("42", &style, None, &[])?;
            assert!(
                canvas.push_shaped_text(&line, Point::new(4.0, 4.0)),
                "the fixture's shaped line is a live lease"
            );
            assert!(canvas.pop_clip(), "the fixture closes its clip");
            cx.set_canvas(node, canvas)?;
            self.painted = Some(key);
            if self.progress < CANVAS_TICKS {
                self.progress += 1;
                cx.request_frame()?;
            }
            Ok(())
        }

        fn release(&mut self, cx: &mut WidgetCx<'_>) {
            if let Some(mesh) = self.mesh.take() {
                assert!(
                    cx.app_mut().geometry_mut().release(mesh),
                    "the fixture releases the mesh it registered"
                );
            }
        }
    }

    /// Registers one `GeometryPipeline::Ui` triangle in the process-wide store.
    fn register_triangle(app: &mut App) -> Result<GeometryId, EngineError> {
        let mut vertex_bytes = Vec::with_capacity(36);
        for vertex in [[0.0_f32, 0.0, -1.0], [40.0, 0.0, -1.0], [0.0, 20.0, -1.0]] {
            for component in vertex {
                vertex_bytes.extend_from_slice(&component.to_le_bytes());
            }
        }
        app.geometry_mut().insert(GeometryDesc {
            vertex_bytes: Bytes::from(vertex_bytes),
            index_bytes: Bytes::from_static(&[0, 0, 1, 0, 2, 0]),
            vertex_stride: 12,
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: Rect::new(0.0, 0.0, 40.0, 20.0),
            pipeline: GeometryPipeline::Ui,
        })
    }

    /// The canvas node the fixture built under its surface.
    fn canvas_node(harness: &WidgetTestApp, widget: &AnyWidget) -> NodeId {
        harness
            .find_by_debug_prefix(widget.surface(), "TEST_CANVAS")
            .expect("the fixture built its canvas")
    }

    /// The colour one appearance resolves `role` to.
    fn role_color(harness: &WidgetTestApp, appearance: Appearance, role: Role) -> Color {
        harness.services().theme.theme_for(appearance).role(role)
    }

    /// The kinds of a canvas' primitives, in paint order.
    fn kinds(primitives: &[CanvasPrimitive]) -> Vec<PrimitiveKind> {
        primitives.iter().map(|primitive| primitive.kind).collect()
    }

    /// P3-1j: the canvas fixture's paint is readable primitive by primitive — the clip bracket,
    /// the quad (bounds/radius/colour), the path over registered geometry and the text run —
    /// instead of one opaque `Canvas` entry (W4b F1, W5a).
    #[test]
    fn a_canvas_widget_s_draws_are_readable_primitive_by_primitive() {
        let mut harness = WidgetTestApp::new();
        let widget = harness.mount::<CanvasWidget>(());
        harness.drain();
        assert!(harness.rendered(), "the first frame paints the widget");
        let node = canvas_node(&harness, &widget);

        let own = harness.paint_entries(node);
        assert_eq!(
            own.iter()
                .filter(|entry| entry.kind == PrimitiveKind::Canvas)
                .count(),
            1,
            "the node's own paint is one opaque canvas entry: {own:?}"
        );

        let primitives = harness.canvas_primitives(node);
        assert_eq!(
            kinds(&primitives),
            [
                PrimitiveKind::ClipPush,
                PrimitiveKind::Quad,
                PrimitiveKind::Path,
                PrimitiveKind::GlyphRun,
                PrimitiveKind::ClipPop,
            ],
            "the canvas contents in push order: {primitives:?}"
        );
        let clip = &primitives[0];
        assert_eq!(clip.bounds, Some(CANVAS_BOX));
        assert_eq!(clip.corner_radius, Some([4.0; 4]));
        let quad = &primitives[1];
        assert_eq!(
            quad.bounds,
            Some(Rect::new(0.0, 0.0, 10.0, 20.0)),
            "the first paint is progress 0"
        );
        assert_eq!(quad.corner_radius, Some([2.0; 4]));
        assert_eq!(
            quad.color,
            Some(role_color(&harness, Appearance::Light, Role::MarketUp)),
            "the quad carries the colour the widget wrote"
        );
        let path = &primitives[2];
        assert!(
            path.geometry.is_some(),
            "the path names the geometry it draws: {path:?}"
        );
        let run = &primitives[3];
        assert!(
            run.glyphs > 0,
            "one GlyphRun entry per text run, with its placed glyphs: {run:?}"
        );
        assert_eq!(
            run.color,
            Some(role_color(&harness, Appearance::Light, Role::FgDefault))
        );

        // The animation moved: the next frame draws the next progress.
        harness.drain();
        let grown = harness.canvas_primitives(node);
        assert_eq!(
            grown[1].bounds,
            Some(Rect::new(0.0, 0.0, 20.0, 20.0)),
            "the next frame repainted the quad at the next progress"
        );
    }

    /// P3-1j / W4b F1 ②③: a widget that animates requests the frames it draws, and once it
    /// settles it neither rewrites the canvas (`Arc::ptr_eq`) nor asks for another frame (I3).
    #[test]
    fn a_settled_canvas_is_not_rewritten_and_asks_for_no_more_frames() {
        let mut harness = WidgetTestApp::new();
        let widget = harness.mount::<CanvasWidget>(());
        for _ in 0..=CANVAS_TICKS {
            harness.drain();
        }
        let node = canvas_node(&harness, &widget);
        let settled_requests = harness.frame_requests();
        let before = harness
            .canvas_handle(node)
            .expect("the node holds a canvas");
        let painted = harness.canvas_primitives(node);
        assert_eq!(
            painted[1].bounds,
            Some(Rect::new(0.0, 0.0, 40.0, 20.0)),
            "the animation reached its last progress"
        );

        harness.drain();
        assert!(!harness.rendered(), "a settled canvas paints nothing new");
        assert_eq!(
            harness.frame_requests(),
            settled_requests,
            "the sync must not ask for another frame (I3)"
        );
        let after = harness
            .canvas_handle(node)
            .expect("the node holds a canvas");
        assert!(
            Arc::ptr_eq(&before, &after),
            "a steady state leaves the same canvas value in place (set_canvas was not called)"
        );
        assert!(
            harness.canvas_primitives(node).is_empty(),
            "an idle round has no paint to read"
        );
    }

    /// P3-1j / W4b F1 ②: an animating canvas keeps asking for the frames it draws. A widget that
    /// only calls `set_canvas` in `sync` without requesting the next frame freezes in production
    /// (no pump wake), which this reads back through `frame_requests`.
    #[test]
    fn an_animating_canvas_keeps_asking_for_frames() {
        let mut harness = WidgetTestApp::new();
        let _widget = harness.mount::<CanvasWidget>(());
        harness.drain();
        let first = harness.frame_requests();
        assert!(
            first >= 1,
            "the fixture asks for the frame its next progress needs"
        );
        harness.drain();
        assert!(
            harness.frame_requests() > first,
            "each animated round requests the next one"
        );
    }

    /// P3-1j / W4b F1 ② (theme half): the canvas repaints in the colours the appearance now
    /// resolves — the assertion a "colours baked in at mount" regression fails.
    #[test]
    fn switching_the_appearance_repaints_the_canvas_in_the_new_roles() {
        let mut harness = WidgetTestApp::new();
        let widget = harness.mount::<CanvasWidget>(());
        harness.drain();
        let node = canvas_node(&harness, &widget);
        let light = harness.canvas_primitives(node);
        let light_fill = light[1].color.expect("the quad is filled");
        assert_eq!(
            light_fill,
            role_color(&harness, Appearance::Light, Role::MarketUp)
        );

        let before = harness.frame_requests();
        let theme = harness.services().theme.clone();
        assert!(
            theme.set_appearance(harness.app_mut(), Appearance::Dark),
            "the appearance moves"
        );
        assert!(
            harness.frame_requests() > before,
            "the plate reads the theme, so the switch asks for the frame that repaints"
        );
        harness.drain();
        let dark = harness.canvas_primitives(node);
        let dark_fill = dark[1].color.expect("the quad is filled");
        assert_eq!(
            dark_fill,
            role_color(&harness, Appearance::Dark, Role::MarketUp)
        );
        assert_ne!(
            dark_fill, light_fill,
            "the dark appearance resolves another colour"
        );
        assert_eq!(
            dark[3].color,
            Some(role_color(&harness, Appearance::Dark, Role::FgDefault)),
            "the text run follows the theme too"
        );
    }

    /// P3-1j / P3-1j C: a canvas node that was never written paints no primitives (the node is
    /// still one opaque canvas entry) — the "`set_canvas` was removed" assertion shape.
    #[test]
    fn a_canvas_node_that_was_never_written_paints_no_primitives() {
        let mut harness = WidgetTestApp::new();
        let node = harness.with_cx(|cx| {
            let root = cx.tree().root();
            NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 16.0, 16.0)))
                .layout_override(Rect::new(0.0, 0.0, 16.0, 16.0))
                .id("TEST_EMPTY_CANVAS")
                .build(cx.tree_mut(), root)
                .expect("the root is alive")
        });
        harness.drain();
        assert!(harness.rendered(), "the node itself paints");
        assert!(
            harness.canvas_primitives(node).is_empty(),
            "no set_canvas means nothing inside the canvas"
        );
        assert_eq!(
            harness
                .paint_entries(node)
                .iter()
                .filter(|entry| entry.kind == PrimitiveKind::Canvas)
                .count(),
            1,
            "the node is still one opaque canvas in the frame's paint"
        );
    }

    /// P3-1j / W5a D: a canvas that draws registered geometry is observable, and releasing the
    /// widget hands the mesh back to the process-wide store.
    #[test]
    fn releasing_a_canvas_widget_hands_its_geometry_back() {
        let mut harness = WidgetTestApp::new();
        let baseline = harness.app().geometry().len();
        let widget = harness.mount::<CanvasWidget>(());
        harness.drain();
        let node = canvas_node(&harness, &widget);
        let primitives = harness.canvas_primitives(node);
        let path = primitives
            .iter()
            .find(|primitive| primitive.kind == PrimitiveKind::Path)
            .expect("the canvas draws its mesh");
        assert!(
            path.geometry.is_some(),
            "the path names the geometry it draws: {path:?}"
        );
        assert_eq!(
            harness.app().geometry().len(),
            baseline + 1,
            "one mesh is registered while the widget lives"
        );

        widget.release(harness.app_mut());
        assert_eq!(
            harness.app().geometry().len(),
            baseline,
            "releasing the widget returns its geometry"
        );
        assert!(
            harness.canvas_handle(node).is_none(),
            "the released widget's canvas node is gone"
        );
    }
}

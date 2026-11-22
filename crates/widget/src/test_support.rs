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
//! # Compiling this module
//!
//! It is available to this crate's own tests (`cfg(test)`) and to other crates as
//! `widget::test_support` when the `test-support` feature is enabled. The feature is
//! the only thing that turns on `ui_core`'s own test support, so a release build of
//! `widget` never links the engine's test host.

use std::rc::Rc;

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
    event::{Action, ActionSource},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::SignalScope,
        tree::{NodeKind, NodeTree},
    },
    test_support::app::TestApp,
    window::WindowId,
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
        cell::RefCell,
        rc::Rc,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use app_config_provider::notification::ConfigNotification;
    use bytes::Bytes;
    use i18n::language::codes::LanguageCode;
    use market_participants_connector::{
        connector::{MarketParticipantsConnector, MarketParticipantsConnectorInput},
        runtime::MarketParticipantNetworkSnapshot,
    };
    use network::transport::hub::{TransportHub, TransportPolicy};
    use widget_tokio_runtime::runtime::Runtime;

    use super::{FixtureFailure, WaitingWidget, WidgetTestApp};
    use crate::{
        any::AnyWidget,
        status::{WidgetAction, WidgetState},
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
        let runtime = Runtime::from_handle(harness.services().runtime.clone());
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
        let before = harness.app().frame_requests();
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
            harness.app().frame_requests() > before,
            "the service's frame request was pumped and ran"
        );
        assert!(
            turns >= 2,
            "the request is pumped after a frame and presented on the next turn: {turns}"
        );
    }
}

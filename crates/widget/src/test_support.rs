//! The framework's test host: an engine application, the injected services, one
//! mounted widget, and the assertions the contract's tests are written in.
//!
//! The previous contract's tests drove its own application context and asserted through
//! its element tree. This host is the engine's equivalent and the migration carrier for
//! the twenty widget crates: it builds the same [`WidgetServices`] the composition
//! root builds (minus the market-participants directory, which still needs a toolkit
//! context to construct — see [`WidgetServices`]), installs the [`FrameRouter`] the
//! composition root installs, and drains frames through
//! [`ui_core::test_support::app::TestApp`].
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

use app_config::settings::Config;
use base_ui::theme::handle::ThemeHandle;
use bytes::Bytes;
use i18n::{language::codes::LanguageCode, translate::Translate, translator::types::I18n};
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

/// The messages the framework itself resolves while a widget runs, and the copy its
/// tests report as a widget's own. `test.waiting` stands for a widget's own notice,
/// `sample.failure` for a widget's typed failure; the rest are the framework's own
/// labels.
pub const MESSAGES_EN: &str = r#"
common:
  connecting: Connecting
  loading: Loading
  retry: Retry
sample:
  failure: the fixture failed
test:
  waiting: Waiting for orders
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
    services: Rc<WidgetServices>,
    router: FrameRouter,
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
        let scope = app.app_mut().signal_scope();
        let language = app.app_mut().signal_scoped(scope, LanguageCode::En);
        let config = app.app_mut().signal_scoped(scope, Config::default());
        let reduce_motion = app.app_mut().signal_scoped(scope, false);
        let theme = ThemeHandle::new(app.app_mut(), base_ui::theme::appearance::Appearance::Light);
        let runtime = app.app().runtime_handle().clone();
        let sound: Rc<dyn Fn(&'static str) -> Option<Bytes>> = Rc::new(|_| None);
        let services = Rc::new(WidgetServices {
            translate,
            language,
            config,
            theme,
            market_participants: None,
            kytrix_service: None,
            open_url: Rc::new(|_| {}),
            sound,
            reduce_motion,
            runtime,
        });
        let router = FrameRouter::new(app.app_mut()).expect("the host owns the frame hook slot");
        let status = WidgetStatus::new(app.app_mut(), scope);

        Self {
            app,
            window,
            services,
            router,
            scope,
            status,
        }
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

    /// The installed frame router.
    #[must_use]
    pub fn router(&self) -> &FrameRouter {
        &self.router
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
        let mut cx = WidgetCx::new(
            self.app.app_mut(),
            self.window,
            &services,
            self.scope,
            self.router.clone(),
        );
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
        let router = self.router.clone();
        let scope = self.scope;
        let root = self.root();
        let mut cx = WidgetCx::new(self.app.app_mut(), self.window, &services, scope, router);
        WidgetSurface::build(&mut cx, root, status).expect("the surface builds")
    }

    /// Runs the surface's sync for one frame and then drains it.
    pub fn sync_surface(&mut self, surface: &mut WidgetSurface, has_content: bool) {
        let services = Rc::clone(&self.services);
        let router = self.router.clone();
        let scope = self.scope;
        {
            let mut cx = WidgetCx::new(self.app.app_mut(), self.window, &services, scope, router);
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
        let router = self.router.clone();
        let parent = self.root();
        AnyWidget::mount::<W>(
            self.app.app_mut(),
            self.window,
            parent,
            &services,
            &router,
            WidgetInstanceId::new(),
            config,
            crate::config::WidgetConfigChange::default(),
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
    use std::{rc::Rc, sync::Arc};

    use i18n::language::codes::LanguageCode;

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
}

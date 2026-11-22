//! The test host the crate's dialogs are driven against.
//!
//! A selector is not a widget, so the contract layer's `WidgetTestApp` (which is built
//! around `WidgetView`) does not fit it. This harness composes the same aggregate the
//! application root composes — the frozen [`WidgetServices`], the layer stack, the frame
//! router and the animation router — over `ui_core::test_support::TestApp`, and adds the
//! three things a dialog test needs: a published catalogue fixture, a real pointer click,
//! and a settled frame.
//!
//! It deliberately does **not** live in `base_ui::kit::test_support`: the kit's test
//! support is a single-writer surface (P3 §3.6), and a dialog harness is this crate's own
//! business (assess D-W1-6, option b).
//!
//! # Frames
//!
//! [`Harness::drain`] runs the frame router's routes (the dialogs' `sync`) and then one
//! engine frame — the order the platform pump uses. `TestApp::drain` never runs
//! `App::on_frame`, which is why the tick is explicit here.

use std::{rc::Rc, sync::Arc, time::Duration};

use app_config::settings::Config;
use base_ui::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::{appearance::Appearance, handle::ThemeHandle},
};
use i18n::{language::codes::LanguageCode, translate::Translate};
use market_participants_connector::catalog::{
    batch::CatalogEntry,
    source::{CatalogRouteRequirements, CatalogRouteUpdate},
    store::Catalog,
    test_support,
};
use market_participants_model::{listing::route::VenueRoute, routing::CapabilityId};
use smol_str::SmolStr;
use ui_core::{
    app::App,
    event::{InputEvent, Modifiers, PointerButton},
    math::{Point, Rect},
    node::{
        id::NodeId,
        tree::{Node, NodeKind},
    },
    paint::style::StylePatch,
    test_support::app::TestApp,
    window::WindowId,
};
use widget::{frame::FrameRouter, services::WidgetServices};

use crate::{
    host::SelectorHost,
    instrument_selector::LoadedScope,
    prediction_translation::{
        LocalTranslator, TranslatedSegment, TranslationCancel, TranslationFailure,
        TranslationSegment, TranslationSetup,
    },
};

/// The translation context a test renders through: the product's own catalogue, so a test
/// asserts the sentence the user reads rather than a fixture of this crate's making.
pub(crate) fn translate() -> Translate {
    translate_in(LanguageCode::En)
}

/// The product's catalogue in one language.
///
/// # Panics
/// The embedded catalogue does not load (a build fault, not a test condition).
pub(crate) fn translate_in(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::global::embedded::load().expect("the embedded locales load")),
        language,
    )
}

/// One scope of a fixture that publishes a catalog and carries `capabilities`.
pub(crate) fn scope(route: VenueRoute, capabilities: &[CapabilityId]) -> LoadedScope {
    let mut carried = Vec::with_capacity(capabilities.len() + 1);
    carried.push(CapabilityId::MarketCatalog);
    carried.extend_from_slice(capabilities);

    LoadedScope {
        route,
        capabilities: carried,
        provides_catalog: true,
    }
}

/// A caller's snapshot of the loaded scopes.
pub(crate) fn loaded(routes: Vec<LoadedScope>) -> Arc<[LoadedScope]> {
    routes.into()
}

/// A catalogue that published `entries` on `route`, built without a directory root.
///
/// The entries travel the catalogue's own fixture publication path
/// (`commit_fixture_routes` + `publish_fixture_route`), so a test snapshot is the same
/// kind of value a fetch produces.
///
/// # Panics
/// The fixture registration or publication is refused (a fixture bug).
pub(crate) fn catalog_with(route: &VenueRoute, entries: Vec<CatalogEntry>) -> Arc<Catalog> {
    let catalog = Arc::new(Catalog::new());
    let requirements = CatalogRouteRequirements::from_catalog_routes([route.clone()]);
    let registration = test_support::registration(route, "fixture-source");
    let update = CatalogRouteUpdate::new(
        [test_support::binding(
            "fixture-source",
            test_support::listing(entries.clone()),
        )],
        [registration],
    );
    test_support::commit_fixture_routes(&catalog, update, &requirements)
        .expect("the fixture route registration is accepted");
    test_support::publish_fixture_route(&catalog, route, entries)
        .expect("the fixture route publishes");
    catalog
}

/// The headless host every dialog test runs against.
pub(crate) struct Harness {
    app: TestApp,
    services: Rc<WidgetServices>,
    router: Rc<FrameRouter>,
    host: SelectorHost,
}

impl Harness {
    /// A host whose services have no directory root (the root's constructor needs the old
    /// toolkit) and whose catalogue is absent.
    ///
    /// A dialog opened against it draws its "market not loaded" state, which is a state a
    /// test asserts on its own.
    pub(crate) fn new() -> Self {
        // The dialogs are 720×520 plus the kit's panel chrome; a window that fits them keeps
        // a click on the confirm row inside the content area.
        let mut app = TestApp::with_options(ui_core::window::WindowOptions {
            size: ui_core::math::Size::new(1280.0, 900.0),
            ..ui_core::window::WindowOptions::default()
        });
        // The same aggregate and the same three composition-root singletons the application
        // root builds: the dialogs reach none of them through a global of their own.
        let runtime = app.app().runtime_handle().clone();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let layer = Rc::new(Layer::new());
        let router =
            Rc::new(FrameRouter::new(app.app_mut()).expect("the test host owns the frame hook"));
        let animation = Rc::new(AnimationRouter::new());
        let services = Rc::new(
            WidgetServices::builder(
                app.app_mut(),
                runtime,
                translate(),
                theme,
                Rc::clone(&layer),
                Rc::clone(&router),
                Rc::clone(&animation),
            )
            .build(),
        );
        let host = SelectorHost::new(Rc::clone(&services));
        Self {
            app,
            services,
            router,
            host,
        }
    }

    /// A host whose dialogs read `catalog`.
    pub(crate) fn with_catalog(catalog: Arc<Catalog>) -> Self {
        let mut harness = Self::new();
        harness.host = harness.host.clone().with_catalog(Some(catalog));
        harness
    }

    /// A host whose prediction dialog can translate.
    pub(crate) fn with_translator(mut self, translator: Arc<dyn LocalTranslator>) -> Self {
        self.host = self.host.clone().with_translator(Some(translator));
        self
    }

    /// The host the dialogs are opened with.
    pub(crate) fn host(&self) -> &SelectorHost {
        &self.host
    }

    /// The injected services.
    pub(crate) fn services(&self) -> &WidgetServices {
        &self.services
    }

    /// The injected services, as the shared handle the host was built from.
    pub(crate) fn services_rc(&self) -> Rc<WidgetServices> {
        Rc::clone(&self.services)
    }

    /// The application's one layer stack (the composition-root singleton the services carry).
    pub(crate) fn layer(&self) -> &Layer {
        &self.services.layer
    }

    /// The frame router: its route count is what says a closed dialog released itself.
    pub(crate) fn router(&self) -> &FrameRouter {
        &self.router
    }

    /// The dialogs' overlay count in this window.
    pub(crate) fn overlay_count(&self) -> usize {
        self.app.app().overlays(self.app.window_id()).len()
    }

    pub(crate) fn app(&self) -> &App {
        self.app.app()
    }

    pub(crate) fn app_mut(&mut self) -> &mut App {
        self.app.app_mut()
    }

    pub(crate) fn window(&self) -> WindowId {
        self.app.window_id()
    }

    pub(crate) fn root(&self) -> NodeId {
        self.app
            .app()
            .window(self.app.window_id())
            .expect("the harness window exists")
            .tree()
            .root()
    }

    /// Runs the dialogs' frame routes and then one engine frame — the platform pump order.
    pub(crate) fn drain(&mut self) {
        let _ = self.router.tick(self.app.app_mut());
        self.app.drain().expect("the harness frame");
    }

    /// Runs `frames` production-shaped frames.
    pub(crate) fn settle(&mut self, frames: usize) {
        for _ in 0..frames {
            self.drain();
        }
    }

    /// Drains until `done` is true, or `attempts` frames passed; returns whether it became
    /// true.
    ///
    /// A background matcher publishes its ranking on its own thread, so a test that reads
    /// a ranked list gives it a few frames and a little wall time rather than a fixed
    /// count.
    pub(crate) fn settle_until(
        &mut self,
        attempts: usize,
        mut done: impl FnMut(&Harness) -> bool,
    ) -> bool {
        for _ in 0..attempts {
            self.drain();
            if done(self) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        done(self)
    }

    /// Advances the engine clock, which is what the one-shot timers (search debounce,
    /// translation deadline) are measured against, then drains.
    pub(crate) fn advance(&mut self, by: Duration) {
        self.app.advance(by);
    }

    /// The layout box of one node, if it is laid out.
    pub(crate) fn bounds(&self, node: NodeId) -> Option<Rect> {
        self.app.probe().bounds(node)
    }

    /// Clicks one window position with the primary button.
    pub(crate) fn click(&mut self, position: Point) {
        self.app.inject(InputEvent::pointer_down(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        self.app.inject(InputEvent::pointer_up(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
    }

    /// Moves the pointer over one window position, as the platform pump reports motion.
    pub(crate) fn move_pointer(&mut self, position: Point) {
        let _ = self.app.move_pointer(position);
    }

    /// Delivers one platform event the way the platform pump does: the engine's interaction
    /// dispatch runs, and the event is queued for the application to read.
    pub(crate) fn pump(&mut self, event: ui_core::event::Event) {
        self.app.dispatch_event(event);
    }

    /// The events the application can still read from the observation queue.
    pub(crate) fn take_events(&mut self) -> Vec<(WindowId, ui_core::event::Event)> {
        self.app.app_mut().drain_events()
    }

    /// The style patch the engine computed for `node` (what the node's `bind_style` wrote).
    pub(crate) fn style_patch(&self, node: NodeId) -> Option<&StylePatch> {
        let tree = self.app.app().window(self.app.window_id())?.tree();
        tree.get(node).ok().map(Node::style_patch)
    }

    /// The first child of `node`.
    pub(crate) fn first_child(&self, node: NodeId) -> Option<NodeId> {
        let tree = self.app.app().window(self.app.window_id())?.tree();
        tree.get(node)
            .ok()
            .and_then(|entry| entry.children().first().copied())
    }

    /// The number of nodes in the harness window's tree (a closed dialog leaves none behind).
    pub(crate) fn tree_len(&self) -> usize {
        self.app
            .app()
            .window(self.app.window_id())
            .expect("the harness window exists")
            .tree()
            .len()
    }

    /// The text one text node draws.
    pub(crate) fn node_text(&self, node: NodeId) -> SmolStr {
        let tree = self
            .app
            .app()
            .window(self.app.window_id())
            .expect("the harness window exists")
            .tree();
        match tree.get(node).expect("the node exists").kind() {
            NodeKind::Text(content) => content.text.clone(),
            other => panic!("expected a text node, got {other:?}"),
        }
    }

    /// The nearest node under the window root whose debug id starts with `prefix`.
    pub(crate) fn find(&self, prefix: &str) -> Option<NodeId> {
        let tree = self.app.app().window(self.app.window_id())?.tree();
        let mut stack = vec![tree.root()];
        while let Some(node) = stack.pop() {
            if tree.debug_id(node).is_some_and(|id| id.starts_with(prefix)) {
                return Some(node);
            }
            if let Ok(entry) = tree.get(node) {
                stack.extend(entry.children().iter().rev().copied());
            }
        }
        None
    }

    /// Every debug id in the tree, in DFS order.
    pub(crate) fn debug_ids(&self) -> Vec<SmolStr> {
        let mut ids = Vec::new();
        let Some(window) = self.app.app().window(self.app.window_id()) else {
            return ids;
        };
        let tree = window.tree();
        let mut stack = vec![tree.root()];
        while let Some(node) = stack.pop() {
            if let Some(id) = tree.debug_id(node) {
                ids.push(SmolStr::new(id));
            }
            if let Ok(entry) = tree.get(node) {
                stack.extend(entry.children().iter().rev().copied());
            }
        }
        ids
    }

    /// The text the subtree under `node` draws, one line per text node, in DFS order.
    pub(crate) fn text_of(&self, node: NodeId) -> String {
        let mut lines = Vec::new();
        let Some(window) = self.app.app().window(self.app.window_id()) else {
            return String::new();
        };
        let tree = window.tree();
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            let Ok(entry) = tree.get(current) else {
                continue;
            };
            if let NodeKind::Text(content) = entry.kind() {
                lines.push(content.text.clone());
            }
            stack.extend(entry.children().iter().rev().copied());
        }
        lines.join("\n")
    }

    /// Whether the subtree draws `text` anywhere.
    pub(crate) fn has_text(&self, node: NodeId, text: &str) -> bool {
        self.text_of(node).lines().any(|line| line == text)
    }

    /// Switches the interface language the way the composition root does: the translator
    /// first, then the signal every `bind_text` closure reads.
    pub(crate) fn switch_language(&mut self, language: LanguageCode) {
        self.services.translate.set_language(language);
        let signal = self.services.language;
        let _ = self.app.app_mut().write(&signal, language);
    }

    /// Rewrites the settings snapshot the services carry, as the application root does.
    pub(crate) fn update_config(&mut self, update: impl FnOnce(&mut Config)) {
        let mut config = (*self.app.app().read(&self.services.config)).clone();
        update(&mut config);
        let signal = self.services.config;
        let _ = self.app.app_mut().write(&signal, config);
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // A dialog that is still open holds nodes and a route; dropping the app first would
        // leak the route table's closure for the rest of the process (the router is shared
        // with nothing else in a test, but the state must not outlive the frame host).
        let _ = self.router.tick(self.app.app_mut());
        self.app.drain().ok();
    }
}

/// A translator that answers every segment with the text it was given.
pub(crate) struct EchoTranslator;

impl LocalTranslator for EchoTranslator {
    fn prepare(
        &self,
        _setup: &TranslationSetup,
        _cancel: TranslationCancel,
    ) -> Result<(), TranslationFailure> {
        Ok(())
    }

    fn translate(
        &self,
        _setup: &TranslationSetup,
        _target: app_config::settings::Language,
        segments: Vec<TranslationSegment>,
        _cancel: TranslationCancel,
    ) -> Result<Vec<TranslatedSegment>, TranslationFailure> {
        Ok(segments
            .into_iter()
            .map(|segment| TranslatedSegment {
                id: segment.id,
                text: segment.text,
            })
            .collect())
    }
}

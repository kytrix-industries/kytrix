use std::{rc::Rc, sync::Arc, time::Duration};

use app_config::settings::Config;
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
    math::{LogicalPoint, LogicalRect, px},
    node::{
        id::NodeId,
        tree::{Node, NodeKind},
    },
    paint::style::StylePatch,
    test_support::app::TestApp,
    window::WindowId,
};
use ui_kit::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::{appearance::Appearance, handle::ThemeHandle},
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

pub(crate) fn translate() -> Translate {
    translate_in(LanguageCode::En)
}

pub(crate) fn translate_in(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::embedded::load().expect("the embedded locales load")),
        language,
    )
}

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

pub(crate) fn loaded(routes: Vec<LoadedScope>) -> Arc<[LoadedScope]> {
    routes.into()
}

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

pub(crate) struct Harness {
    app: TestApp,
    services: Rc<WidgetServices>,
    router: Rc<FrameRouter>,
    host: SelectorHost,
}

impl Harness {
    pub(crate) fn new() -> Self {
        let mut app = TestApp::with_options(ui_core::window::WindowOptions {
            size: ui_core::math::LogicalSize::new(px(1280.0), px(900.0)),
            ..ui_core::window::WindowOptions::default()
        });
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

    pub(crate) fn with_catalog(catalog: Arc<Catalog>) -> Self {
        let mut harness = Self::new();
        harness.host = harness.host.clone().with_catalog(Some(catalog));
        harness
    }

    pub(crate) fn with_translator(mut self, translator: Arc<dyn LocalTranslator>) -> Self {
        self.host = self.host.clone().with_translator(Some(translator));
        self
    }

    pub(crate) fn host(&self) -> &SelectorHost {
        &self.host
    }

    pub(crate) fn services(&self) -> &WidgetServices {
        &self.services
    }

    pub(crate) fn services_rc(&self) -> Rc<WidgetServices> {
        Rc::clone(&self.services)
    }

    pub(crate) fn layer(&self) -> &Layer {
        &self.services.layer
    }

    pub(crate) fn router(&self) -> &FrameRouter {
        &self.router
    }

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

    pub(crate) fn drain(&mut self) {
        let _ = self.router.tick(self.app.app_mut());
        self.app.drain().expect("the harness frame");
    }

    pub(crate) fn settle(&mut self, frames: usize) {
        for _ in 0..frames {
            self.drain();
        }
    }

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

    pub(crate) fn advance(&mut self, by: Duration) {
        self.app.advance(by);
    }

    pub(crate) fn bounds(&self, node: NodeId) -> Option<LogicalRect> {
        self.app.probe().bounds(node)
    }

    pub(crate) fn click(&mut self, position: LogicalPoint) {
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

    pub(crate) fn move_pointer(&mut self, position: LogicalPoint) {
        let _ = self.app.move_pointer(position);
    }

    pub(crate) fn pump(&mut self, event: ui_core::event::Event) {
        self.app.dispatch_event(event);
    }

    pub(crate) fn take_events(&mut self) -> Vec<(WindowId, ui_core::event::Event)> {
        self.app.app_mut().drain_events()
    }

    pub(crate) fn style_patch(&self, node: NodeId) -> Option<&StylePatch> {
        let tree = self.app.app().window(self.app.window_id())?.tree();
        tree.get(node).ok().map(Node::style_patch)
    }

    pub(crate) fn first_child(&self, node: NodeId) -> Option<NodeId> {
        let tree = self.app.app().window(self.app.window_id())?.tree();
        tree.get(node)
            .ok()
            .and_then(|entry| entry.children().first().copied())
    }

    pub(crate) fn tree_len(&self) -> usize {
        self.app
            .app()
            .window(self.app.window_id())
            .expect("the harness window exists")
            .tree()
            .len()
    }

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

    pub(crate) fn find(&self, prefix: &str) -> Option<NodeId> {
        self.find_all(prefix).into_iter().next()
    }

    pub(crate) fn find_all(&self, prefix: &str) -> Vec<NodeId> {
        let Some(window) = self.app.app().window(self.app.window_id()) else {
            return Vec::new();
        };
        let tree = window.tree();
        let mut found = Vec::new();
        let mut stack = vec![tree.root()];
        while let Some(node) = stack.pop() {
            if tree.debug_id(node).is_some_and(|id| id.starts_with(prefix)) {
                found.push(node);
            }
            if let Ok(entry) = tree.get(node) {
                stack.extend(entry.children().iter().rev().copied());
            }
        }
        found
    }

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

    pub(crate) fn has_text(&self, node: NodeId, text: &str) -> bool {
        self.text_of(node).lines().any(|line| line == text)
    }

    pub(crate) fn switch_language(&mut self, language: LanguageCode) {
        self.services.translate.set_language(language);
        let signal = self.services.language;
        let _ = self.app.app_mut().write(&signal, language);
    }

    pub(crate) fn update_config(&mut self, update: impl FnOnce(&mut Config)) {
        let mut config = (*self.app.app().read(&self.services.config)).clone();
        update(&mut config);
        let signal = self.services.config;
        let _ = self.app.app_mut().write(&signal, config);
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.router.tick(self.app.app_mut());
        self.app.drain().ok();
    }
}

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

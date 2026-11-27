use std::{
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::{
    future::BoxFuture,
    task::{Context as TaskContext, noop_waker},
};
use i18n::{language::codes::LanguageCode, translate::Translate};
use market_participants_connector::{connector::VenueLoadError, venue::definition};
use market_participants_model::identifier::VenueId;
use tokio::sync::oneshot;
use ui_core::{
    app::App,
    event::{InputEvent, Modifiers, PointerButton},
    math::{LogicalPoint, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId, tree::NodeKind},
    popup::PopupTier,
    test_support::app::TestApp,
    window::{WindowId, WindowOptions},
};
use ui_kit::theme::{appearance::Appearance, handle::ThemeHandle};
use widget::{frame::FrameRouter, services::WidgetServices};

use crate::{
    host::{MarketParticipantsHost, RuntimeExecutor, VenueDirectory},
    menu::MarketParticipants,
};

#[derive(Default)]
pub(crate) struct TestExecutor {
    tasks: std::cell::RefCell<Vec<BoxFuture<'static, ()>>>,
}

impl TestExecutor {
    fn pump(&self) {
        let waker = noop_waker();
        let mut context = TaskContext::from_waker(&waker);
        let mut tasks = self.tasks.borrow_mut();
        let mut pending = Vec::with_capacity(tasks.len());
        for mut task in tasks.drain(..) {
            if task.as_mut().poll(&mut context).is_pending() {
                pending.push(task);
            }
        }
        *tasks = pending;
    }
}

impl RuntimeExecutor for TestExecutor {
    fn spawn(&self, task: BoxFuture<'static, ()>) {
        self.tasks.borrow_mut().push(task);
    }
}

pub(crate) fn translate() -> Translate {
    translate_in(LanguageCode::En)
}

pub(crate) fn translate_in(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::embedded::load().expect("the embedded locales load")),
        language,
    )
}

pub(crate) fn supported_venues() -> Vec<VenueId> {
    definition::supported_venues()
}

pub(crate) fn venue(name: &str) -> VenueId {
    VenueId::from_name(name).expect("the built-in venue identity is canonical")
}

#[derive(Default)]
struct DirectoryState {
    loaded: ahash::AHashSet<VenueId>,
    gated: ahash::AHashSet<VenueId>,
    loads: Vec<VenueId>,
    cancelled: Vec<VenueId>,
    pending: ahash::AHashMap<VenueId, oneshot::Sender<Result<(), VenueLoadError>>>,
}

pub(crate) struct FakeDirectory {
    venues: Vec<VenueId>,
    state: Mutex<DirectoryState>,
}

impl FakeDirectory {
    pub(crate) fn new(venues: Vec<VenueId>) -> Arc<Self> {
        Arc::new(Self {
            venues,
            state: Mutex::new(DirectoryState::default()),
        })
    }

    pub(crate) fn require_credentials(&self, venue: VenueId) {
        self.state
            .lock()
            .expect("the directory is readable")
            .gated
            .insert(venue);
    }

    pub(crate) fn mark_loaded(&self, venue: VenueId) {
        self.state
            .lock()
            .expect("the directory is readable")
            .loaded
            .insert(venue);
    }

    pub(crate) fn loads(&self) -> Vec<VenueId> {
        self.state
            .lock()
            .expect("the directory is readable")
            .loads
            .clone()
    }

    pub(crate) fn cancelled(&self) -> Vec<VenueId> {
        self.state
            .lock()
            .expect("the directory is readable")
            .cancelled
            .clone()
    }

    pub(crate) fn pending(&self) -> usize {
        self.state
            .lock()
            .expect("the directory is readable")
            .pending
            .len()
    }

    pub(crate) fn complete(&self, venue: VenueId, result: Result<(), VenueLoadError>) {
        let sender = self
            .state
            .lock()
            .expect("the directory is readable")
            .pending
            .remove(&venue);
        if result.is_ok() {
            self.state
                .lock()
                .expect("the directory is readable")
                .loaded
                .insert(venue);
        }
        let sender = sender.unwrap_or_else(|| panic!("{venue} has no load in flight"));
        let _ = sender.send(result);
    }

    fn cancel(&self, venue: VenueId) {
        let sender = self
            .state
            .lock()
            .expect("the directory is readable")
            .pending
            .remove(&venue);
        if let Some(sender) = sender {
            self.state
                .lock()
                .expect("the directory is readable")
                .cancelled
                .push(venue);
            let _ = sender.send(Err(VenueLoadError::Interrupted { venue }));
        }
    }
}

impl VenueDirectory for FakeDirectory {
    fn venues(&self) -> Vec<VenueId> {
        self.venues.clone()
    }

    fn is_loaded(&self, venue: &VenueId) -> bool {
        self.state
            .lock()
            .expect("the directory is readable")
            .loaded
            .contains(venue)
    }

    fn needs_credentials(&self, venue: VenueId) -> bool {
        self.state
            .lock()
            .expect("the directory is readable")
            .gated
            .contains(&venue)
    }

    fn load(&self, venue: VenueId) -> BoxFuture<'static, Result<(), VenueLoadError>> {
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = self.state.lock().expect("the directory is readable");
            state.loads.push(venue);
            state.pending.insert(venue, sender);
        }
        Box::pin(async move {
            match receiver.await {
                Ok(result) => result,
                Err(_) => Err(VenueLoadError::Interrupted { venue }),
            }
        })
    }

    fn cancel_load(&self, venue: &VenueId) {
        self.cancel(*venue);
    }
}

pub(crate) struct Harness {
    app: TestApp,
    services: Rc<WidgetServices>,
    router: Rc<FrameRouter>,
    directory: Arc<FakeDirectory>,
    executor: Rc<TestExecutor>,
    menu: MarketParticipants,
}

impl Harness {
    pub(crate) fn new() -> Self {
        Self::with_venues(supported_venues())
    }

    pub(crate) fn with_venues(venues: Vec<VenueId>) -> Self {
        let mut app = TestApp::with_options(WindowOptions {
            size: LogicalSize::new(px(1280.0), px(900.0)),
            ..WindowOptions::default()
        });
        let runtime = app.app().runtime_handle().clone();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let router =
            Rc::new(FrameRouter::new(app.app_mut()).expect("the test host owns the frame hook"));
        let services = Rc::new(
            WidgetServices::builder(
                app.app_mut(),
                runtime,
                translate(),
                theme,
                Rc::clone(&router),
            )
            .build(),
        );
        let directory = FakeDirectory::new(venues);
        let window = app.window_id();
        let toolbar = {
            let tree = app
                .app_mut()
                .window_mut(window)
                .expect("the harness window exists")
                .tree_mut();
            let root = tree.root();
            NodeBuilder::boxed()
                .id("TEST_MARKET_PARTICIPANTS_TOOLBAR")
                .build(tree, root)
                .expect("the harness toolbar builds")
        };
        let executor = Rc::new(TestExecutor::default());
        let host = MarketParticipantsHost::new(Rc::clone(&services), directory.clone())
            .with_executor(executor.clone());
        let mut menu = MarketParticipants::new(host, app.app_mut());
        menu.build(app.app_mut(), window, toolbar)
            .expect("the menu's trigger builds");
        let mut harness = Self {
            app,
            services,
            router,
            directory,
            executor,
            menu,
        };
        harness.settle(2);
        harness
    }

    pub(crate) fn services_rc(&self) -> Rc<WidgetServices> {
        Rc::clone(&self.services)
    }

    pub(crate) fn directory(&self) -> Arc<FakeDirectory> {
        Arc::clone(&self.directory)
    }

    pub(crate) fn menu(&self) -> &MarketParticipants {
        &self.menu
    }

    pub(crate) fn menu_mut(&mut self) -> &mut MarketParticipants {
        &mut self.menu
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

    pub(crate) fn drain(&mut self) {
        self.executor.pump();
        let _ = self.router.tick(self.app.app_mut());
        self.menu
            .sync(self.app.app_mut())
            .expect("the menu's frame");
        self.app.drain().expect("the harness frame");
    }

    pub(crate) fn executor(&self) -> Rc<TestExecutor> {
        Rc::clone(&self.executor)
    }

    pub(crate) fn settle(&mut self, frames: usize) {
        for _ in 0..frames {
            self.drain();
        }
    }

    pub(crate) fn bounds(&self, node: NodeId) -> Option<ui_core::math::LogicalRect> {
        self.app.probe().bounds(node)
    }

    pub(crate) fn click(&mut self, node: NodeId) {
        let position = centre_of(
            self.bounds(node)
                .unwrap_or_else(|| panic!("the harness draws the node it is asked to click")),
        );
        let _ = self.app.move_pointer(position);
        self.app.inject(InputEvent::pointer_down(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        self.drain();
        self.app.inject(InputEvent::pointer_up(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        self.drain();
    }

    pub(crate) fn click_button(&mut self, button: &ui_kit::kit::button::Button) {
        let node = button.node().expect("the button is built");
        self.click(node);
    }

    pub(crate) fn press_key(&mut self, code: ui_core::event::KeyCode, modifiers: Modifiers) {
        self.app.inject(InputEvent::key_down(code, modifiers));
        self.app.inject(InputEvent::key_up(code, modifiers));
        self.settle(1);
    }

    pub(crate) fn click_point(&mut self, position: LogicalPoint) {
        self.app.inject(InputEvent::pointer_down(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        self.drain();
        self.app.inject(InputEvent::pointer_up(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        self.drain();
    }

    pub(crate) fn settle_fade(&mut self) {
        for _ in 0..3 {
            self.app.advance(Duration::from_millis(200));
            self.drain();
        }
    }

    pub(crate) fn find(&self, prefix: &str) -> Option<NodeId> {
        self.find_all(prefix).into_iter().next()
    }

    pub(crate) fn find_all(&self, prefix: &str) -> Vec<NodeId> {
        let Some(window) = self.app.app().window(self.window()) else {
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

    pub(crate) fn text_of(&self, node: NodeId) -> String {
        let mut lines = Vec::new();
        let Some(window) = self.app.app().window(self.window()) else {
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

    pub(crate) fn has_text_anywhere(&self, text: &str) -> bool {
        let Some(window) = self.app.app().window(self.window()) else {
            return false;
        };
        let tree = window.tree();
        let mut stack = vec![tree.root()];
        while let Some(node) = stack.pop() {
            let Ok(entry) = tree.get(node) else {
                continue;
            };
            if matches!(entry.kind(), NodeKind::Text(content) if content.text == text) {
                return true;
            }
            stack.extend(entry.children().iter().copied());
        }
        false
    }

    pub(crate) fn remove_node(&mut self, node: NodeId) {
        let window = self.window();
        self.app
            .app_mut()
            .remove_node(window, node)
            .expect("the harness removes a node it built");
    }

    pub(crate) fn release_menu(&mut self) {
        self.menu.release(self.app.app_mut());
    }

    pub(crate) fn start_load(&mut self, venue: VenueId) -> Result<(), crate::error::MenuError> {
        self.menu.start_load(self.app.app_mut(), venue)
    }

    pub(crate) fn tree_len(&self) -> usize {
        self.app
            .app()
            .window(self.window())
            .expect("the harness window exists")
            .tree()
            .len()
    }

    pub(crate) fn popup_total(&self) -> usize {
        self.app
            .app()
            .popup_stack(self.window())
            .map_or(0, |stack| stack.entries.len())
    }
    pub(crate) fn overlay_count(&self) -> usize {
        self.app
            .app()
            .popup_stack(self.window())
            .map_or(0, |stack| {
                stack
                    .entries
                    .iter()
                    .filter(|entry| entry.tier != PopupTier::Scrim)
                    .count()
            })
    }

    pub(crate) fn frame_requests(&self) -> u64 {
        self.app.app().frame_requests()
    }

    pub(crate) fn row(&self, venue: VenueId) -> Option<NodeId> {
        self.find(&format!("TEST_MARKET_PARTICIPANTS_ROW_{venue:?}"))
    }

    pub(crate) fn action(&self, venue: VenueId, loaded: bool) -> Option<NodeId> {
        let state = if loaded { "REFRESH" } else { "LOAD" };
        self.find(&format!(
            "TEST_MARKET_PARTICIPANTS_ACTION_{state}_{venue:?}"
        ))
    }

    pub(crate) fn shows_mark(&self, venue: VenueId, state: &str) -> bool {
        self.find(&format!("TEST_MARKET_PARTICIPANTS_{state}_{venue:?}"))
            .is_some()
    }

    pub(crate) fn shows_row(&self, venue: VenueId) -> bool {
        self.row(venue).is_some()
    }

    pub(crate) fn shows_panel(&self) -> bool {
        self.find("TEST_MARKET_PARTICIPANTS_ROW_").is_some()
    }

    pub(crate) fn switch_language(&mut self, language: LanguageCode) {
        self.services.translate.set_language(language);
        let signal = self.services.language;
        let _ = self.app.app_mut().write(&signal, language);
    }
}

fn centre_of(rect: ui_core::math::LogicalRect) -> LogicalPoint {
    LogicalPoint {
        x: rect.origin.x + rect.size.width / 2.0,
        y: rect.origin.y + rect.size.height / 2.0,
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.menu.release(self.app.app_mut());
    }
}

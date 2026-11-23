//! The headless host the menu's tests drive it against.
//!
//! A title-bar menu is not a widget, so the contract layer's `WidgetTestApp` (which is built
//! around `WidgetView`) does not fit it. This harness composes the same aggregate the
//! application root composes — the frozen [`WidgetServices`], the layer stack, the frame
//! router and the animation router — over `ui_core::test_support::TestApp`, and adds the
//! three things a menu test needs: a directory whose loads the test settles by hand, a real
//! pointer click, and the platform pump's order (frame hook first, then the frame).
//!
//! The directory is a fake on purpose: the connector's constructor still needs the old
//! toolkit's runtime type, so a test that built one would be testing the connector, not the
//! menu. Every decision the menu makes — which venue to list, whether it is loaded, whether
//! its load is gated on credentials, what one load reported — is a decision over
//! [`VenueDirectory`], and the fake is that trait with the answers a test states.

use std::{
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};

use base_ui::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::{appearance::Appearance, handle::ThemeHandle},
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
    math::{Point, Size},
    node::{builder::NodeBuilder, id::NodeId, tree::NodeKind},
    test_support::app::TestApp,
    window::{WindowId, WindowOptions},
};
use widget::{frame::FrameRouter, services::WidgetServices};

use crate::{
    host::{MarketParticipantsHost, RuntimeExecutor, VenueDirectory},
    menu::MarketParticipants,
};

/// The runtime seam a headless menu is handed: the futures are pumped by the harness.
///
/// The engine's test host owns a current-thread runtime nothing polls, so a load started
/// through it would never make progress. The harness polls the tasks itself, which also keeps
/// every load deterministic — a load settles exactly when a test settles it.
#[derive(Default)]
pub(crate) struct TestExecutor {
    tasks: std::cell::RefCell<Vec<BoxFuture<'static, ()>>>,
}

impl TestExecutor {
    /// Polls every task once; tasks that finished are dropped, the rest wait for the next
    /// poll.
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
        Rc::new(i18n::embedded::load().expect("the embedded locales load")),
        language,
    )
}

/// The venues every test's directory speaks, in declaration order.
pub(crate) fn supported_venues() -> Vec<VenueId> {
    definition::supported_venues()
}

/// One built-in venue by its canonical name.
///
/// # Panics
/// The name is not one this build carries (a test bug).
pub(crate) fn venue(name: &str) -> VenueId {
    VenueId::from_name(name).expect("the built-in venue identity is canonical")
}

/// The state one test's fake directory keeps.
#[derive(Default)]
struct DirectoryState {
    loaded: ahash::AHashSet<VenueId>,
    gated: ahash::AHashSet<VenueId>,
    loads: Vec<VenueId>,
    cancelled: Vec<VenueId>,
    pending: ahash::AHashMap<VenueId, oneshot::Sender<Result<(), VenueLoadError>>>,
}

/// The directory a test hands the menu: every load waits until the test settles it.
pub(crate) struct FakeDirectory {
    venues: Vec<VenueId>,
    state: Mutex<DirectoryState>,
}

impl FakeDirectory {
    /// A directory that speaks `venues` and has loaded none of them.
    pub(crate) fn new(venues: Vec<VenueId>) -> Arc<Self> {
        Arc::new(Self {
            venues,
            state: Mutex::new(DirectoryState::default()),
        })
    }

    /// Makes loading `venue` require credentials, as a venue whose reads are signed does.
    pub(crate) fn require_credentials(&self, venue: VenueId) {
        self.state
            .lock()
            .expect("the directory is readable")
            .gated
            .insert(venue);
    }

    /// Makes `venue` read as loaded without a load, as a directory a previous run filled does.
    pub(crate) fn mark_loaded(&self, venue: VenueId) {
        self.state
            .lock()
            .expect("the directory is readable")
            .loaded
            .insert(venue);
    }

    /// The loads started, in the order they were requested.
    pub(crate) fn loads(&self) -> Vec<VenueId> {
        self.state
            .lock()
            .expect("the directory is readable")
            .loads
            .clone()
    }

    /// The loads the menu asked the directory to cancel.
    pub(crate) fn cancelled(&self) -> Vec<VenueId> {
        self.state
            .lock()
            .expect("the directory is readable")
            .cancelled
            .clone()
    }

    /// How many loads are still waiting for their answer.
    pub(crate) fn pending(&self) -> usize {
        self.state
            .lock()
            .expect("the directory is readable")
            .pending
            .len()
    }

    /// Settles one load with `result`, as the connector's own future would settle it.
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

    /// Cancels one load from the directory side, answering it the way the connector does.
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

/// The headless host every menu test runs against.
pub(crate) struct Harness {
    app: TestApp,
    services: Rc<WidgetServices>,
    router: Rc<FrameRouter>,
    directory: Arc<FakeDirectory>,
    executor: Rc<TestExecutor>,
    menu: MarketParticipants,
}

impl Harness {
    /// A harness whose directory speaks every venue this build declares.
    pub(crate) fn new() -> Self {
        Self::with_venues(supported_venues())
    }

    /// A harness whose directory speaks exactly `venues`.
    pub(crate) fn with_venues(venues: Vec<VenueId>) -> Self {
        let mut app = TestApp::with_options(WindowOptions {
            size: Size::new(1280.0, 900.0),
            ..WindowOptions::default()
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

    /// The injected services, as the shared handle another menu is built from.
    pub(crate) fn services_rc(&self) -> Rc<WidgetServices> {
        Rc::clone(&self.services)
    }

    /// The directory the menu loads through.
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

    /// Runs one production-shaped round: the load futures the runtime would have driven, the
    /// frame hook's routes, then one frame.
    pub(crate) fn drain(&mut self) {
        self.executor.pump();
        let _ = self.router.tick(self.app.app_mut());
        self.menu
            .sync(self.app.app_mut())
            .expect("the menu's frame");
        self.app.drain().expect("the harness frame");
    }

    /// The executor a second menu of the same test is handed.
    pub(crate) fn executor(&self) -> Rc<TestExecutor> {
        Rc::clone(&self.executor)
    }

    /// Runs `frames` production-shaped rounds.
    pub(crate) fn settle(&mut self, frames: usize) {
        for _ in 0..frames {
            self.drain();
        }
    }

    /// The layout box of one node, if it is laid out.
    pub(crate) fn bounds(&self, node: NodeId) -> Option<ui_core::math::Rect> {
        self.app.probe().bounds(node)
    }

    /// Clicks one node's centre with the primary button, the way the platform pump delivers
    /// a press: down, a frame, up, then the frames that settle what it opened.
    pub(crate) fn click(&mut self, node: NodeId) {
        let position = centre_of(
            self.bounds(node)
                .unwrap_or_else(|| panic!("the harness draws the node it is asked to click")),
        );
        // The platform pump reports where the pointer is before the button goes down; a kit
        // control's press handling reads that position.
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

    /// Clicks one held button (one whose node the component built).
    pub(crate) fn click_button(&mut self, button: &base_ui::kit::button::Button) {
        let node = button.node().expect("the button is built");
        self.click(node);
    }

    /// Clicks one window position, the way the platform pump delivers a press outside every
    /// control.
    pub(crate) fn click_point(&mut self, position: Point) {
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

    /// Advances the clock past every fade this menu starts and drains the frames that finish
    /// them, so a panel or an alert that is closing really closes.
    pub(crate) fn settle_fade(&mut self) {
        for _ in 0..3 {
            self.app.advance(Duration::from_millis(200));
            self.drain();
        }
    }

    /// The first node under the window root whose debug id starts with `prefix`.
    pub(crate) fn find(&self, prefix: &str) -> Option<NodeId> {
        self.find_all(prefix).into_iter().next()
    }

    /// Every node under the window root whose debug id starts with `prefix`, in DFS order.
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

    /// The text the subtree under `node` draws, one line per text node, in DFS order.
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

    /// Whether the subtree under `node` draws `text` anywhere.
    pub(crate) fn has_text(&self, node: NodeId, text: &str) -> bool {
        self.text_of(node).lines().any(|line| line == text)
    }

    /// Whether the window's tree draws `text` anywhere.
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

    /// Removes one node from the harness window, the way the caller remounts its title bar.
    pub(crate) fn remove_node(&mut self, node: NodeId) {
        let window = self.window();
        self.app
            .app_mut()
            .remove_node(window, node)
            .expect("the harness removes a node it built");
    }

    /// Releases the harness's menu (the caller removes the trigger first).
    pub(crate) fn release_menu(&mut self) {
        self.menu.release(self.app.app_mut());
    }

    /// Runs one row's action for the harness's menu, without a press.
    pub(crate) fn start_load(&mut self, venue: VenueId) -> Result<(), crate::error::MenuError> {
        self.menu.start_load(self.app.app_mut(), venue)
    }

    /// The number of nodes in the harness window's tree.
    pub(crate) fn tree_len(&self) -> usize {
        self.app
            .app()
            .window(self.window())
            .expect("the harness window exists")
            .tree()
            .len()
    }

    /// The open overlays of the harness window (the panel is one; the alert another).
    pub(crate) fn overlay_count(&self) -> usize {
        self.app.app().overlays(self.window()).len()
    }

    /// The engine's frame-request counter (the I3 assertion face: a steady frame does not
    /// grow it).
    pub(crate) fn frame_requests(&self) -> u64 {
        self.app.app().frame_requests()
    }

    /// The row a venue is drawn as, by its stable debug id.
    pub(crate) fn row(&self, venue: VenueId) -> Option<NodeId> {
        self.find(&format!("TEST_MARKET_PARTICIPANTS_ROW_{venue:?}"))
    }

    /// The action one venue's row offers, by its stable debug id.
    pub(crate) fn action(&self, venue: VenueId, loaded: bool) -> Option<NodeId> {
        let state = if loaded { "REFRESH" } else { "LOAD" };
        self.find(&format!(
            "TEST_MARKET_PARTICIPANTS_ACTION_{state}_{venue:?}"
        ))
    }

    /// Whether one venue's row carries a mark of `state` (`LOADING` / `LOADED` / `UNLOADED`).
    pub(crate) fn shows_mark(&self, venue: VenueId, state: &str) -> bool {
        self.find(&format!("TEST_MARKET_PARTICIPANTS_{state}_{venue:?}"))
            .is_some()
    }

    /// Whether the row a venue is drawn as is drawn at all.
    pub(crate) fn shows_row(&self, venue: VenueId) -> bool {
        self.row(venue).is_some()
    }

    /// The panel's own rows are drawn inside a popup overlay; this says whether the panel is
    /// up by looking for a venue row.
    pub(crate) fn shows_panel(&self) -> bool {
        self.find("TEST_MARKET_PARTICIPANTS_ROW_").is_some()
    }

    /// Switches the language the way the composition root does: the translator first, then
    /// the signal every `bind_text` closure reads.
    pub(crate) fn switch_language(&mut self, language: LanguageCode) {
        self.services.translate.set_language(language);
        let signal = self.services.language;
        let _ = self.app.app_mut().write(&signal, language);
    }
}

/// The centre of a laid-out box, in window coordinates.
fn centre_of(rect: ui_core::math::Rect) -> Point {
    Point {
        x: rect.origin.x + rect.size.width / 2.0,
        y: rect.origin.y + rect.size.height / 2.0,
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // A menu released after its window is gone holds handles into a tree that no longer
        // exists; the order the application uses is "release, then close".
        self.menu.release(self.app.app_mut());
    }
}

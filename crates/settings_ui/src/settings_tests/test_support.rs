//! The test host the settings page is driven against.
//!
//! A settings page is not a widget, so the contract layer's `WidgetTestApp` (which is built around
//! `WidgetView`) does not fit it. This harness composes the same aggregate the application root
//! composes — the frozen [`WidgetServices`], the layer stack, the frame router and the animation
//! router — over `ui_core::test_support::TestApp`, and adds the things a settings test needs: a
//! settings session it can make fail on purpose, a settled frame, and the debug-id lookups and
//! text reads the assertions are written with.
//!
//! It deliberately does **not** live in `base_ui::kit::test_support`: the kit's test support is a
//! single-writer surface, and a settings harness is this crate's own business.
//!
//! # Frames
//!
//! [`Harness::drain`] is one production-shaped round: the application reads the events the platform
//! queued (and hands them to the page's controls), the page syncs, and then the engine's frame runs
//! (styles → text → layout → paint). That is the order the platform pump uses, so a test asserts on
//! what a frame of the real application would have drawn.

use std::{cell::RefCell, rc::Rc};

use app_config::{
    market_participants::{
        credentials::{
            BinanceMarketCredentials, HyperliquidCredentials, LongbridgeCredentials, OkxCredentials,
        },
        settings::VenueSettings,
    },
    network::{ProxyAuthorizationConfig, ProxyConfig},
    settings::{AccountConfig, AccountWalletKind, Config},
};
use base_ui::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::{appearance::Appearance as ThemeAppearance, handle::ThemeHandle},
};
use i18n::{language::codes::LanguageCode, translate::Translate};
use smol_str::SmolStr;
use ui_core::{
    app::App,
    event::{Event, InputEvent, Modifiers, PointerButton},
    math::{Point, Rect, Size},
    node::{id::NodeId, tree::NodeKind},
    test_support::app::TestApp,
    window::{WindowId, WindowOptions},
};
use widget::{frame::FrameRouter, services::WidgetServices};

use crate::{
    session::{SettingsStore, SettingsWriteError},
    settings::SettingsPage,
};

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

/// The language code one setting names.
pub(crate) fn language_code(language: app_config::settings::Language) -> LanguageCode {
    match language {
        app_config::settings::Language::ZhCn => LanguageCode::ZhCn,
        app_config::settings::Language::En => LanguageCode::En,
    }
}

/// The headless host every settings test runs against.
pub(crate) struct Harness {
    app: TestApp,
    services: Rc<WidgetServices>,
    theme: ThemeHandle,
    router: Rc<FrameRouter>,
    store: SettingsStore,
    host: crate::host::SettingsHost,
    page: SettingsPage,
    /// A second settings page mounted in the same window, for the two-pane tests.
    second: Option<SettingsPage>,
    window: WindowId,
    /// The configuration snapshots the store published, in write order.
    writes: Rc<RefCell<Vec<Config>>>,
    /// The failure the next write reports instead of committing.
    fail_next: Rc<RefCell<Option<SettingsWriteError>>>,
}

impl Harness {
    /// A host over the default settings, with the interface drawn in English.
    pub(crate) fn new() -> Self {
        let mut config = Config::default();
        config.language = app_config::settings::Language::En;
        Self::with_config(config)
    }

    /// A host over `config`.
    pub(crate) fn with_config(config: Config) -> Self {
        Self::with_config_in(config, app_config::settings::Language::En)
    }

    /// A host over `config`, with the interface and the settings in `language`.
    ///
    /// The settings are put in `language` as well as the interface: a start with an interface that
    /// agrees with the settings is what a test writing another language is then writing a change
    /// against.
    pub(crate) fn with_config_in(config: Config, language: app_config::settings::Language) -> Self {
        let mut config = config;
        config.language = language;
        Self::build(config, translate_in(language_code(language)))
    }

    /// A host over `config` with a test-chosen translation context.
    pub(crate) fn build(config: Config, translate: Translate) -> Self {
        // The settings page is a 1000-wide column inside the window; a window that fits it keeps
        // every sidebar row and page row inside the content area.
        let mut app = TestApp::with_options(WindowOptions {
            size: Size::new(1280.0, 900.0),
            ..WindowOptions::default()
        });
        let window = app.window_id();
        let runtime = app.app().runtime_handle().clone();
        let theme = ThemeHandle::new(app.app_mut(), ThemeAppearance::Light);
        let layer = Rc::new(Layer::new());
        let router =
            Rc::new(FrameRouter::new(app.app_mut()).expect("the test host owns the frame hook"));
        let animation = Rc::new(AnimationRouter::new());
        let code = language_code(config.language);
        let services = Rc::new(
            WidgetServices::builder(
                app.app_mut(),
                runtime,
                translate.clone(),
                theme.clone(),
                Rc::clone(&layer),
                Rc::clone(&router),
                Rc::clone(&animation),
            )
            .language(code)
            .config(config)
            .build(),
        );

        let writes = Rc::new(RefCell::new(Vec::new()));
        let fail_next = Rc::new(RefCell::new(None));
        let store = {
            let services = Rc::clone(&services);
            let writes = Rc::clone(&writes);
            let fail_next = Rc::clone(&fail_next);
            // The commit seam the composition root implements: persist, publish, and let the
            // language reach the translator the interface is drawn with.
            SettingsStore::new(app.app_mut(), services.config, move |target, candidate| {
                if let Some(failure) = fail_next.borrow_mut().take() {
                    return Err(failure);
                }
                let code = language_code(candidate.language);
                target.write(&services.config, candidate.clone());
                target.write(&services.language, code);
                translate.set_language(code);
                writes.borrow_mut().push(candidate);
                Ok(())
            })
        };

        let host = crate::host::SettingsHost::new(Rc::clone(&services), store.clone());
        let host_for_page = host.clone();
        let root = app
            .app()
            .window(window)
            .expect("the harness window exists")
            .tree()
            .root();
        let page = SettingsPage::mount(host_for_page, "settings.test", app.app_mut(), window, root)
            .expect("the settings page mounts");

        Self {
            app,
            services,
            theme,
            router,
            store,
            host,
            page,
            second: None,
            window,
            writes,
            fail_next,
        }
    }

    pub(crate) fn app(&self) -> &App {
        self.app.app()
    }

    pub(crate) fn app_mut(&mut self) -> &mut App {
        self.app.app_mut()
    }

    pub(crate) fn window(&self) -> WindowId {
        self.window
    }

    pub(crate) fn page(&self) -> &SettingsPage {
        &self.page
    }

    pub(crate) fn page_mut(&mut self) -> &mut SettingsPage {
        &mut self.page
    }

    pub(crate) fn services(&self) -> &WidgetServices {
        &self.services
    }

    pub(crate) fn store(&self) -> &SettingsStore {
        &self.store
    }

    pub(crate) fn theme(&self) -> &ThemeHandle {
        &self.theme
    }

    /// Mounts a second settings page in the same window, under its own instance key.
    pub(crate) fn mount_second(&mut self, key: &str) -> &mut SettingsPage {
        let host = self.host.clone();
        let root = self.root();
        let second = SettingsPage::mount(host, key, self.app.app_mut(), self.window, root)
            .expect("the second settings page mounts");
        self.second = Some(second);
        self.second
            .as_mut()
            .expect("the second page was just mounted")
    }

    /// Takes the second settings page out of the harness, for a test that releases it itself.
    pub(crate) fn take_second(&mut self) -> Option<SettingsPage> {
        self.second.take()
    }

    /// The host the page was built with, for a test that mounts a page of its own.
    pub(crate) fn host(&self) -> &crate::host::SettingsHost {
        &self.host
    }

    /// The settings as they are now.
    pub(crate) fn config(&self) -> Config {
        self.store.read(self.app(), Config::clone)
    }

    /// Whether a write failed and was not reported yet.
    pub(crate) fn has_write_failure(&self) -> bool {
        self.store.has_failure()
    }

    /// The configuration snapshots the store published, in write order.
    pub(crate) fn writes(&self) -> Vec<Config> {
        self.writes.borrow().clone()
    }

    /// Makes the next write report `failure` instead of committing.
    pub(crate) fn fail_next_write(&self, failure: SettingsWriteError) {
        *self.fail_next.borrow_mut() = Some(failure);
    }

    /// Writes one change through the settings session.
    ///
    /// # Errors
    /// The session refused to store the change (the settings are then as they were).
    pub(crate) fn write(
        &mut self,
        change: crate::session::SettingsChange,
    ) -> Result<(), SettingsWriteError> {
        let window = self.window;
        let store = self.store.clone();
        store.write(self.app.app_mut(), window, change)
    }

    /// Writes a group of changes through the settings session.
    ///
    /// # Errors
    /// The session refused to store the change (the settings are then as they were).
    pub(crate) fn write_all(
        &mut self,
        changes: impl IntoIterator<Item = crate::session::SettingsChange>,
    ) -> Result<(), SettingsWriteError> {
        let window = self.window;
        let store = self.store.clone();
        store.write_all(self.app.app_mut(), window, changes)
    }

    /// Writes a configuration as another subsystem of the application would (the settings signal
    /// moves; the translator follows the way the root keeps it in step).
    pub(crate) fn update_config(&mut self, update: impl FnOnce(&mut Config)) {
        let mut config = self.config();
        update(&mut config);
        let code = language_code(config.language);
        self.services.translate.set_language(code);
        let signal = self.services.config;
        let language = self.services.language;
        let app = self.app.app_mut();
        app.write(&signal, config);
        app.write(&language, code);
    }

    /// The layout box of one node, if it is laid out.
    pub(crate) fn bounds(&self, node: NodeId) -> Option<Rect> {
        self.app.probe().bounds(node)
    }

    /// The text one text node draws.
    pub(crate) fn node_text(&self, node: NodeId) -> SmolStr {
        let tree = self
            .app
            .app()
            .window(self.window)
            .expect("the harness window exists")
            .tree();
        match tree.get(node).expect("the node exists").kind() {
            NodeKind::Text(content) => content.text.clone(),
            other => panic!("expected a text node, got {other:?}"),
        }
    }

    /// The text the subtree under `node` draws, one line per text node, in DFS order.
    pub(crate) fn text_of(&self, node: NodeId) -> String {
        let tree = self
            .app
            .app()
            .window(self.window)
            .expect("the harness window exists")
            .tree();
        let mut lines = Vec::new();
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

    /// The window root.
    pub(crate) fn root(&self) -> NodeId {
        self.app
            .app()
            .window(self.window)
            .expect("the harness window exists")
            .tree()
            .root()
    }

    /// The nearest node under the window root whose debug id starts with `prefix`.
    pub(crate) fn find(&self, prefix: &str) -> Option<NodeId> {
        self.find_all(prefix).into_iter().next()
    }

    /// Every node under the window root whose debug id starts with `prefix`, in DFS order.
    pub(crate) fn find_all(&self, prefix: &str) -> Vec<NodeId> {
        let Some(window) = self.app.app().window(self.window) else {
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

    /// Every debug id in the tree, in DFS order.
    pub(crate) fn debug_ids(&self) -> Vec<SmolStr> {
        let mut ids = Vec::new();
        let Some(window) = self.app.app().window(self.window) else {
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

    /// Runs one production-shaped round: events → page dispatch → page sync → engine frame.
    ///
    /// # Panics
    /// The page's sync failed (a node the page built is gone, which is a test's bug).
    pub(crate) fn drain(&mut self) {
        if self.app.app().window(self.window).is_none() {
            return;
        }
        let events = self
            .app
            .app_mut()
            .drain_events()
            .into_iter()
            .filter_map(|(window, event)| (window == self.window).then_some(event))
            .collect::<Vec<_>>();
        let app = self.app.app_mut();
        for event in &events {
            self.page.dispatch(app, event);
            if let Some(second) = self.second.as_mut() {
                second.dispatch(app, event);
            }
        }
        self.page.sync(app).expect("the settings page syncs");
        if let Some(second) = self.second.as_mut() {
            second.sync(app).expect("the second settings page syncs");
        }
        let _ = self.router.tick(app);
        self.app.drain().expect("the harness frame");
    }

    /// Runs `frames` production-shaped frames.
    pub(crate) fn settle(&mut self, frames: usize) {
        for _ in 0..frames {
            self.drain();
        }
    }

    /// Delivers one platform event the way the platform pump does, then runs a round.
    ///
    /// The event is not handed to the page here: drain reads the observation queue and hands each
    /// event to the page once, which is the order the platform pump uses. Delivering it twice would
    /// insert typed text twice.
    pub(crate) fn dispatch(&mut self, event: Event) {
        self.app.dispatch_event(event);
        self.drain();
    }

    /// Injects one pointer click at a window position, then runs a round.
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
        self.drain();
    }

    /// Clicks the centre of the node whose debug id starts with `prefix`.
    ///
    /// # Panics
    /// No node matches, or it has not been laid out (the test has not settled a frame yet).
    pub(crate) fn click_node(&mut self, prefix: &str) {
        let node = self
            .find(prefix)
            .unwrap_or_else(|| panic!("no node whose id starts with {prefix}"));
        let bounds = self
            .bounds(node)
            .unwrap_or_else(|| panic!("{prefix} has not been laid out"));
        self.click(Point::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        ));
    }

    /// Delivers one key press (and its release) to the page.
    pub(crate) fn press(&mut self, code: ui_core::event::KeyCode) {
        self.dispatch(Event::Input(InputEvent::key_down(code, Modifiers::empty())));
        self.dispatch(Event::Input(InputEvent::key_up(code, Modifiers::empty())));
    }

    /// The rows of the current page, by the debug id every one of them carries.
    pub(crate) fn field(&self, id: &str) -> Option<NodeId> {
        self.find(&format!("SETTINGS_FIELD_{id}"))
    }

    /// Types query into the search field, then runs a round.
    pub(crate) fn set_query(&mut self, query: &str) {
        let Self { app, page, .. } = self;
        page.set_query(app.app_mut(), query);
        self.drain();
    }

    /// Clicks the sidebar row of one page, as a user selecting it would.
    pub(crate) fn select_page(&mut self, page: &str) {
        self.click_node(&format!("SETTINGS_ENTRY_{page}"));
        self.settle(1);
    }

    /// The labels the sidebar draws, in the order it draws them.
    pub(crate) fn labels(&self) -> Vec<SmolStr> {
        self.page().sidebar_labels()
    }

    /// The label of one setting row, as it is drawn.
    pub(crate) fn field_label(&self, id: &str) -> SmolStr {
        let node = self
            .find(&format!("SETTINGS_LABEL_{id}"))
            .unwrap_or_else(|| panic!("no label for the setting {id}"));
        self.node_text(node)
    }

    /// Types `text` into the focused control, the way the platform delivers a character event.
    ///
    /// The event takes the platform's dispatch path only ([`TestApp::dispatch_event`]): that path
    /// runs the engine's dispatch **and** queues the event for the round to read, and
    /// [`Harness::dispatch`] hands the same event to the page as well — a text event would then be
    /// inserted twice.
    pub(crate) fn type_text(&mut self, text: &str) {
        self.app.dispatch_event(Event::Input(InputEvent::Text {
            text: SmolStr::new(text),
            timestamp: std::time::Instant::now(),
        }));
        self.drain();
    }

    /// Advances the engine clock and runs one round, the way the platform pump measures a frame.
    ///
    /// A fade-in / fade-out finishes on the engine's clock, not on the number of frames: a round
    /// that does not advance the clock leaves the animation where it was, and the layer it draws
    /// keeps its modal scope over everything under it.
    pub(crate) fn advance(&mut self, milliseconds: u64) {
        self.app
            .advance(std::time::Duration::from_millis(milliseconds));
        self.drain();
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Release the page while the window is still alive: a kit component left registered would
        // leak its action handler for the rest of the process.
        if self.app.app().window(self.window).is_some() {
            if let Some(second) = self.second.as_mut() {
                second.release(self.app.app_mut());
            }
            self.page.release(self.app.app_mut());
            self.app.drain().ok();
        }
    }
}

/// A download task a test can publish without a transfer behind it.
pub(crate) fn download_task(
    id: download_provider::task::DownloadId,
    destination: std::path::PathBuf,
    status: download_provider::task::DownloadStatus,
) -> download_provider::task::DownloadTask {
    download_provider::task::DownloadTask {
        id,
        url: url_for(destination.as_path()),
        destination,
        status,
    }
}

/// A URL that names one model file, for a task a test publishes itself.
fn url_for(path: &std::path::Path) -> url::Url {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("model.bin");
    url::Url::parse(&format!("https://example.invalid/{name}")).expect("the fixture URL parses")
}

/// Settings with something written everywhere the settings can hold something, so that a commit
/// overwriting them wholesale rather than replaying changes would be visible.
pub(crate) fn configured_settings() -> Config {
    let mut config = Config::default();
    config.account = Some(AccountConfig {
        user_id: "11111111-1111-4111-8111-111111111111".into(),
        wallet_address: "0x0123456789abcdef".into(),
        wallet_kind: AccountWalletKind::Ethereum,
    });
    config.avatar = Some("<svg>configured-avatar</svg>".to_owned());
    config.proxies.insert(
        SmolStr::new_static("hk"),
        ProxyConfig {
            addr: SmolStr::new_static("127.0.0.1"),
            port: 1080,
            authorization: ProxyAuthorizationConfig::UsernameAndPassword {
                username: SmolStr::new_static("proxy-user"),
                password: SmolStr::new_static("proxy-password"),
            },
            resolve_dns_at_remote: true,
        },
    );

    {
        let VenueSettings::Binance(binance) = config
            .venues
            .get_mut(&market_participants_model::identifier::VenueId::Binance)
            .expect("the default settings name every venue this build speaks")
        else {
            panic!("binance is configured as Binance");
        };
        binance.spot_credentials = Some(BinanceMarketCredentials {
            api_key: SmolStr::new_static("binance-spot-key"),
            private_key: SmolStr::new_static("binance-spot-private"),
            passphrase: SmolStr::new_static("binance-spot-passphrase"),
        });
        binance.usd_m_perp_credentials = Some(BinanceMarketCredentials {
            api_key: SmolStr::new_static("binance-perp-key"),
            private_key: SmolStr::new_static("binance-perp-private"),
            passphrase: SmolStr::new_static("binance-perp-passphrase"),
        });
    }

    {
        let VenueSettings::Okx(okx) = config
            .venues
            .get_mut(&market_participants_model::identifier::VenueId::Okx)
            .expect("the default settings name every venue this build speaks")
        else {
            panic!("okx is configured as OKX");
        };
        okx.credentials = Some(OkxCredentials {
            api_key: SmolStr::new_static("okx-api-key"),
            secret_key: SmolStr::new_static("okx-secret-key"),
            passphrase: SmolStr::new_static("okx-passphrase"),
        });
    }

    {
        let VenueSettings::Hyperliquid(settings) = config
            .venues
            .get_mut(&market_participants_model::identifier::VenueId::Hyperliquid)
            .expect("the default settings name every venue this build speaks")
        else {
            panic!("hyperliquid is configured as Hypercore");
        };
        settings.credentials = Some(HyperliquidCredentials {
            main_account_address: SmolStr::new_static("0xhyperliquid-main"),
            api_agent_private_key: SmolStr::new_static("hyperliquid-agent-private"),
        });
    }

    {
        let VenueSettings::Longbridge(settings) = config
            .venues
            .get_mut(&market_participants_model::identifier::VenueId::Longbridge)
            .expect("the default settings name every venue this build speaks")
        else {
            panic!("longbridge is configured as Longbridge");
        };
        settings.credentials = Some(LongbridgeCredentials {
            app_key: SmolStr::new_static("longbridge-app-key"),
            app_secret: SmolStr::new_static("longbridge-app-secret"),
            access_token: SmolStr::new_static("longbridge-access-token"),
        });
    }

    config
}

/// The engine unload the translation download manager is handed.
pub(crate) type Unload = std::sync::Arc<
    dyn Fn(&std::path::Path) -> Result<(), crate::translation_downloads::UnloadFailure>
        + Send
        + Sync,
>;

/// One command the translation downloads asked its download manager to run.
#[derive(Clone, Debug)]
pub(crate) enum DownloadCommand {
    /// A new transfer, under the identity the downloads pre-allocated.
    Add(
        download_provider::request::DownloadRequest,
        download_provider::task::DownloadId,
    ),
    /// Stop a transfer at its partial file.
    Pause(download_provider::task::DownloadId),
    /// Continue a transfer from its partial file.
    Resume(download_provider::task::DownloadId),
    /// Remove a task and its files.
    Delete(download_provider::task::DownloadId),
}

/// A download manager a test drives by hand.
///
/// It records the commands [`crate::translation_downloads::TranslationDownloads`] issues and
/// publishes the task snapshots the test asks it to: a test is the download manager, so a row can
/// be made to progress, fail or be removed without a network, a transfer or a runtime task.
pub(crate) struct FakeDownloads {
    tasks: ui_core::node::signal::Signal<Vec<download_provider::task::DownloadTask>>,
    commands: Rc<RefCell<Vec<DownloadCommand>>>,
}

impl FakeDownloads {
    /// A fake manager whose task signal starts empty.
    pub(crate) fn new(app: &mut App) -> Self {
        Self {
            tasks: app.signal(Vec::new()),
            commands: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// The seam the translation downloads are built over.
    pub(crate) fn service(&self) -> crate::translation_downloads::DownloadService {
        crate::translation_downloads::DownloadService {
            tasks: self.tasks,
            add: Rc::new({
                let commands = Rc::clone(&self.commands);
                move |request, id| {
                    commands
                        .borrow_mut()
                        .push(DownloadCommand::Add(request, id));
                }
            }),
            pause: Rc::new({
                let commands = Rc::clone(&self.commands);
                move |id| commands.borrow_mut().push(DownloadCommand::Pause(id))
            }),
            resume: Rc::new({
                let commands = Rc::clone(&self.commands);
                move |id| commands.borrow_mut().push(DownloadCommand::Resume(id))
            }),
            delete: Rc::new({
                let commands = Rc::clone(&self.commands);
                move |id| commands.borrow_mut().push(DownloadCommand::Delete(id))
            }),
        }
    }

    /// The commands the manager issued, in order.
    fn commands(&self) -> Vec<DownloadCommand> {
        self.commands.borrow().clone()
    }

    /// The identity of the last `add` it was asked for.
    pub(crate) fn added(&self) -> Option<download_provider::task::DownloadId> {
        self.commands
            .borrow()
            .iter()
            .rev()
            .find_map(|command| match command {
                DownloadCommand::Add(_, id) => Some(*id),
                _ => None,
            })
    }

    /// Adds one task snapshot to what the fake manager publishes.
    pub(crate) fn push(&self, app: &mut App, task: download_provider::task::DownloadTask) {
        let mut tasks = app.read_with(&self.tasks, Clone::clone);
        tasks.push(task);
        app.write(&self.tasks, tasks);
    }

    /// Removes one published task, the way a finished delete does.
    pub(crate) fn remove(&self, app: &mut App, id: download_provider::task::DownloadId) {
        let mut tasks = app.read_with(&self.tasks, Clone::clone);
        tasks.retain(|task| task.id != id);
        app.write(&self.tasks, tasks);
    }

    /// The transfers the manager asked its download manager to add, in order.
    pub(crate) fn adds(
        &self,
    ) -> Vec<(
        download_provider::request::DownloadRequest,
        download_provider::task::DownloadId,
    )> {
        self.commands()
            .into_iter()
            .filter_map(|command| match command {
                DownloadCommand::Add(request, id) => Some((request, id)),
                _ => None,
            })
            .collect()
    }

    /// Whether the manager was asked to pause `id`.
    pub(crate) fn paused(&self, id: download_provider::task::DownloadId) -> bool {
        self.commands()
            .iter()
            .any(|command| matches!(command, DownloadCommand::Pause(paused) if *paused == id))
    }

    /// Whether the manager was asked to resume `id`.
    pub(crate) fn resumed(&self, id: download_provider::task::DownloadId) -> bool {
        self.commands()
            .iter()
            .any(|command| matches!(command, DownloadCommand::Resume(resumed) if *resumed == id))
    }

    /// Whether the manager was asked to delete `id`.
    pub(crate) fn deleted(&self, id: download_provider::task::DownloadId) -> bool {
        self.commands()
            .iter()
            .any(|command| matches!(command, DownloadCommand::Delete(deleted) if *deleted == id))
    }

    /// How many deletes the manager asked its download manager for.
    pub(crate) fn delete_count(&self) -> usize {
        self.commands()
            .iter()
            .filter(|command| matches!(command, DownloadCommand::Delete(..)))
            .count()
    }
}

/// Mounts the settings page again with the process-wide translation downloads attached.
///
/// The host is built before the page mounts, so a test that wants downloads injects them here: the
/// page is released and mounted again over the same services and settings session.
pub(crate) fn attach_downloads(
    harness: &mut Harness,
    downloads: Rc<crate::translation_downloads::TranslationDownloads>,
) {
    if harness.app.app().window(harness.window).is_none() {
        return;
    }
    if let Some(second) = harness.second.as_mut() {
        second.release(harness.app.app_mut());
    }
    harness.second = None;
    harness.page.release(harness.app.app_mut());
    harness.host = harness.host.clone().with_downloads(downloads);
    let host = harness.host.clone();
    let root = harness.root();
    harness.page = crate::settings::SettingsPage::mount(
        host,
        "settings.test",
        harness.app.app_mut(),
        harness.window,
        root,
    )
    .expect("the settings page mounts with the downloads");
}

/// Builds the translation downloads over the harness's settings session and a fake manager, and
/// attaches them to the settings page.
pub(crate) fn fake_downloads(
    harness: &mut Harness,
    unload: Unload,
) -> (
    Rc<crate::translation_downloads::TranslationDownloads>,
    FakeDownloads,
) {
    let fake = FakeDownloads::new(harness.app_mut());
    let store = harness.store().clone();
    let runtime = harness.app().runtime_handle().clone();
    let manager = crate::translation_downloads::TranslationDownloads::new(
        harness.app_mut(),
        crate::translation_downloads::TranslationDownloadsDeps {
            store,
            downloads: fake.service(),
            runtime,
            unload,
        },
    );
    attach_downloads(harness, Rc::clone(&manager));

    (manager, fake)
}
/// Releases the settings page the harness drives, as closing it does.
pub(crate) fn release_page(harness: &mut Harness) {
    harness.page.release(harness.app.app_mut());
}
/// Delivers one platform event through the pump's path: the engine dispatches it, then the round
/// that follows hands the drained event to the page exactly once.
///
/// [`Harness::dispatch`] hands the event to the page *and* lets the engine dispatch it, so a text
/// or key event reaches a control twice; a test that types into a field uses this instead.
pub(crate) fn send_event(harness: &mut Harness, event: Event) {
    harness.app.dispatch_event(event);
    harness.drain();
}

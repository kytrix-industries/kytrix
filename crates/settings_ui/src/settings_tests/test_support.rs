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
use i18n::{language::codes::LanguageCode, translate::Translate};
use smol_str::SmolStr;
use ui_core::{
    app::App,
    event::{Event, InputEvent, Modifiers, PointerButton},
    math::{LogicalPoint, LogicalRect, LogicalSize, px},
    node::{id::NodeId, tree::NodeKind},
    test_support::app::TestApp,
    window::{WindowId, WindowOptions},
};
use ui_kit::theme::{appearance::Appearance as ThemeAppearance, handle::ThemeHandle};
use widget::{frame::FrameRouter, services::WidgetServices};

use crate::{
    session::{SettingsStore, SettingsWriteError},
    settings::SettingsPage,
};

pub(crate) fn translate_in(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::embedded::load().expect("the embedded locales load")),
        language,
    )
}

pub(crate) fn language_code(language: app_config::settings::Language) -> LanguageCode {
    match language {
        app_config::settings::Language::ZhCn => LanguageCode::ZhCn,
        app_config::settings::Language::En => LanguageCode::En,
    }
}

pub(crate) struct Harness {
    app: TestApp,
    services: Rc<WidgetServices>,
    theme: ThemeHandle,
    router: Rc<FrameRouter>,
    store: SettingsStore,
    host: crate::host::SettingsHost,
    page: SettingsPage,
    second: Option<SettingsPage>,
    window: WindowId,
    writes: Rc<RefCell<Vec<Config>>>,
    fail_next: Rc<RefCell<Option<SettingsWriteError>>>,
}

impl Harness {
    pub(crate) fn new() -> Self {
        let mut config = Config::default();
        config.language = app_config::settings::Language::En;
        Self::with_config(config)
    }

    pub(crate) fn with_config(config: Config) -> Self {
        Self::with_config_in(config, app_config::settings::Language::En)
    }

    pub(crate) fn with_config_in(config: Config, language: app_config::settings::Language) -> Self {
        let mut config = config;
        config.language = language;
        Self::build(config, translate_in(language_code(language)))
    }

    pub(crate) fn build(config: Config, translate: Translate) -> Self {
        let mut app = TestApp::with_options(WindowOptions {
            size: LogicalSize::new(px(1280.0), px(900.0)),
            ..WindowOptions::default()
        });
        let window = app.window_id();
        let runtime = app.app().runtime_handle().clone();
        let theme = ThemeHandle::new(app.app_mut(), ThemeAppearance::Light);
        let router =
            Rc::new(FrameRouter::new(app.app_mut()).expect("the test host owns the frame hook"));
        let code = language_code(config.language);
        let services = Rc::new(
            WidgetServices::builder(
                app.app_mut(),
                runtime,
                translate.clone(),
                theme.clone(),
                Rc::clone(&router),
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

    pub(crate) fn take_second(&mut self) -> Option<SettingsPage> {
        self.second.take()
    }

    pub(crate) fn host(&self) -> &crate::host::SettingsHost {
        &self.host
    }

    pub(crate) fn config(&self) -> Config {
        self.store.read(self.app(), Config::clone)
    }

    pub(crate) fn has_write_failure(&self) -> bool {
        self.store.has_failure()
    }

    pub(crate) fn writes(&self) -> Vec<Config> {
        self.writes.borrow().clone()
    }

    pub(crate) fn fail_next_write(&self, failure: SettingsWriteError) {
        *self.fail_next.borrow_mut() = Some(failure);
    }

    pub(crate) fn write(
        &mut self,
        change: crate::session::SettingsChange,
    ) -> Result<(), SettingsWriteError> {
        let window = self.window;
        let store = self.store.clone();
        store.write(self.app.app_mut(), window, change)
    }

    pub(crate) fn write_all(
        &mut self,
        changes: impl IntoIterator<Item = crate::session::SettingsChange>,
    ) -> Result<(), SettingsWriteError> {
        let window = self.window;
        let store = self.store.clone();
        store.write_all(self.app.app_mut(), window, changes)
    }

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

    pub(crate) fn bounds(&self, node: NodeId) -> Option<LogicalRect> {
        self.app.probe().bounds(node)
    }

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

    pub(crate) fn has_text(&self, node: NodeId, text: &str) -> bool {
        self.text_of(node).lines().any(|line| line == text)
    }

    pub(crate) fn root(&self) -> NodeId {
        self.app
            .app()
            .window(self.window)
            .expect("the harness window exists")
            .tree()
            .root()
    }

    pub(crate) fn find(&self, prefix: &str) -> Option<NodeId> {
        self.find_all(prefix).into_iter().next()
    }

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

    pub(crate) fn settle(&mut self, frames: usize) {
        for _ in 0..frames {
            self.drain();
        }
    }

    pub(crate) fn dispatch(&mut self, event: Event) {
        self.app.dispatch_event(event);
        self.drain();
    }

    pub(crate) fn dispatch_without_frame(&mut self, event: Event) {
        self.app.dispatch_event(event);
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
        self.drain();
    }

    pub(crate) fn click_node(&mut self, prefix: &str) {
        let node = self
            .find(prefix)
            .unwrap_or_else(|| panic!("no node whose id starts with {prefix}"));
        let bounds = self
            .bounds(node)
            .unwrap_or_else(|| panic!("{prefix} has not been laid out"));
        self.click(LogicalPoint::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        ));
    }

    pub(crate) fn press(&mut self, code: ui_core::event::KeyCode) {
        self.dispatch(Event::Input(InputEvent::key_down(code, Modifiers::empty())));
        self.dispatch(Event::Input(InputEvent::key_up(code, Modifiers::empty())));
    }

    pub(crate) fn field(&self, id: &str) -> Option<NodeId> {
        self.find(&format!("SETTINGS_FIELD_{id}"))
    }

    pub(crate) fn set_query(&mut self, query: &str) {
        let Self { app, page, .. } = self;
        page.set_query(app.app_mut(), query);
        self.drain();
    }

    pub(crate) fn select_page(&mut self, page: &str) {
        self.click_node(&format!("SETTINGS_ENTRY_{page}"));
        self.settle(1);
    }

    pub(crate) fn labels(&self) -> Vec<SmolStr> {
        self.page().sidebar_labels()
    }

    pub(crate) fn field_label(&self, id: &str) -> SmolStr {
        let node = self
            .find(&format!("SETTINGS_LABEL_{id}"))
            .unwrap_or_else(|| panic!("no label for the setting {id}"));
        self.node_text(node)
    }

    pub(crate) fn type_text(&mut self, text: &str) {
        self.app.dispatch_event(Event::Input(InputEvent::Text {
            text: SmolStr::new(text),
            timestamp: std::time::Instant::now(),
        }));
        self.drain();
    }

    pub(crate) fn advance(&mut self, milliseconds: u64) {
        self.app
            .advance(std::time::Duration::from_millis(milliseconds));
        self.drain();
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if self.app.app().window(self.window).is_some() {
            if let Some(second) = self.second.as_mut() {
                second.release(self.app.app_mut());
            }
            self.page.release(self.app.app_mut());
            self.app.drain().ok();
        }
    }
}

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

fn url_for(path: &std::path::Path) -> url::Url {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("model.bin");
    url::Url::parse(&format!("https://example.invalid/{name}")).expect("the fixture URL parses")
}

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

pub(crate) type Unload = std::sync::Arc<
    dyn Fn(&std::path::Path) -> Result<(), crate::translation_downloads::UnloadFailure>
        + Send
        + Sync,
>;

#[derive(Clone, Debug)]
pub(crate) enum DownloadCommand {
    Add(
        download_provider::request::DownloadRequest,
        download_provider::task::DownloadId,
    ),
    Pause(download_provider::task::DownloadId),
    Resume(download_provider::task::DownloadId),
    Delete(download_provider::task::DownloadId),
}

pub(crate) struct FakeDownloads {
    tasks: ui_core::node::signal::Signal<Vec<download_provider::task::DownloadTask>>,
    commands: Rc<RefCell<Vec<DownloadCommand>>>,
}

impl FakeDownloads {
    pub(crate) fn new(app: &mut App) -> Self {
        Self {
            tasks: app.signal(Vec::new()),
            commands: Rc::new(RefCell::new(Vec::new())),
        }
    }

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

    fn commands(&self) -> Vec<DownloadCommand> {
        self.commands.borrow().clone()
    }

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

    pub(crate) fn push(&self, app: &mut App, task: download_provider::task::DownloadTask) {
        let mut tasks = app.read_with(&self.tasks, Clone::clone);
        tasks.push(task);
        app.write(&self.tasks, tasks);
    }

    pub(crate) fn remove(&self, app: &mut App, id: download_provider::task::DownloadId) {
        let mut tasks = app.read_with(&self.tasks, Clone::clone);
        tasks.retain(|task| task.id != id);
        app.write(&self.tasks, tasks);
    }

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

    pub(crate) fn paused(&self, id: download_provider::task::DownloadId) -> bool {
        self.commands()
            .iter()
            .any(|command| matches!(command, DownloadCommand::Pause(paused) if *paused == id))
    }

    pub(crate) fn resumed(&self, id: download_provider::task::DownloadId) -> bool {
        self.commands()
            .iter()
            .any(|command| matches!(command, DownloadCommand::Resume(resumed) if *resumed == id))
    }

    pub(crate) fn deleted(&self, id: download_provider::task::DownloadId) -> bool {
        self.commands()
            .iter()
            .any(|command| matches!(command, DownloadCommand::Delete(deleted) if *deleted == id))
    }

    pub(crate) fn delete_count(&self) -> usize {
        self.commands()
            .iter()
            .filter(|command| matches!(command, DownloadCommand::Delete(..)))
            .count()
    }
}

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
pub(crate) fn release_page(harness: &mut Harness) {
    harness.page.release(harness.app.app_mut());
}
pub(crate) fn send_event(harness: &mut Harness, event: Event) {
    harness.app.dispatch_event(event);
    harness.drain();
}

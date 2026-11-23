use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};

use app_config::{
    network::Config as NetworkConfig,
    settings::{Appearance as StoredAppearance, Config},
};
use app_config_provider::{
    notification::ConfigNotification,
    store::{ConfigPersistenceError, ConfigRecovery, ConfigStore},
};
use i18n::{embedded, translate::Translate};
use kytrix_service_provider::service::connector::KytrixServiceConnector;
use market_participants_connector::{
    connector::{MarketParticipantsConnector, MarketParticipantsConnectorInput},
    runtime::MarketParticipantNetworkSnapshot,
};
use network::transport::hub::{TransportHub, TransportPolicy};
use profile_ui::host::ProfileHost;
use settings_ui::{
    session::{SettingsStore, SettingsWriteError},
    translation_downloads::{TranslationDownloads, TranslationDownloadsDeps},
};
use tokio::runtime::Handle;
use ui_core::{app::App, node::signal::Signal};
use ui_kit::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::{appearance::Appearance, handle::ThemeHandle},
};
use updater_ui::updater::UpdaterDeps;
use widget::{frame::FrameRouter, services::WidgetServices};
use widget_registry::{catalogue, registry::WidgetRegistry};

use crate::{
    InitializationError,
    account::StoredAccounts,
    downloads::{Downloads, translation_unload},
};

pub(crate) struct ConfigHub {
    store: RefCell<ConfigStore>,
    services: Rc<WidgetServices>,
    translate: Translate,
    theme: ThemeHandle,
    network: ConfigNotification<NetworkConfig>,
    market: ConfigNotification<MarketParticipantNetworkSnapshot>,
    commits: Cell<u64>,
}

impl ConfigHub {
    pub(crate) const fn translate(&self) -> &Translate {
        &self.translate
    }

    pub(crate) fn config_signal(&self) -> Signal<Config> {
        self.services.config
    }

    #[cfg(test)]
    pub(crate) fn commits(&self) -> u64 {
        self.commits.get()
    }

    pub(crate) fn read<R>(&self, read: impl FnOnce(&Config) -> R) -> R {
        let snapshot = self.store.borrow().read();
        read(&snapshot)
    }

    pub(crate) fn edit(
        &self,
        app: &mut App,
        edit: impl FnOnce(&mut Config),
    ) -> Result<(), ConfigPersistenceError> {
        let previous = self.store.borrow().read();
        let mut candidate = previous.as_ref().clone();
        edit(&mut candidate);
        if candidate == *previous {
            return Ok(());
        }

        let stored = self.store.borrow_mut().write(candidate.clone())?;
        if stored.is_none() {
            let current = self.store.borrow().read();
            self.publish(app, &previous, current.as_ref().clone());
            return Ok(());
        }

        self.publish(app, &previous, candidate);
        Ok(())
    }

    fn repaint_all(app: &mut App) {
        let windows: Vec<_> = app.windows().map(|(id, _)| id).collect();
        for window in windows {
            let _ = app.request_repaint(window);
        }
    }

    fn publish(&self, app: &mut App, previous: &Config, candidate: Config) {
        self.commits.set(self.commits.get() + 1);

        if previous.language != candidate.language {
            let code = crate::language::code(candidate.language);
            self.translate.set_language(code);
            app.write(&self.services.language, code);
        }
        if previous.appearance != candidate.appearance {
            self.theme
                .set_appearance(app, appearance_of(candidate.appearance));
        }
        let network = candidate.network_config();
        if previous.network_config() != network {
            self.network.publish(Arc::new(network));
            let published = self.market.current();
            let market = MarketParticipantNetworkSnapshot::from_previous(
                candidate.clone(),
                Some(&published),
            );
            self.market.publish(Arc::new(market));
        }

        app.write(&self.services.config, candidate);
        Self::repaint_all(app);
    }

    pub(crate) fn apply_system_appearance(&self, app: &mut App, dark: bool) {
        let reported = if dark {
            Appearance::Dark
        } else {
            Appearance::Light
        };
        if self.read(|config| config.appearance) != StoredAppearance::System {
            return;
        }
        if self.theme.set_appearance(app, reported) {
            Self::repaint_all(app);
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(app: &mut App, store: ConfigStore) -> Rc<Self> {
        let initial = store.read();
        let theme = ThemeHandle::new(app, appearance_of(initial.appearance));
        let translate = crate::test_support::translate(i18n::language::codes::LanguageCode::En);
        let runtime = app.runtime_handle().clone();
        let layer = Rc::new(Layer::new());
        let frame_router =
            Rc::new(FrameRouter::new(app).expect("the test host installs the route"));
        let animation_router = Rc::new(AnimationRouter::new());
        let services = Rc::new(
            WidgetServices::builder(
                app,
                runtime,
                translate.clone(),
                theme.clone(),
                layer,
                frame_router,
                animation_router,
            )
            .language(i18n::language::codes::LanguageCode::En)
            .config(initial.as_ref().clone())
            .build(),
        );
        let network: ConfigNotification<NetworkConfig> =
            ConfigNotification::new(Arc::new(initial.as_ref().network_config()));
        let market = ConfigNotification::new(Arc::new(MarketParticipantNetworkSnapshot::new(
            initial.as_ref().clone(),
        )));

        Rc::new(Self {
            store: RefCell::new(store),
            services,
            translate,
            theme,
            network,
            market,
            commits: Cell::new(0),
        })
    }
}

pub(crate) const fn appearance_of(appearance: StoredAppearance) -> Appearance {
    match appearance {
        StoredAppearance::Light => Appearance::Light,
        StoredAppearance::Dark => Appearance::Dark,
        StoredAppearance::System => Appearance::Dark,
    }
}

pub(crate) const fn transport_policy() -> TransportPolicy {
    TransportPolicy {
        allow_plaintext: !cfg!(feature = "production"),
        extra_root_certificates: Vec::new(),
    }
}

#[derive(Clone, Copy)]
enum Accounts {
    Production,
    #[cfg(any(test, feature = "acceptance"))]
    SignedOut,
}

pub(crate) struct Composition {
    pub hub: Rc<ConfigHub>,
    pub theme: ThemeHandle,
    pub translate: Translate,
    pub services: Rc<WidgetServices>,
    pub layer: Rc<Layer>,
    pub frame_router: Rc<FrameRouter>,
    pub animation_router: Rc<AnimationRouter>,
    pub catalogue: Rc<WidgetRegistry>,
    pub market_participants: Option<MarketParticipantsConnector>,
    accounts: Arc<KytrixServiceConnector>,
    transport: Arc<TransportHub>,
    downloads: Rc<Downloads>,
    translation_downloads: Rc<TranslationDownloads>,
    pub settings_store: SettingsStore,

    pub recovery: Option<ConfigRecovery>,
}

impl Composition {
    pub(crate) fn new(
        app: &mut App,
        runtime: tokio::runtime::Handle,
    ) -> Result<Rc<Self>, InitializationError> {
        let (store, recovery) = crate::open_config_store();
        Self::build(app, runtime, store, recovery, Accounts::Production)
    }

    #[cfg(test)]
    pub(crate) fn with_store_for_test(
        app: &mut App,
        runtime: tokio::runtime::Handle,
        store: ConfigStore,
        recovery: Option<ConfigRecovery>,
    ) -> Result<Rc<Self>, InitializationError> {
        Self::build(app, runtime, store, recovery, Accounts::SignedOut)
    }

    #[cfg(feature = "acceptance")]
    pub(crate) fn for_acceptance(
        app: &mut App,
        runtime: tokio::runtime::Handle,
    ) -> Result<Rc<Self>, InitializationError> {
        Self::build(
            app,
            runtime,
            ConfigStore::in_memory(Config::default()),
            None,
            Accounts::SignedOut,
        )
    }

    fn build(
        app: &mut App,
        runtime: tokio::runtime::Handle,
        store: ConfigStore,
        recovery: Option<ConfigRecovery>,
        accounts: Accounts,
    ) -> Result<Rc<Self>, InitializationError> {
        let initial = store.read();
        let theme = ThemeHandle::new(app, appearance_of(initial.appearance));
        let translate = Translate::new(
            Rc::new(embedded::load()?),
            crate::language::code(initial.language),
        );

        let layer = Rc::new(Layer::new());
        let frame_router = Rc::new(FrameRouter::new(app)?);
        let animation_router = Rc::new(AnimationRouter::new());

        let transport = Arc::new(TransportHub::new(transport_policy()));
        let network: ConfigNotification<NetworkConfig> =
            ConfigNotification::new(Arc::new(initial.as_ref().network_config()));
        let market_notification = ConfigNotification::new(Arc::new(
            MarketParticipantNetworkSnapshot::new(initial.as_ref().clone()),
        ));
        let market_participants = install_market_participants(
            &runtime,
            Arc::clone(&transport),
            market_notification.clone(),
        );
        let accounts =
            spawn_connector(accounts, &runtime, network.clone(), Arc::clone(&transport))?;

        let services = Rc::new(
            WidgetServices::builder(
                app,
                runtime.clone(),
                translate.clone(),
                theme.clone(),
                Rc::clone(&layer),
                Rc::clone(&frame_router),
                Rc::clone(&animation_router),
            )
            .language(crate::language::code(initial.language))
            .config(initial.as_ref().clone())
            .catalog_generation(0)
            .reduce_motion(false)
            .market_participants(market_participants.clone())
            .kytrix_service(Some((*accounts).clone()))
            .open_url(open_url)
            .sound(no_audio)
            .build(),
        );

        let hub = Rc::new(ConfigHub {
            store: RefCell::new(store),
            services: Rc::clone(&services),
            translate: translate.clone(),
            theme: theme.clone(),
            network: network.clone(),
            market: market_notification,
            commits: Cell::new(0),
        });

        let settings_store = settings_ui::session::SettingsStore::new(app, services.config, {
            let hub = Rc::clone(&hub);
            move |app, candidate| {
                hub.edit(app, |settings| *settings = candidate)
                    .map_err(SettingsWriteError::new)
            }
        });

        let downloads = Rc::new(Downloads::new(
            app,
            Arc::clone(&transport),
            runtime.clone(),
            network,
        ));
        let translation_downloads = TranslationDownloads::new(
            app,
            TranslationDownloadsDeps {
                store: settings_store.clone(),
                downloads: downloads.settings_service(),
                runtime,
                unload: translation_unload(),
            },
        );

        if prediction_translator(initial.as_ref()).is_none() {
            log::warn!("the prediction translation engine is unavailable on this build");
        }

        Ok(Rc::new(Self {
            hub,
            theme,
            translate,
            services,
            layer,
            frame_router,
            animation_router,
            catalogue: Rc::new(catalogue::registry()?),
            market_participants,
            accounts,
            transport,
            downloads,
            translation_downloads,
            settings_store,
            recovery,
        }))
    }

    pub(crate) const fn services(&self) -> &Rc<WidgetServices> {
        &self.services
    }

    #[cfg(test)]
    pub(crate) const fn downloads(&self) -> &Rc<Downloads> {
        &self.downloads
    }

    pub(crate) const fn translation_downloads(&self) -> &Rc<TranslationDownloads> {
        &self.translation_downloads
    }

    pub(crate) fn updater_deps(&self) -> UpdaterDeps {
        UpdaterDeps::new(
            Arc::clone(&self.transport),
            self.downloads.updater_service(),
        )
    }

    pub(crate) fn profile_host(&self) -> ProfileHost {
        ProfileHost::new(
            Rc::clone(&self.services),
            self.accounts.clone(),
            Rc::new(StoredAccounts::new(Rc::clone(&self.hub))),
        )
    }

    pub(crate) fn sync_downloads(&self, app: &mut App) {
        self.downloads.sync(app);
    }

    pub(crate) fn sync_catalog_generation(&self, app: &mut App) {
        let Some(connector) = &self.market_participants else {
            return;
        };
        self.publish_catalog_generation(app, connector.catalog().generation_id().get());
    }

    fn publish_catalog_generation(&self, app: &mut App, generation: u64) {
        if app.read_with(&self.services.catalog_generation, |current| *current) != generation {
            app.write(&self.services.catalog_generation, generation);
        }
    }

    pub(crate) fn write_workspaces(
        &self,
        app: &mut App,
        workspaces: app_config::workspace::WorkspacesConfig,
    ) -> Result<(), ConfigPersistenceError> {
        self.hub.edit(app, |config| config.workspaces = workspaces)
    }

    pub(crate) fn write_sidebar_mode(&self, app: &mut App, mode: app_config::ui::SidebarMode) {
        let _ = self.hub.edit(app, |config| config.ui.sidebar = mode);
    }
}

fn spawn_connector(
    accounts: Accounts,
    runtime: &Handle,
    network: ConfigNotification<NetworkConfig>,
    transport: Arc<TransportHub>,
) -> Result<Arc<KytrixServiceConnector>, InitializationError> {
    let connector = match accounts {
        Accounts::Production => KytrixServiceConnector::spawn(runtime.clone(), network, transport)?,
        #[cfg(any(test, feature = "acceptance"))]
        Accounts::SignedOut => {
            KytrixServiceConnector::spawn_signed_out(runtime.clone(), network, transport)?
        }
    };

    Ok(Arc::new(connector))
}

#[cfg(ai_translation)]
fn prediction_translator(
    config: &Config,
) -> Option<std::sync::Arc<dyn instrument_selector_ui::prediction_translation::LocalTranslator>> {
    match crate::translation::probe(config) {
        Ok(()) => Some(crate::translation::production()),
        Err(error) => {
            log::error!("the local translation engine is unavailable: {error}");
            None
        }
    }
}

#[cfg(not(ai_translation))]
fn prediction_translator(
    _config: &Config,
) -> Option<std::sync::Arc<dyn instrument_selector_ui::prediction_translation::LocalTranslator>> {
    None
}

fn install_market_participants(
    runtime: &Handle,
    transport: Arc<TransportHub>,
    notification: ConfigNotification<MarketParticipantNetworkSnapshot>,
) -> Option<MarketParticipantsConnector> {
    let input = MarketParticipantsConnectorInput::new(transport, notification)
        .with_runtime(runtime.clone());
    match MarketParticipantsConnector::new(input) {
        Ok(connector) => Some(connector),
        Err(error) => {
            log::error!("the market participants connector is unavailable: {error}");
            None
        }
    }
}

pub(crate) fn open_url(url: &str) {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler");
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = std::process::Command::new("xdg-open");
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    let mut command = std::process::Command::new("true");

    if let Err(error) = command.arg(url).spawn() {
        log::error!("failed to open `{url}`: {error}");
    }
}

fn no_audio(_name: &'static str) -> Option<bytes::Bytes> {
    None
}

#[cfg(test)]
mod tests {
    use app_config::{
        network::{ProxyAuthorizationConfig, ProxyConfig},
        settings::Language,
    };
    use app_config_provider::store::ConfigStore;
    use i18n::language::codes::LanguageCode;
    use smol_str::SmolStr;
    use ui_core::{
        node::builder::NodeBuilder, paint::style::StylePatch, test_support::app::TestApp,
    };

    use super::*;

    fn headless() -> (TestApp, Rc<Composition>) {
        let mut app = TestApp::new();
        let runtime = app.app().runtime_handle().clone();
        let composition = Composition::with_store_for_test(
            app.app_mut(),
            runtime,
            ConfigStore::in_memory(Config::default()),
            None,
        )
        .expect("the composition root builds headlessly");

        (app, composition)
    }

    #[test]
    fn the_stored_appearance_should_resolve_to_a_palette() {
        assert_eq!(appearance_of(StoredAppearance::Light), Appearance::Light);
        assert_eq!(appearance_of(StoredAppearance::Dark), Appearance::Dark);
        assert_eq!(appearance_of(StoredAppearance::System), Appearance::Dark);
    }

    #[test]
    fn the_audio_seam_should_have_no_backend() {
        assert!(no_audio("widget.news.sound").is_none());
    }

    #[test]
    fn a_settings_write_should_publish_into_the_services_signals() {
        let (mut app, composition) = headless();
        let services = Rc::clone(composition.services());
        let app = app.app_mut();

        composition
            .hub
            .edit(app, |config| {
                config.language = Language::ZhCn;
                config.appearance = StoredAppearance::Light;
            })
            .expect("an in-memory store stores every write");

        assert_eq!(composition.hub.commits(), 1);
        assert_eq!(*app.read(&services.language), LanguageCode::ZhCn);
        assert_eq!(
            app.read_with(&services.config, |config| config.language),
            Language::ZhCn
        );
        assert_eq!(
            app.read_with(&services.config, |config| config.appearance),
            StoredAppearance::Light
        );
        assert_eq!(composition.theme.current(app), Appearance::Light);
        assert_eq!(composition.translate.language(), LanguageCode::ZhCn);
    }

    #[test]
    fn a_refused_write_should_leave_the_services_snapshot_alone() {
        let mut app = TestApp::new();
        let runtime = app.app().runtime_handle().clone();
        let blocker =
            std::env::temp_dir().join(format!("kytrix-p4-3b-refused-write-{}", std::process::id()));
        std::fs::write(&blocker, b"not a directory").expect("the blocker file is written");
        let store = ConfigStore::from_path(blocker.join("config.json"))
            .expect("a missing settings file loads the defaults");
        let composition = Composition::with_store_for_test(app.app_mut(), runtime, store, None)
            .expect("the composition root builds over the store");
        let services = Rc::clone(composition.services());
        let before = app.app().read_with(&services.config, Clone::clone);
        let before_language = *app.app().read(&services.language);

        let result = composition.hub.edit(app.app_mut(), |config| {
            config.appearance = StoredAppearance::Light;
        });

        assert!(result.is_err(), "the store cannot write under a file");
        assert_eq!(composition.hub.commits(), 0, "nothing was published");
        assert_eq!(
            app.app().read_with(&services.config, Clone::clone),
            before,
            "the frozen snapshot is exactly what it was"
        );
        assert_eq!(*app.app().read(&services.language), before_language);
        let _ = std::fs::remove_file(&blocker);
    }

    #[test]
    fn a_catalog_generation_write_should_redraw_its_readers() {
        let (mut app, composition) = headless();
        let services = Rc::clone(composition.services());
        let window = app.window_id();
        let generation = services.catalog_generation;

        let root = {
            let target = app
                .app()
                .window(window)
                .expect("the test host has a window");
            target.tree().root()
        };
        let probe = NodeBuilder::boxed()
            .id(smol_str::SmolStr::new_static("CATALOG_GENERATION_PROBE"))
            .bind_style(move |cx| StylePatch {
                visible: Some(*cx.read(&generation) > 0),
                ..StylePatch::default()
            })
            .build(
                app.app_mut()
                    .window_mut(window)
                    .expect("the test host has a window")
                    .tree_mut(),
                root,
            )
            .expect("the probe node builds");
        let visible = |app: &TestApp| {
            app.app()
                .window(window)
                .and_then(|target| target.tree().get(probe).ok())
                .and_then(|node| node.style_patch().visible)
        };

        let _ = app.drain();
        assert_eq!(visible(&app), Some(false), "nothing is published yet");

        composition.publish_catalog_generation(app.app_mut(), 7);
        let _ = app.drain();

        assert_eq!(*app.app().read(&services.catalog_generation), 7);
        assert_eq!(
            visible(&app),
            Some(true),
            "the node that reads the signal is redrawn with the new value"
        );
    }

    #[test]
    fn a_network_write_should_republish_the_routes() {
        let mut app = TestApp::new();
        let hub = ConfigHub::for_test(app.app_mut(), ConfigStore::in_memory(Config::default()));
        let before = hub.network.current();

        hub.edit(app.app_mut(), |config| {
            config.language = Language::En;
        })
        .expect("an in-memory store stores every write");
        assert!(
            Arc::ptr_eq(&before, &hub.network.current()),
            "a language write says nothing about the network"
        );

        hub.edit(app.app_mut(), |config| {
            config.proxies.insert(
                SmolStr::new_static("p4-5"),
                ProxyConfig {
                    addr: SmolStr::new_static("127.0.0.1"),
                    port: 9,
                    authorization: ProxyAuthorizationConfig::None,
                    resolve_dns_at_remote: false,
                },
            );
        })
        .expect("an in-memory store stores every write");
        assert!(
            !Arc::ptr_eq(&before, &hub.network.current()),
            "a network write republishes the projection"
        );
        assert!(
            hub.network
                .current()
                .proxies
                .contains_key(&SmolStr::new_static("p4-5")),
            "the published projection is the settings the store now holds"
        );
    }
}

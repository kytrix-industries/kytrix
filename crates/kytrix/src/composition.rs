//! The composition root: the one place the application's collaborators are built.
//!
//! Every value here is created exactly once, in dependency order, and handed down; nothing
//! in this process is a type-keyed global (the engine has no global store). The root owns:
//!
//! - the **settings store** ([`ConfigStore`]) and its published snapshot, whose writes go
//!   through one seam ([`ConfigHub::edit`]: persist the candidate, publish it into the
//!   frozen services signals, then project what changed — the translator's language, the
//!   engine language signal, the theme's appearance, and the network projections the
//!   service connector, the download manager and the market-participants connector route
//!   with);
//! - the **translator** ([`Translate`]) and the language signal every drawn node registers
//!   when its text must follow a language switch;
//! - the **theme** ([`ThemeHandle`]), resolved at startup from the stored appearance and
//!   re-resolved on every appearance write; system appearance changes arrive as engine
//!   events and are applied by the shell through [`ConfigHub::apply_system_appearance`];
//! - the **three application-level registries** kit components and widgets require:
//!   [`Layer`], [`FrameRouter`] (the single `App::on_frame` owner) and [`AnimationRouter`]
//!   (the single `App::on_animation_complete` owner);
//! - the **services aggregate** ([`WidgetServices`]) every widget, page and title-bar
//!   control is handed, including the two connector slots (§13.9 R-1). It is also the
//!   **one home of the four frozen signals** ([`WidgetServices::language`],
//!   [`WidgetServices::config`], [`WidgetServices::catalog_generation`],
//!   [`WidgetServices::reduce_motion`]): the root is their only writer —
//!   [`ConfigHub::publish`] writes the language and the config snapshot, the shell's route
//!   mirrors the catalogue generation, and the reduced-motion value is seeded here at
//!   startup (this batch's settings carry no accessibility preference yet, so it stays at
//!   the product default);
//! - the **service connection** ([`KytrixServiceConnector`]) the account control and every
//!   service-backed widget read, and the **download manager** ([`Downloads`]) the
//!   translation page and the updater share;
//! - the **widget catalogue** ([`WidgetRegistry`]) a pane mounts a widget through, the
//!   **market participants connector** the title bar's venue menu loads through, and the
//!   **settings page seam** ([`SettingsStore`]) whose commit closure is [`ConfigHub::edit`].
//!
//! # Signals are one identity each
//!
//! The aggregate's `language`, `config` and `catalog_generation` signals are the ones this
//! root writes and every consumer reads: the builder mints them, and [`ConfigHub`] holds
//! the aggregate. A write through the hub is therefore a write every widget, page and
//! title-bar control observes — a second signal carrying "the same" value would be a
//! snapshot nothing follows.
//!
//! # Network projections
//!
//! The connector, the download manager and the market-participants connector each route
//! with a network projection of the settings. Every commit republishes the projections when
//! the network settings actually moved ([`ConfigNotification::publish`]), so a proxy change
//! reaches the next session reconfigure / transfer start instead of the next launch.

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
use base_ui::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::{appearance::Appearance, handle::ThemeHandle},
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
use updater_ui::updater::UpdaterDeps;
use widget::{frame::FrameRouter, services::WidgetServices};
use widget_registry::{catalogue, registry::WidgetRegistry};

use crate::{
    InitializationError,
    account::StoredAccounts,
    downloads::{Downloads, translation_unload},
};

/// The settings store, its published snapshot, and everything a commit projects from it.
///
/// The seam is one function on purpose: a settings write is persisted first and published
/// only when the file holds it, so a write that cannot be stored leaves the application
/// running on the settings it already has (the settings page reports the reason and the
/// user's draft stays in the dialog they typed it in).
pub(crate) struct ConfigHub {
    /// The persisted settings. The store is behind an `Rc<RefCell<..>>` because the commit
    /// seam is a `'static` closure shared by every control that writes.
    store: RefCell<ConfigStore>,
    /// The services aggregate whose frozen signals every consumer reads
    /// ([`WidgetServices::config`], [`WidgetServices::language`]). The hub writes them;
    /// nothing else mints a second copy.
    services: Rc<WidgetServices>,
    /// The translator, whose language follows the stored one.
    translate: Translate,
    /// The theme service, whose appearance follows the stored one.
    theme: ThemeHandle,
    /// The network projection the service connector and the download manager route with.
    ///
    /// One notification for both: the connector's forwarding task is its single waiter,
    /// and the download manager only ever reads [`ConfigNotification::current`] when it
    /// starts an attempt.
    network: ConfigNotification<NetworkConfig>,
    /// The market-participants projection the directory connector loads venues under.
    market: ConfigNotification<MarketParticipantNetworkSnapshot>,
    /// How many writes the seam has committed, for tests and diagnostics.
    commits: Cell<u64>,
}

impl ConfigHub {
    /// The translator.
    pub(crate) const fn translate(&self) -> &Translate {
        &self.translate
    }

    /// The settings snapshot every consumer reads.
    pub(crate) fn config_signal(&self) -> Signal<Config> {
        self.services.config
    }

    /// The number of commits the seam has made.
    #[cfg(test)]
    pub(crate) fn commits(&self) -> u64 {
        self.commits.get()
    }

    /// Reads one value out of the settings of the moment.
    pub(crate) fn read<R>(&self, read: impl FnOnce(&Config) -> R) -> R {
        let snapshot = self.store.borrow().read();
        read(&snapshot)
    }

    /// Writes one change into the settings through the commit seam.
    ///
    /// # Errors
    /// The candidate could not be stored (the settings are then as they were).
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
            // The store treats an unchanged snapshot as no write; the equality check above
            // already filtered that case, so reaching here means the store normalised the
            // candidate (network generations) — publish what it now holds.
            let current = self.store.borrow().read();
            self.publish(app, &previous, current.as_ref().clone());
            return Ok(());
        }

        self.publish(app, &previous, candidate);
        Ok(())
    }

    /// Repaints every window after a settings write.
    fn repaint_all(app: &mut App) {
        let windows: Vec<_> = app.windows().map(|(id, _)| id).collect();
        for window in windows {
            let _ = app.request_repaint(window);
        }
    }

    /// Publishes a committed candidate and projects the fields that changed.
    fn publish(&self, app: &mut App, previous: &Config, candidate: Config) {
        self.commits.set(self.commits.get() + 1);

        if previous.language != candidate.language {
            let code = crate::language::code(candidate.language);
            // Translator first: a node that reads the new language from the signal below
            // must find the catalogue already rendering it.
            self.translate.set_language(code);
            app.write(&self.services.language, code);
        }
        if previous.appearance != candidate.appearance {
            self.theme
                .set_appearance(app, appearance_of(candidate.appearance));
        }
        // The network projections are republished only when the network settings moved: a
        // language or sidebar write must not reconfigure the service sessions or the
        // directory connector.
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

    /// Applies an appearance the operating system reported.
    ///
    /// A pinned preference wins: the stored appearance is re-resolved and, when it is
    /// `System`, follows the reported one. The write goes through the same publication path
    /// the settings page uses, so the theme swap and the repaint are one event.
    pub(crate) fn apply_system_appearance(&self, app: &mut App, dark: bool) {
        let reported = if dark {
            Appearance::Dark
        } else {
            Appearance::Light
        };
        if self.read(|config| config.appearance) != StoredAppearance::System {
            // A pinned preference does not follow the system; the theme is already what the
            // user asked for.
            return;
        }
        if self.theme.set_appearance(app, reported) {
            Self::repaint_all(app);
        }
    }

    /// A hub over `store` for a unit test that needs the commit seam without the shell.
    ///
    /// The hub writes the aggregate's own signals, so the aggregate is built here too: a
    /// test must observe the same signal identity the application writes, never a private
    /// copy the root would not follow (the P1-1 regression this seam exists to prevent).
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

/// The theme appearance a stored preference names.
pub(crate) const fn appearance_of(appearance: StoredAppearance) -> Appearance {
    match appearance {
        StoredAppearance::Light => Appearance::Light,
        StoredAppearance::Dark => Appearance::Dark,
        // `System` resolves against what the platform reports; the engine starts in the
        // dark palette and the first system appearance event applies the truth. A start
        // that never receives one (headless) draws dark, which is the product default.
        StoredAppearance::System => Appearance::Dark,
    }
}

/// The transport policy the build calls for.
///
/// A development build talks to the service on the loopback, which is plaintext: the
/// account origin is `http://127.0.0.1:18010` and the service socket is `ws://`, so a
/// TLS-only transport would refuse every request. A production build talks to
/// `https://account.kytrix.xyz` and `wss://api.kytrix.xyz` and never sends plaintext
/// anywhere. The policy is derived from the same `production` switch as those origins, so
/// the two cannot disagree about which environment this build is for.
pub(crate) const fn transport_policy() -> TransportPolicy {
    TransportPolicy {
        allow_plaintext: !cfg!(feature = "production"),
        extra_root_certificates: Vec::new(),
    }
}

/// Which account session the root installs.
#[derive(Clone, Copy)]
enum Accounts {
    /// The real OAuth session (the keyring credential store).
    Production,
    /// The signed-out in-memory session a headless host runs over.
    #[cfg(any(test, feature = "acceptance"))]
    SignedOut,
}

/// Everything the shell draws with, assembled once.
pub(crate) struct Composition {
    /// The settings seam.
    pub hub: Rc<ConfigHub>,
    /// The theme service the shell and every kit component read.
    pub theme: ThemeHandle,
    /// The translator every page and component is handed.
    pub translate: Translate,
    /// The services aggregate every widget, page and title-bar control is handed. It holds
    /// the four frozen signals; the root writes them and the shell reads them there, so
    /// there is no second set of engine signals to keep in step.
    pub services: Rc<WidgetServices>,
    /// The three registries kit components resolve through.
    pub layer: Rc<Layer>,
    pub frame_router: Rc<FrameRouter>,
    pub animation_router: Rc<AnimationRouter>,
    /// The widget catalogue a pane mounts widgets from.
    pub catalogue: Rc<WidgetRegistry>,
    /// The market participants root the title bar loads venues through.
    pub market_participants: Option<MarketParticipantsConnector>,
    /// The service connection every service-backed widget and the account control read.
    accounts: Arc<KytrixServiceConnector>,
    /// The process transport the connector, the downloads and the updater share.
    transport: Arc<TransportHub>,
    /// The process-wide download manager.
    downloads: Rc<Downloads>,
    /// The translation model downloads the settings page draws.
    translation_downloads: Rc<TranslationDownloads>,
    /// The settings page seam (persist → publish → project).
    pub settings_store: SettingsStore,

    /// Whether the settings file had to be replaced at startup.
    pub recovery: Option<ConfigRecovery>,
}

impl Composition {
    /// Builds the application's collaborators, in dependency order.
    pub(crate) fn new(
        app: &mut App,
        runtime: tokio::runtime::Handle,
    ) -> Result<Rc<Self>, InitializationError> {
        let (store, recovery) = crate::open_config_store();
        Self::build(app, runtime, store, recovery, Accounts::Production)
    }

    /// Builds the application over the signed-out in-memory account session.
    ///
    /// The headless host has no keyring and must not contact the account service; every
    /// other collaborator is the production one, with an in-memory settings store.
    #[cfg(test)]
    pub(crate) fn with_store_for_test(
        app: &mut App,
        runtime: tokio::runtime::Handle,
        store: ConfigStore,
        recovery: Option<ConfigRecovery>,
    ) -> Result<Rc<Self>, InitializationError> {
        Self::build(app, runtime, store, recovery, Accounts::SignedOut)
    }

    /// Builds the acceptance self-check's application.
    ///
    /// The acceptance run drives a real window, but it must not read or write the user's
    /// settings file and must not touch the keyring: the run switches the language and the
    /// appearance as part of its steps, so it runs over a fresh in-memory installation and
    /// the signed-out session. Everything else — the catalogue, the registries, the shell —
    /// is the production build's.
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

    /// Builds every collaborator, in dependency order.
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

        // The process transport is one value: the service connection, the download manager
        // and the updater all route through it, under the same lifecycle barrier.
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

        // The frozen signal set is minted with the aggregate and lives there: the shell and
        // every widget read the same four signals, and the hub below is their one writer.
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
            // This batch's settings carry no accessibility preference yet, so the product
            // default (animate) is what the root seeds. When a preference lands, it is the
            // root that writes this signal from it, never a component.
            .reduce_motion(false)
            .market_participants(market_participants.clone())
            .kytrix_service(Some((*accounts).clone()))
            .open_url(open_url)
            .sound(no_audio)
            .build(),
        );

        // The hub publishes the aggregate's own signals: a widget, a page or the account
        // control that reads one of them must observe this root's writes (see the module
        // documentation, "Signals are one identity each").
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

        // The local translation engine is proved usable at startup (it is the only thing
        // that would load the model, and this call answers empty content without one). The
        // adapter itself is handed to the selector host when panes build selector controls;
        // the previous shell only ever drew a placeholder there, so nothing consumes it yet
        // (P4-3 ledger, "selector wiring").
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

    /// The services aggregate.
    pub(crate) const fn services(&self) -> &Rc<WidgetServices> {
        &self.services
    }

    /// The process-wide download manager, for the shell's tests.
    #[cfg(test)]
    pub(crate) const fn downloads(&self) -> &Rc<Downloads> {
        &self.downloads
    }

    /// The translation model downloads the settings page draws.
    pub(crate) const fn translation_downloads(&self) -> &Rc<TranslationDownloads> {
        &self.translation_downloads
    }

    /// What the updater needs from the root: the process transport and the download
    /// manager it asks for its payload.
    pub(crate) fn updater_deps(&self) -> UpdaterDeps {
        UpdaterDeps::new(
            Arc::clone(&self.transport),
            self.downloads.updater_service(),
        )
    }

    /// The account control's collaborators.
    pub(crate) fn profile_host(&self) -> ProfileHost {
        ProfileHost::new(
            Rc::clone(&self.services),
            self.accounts.clone(),
            Rc::new(StoredAccounts::new(Rc::clone(&self.hub))),
        )
    }

    /// Mirrors the download manager's snapshots onto the signal the settings page reads.
    ///
    /// Called once per frame by the shell's route: the settings page synchronizes itself
    /// after this, so a transfer's progress reaches the row that draws it on that frame.
    pub(crate) fn sync_downloads(&self, app: &mut App) {
        self.downloads.sync(app);
    }

    /// Mirrors the market-participants catalogue generation into its frozen services signal.
    ///
    /// Called once per frame by the shell's route: reading the generation costs nothing and
    /// writing it only when it moved is what asks every widget waiting on a directory for
    /// the frame that reruns its `load` (§13.5). An unchanged generation writes nothing, so
    /// an idle application still renders no frames (I3).
    pub(crate) fn sync_catalog_generation(&self, app: &mut App) {
        let Some(connector) = &self.market_participants else {
            return;
        };
        self.publish_catalog_generation(app, connector.catalog().generation_id().get());
    }

    /// Publishes `generation` as the services signal when it moved.
    ///
    /// The shell's route reaches this through [`Self::sync_catalog_generation`]; the
    /// split lets a test drive the publication without a network connector behind it.
    fn publish_catalog_generation(&self, app: &mut App, generation: u64) {
        if app.read_with(&self.services.catalog_generation, |current| *current) != generation {
            app.write(&self.services.catalog_generation, generation);
        }
    }

    /// Writes the workspace list into the settings (a change the shell owns).
    pub(crate) fn write_workspaces(
        &self,
        app: &mut App,
        workspaces: app_config::workspace::WorkspacesConfig,
    ) -> Result<(), ConfigPersistenceError> {
        self.hub.edit(app, |config| config.workspaces = workspaces)
    }

    /// Writes the interface preference (the sidebar state the toggle walks).
    pub(crate) fn write_sidebar_mode(&self, app: &mut App, mode: app_config::ui::SidebarMode) {
        let _ = self.hub.edit(app, |config| config.ui.sidebar = mode);
    }
}

/// Connects the application to the account service over this build's session.
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

/// The prediction-event dialog's translation engine of this build.
///
/// The engine is in the build when the `ai` feature is on and the target has a llama.cpp
/// build in this workspace (Windows ARM64 has none). A build without it — the acceptance
/// build — runs the dialog without a translator and reports it cannot translate.
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

/// A build without the local translation engine: the dialog reports it cannot translate.
#[cfg(not(ai_translation))]
fn prediction_translator(
    _config: &Config,
) -> Option<std::sync::Arc<dyn instrument_selector_ui::prediction_translation::LocalTranslator>> {
    None
}

/// Builds the market participants root the title bar's venue menu loads through.
///
/// The connector is built over the process transport and the market-participants network
/// projection of the settings. It starts no load: no venue is fetched at startup, the
/// user's own load is the only thing that fetches. The projection is republished after
/// every commit that changes the network or venue settings (see [`ConfigHub::publish`]),
/// and the root's invalidation task closes the pooled connections the new projection no
/// longer serves.
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

/// Opens a URL in the user's browser.
///
/// The application's own command (the frozen services contract hands it to widgets rather
/// than letting each shell out): each platform's opener takes the URL as one argument, so
/// nothing here is reinterpreted by a shell.
pub(crate) fn open_url(url: &str) {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler");
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command
    };
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    let mut command = std::process::Command::new("true");

    if let Err(error) = command.arg(url).spawn() {
        log::error!("failed to open `{url}`: {error}");
    }
}

/// The audio seam: this build has no sound backend, so a widget asking for a sound gets
/// none and plays nothing rather than panicking (§13.2 `WidgetServices::sound`).
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

    /// The application the binary builds, over the headless engine and an in-memory store.
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

    /// The stored appearance preference resolves to a palette, and `System` starts in the
    /// product default (dark) until the platform reports otherwise.
    #[test]
    fn the_stored_appearance_should_resolve_to_a_palette() {
        assert_eq!(appearance_of(StoredAppearance::Light), Appearance::Light);
        assert_eq!(appearance_of(StoredAppearance::Dark), Appearance::Dark);
        assert_eq!(appearance_of(StoredAppearance::System), Appearance::Dark);
    }

    /// The audio seam answers none rather than pretending to have a backend.
    #[test]
    fn the_audio_seam_should_have_no_backend() {
        assert!(no_audio("widget.news.sound").is_none());
    }

    /// A committed settings write lands in the frozen services signals — the ones every
    /// widget and page reads — and in the translator and the theme, in one publication.
    ///
    /// This is what P1-1 regressed: the composition root used to keep a second set of
    /// engine signals of its own, so consumers of `WidgetServices` never saw a write.
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

    /// The commit seam persists first: a write the store refuses leaves the frozen
    /// services snapshot (and the theme and the translator) exactly as they were.
    #[test]
    fn a_refused_write_should_leave_the_services_snapshot_alone() {
        let mut app = TestApp::new();
        let runtime = app.app().runtime_handle().clone();
        // A file where the settings directory would go makes the store's own write fail
        // (`create_dir_all` on a file is refused) without needing a read-only filesystem.
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

    /// A catalogue generation written by the root reaches the frozen services signal and
    /// redraws the nodes that read it: a widget waiting on the directory reruns its `load`
    /// on that frame (§13.5).
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

    /// A commit that moves the network settings republishes the projection the connector
    /// and the download manager route with; a commit that moves nothing else does not.
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

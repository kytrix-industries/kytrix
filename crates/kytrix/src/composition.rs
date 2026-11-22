//! The composition root: the one place the application's collaborators are built.
//!
//! Every value here is created exactly once, in dependency order, and handed down; nothing
//! in this process is a type-keyed global (the engine has no global store). The root owns:
//!
//! - the **settings store** ([`ConfigStore`]) and its published snapshot, whose writes go
//!   through one seam ([`ConfigHub::edit`]: persist the candidate, publish it into the
//!   frozen services signal, then project what changed — the translator's language, the
//!   engine language signal, the theme's appearance);
//! - the **translator** ([`Translate`]);
//! - the **theme** ([`ThemeHandle`]), resolved at startup from the stored appearance and
//!   re-resolved on every appearance write; system appearance changes arrive as engine
//!   events and are applied by the shell through [`ConfigHub::apply_system_appearance`];
//! - the **three application-level registries** kit components and widgets require:
//!   [`Layer`], [`FrameRouter`] (the single `App::on_frame` owner) and [`AnimationRouter`]
//!   (the single `App::on_animation_complete` owner);
//! - the **services aggregate** ([`WidgetServices`]) every widget, page and title-bar
//!   control is handed, with the two connector slots the frozen contract models as
//!   `Option` (§13.9 R-1). It is also the **one home of the four frozen signals**
//!   ([`WidgetServices::language`], [`WidgetServices::config`],
//!   [`WidgetServices::catalog_generation`], [`WidgetServices::reduce_motion`]): the root
//!   is their only writer — [`ConfigHub::publish`] writes the language and the config
//!   snapshot, the shell's route mirrors the catalogue generation, and the reduced-motion
//!   value is seeded here at startup (this batch's settings carry no accessibility
//!   preference yet, so it stays at the product default);
//! - the **widget catalogue** ([`WidgetRegistry`]) a pane mounts a widget through, and the
//!   **market participants connector** the title bar's venue menu loads through;
//! - the **settings page seam** ([`SettingsStore`]) whose commit closure is
//!   [`ConfigHub::edit`], and the **selector host**
//!   ([`SelectorHost`]) with the translation engine and the catalogue injected.
//!
//! # Connector gaps (P4-3)
//!
//! Two collaborators the previous toolkit shell installed cannot be constructed without a
//! toolkit `App` context, so the root leaves their contract slots empty and records the
//! gap (§13.9 R-1 already models both as `Option`):
//!
//! - [`WidgetServices::kytrix_service`] — `KytrixServiceConnector::install` needs a toolkit
//!   `App` for its configuration-event bridge; a toolkit-free constructor (runtime +
//!   [`ConfigNotification`](app_config_provider::notification::ConfigNotification) +
//!   transport, with the root publishing network snapshots after each commit) is the
//!   seam the next wiring batch adds;
//! - the translation download service (`DownloadService`) — `download_provider` is a toolkit
//!   entity (`Context`, `Task`, `EventEmitter`) and is deleted/replaced by P5-1; the
//!   settings page therefore opens without a download table.
//!
//! Everything else — settings persistence, translation, theme, catalogue, market
//! participants, selector host, frame ownership — is wired here and is what the shell
//! draws.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};

use app_config::settings::{Appearance as StoredAppearance, Config};
use app_config_provider::store::{ConfigPersistenceError, ConfigRecovery, ConfigStore};
use base_ui::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::{appearance::Appearance, handle::ThemeHandle},
};
use i18n::{global::embedded, translate::Translate};
use market_participants_connector::connector::{
    MarketParticipantsConnector, MarketParticipantsConnectorInput,
};
use network::transport::hub::{TransportHub, TransportPolicy};
use ui_core::app::App;
use widget::{frame::FrameRouter, services::WidgetServices};
use widget_registry::{catalogue, registry::WidgetRegistry};

use crate::InitializationError;

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
    /// How many writes the seam has committed, for tests and diagnostics.
    commits: Cell<u64>,
}

impl ConfigHub {
    /// The translator.
    pub(crate) const fn translate(&self) -> &Translate {
        &self.translate
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
    /// The settings page seam (persist → publish → project).
    pub settings_store: settings_ui::session::SettingsStore,

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
        Self::with_store(app, runtime, store, recovery)
    }

    /// Builds the application's collaborators over the settings `store`.
    ///
    /// The store is an argument so a headless host can hand in an in-memory one; the
    /// production entry point opens the on-disk store and hands that in.
    pub(crate) fn with_store(
        app: &mut App,
        runtime: tokio::runtime::Handle,
        store: ConfigStore,
        recovery: Option<ConfigRecovery>,
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

        let market_participants = install_market_participants(&runtime, initial.as_ref());

        // The frozen signal set is minted with the aggregate and lives there: the shell and
        // every widget read the same four signals, and the hub below is their one writer.
        let services = Rc::new(
            WidgetServices::builder(
                app,
                runtime,
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
            .kytrix_service(None)
            .open_url(open_url)
            .sound(no_audio)
            .build(),
        );

        let hub = Rc::new(ConfigHub {
            store: RefCell::new(store),
            services: Rc::clone(&services),
            translate: translate.clone(),
            theme: theme.clone(),
            commits: Cell::new(0),
        });

        let settings_store = settings_ui::session::SettingsStore::new(app, services.config, {
            let hub = Rc::clone(&hub);
            move |app, candidate| {
                hub.edit(app, |settings| *settings = candidate)
                    .map_err(settings_ui::session::SettingsWriteError::new)
            }
        });

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
            settings_store,
            recovery,
        }))
    }

    /// The services aggregate.
    pub(crate) const fn services(&self) -> &Rc<WidgetServices> {
        &self.services
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

/// The prediction-event dialog's translation engine of this build.
///
/// Windows ARM64 has no llama.cpp build, so there the dialog runs without a translator and
/// reports it cannot translate; every other target gets the local model adapter.
#[cfg(not(all(target_os = "windows", target_arch = "aarch64")))]
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

#[cfg(all(target_os = "windows", target_arch = "aarch64"))]
fn prediction_translator(
    _config: &Config,
) -> Option<std::sync::Arc<dyn instrument_selector_ui::prediction_translation::LocalTranslator>> {
    None
}

/// Builds the market participants root the title bar's venue menu loads through.
///
/// The connector is built over the process transport and the market-participants network
/// projection of the settings, and it starts no work: no venue is loaded at startup, the
/// user's own load is the only thing that fetches. The projection is a startup snapshot —
/// the toolkit configuration-event bridge that would republish it on a network-settings write
/// is the seam this batch is missing (see the module documentation); a proxy change
/// therefore takes effect on the next start rather than on the next load.
fn install_market_participants(
    runtime: &tokio::runtime::Handle,
    config: &Config,
) -> Option<MarketParticipantsConnector> {
    let transport = Arc::new(TransportHub::new(transport_policy()));
    let snapshot = Arc::new(
        market_participants_connector::runtime::MarketParticipantNetworkSnapshot::new(
            config.clone(),
        ),
    );
    let notification = app_config_provider::notification::ConfigNotification::new(snapshot);
    let input = MarketParticipantsConnectorInput::new(transport, notification).with_runtime(
        widget_tokio_runtime::runtime::Runtime::from_handle(runtime.clone()),
    );
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
    use app_config::settings::Language;
    use i18n::language::codes::LanguageCode;
    use ui_core::{
        node::builder::NodeBuilder, paint::style::StylePatch, test_support::app::TestApp,
    };

    use super::*;

    /// The application the binary builds, over the headless engine and an in-memory store.
    fn headless() -> (TestApp, Rc<Composition>) {
        let mut app = TestApp::new();
        let runtime = app.app().runtime_handle().clone();
        let composition = Composition::with_store(
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
        let composition = Composition::with_store(app.app_mut(), runtime, store, None)
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
}

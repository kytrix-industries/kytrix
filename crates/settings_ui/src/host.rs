//! The collaborators the settings page is handed instead of reading process globals.
//!
//! The page is an application-layer view, so the things it cannot own are assembled by the
//! composition root and passed in:
//!
//! - [`WidgetServices`] — the translation context, the language signal, the settings snapshot, the
//!   theme and the three application-level registries kit components require (the layer stack, the
//!   frame router and the animation router). It is the aggregate a widget is handed; the settings
//!   page is assembled from exactly the same services, so a root that already builds one has
//!   everything a host needs.
//! - [`SettingsStore`] — the settings snapshot signal plus the commit seam every write goes
//!   through. The aggregate carries a snapshot for *reading*; storing a change is the application
//!   root's persistence, which it hands over as this seam (see [`crate::session`]).
//! - the process-wide translation downloads, when the build has them.
//!
//! Nothing here is installed process-wide and nothing is looked up by type, so a test composes the
//! same aggregate the application does (see `crate::settings_tests::test_support`).

use std::rc::Rc;

use app_config::settings::Config;
use base_ui::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::handle::ThemeHandle,
};
use i18n::{language::codes::LanguageCode, translate::Translate};
use ui_core::{app::App, window::WindowId};
use widget::{frame::FrameRouter, services::WidgetServices};

use crate::{
    session::{SettingsChange, SettingsStore, SettingsWriteError},
    translation_downloads::TranslationDownloads,
};

/// Everything one settings page is handed by the composition root.
///
/// Cheap to clone: it carries an `Rc<WidgetServices>` and the store's `Rc`, so a control's callback
/// keeps the collaborators it drew with after the caller drops its own copy.
#[derive(Clone)]
pub struct SettingsHost {
    services: Rc<WidgetServices>,
    store: SettingsStore,
    downloads: Option<Rc<TranslationDownloads>>,
}

impl SettingsHost {
    /// Assembles a host from the frozen services and the settings session.
    ///
    /// The three kit singletons the page builds through are the ones inside `services`
    /// ([`WidgetServices::layer`], [`WidgetServices::frame_router`],
    /// [`WidgetServices::animation_router`]): the page never builds a second layer stack / frame
    /// router / animation router, because the engine has one slot for each and the composition root
    /// already owns it.
    #[must_use]
    pub fn new(services: Rc<WidgetServices>, store: SettingsStore) -> Self {
        Self {
            services,
            store,
            downloads: None,
        }
    }

    /// Attaches the process-wide translation downloads.
    #[must_use]
    pub fn with_downloads(mut self, downloads: Rc<TranslationDownloads>) -> Self {
        self.downloads = Some(downloads);
        self
    }

    /// The injected services.
    #[must_use]
    pub fn services(&self) -> &WidgetServices {
        &self.services
    }

    /// The injected services, as the shared handle the host was built from.
    #[must_use]
    pub fn services_rc(&self) -> Rc<WidgetServices> {
        Rc::clone(&self.services)
    }

    /// The translation context every interface text is rendered through.
    #[must_use]
    pub fn translate(&self) -> &Translate {
        &self.services.translate
    }

    /// The theme handle; colours are read through it inside `bind_style` closures.
    #[must_use]
    pub fn theme(&self) -> &ThemeHandle {
        &self.services.theme
    }

    /// The theme handle as a value, for a component that keeps one.
    #[must_use]
    pub fn theme_handle(&self) -> ThemeHandle {
        self.services.theme.clone()
    }

    /// The selected language, as the composition root last wrote it.
    ///
    /// Reading the signal is what registers a `bind_*` closure's dependency on it; a page that
    /// rebuilds its text compares it in `sync` (see `crate::pages::page`).
    #[must_use]
    pub fn language(&self, app: &App) -> LanguageCode {
        *app.read(&self.services.language)
    }

    /// Runs `read` with a borrowed view of the current settings snapshot.
    pub fn config<R>(&self, app: &App, read: impl FnOnce(&Config) -> R) -> R {
        app.read_with(&self.services.config, read)
    }

    /// The settings session.
    #[must_use]
    pub fn store(&self) -> &SettingsStore {
        &self.store
    }

    /// The process-wide translation downloads, when this build has them.
    #[must_use]
    pub fn downloads(&self) -> Option<&Rc<TranslationDownloads>> {
        self.downloads.as_ref()
    }

    /// The application runtime.
    #[must_use]
    pub fn runtime(&self) -> tokio::runtime::Handle {
        self.services.runtime.clone()
    }

    /// The application's layer stack (the one the services carry).
    #[must_use]
    pub fn layer(&self) -> Layer {
        (*self.services.layer).clone()
    }

    /// The application's frame router (the one the services carry).
    #[must_use]
    pub fn router(&self) -> Rc<FrameRouter> {
        Rc::clone(&self.services.frame_router)
    }

    /// The application's animation completion router (the one the services carry).
    #[must_use]
    pub fn animation(&self) -> AnimationRouter {
        (*self.services.animation_router).clone()
    }

    /// Writes one change into the settings through the commit seam.
    ///
    /// # Errors
    /// The store refused to persist the change (the settings are then as they were).
    pub fn write(
        &self,
        app: &mut App,
        window: WindowId,
        change: SettingsChange,
    ) -> Result<(), SettingsWriteError> {
        self.store.write(app, window, change)
    }

    /// Writes a group of changes as one stored configuration.
    ///
    /// # Errors
    /// The store refused to persist the change (the settings are then as they were).
    pub fn write_all(
        &self,
        app: &mut App,
        window: WindowId,
        changes: impl IntoIterator<Item = SettingsChange>,
    ) -> Result<(), SettingsWriteError> {
        self.store.write_all(app, window, changes)
    }

    /// Takes the last write that could not be stored, so the page reports one reason once.
    pub fn take_failure(&self) -> Option<SettingsWriteError> {
        self.store.take_failure()
    }
}

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

#[derive(Clone)]
pub struct SettingsHost {
    services: Rc<WidgetServices>,
    store: SettingsStore,
    downloads: Option<Rc<TranslationDownloads>>,
}

impl SettingsHost {
    #[must_use]
    pub fn new(services: Rc<WidgetServices>, store: SettingsStore) -> Self {
        Self {
            services,
            store,
            downloads: None,
        }
    }

    #[must_use]
    pub fn with_downloads(mut self, downloads: Rc<TranslationDownloads>) -> Self {
        self.downloads = Some(downloads);
        self
    }

    #[must_use]
    pub fn services(&self) -> &WidgetServices {
        &self.services
    }

    #[must_use]
    pub fn services_rc(&self) -> Rc<WidgetServices> {
        Rc::clone(&self.services)
    }

    #[must_use]
    pub fn translate(&self) -> &Translate {
        &self.services.translate
    }

    #[must_use]
    pub fn theme(&self) -> &ThemeHandle {
        &self.services.theme
    }

    #[must_use]
    pub fn theme_handle(&self) -> ThemeHandle {
        self.services.theme.clone()
    }

    #[must_use]
    pub fn language(&self, app: &App) -> LanguageCode {
        *app.read(&self.services.language)
    }

    pub fn config<R>(&self, app: &App, read: impl FnOnce(&Config) -> R) -> R {
        app.read_with(&self.services.config, read)
    }

    #[must_use]
    pub fn store(&self) -> &SettingsStore {
        &self.store
    }

    #[must_use]
    pub fn downloads(&self) -> Option<&Rc<TranslationDownloads>> {
        self.downloads.as_ref()
    }

    #[must_use]
    pub fn runtime(&self) -> tokio::runtime::Handle {
        self.services.runtime.clone()
    }

    #[must_use]
    pub fn layer(&self) -> Layer {
        (*self.services.layer).clone()
    }

    #[must_use]
    pub fn router(&self) -> Rc<FrameRouter> {
        Rc::clone(&self.services.frame_router)
    }

    #[must_use]
    pub fn animation(&self) -> AnimationRouter {
        (*self.services.animation_router).clone()
    }

    pub fn write(
        &self,
        app: &mut App,
        window: WindowId,
        change: SettingsChange,
    ) -> Result<(), SettingsWriteError> {
        self.store.write(app, window, change)
    }

    pub fn write_all(
        &self,
        app: &mut App,
        window: WindowId,
        changes: impl IntoIterator<Item = SettingsChange>,
    ) -> Result<(), SettingsWriteError> {
        self.store.write_all(app, window, changes)
    }

    pub fn take_failure(&self) -> Option<SettingsWriteError> {
        self.store.take_failure()
    }
}

use std::{rc::Rc, sync::Arc};

use app_config::settings::Config;
use i18n::{language::codes::LanguageCode, translate::Translate};
use market_participants_connector::catalog::store::Catalog;
use time::UtcOffset;
use ui_core::app::App;
use ui_kit::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::handle::ThemeHandle,
};
use widget::{frame::FrameRouter, services::WidgetServices};

use crate::prediction_translation::LocalTranslator;

#[derive(Clone)]
pub struct SelectorHost {
    services: Rc<WidgetServices>,
    catalog: Option<Arc<Catalog>>,
    translator: Option<Arc<dyn LocalTranslator>>,
}

impl SelectorHost {
    #[must_use]
    pub fn new(services: Rc<WidgetServices>) -> Self {
        let catalog = services.market_participants().map(
            market_participants_connector::connector::MarketParticipantsConnector::catalog_arc,
        );
        Self {
            services,
            catalog,
            translator: None,
        }
    }

    #[must_use]
    pub fn with_catalog(mut self, catalog: Option<Arc<Catalog>>) -> Self {
        self.catalog = catalog;
        self
    }

    pub fn with_translator(mut self, translator: Option<Arc<dyn LocalTranslator>>) -> Self {
        self.translator = translator;
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
    pub fn catalog(&self) -> Option<&Catalog> {
        self.catalog.as_deref()
    }

    #[must_use]
    pub fn translator(&self) -> Option<Arc<dyn LocalTranslator>> {
        self.translator.clone()
    }

    #[must_use]
    pub fn layer(&self) -> &Layer {
        &self.services.layer
    }

    #[must_use]
    pub fn router(&self) -> &FrameRouter {
        &self.services.frame_router
    }

    #[must_use]
    pub fn animation(&self) -> AnimationRouter {
        (*self.services.animation_router).clone()
    }

    pub fn config_ref<R>(&self, app: &App, read: impl FnOnce(&Config) -> R) -> R {
        app.read_with(&self.services.config, read)
    }

    #[must_use]
    pub fn language(&self, app: &App) -> LanguageCode {
        *app.read(&self.services.language)
    }

    #[must_use]
    pub fn reduce_motion(&self, app: &App) -> bool {
        *app.read(&self.services.reduce_motion)
    }

    #[must_use]
    pub fn data_time_zone(&self, app: &App) -> UtcOffset {
        self.config_ref(app, widget::time_zone::data_time_zone)
    }
}

use std::{fmt, rc::Rc, sync::Arc};

use futures::future::BoxFuture;
use i18n::{language::codes::LanguageCode, translate::Translate};
use market_participants_connector::connector::{MarketParticipantsConnector, VenueLoadError};
use market_participants_model::identifier::VenueId;
use ui_core::app::App;
use ui_kit::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::handle::ThemeHandle,
};
use widget::services::WidgetServices;

pub trait VenueDirectory: Send + Sync + 'static {
    fn venues(&self) -> Vec<VenueId>;

    fn is_loaded(&self, venue: &VenueId) -> bool;

    fn needs_credentials(&self, venue: VenueId) -> bool;

    fn load(&self, venue: VenueId) -> BoxFuture<'static, Result<(), VenueLoadError>>;

    fn cancel_load(&self, venue: &VenueId);
}

impl VenueDirectory for MarketParticipantsConnector {
    fn venues(&self) -> Vec<VenueId> {
        self.supported_venues()
    }

    fn is_loaded(&self, venue: &VenueId) -> bool {
        self.venues().loaded(venue).is_some()
    }

    fn needs_credentials(&self, venue: VenueId) -> bool {
        self.load_needs_credentials(venue)
    }

    fn load(&self, venue: VenueId) -> BoxFuture<'static, Result<(), VenueLoadError>> {
        let connector = self.clone();
        Box::pin(connector.load_venue(venue))
    }

    fn cancel_load(&self, venue: &VenueId) {
        self.cancel_venue_load(venue);
    }
}

pub trait RuntimeExecutor: 'static {
    fn spawn(&self, task: BoxFuture<'static, ()>);
}

pub struct TokioExecutor {
    handle: tokio::runtime::Handle,
}

impl TokioExecutor {
    #[must_use]
    pub fn new(handle: tokio::runtime::Handle) -> Self {
        Self { handle }
    }
}

impl RuntimeExecutor for TokioExecutor {
    fn spawn(&self, task: BoxFuture<'static, ()>) {
        self.handle.spawn(task);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirectoryUnavailable;

impl fmt::Display for DirectoryUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the market participants menu was mounted without a market directory")
    }
}

impl std::error::Error for DirectoryUnavailable {}

#[derive(Clone)]
pub struct MarketParticipantsHost {
    services: Rc<WidgetServices>,
    directory: Arc<dyn VenueDirectory>,
    executor: Rc<dyn RuntimeExecutor>,
}

impl MarketParticipantsHost {
    #[must_use]
    pub fn new(services: Rc<WidgetServices>, directory: Arc<dyn VenueDirectory>) -> Self {
        let executor = Rc::new(TokioExecutor::new(services.runtime.clone()));
        Self {
            services,
            directory,
            executor,
        }
    }

    #[must_use]
    pub fn with_executor(mut self, executor: Rc<dyn RuntimeExecutor>) -> Self {
        self.executor = executor;
        self
    }

    pub fn from_services(services: Rc<WidgetServices>) -> Result<Self, DirectoryUnavailable> {
        let connector = services
            .market_participants()
            .cloned()
            .ok_or(DirectoryUnavailable)?;
        Ok(Self::new(services, Arc::new(connector)))
    }

    #[must_use]
    pub fn language_signal(&self) -> ui_core::node::signal::Signal<LanguageCode> {
        self.services.language
    }

    #[must_use]
    pub fn directory(&self) -> &dyn VenueDirectory {
        self.directory.as_ref()
    }

    #[must_use]
    pub fn directory_arc(&self) -> Arc<dyn VenueDirectory> {
        Arc::clone(&self.directory)
    }

    #[must_use]
    pub fn executor(&self) -> Rc<dyn RuntimeExecutor> {
        Rc::clone(&self.executor)
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
    pub fn language(&self, app: &App) -> LanguageCode {
        *app.read(&self.services.language)
    }

    #[must_use]
    pub fn layer(&self) -> &Layer {
        &self.services.layer
    }

    #[must_use]
    pub fn animation(&self) -> AnimationRouter {
        (*self.services.animation_router).clone()
    }
}

//! The collaborators the title bar's market menu is handed instead of reading process
//! globals.
//!
//! A title-bar control is an application-layer component, so the composition root assembles
//! what it renders with and passes it in:
//!
//! - [`WidgetServices`] — the frozen aggregate every widget is handed: the translation
//!   context, the language signal, the settings snapshot, the theme, the application runtime,
//!   and the three application-level registries kit components require (the layer stack, the
//!   frame router and the animation router). It is exactly what a widget's
//!   [`WidgetCx`](widget::cx::WidgetCx) already borrows, so the root that builds a pane's
//!   services has everything a menu host needs.
//! - [`VenueDirectory`] — the venue list and the one action that loads a venue. Production is
//!   the [`MarketParticipantsConnector`] the composition root installed; a headless test
//!   provides its own directory, because the connector's constructor still needs the old
//!   toolkit's runtime type (design §13.9 R-1; P5 removes it). The menu never talks to the
//!   connector directly, and never loads the directory itself.
//!
//! # Why the directory is a seam
//!
//! Every decision the menu makes is a decision over the directory: which venues the panel
//! lists, whether one is already loaded, whether its load needs credentials, and what one
//! load reports. The connector answers all four, but it cannot be built without the toolkit;
//! a headless test that had to build one would be testing the connector, not the menu. The
//! trait puts the same four questions in front of the menu, and the production implementation
//! is the four delegating calls below.
//!
//! # Assembling one
//!
//! ```text
//! // Composition root, once — the same services aggregate a widget host builds:
//! let host = MarketParticipantsHost::from_services(Rc::clone(&services))
//!     .expect("the application root always installs the market directory");
//! let mut menu = MarketParticipants::new(host, app.app_mut());
//! menu.build(app.app_mut(), window, title_bar_slot)?;
//!
//! // Each frame, the root's frame hook:
//! menu.sync(app.app_mut())?;
//!
//! // When the title bar goes away:
//! app.remove_node(window, trigger_node)?;
//! menu.release(app.app_mut());
//! ```
//!
//! The menu builds one trigger node in the caller's title bar; the panel and any alert are
//! overlays it owns and releases itself (see [`crate::menu`]).

use std::{fmt, rc::Rc, sync::Arc};

use base_ui::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::handle::ThemeHandle,
};
use futures::future::BoxFuture;
use i18n::{language::codes::LanguageCode, translate::Translate};
use market_participants_connector::connector::{MarketParticipantsConnector, VenueLoadError};
use market_participants_model::identifier::VenueId;
use ui_core::app::App;
use widget::services::WidgetServices;

/// What the menu asks of the market directory.
///
/// The four questions are the whole of the menu's knowledge of the markets: which venues this
/// build speaks, whether one is loaded, whether its load is gated on credentials, and the load
/// itself. A cancelled load is reported through [`VenueDirectory::cancel_load`]; the directory
/// answers the cancelled future with its own typed failure.
pub trait VenueDirectory: Send + Sync + 'static {
    /// Every venue this build speaks, in declaration order.
    fn venues(&self) -> Vec<VenueId>;

    /// Whether `venue`'s scopes, catalogs and metadata have all committed.
    fn is_loaded(&self, venue: &VenueId) -> bool;

    /// Whether loading `venue` would leave it unreadable for want of credentials.
    fn needs_credentials(&self, venue: VenueId) -> bool;

    /// Loads `venue`, reporting what the load did.
    ///
    /// The future is driven on the application runtime by the menu; cancelling it through
    /// [`VenueDirectory::cancel_load`] settles it with the directory's own cancelled answer
    /// rather than dropping it silently.
    fn load(&self, venue: VenueId) -> BoxFuture<'static, Result<(), VenueLoadError>>;

    /// Cancels the load of `venue` in flight, if there is one.
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

/// Drives one load future on the application's runtime.
///
/// The menu never calls `tokio::spawn` from the frame thread (the frame thread is not inside
/// the runtime), so the future it builds is handed to the application's runtime through this
/// seam. Production is the engine's runtime handle
/// ([`TokioExecutor`]); a headless host pumps the future itself, because the engine's test
/// host owns a current-thread runtime nothing polls.
pub trait RuntimeExecutor: 'static {
    /// Runs `task` to completion on the application's runtime.
    fn spawn(&self, task: BoxFuture<'static, ()>);
}

/// The production executor: the application runtime the services carry.
pub struct TokioExecutor {
    handle: tokio::runtime::Handle,
}

impl TokioExecutor {
    /// Wraps the application runtime handle.
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

/// The market directory the application never installed.
///
/// The application root always builds the directory before it mounts the title bar, so this is
/// a wiring error rather than a state a user can see: the root logs it and the menu is not
/// mounted. It is a strong type instead of a string so the root can say what is missing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirectoryUnavailable;

impl fmt::Display for DirectoryUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the market participants menu was mounted without a market directory")
    }
}

impl std::error::Error for DirectoryUnavailable {}

/// Everything one market menu is handed by the composition root.
///
/// The host is `Clone` and carries an `Rc<WidgetServices>` plus an `Arc<dyn VenueDirectory>`,
/// so a menu keeps the collaborators it was built with even after the caller drops its own
/// copy; nothing here is copied per frame.
#[derive(Clone)]
pub struct MarketParticipantsHost {
    /// The frozen widget services: translation, language, settings, theme, the three
    /// composition-root registries and the application runtime.
    services: Rc<WidgetServices>,
    /// The directory the panel lists and loads through.
    directory: Arc<dyn VenueDirectory>,
    /// The runtime seam every load future is handed to.
    executor: Rc<dyn RuntimeExecutor>,
}

impl MarketParticipantsHost {
    /// Assembles a host from the injected services and the directory it lists.
    ///
    /// The production root uses [`Self::from_services`] instead: it already installed the
    /// connector into the services aggregate the widgets are handed. Loads run on the
    /// services' runtime.
    #[must_use]
    pub fn new(services: Rc<WidgetServices>, directory: Arc<dyn VenueDirectory>) -> Self {
        let executor = Rc::new(TokioExecutor::new(services.runtime.clone()));
        Self {
            services,
            directory,
            executor,
        }
    }

    /// Replaces the runtime seam the load futures are handed to.
    ///
    /// The production path never calls this — [`Self::new`] uses the application runtime. It
    /// exists so a headless test, whose engine host owns a current-thread runtime nothing
    /// polls, can pump the load futures itself.
    #[must_use]
    pub fn with_executor(mut self, executor: Rc<dyn RuntimeExecutor>) -> Self {
        self.executor = executor;
        self
    }

    /// Assembles a host around the directory the services carry.
    ///
    /// # Errors
    /// [`DirectoryUnavailable`] when the services have no market directory: the application
    /// root always installs one, so this is a wiring error, not a state the menu can show.
    pub fn from_services(services: Rc<WidgetServices>) -> Result<Self, DirectoryUnavailable> {
        let connector = services
            .market_participants()
            .cloned()
            .ok_or(DirectoryUnavailable)?;
        Ok(Self::new(services, Arc::new(connector)))
    }

    /// The language signal a `bind_text` closure reads to follow a language switch.
    #[must_use]
    pub fn language_signal(&self) -> ui_core::node::signal::Signal<LanguageCode> {
        self.services.language
    }

    /// The directory the panel lists and loads through.
    #[must_use]
    pub fn directory(&self) -> &dyn VenueDirectory {
        self.directory.as_ref()
    }

    /// The directory, as the shared handle a load future may capture.
    #[must_use]
    pub fn directory_arc(&self) -> Arc<dyn VenueDirectory> {
        Arc::clone(&self.directory)
    }

    /// The runtime seam a load future is handed to.
    #[must_use]
    pub fn executor(&self) -> Rc<dyn RuntimeExecutor> {
        Rc::clone(&self.executor)
    }

    /// The translation context every interface text is rendered through.
    #[must_use]
    pub fn translate(&self) -> &Translate {
        &self.services.translate
    }

    /// The theme handle; colours are read through it inside `bind_*` closures.
    #[must_use]
    pub fn theme(&self) -> &ThemeHandle {
        &self.services.theme
    }

    /// The selected language, as the composition root last wrote it.
    #[must_use]
    pub fn language(&self, app: &App) -> LanguageCode {
        *app.read(&self.services.language)
    }

    /// The application's layer stack (the one the services carry).
    #[must_use]
    pub fn layer(&self) -> &Layer {
        &self.services.layer
    }

    /// The application's animation completion router (the one the services carry).
    #[must_use]
    pub fn animation(&self) -> AnimationRouter {
        (*self.services.animation_router).clone()
    }
}

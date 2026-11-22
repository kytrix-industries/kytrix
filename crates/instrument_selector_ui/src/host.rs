//! The collaborators a selector is handed instead of reading process globals.
//!
//! A selector is an application-layer dialog, so the four things it cannot own are
//! assembled by the composition root and passed in:
//!
//! - [`WidgetServices`] — the translation context, the language signal, the settings
//!   snapshot, the theme, the market directory and the tokio runtime, exactly the
//!   aggregate a widget is handed (frozen in P3-1);
//! - the market catalogue, captured from the directory root once per host so a dialog
//!   that outlives a directory commit keeps reading the *same* catalogue;
//! - the local translation engine, when the build has one
//!   ([`prediction_translation::LocalTranslator`]);
//! - the three application-level registries kit components require: the layer stack,
//!   the frame router and the animation router. They are **not** part of
//!   `WidgetServices`; a dialog is not a widget and kit's own contract says these are
//!   the composition root's singletons (see `kit::layer` / `kit::overlay`).
//!
//! The host is `Clone` and carries only `Rc`-shared handles, so a dialog keeps the
//! services it opened with even after the caller drops its own copy.
//!
//! # Why the catalogue is a separate field
//!
//! A headless test cannot build a [`MarketParticipantsConnector`] — its constructor
//! needs the old toolkit's runtime type — but every selector decision is a decision
//! over the *catalogue*: which listings a route publishes, which name the published
//! generation states, which aliases it carries. Injecting the catalogue the root
//! reads makes those decisions testable without a connector, and the production
//! constructor derives the same handle from `services.market_participants`.
//!
//! [`prediction_translation::LocalTranslator`]: crate::prediction_translation::LocalTranslator

use std::{rc::Rc, sync::Arc};

use app_config::settings::Config;
use base_ui::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::handle::ThemeHandle,
};
use i18n::{language::codes::LanguageCode, translate::Translate};
use market_participants_connector::catalog::store::Catalog;
use time::UtcOffset;
use ui_core::app::App;
use widget::{frame::FrameRouter, services::WidgetServices};

use crate::prediction_translation::LocalTranslator;

/// Everything one selector dialog is handed by the composition root.
///
/// See the module documentation for why each handle is a field.
#[derive(Clone)]
pub struct SelectorHost {
    /// The frozen widget services: translation, language, settings, theme, directory,
    /// reduced-motion and the tokio runtime.
    services: Rc<WidgetServices>,
    /// The market catalogue the directory root owns, when the host has a directory.
    catalog: Option<Arc<Catalog>>,
    /// The local model engine, when this build has one.
    translator: Option<Arc<dyn LocalTranslator>>,
    /// The application's layer stack (kit singleton).
    layer: Layer,
    /// The application's frame router (kit singleton; owns the engine's one hook).
    router: FrameRouter,
    /// The application's animation completion router (kit singleton).
    animation: AnimationRouter,
}

impl SelectorHost {
    /// Assembles a host from the frozen services and the three kit singletons.
    ///
    /// The catalogue is derived from `services.market_participants` — the directory
    /// root the application installed — and the translation engine starts absent; the
    /// composition root attaches the one its build has with [`Self::with_translator`].
    #[must_use]
    pub fn new(
        services: Rc<WidgetServices>,
        layer: Layer,
        router: FrameRouter,
        animation: AnimationRouter,
    ) -> Self {
        let catalog = services.market_participants().map(
            market_participants_connector::connector::MarketParticipantsConnector::catalog_arc,
        );
        Self {
            services,
            catalog,
            translator: None,
            layer,
            router,
            animation,
        }
    }

    /// Replaces the catalogue a dialog reads.
    ///
    /// The production path never calls this — [`Self::new`] derives it. It exists so a
    /// headless test can hand a catalogue it published itself to a host whose services
    /// have no directory root (the root's constructor needs the old toolkit).
    #[must_use]
    pub fn with_catalog(mut self, catalog: Option<Arc<Catalog>>) -> Self {
        self.catalog = catalog;
        self
    }

    /// Installs the local translation engine the prediction dialog translates through.
    #[must_use]
    pub fn with_translator(mut self, translator: Option<Arc<dyn LocalTranslator>>) -> Self {
        self.translator = translator;
        self
    }

    /// The injected services.
    #[must_use]
    pub fn services(&self) -> &WidgetServices {
        &self.services
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

    /// The market catalogue, when the host has a directory.
    #[must_use]
    pub fn catalog(&self) -> Option<&Catalog> {
        self.catalog.as_deref()
    }

    /// The local translation engine, when this build has one.
    #[must_use]
    pub fn translator(&self) -> Option<Arc<dyn LocalTranslator>> {
        self.translator.clone()
    }

    /// The application's layer stack.
    #[must_use]
    pub fn layer(&self) -> &Layer {
        &self.layer
    }

    /// The application's frame router.
    #[must_use]
    pub fn router(&self) -> &FrameRouter {
        &self.router
    }

    /// The application's animation completion router.
    #[must_use]
    pub fn animation(&self) -> AnimationRouter {
        self.animation.clone()
    }

    /// Runs `read` with a borrowed view of the current settings snapshot.
    pub fn config_ref<R>(&self, app: &App, read: impl FnOnce(&Config) -> R) -> R {
        app.read_with(&self.services.config, read)
    }

    /// The selected language, as the composition root last wrote it.
    #[must_use]
    pub fn language(&self, app: &App) -> LanguageCode {
        *app.read(&self.services.language)
    }

    /// Whether the application asks for reduced motion.
    #[must_use]
    pub fn reduce_motion(&self, app: &App) -> bool {
        *app.read(&self.services.reduce_motion)
    }

    /// The display time zone the settings currently select.
    #[must_use]
    pub fn data_time_zone(&self, app: &App) -> UtcOffset {
        self.config_ref(app, widget::time_zone::data_time_zone)
    }
}

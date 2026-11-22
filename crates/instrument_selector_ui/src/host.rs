//! The collaborators a selector is handed instead of reading process globals.
//!
//! A selector is an application-layer dialog, so the three things it cannot own are
//! assembled by the composition root and passed in:
//!
//! - [`WidgetServices`] — the translation context, the language signal, the settings
//!   snapshot, the theme, the market directory, the tokio runtime **and the three
//!   application-level registries kit components require** (the layer stack, the frame
//!   router and the animation router). It is the aggregate a widget is handed, frozen in
//!   P3-1d/P3-1e; a selector is a dialog, not a widget, but it is assembled from exactly
//!   the same services, so a pane that has a [`WidgetCx`](widget::cx::WidgetCx) already has
//!   everything a host needs.
//! - the market catalogue, captured from the directory root once per host so a dialog
//!   that outlives a directory commit keeps reading the *same* catalogue;
//! - the local translation engine, when the build has one
//!   ([`prediction_translation::LocalTranslator`]).
//!
//! The host is `Clone` and carries only an `Rc<WidgetServices>` (the services themselves
//! hold `Rc`-shared handles), so a dialog keeps the services it opened with even after the
//! caller drops its own copy, and nothing here is copied per dialog.
//!
//! # Assembling one
//!
//! The composition root builds the services once (the same aggregate the widgets are
//! handed) and wraps it. The host needs nothing else: the catalogue comes from the
//! services' directory root and the translator is attached by the root.
//!
//! ```text
//! // Composition root, once — the same builder a widget host uses:
//! let services = Rc::new(
//!     WidgetServices::builder(app, runtime, translate, theme, layer, frame_router, animation_router)
//!         .language(language)
//!         .config(config)
//!         .build(),
//! );
//! let host = SelectorHost::new(Rc::clone(&services))
//!     .with_translator(Some(Arc::new(ai_translator)));   // the build's model engine, if any
//!
//! // Any pane that draws the control hands it that host:
//! InstrumentButton::new(key, selection, filters, placeholder, on_confirm)
//!     .build(&host, app, window, header_slot)?;
//! ```
//!
//! The `Rc<WidgetServices>` is the only handle a selector needs, and it is exactly what
//! the composition root already owns; a widget that renders through a
//! [`WidgetCx`](widget::cx::WidgetCx) borrows the same aggregate
//! ([`WidgetCx::services`](widget::cx::WidgetCx::services)) and receives the host from
//! the root that built it.
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
/// See the module documentation for why each handle is a field and for the assembly
/// example.
#[derive(Clone)]
pub struct SelectorHost {
    /// The frozen widget services: translation, language, settings, theme, directory,
    /// reduced motion, the tokio runtime and the three composition-root registries
    /// (layer stack, frame router, animation router).
    services: Rc<WidgetServices>,
    /// The market catalogue the directory root owns, when the host has a directory.
    catalog: Option<Arc<Catalog>>,
    /// The local model engine, when this build has one.
    translator: Option<Arc<dyn LocalTranslator>>,
}

impl SelectorHost {
    /// Assembles a host from the frozen services.
    ///
    /// The three kit singletons the dialogs build through are the ones inside
    /// `services` ([`WidgetServices::layer`], [`WidgetServices::frame_router`],
    /// [`WidgetServices::animation_router`]): a selector never builds a second layer
    /// stack / frame router / animation router, because the engine has one slot for each
    /// and the composition root already owns it (see `crates/widgets/AGENTS.md` §5/§9).
    ///
    /// The catalogue is derived from `services.market_participants` — the directory root
    /// the application installed — and the translation engine starts absent; the
    /// composition root attaches the one its build has with [`Self::with_translator`].
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
    pub fn with_translator(mut self, translator: Option<Arc<dyn LocalTranslator>>) -> Self {
        self.translator = translator;
        self
    }

    /// The injected services.
    #[must_use]
    pub fn services(&self) -> &WidgetServices {
        &self.services
    }

    /// The injected services, as the shared handle the host was built from.
    ///
    /// A widget that opens a selector from its own context wraps exactly the handle it
    /// was handed, so the dialog reads the same services the widget renders with.
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

    /// The application's layer stack (the one the services carry).
    #[must_use]
    pub fn layer(&self) -> &Layer {
        &self.services.layer
    }

    /// The application's frame router (the one the services carry).
    #[must_use]
    pub fn router(&self) -> &FrameRouter {
        &self.services.frame_router
    }

    /// The application's animation completion router (the one the services carry).
    #[must_use]
    pub fn animation(&self) -> AnimationRouter {
        (*self.services.animation_router).clone()
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

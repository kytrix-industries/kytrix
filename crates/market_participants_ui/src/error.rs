use std::fmt;

use i18n::translate::Translate;
use market_participants_connector::{
    catalog::error::CatalogError, connector::VenueLoadError, error::MarketParticipantError,
};
use market_participants_model::display::lexical::scope_display_code;
use smol_str::SmolStr;
use ui_core::error::EngineError;
use ui_kit::kit::{brand::BrandError, dialog::DialogError, icon::IconError, overlay::OverlayError};
use widget::{error::TranslatedError, venue::VenueDisplay};

#[derive(Debug)]
pub enum MenuError {
    Engine(EngineError),
    Overlay(OverlayError),
    Dialog(DialogError),
    Icon(IconError),
    Brand(BrandError),
}

impl From<EngineError> for MenuError {
    fn from(error: EngineError) -> Self {
        Self::Engine(error)
    }
}

impl From<OverlayError> for MenuError {
    fn from(error: OverlayError) -> Self {
        Self::Overlay(error)
    }
}

impl From<DialogError> for MenuError {
    fn from(error: DialogError) -> Self {
        Self::Dialog(error)
    }
}

impl From<IconError> for MenuError {
    fn from(error: IconError) -> Self {
        Self::Icon(error)
    }
}

impl From<BrandError> for MenuError {
    fn from(error: BrandError) -> Self {
        Self::Brand(error)
    }
}

impl fmt::Display for MenuError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Engine(error) => write!(formatter, "engine error: {error}"),
            Self::Overlay(error) => write!(formatter, "panel overlay error: {error}"),
            Self::Dialog(error) => write!(formatter, "alert dialog error: {error}"),
            Self::Icon(error) => write!(formatter, "menu icon error: {error}"),
            Self::Brand(error) => write!(formatter, "venue brand error: {error}"),
        }
    }
}

impl std::error::Error for MenuError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Engine(error) => Some(error),
            Self::Overlay(error) => Some(error),
            Self::Dialog(error) => Some(error),
            Self::Icon(error) => Some(error),
            Self::Brand(error) => Some(error),
        }
    }
}

#[derive(Debug)]
pub struct VenueLoadFailure {
    error: VenueLoadError,
}

impl VenueLoadFailure {
    #[must_use]
    pub const fn new(error: VenueLoadError) -> Self {
        Self { error }
    }
}

impl TranslatedError for VenueLoadFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match &self.error {
            VenueLoadError::Discovery { venue, source } => i18n::t_smol!(
                translate,
                "error.venue_load.discovery",
                market = venue.display_string(translate),
                cause = source.as_ref().translated_message(translate)
            ),
            VenueLoadError::DiscoveryUnverifiable { venue, dex } => i18n::t_smol!(
                translate,
                "error.venue_load.discovery_unverifiable",
                market = venue.display_string(translate),
                dex = dex.as_str()
            ),
            VenueLoadError::DiscoveryLimitExceeded {
                venue,
                named,
                limit,
            } => i18n::t_smol!(
                translate,
                "error.venue_load.discovery_limit_exceeded",
                market = venue.display_string(translate),
                named = *named,
                limit = *limit
            ),
            VenueLoadError::Registration { venue, .. } => i18n::t_smol!(
                translate,
                "error.venue_load.registration",
                market = venue.display_string(translate)
            ),
            VenueLoadError::Metadata { venue, source, .. } => i18n::t_smol!(
                translate,
                "error.venue_load.metadata",
                market = venue.display_string(translate),
                cause = source.as_ref().translated_message(translate)
            ),
            VenueLoadError::Catalog {
                venue,
                scope,
                source,
            } => match source.as_ref() {
                MarketParticipantError::Catalog(error)
                    if matches!(error.as_ref(), CatalogError::RouteRevisionChanged { .. }) =>
                {
                    i18n::t_smol!(
                        translate,
                        "error.venue_load.revision_changed",
                        market = venue.display_string(translate),
                        scope = scope_display_code(scope)
                    )
                }
                cause => i18n::t_smol!(
                    translate,
                    "error.venue_load.catalog",
                    market = venue.display_string(translate),
                    scope = scope_display_code(scope),
                    cause = cause.translated_message(translate)
                ),
            },
            VenueLoadError::CatalogValidation {
                venue,
                scope,
                error,
            } => i18n::t_smol!(
                translate,
                "error.venue_load.catalog_validation",
                market = venue.display_string(translate),
                scope = scope_display_code(scope),
                cause = error.translated_message(translate)
            ),
            VenueLoadError::RevisionChanged { venue, scope } => i18n::t_smol!(
                translate,
                "error.venue_load.revision_changed",
                market = venue.display_string(translate),
                scope = scope_display_code(scope)
            ),
            VenueLoadError::Interrupted { venue } => i18n::t_smol!(
                translate,
                "error.venue_load.interrupted",
                market = venue.display_string(translate)
            ),
            VenueLoadError::UnsupportedVenue { venue } => i18n::t_smol!(
                translate,
                "error.venue_load.unsupported_venue",
                market = venue.display_string(translate)
            ),
        }
    }
}

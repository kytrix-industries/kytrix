//! Strongly typed failures: the engine errors the menu propagates, and the one failure a
//! user reads.
//!
//! [`MenuError`] is the internal error face: a node that could not be built, an overlay that
//! could not be opened, a dialog whose content failed. It is never user-visible — the caller
//! logs it — and it exists so no string error is raised for a framework failure.
//!
//! [`VenueLoadFailure`] is the user-facing face of a load that failed. The load answers a
//! typed [`VenueLoadError`]; this wrapper states it in the language the interface is showing,
//! through `widget::error::TranslatedError`, so the alert a row's failure raises reads the same
//! in every language and names the venue and the scope the failure is about.

use std::fmt;

use base_ui::kit::{
    brand::BrandError, dialog::DialogError, icon::IconError, overlay::OverlayError,
};
use i18n::translate::Translate;
use market_participants_connector::{
    catalog::error::CatalogError, connector::VenueLoadError, error::MarketParticipantError,
};
use market_participants_model::display::lexical::scope_display_code;
use smol_str::SmolStr;
use ui_core::error::EngineError;
use widget::{error::TranslatedError, venue::VenueDisplay};

/// Why one frame of the menu could not be built or synchronised.
///
/// The variants are the kit component layers the menu is assembled from: the engine's own
/// error, the anchored overlay, the dialog that reports a failure, and the icon / brand
/// assets the rows are drawn with. Each of them is a strong type of its own — the menu does
/// not flatten any of them into a sentence.
#[derive(Debug)]
pub enum MenuError {
    /// The engine failed to build or find a node / window.
    Engine(EngineError),
    /// The panel overlay could not open or place itself.
    Overlay(OverlayError),
    /// The alert dialog could not open or contain its content.
    Dialog(DialogError),
    /// An icon asset could not be mounted.
    Icon(IconError),
    /// A brand logo could not be mounted.
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

/// The failure of one venue load, as the alert reports it.
///
/// The typed [`VenueLoadError`] names the stage that failed and, where a stage is per scope,
/// the scope it failed on; this wrapper is the one place that failure becomes text, so the
/// row and the alert cannot state two different things about one load.
#[derive(Debug)]
pub struct VenueLoadFailure {
    /// The failure the directory answered.
    error: VenueLoadError,
}

impl VenueLoadFailure {
    /// Wraps one typed load failure.
    #[must_use]
    pub const fn new(error: VenueLoadError) -> Self {
        Self { error }
    }
}

impl TranslatedError for VenueLoadFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match &self.error {
            // Each variant of a load's failure is a sentence of its own: the stage that
            // failed is what the alert has to report, and a scope-carrying stage names the
            // scope it failed on.
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
            // The metadata stage is a venue-level sentence in the error type's own words —
            // its `Display` names the venue and not the scope — so the alert states the same
            // thing the typed failure does. The scope the variant carries is a caller's
            // answer to act on, not part of what the reader is told.
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
                // A catalog that moved under its fetch is reported as the moved catalog it
                // is — the venue's own stage answer — rather than as a read that failed:
                // what the reader has to do is load the market again.
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
            // A cancelled load is the user's own doing, and the alert says so without
            // dressing it up as a failure of the venue.
            VenueLoadError::Interrupted { venue } => i18n::t_smol!(
                translate,
                "error.venue_load.interrupted",
                market = venue.display_string(translate)
            ),
            // A venue the definition table answers no definition for is one this menu draws
            // no row for, so this is a load started against a venue the menu was never able
            // to offer: the sentence names the venue rather than reporting a stage that never
            // ran.
            VenueLoadError::UnsupportedVenue { venue } => i18n::t_smol!(
                translate,
                "error.venue_load.unsupported_venue",
                market = venue.display_string(translate)
            ),
        }
    }
}

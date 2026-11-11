use market_participants_model::identifier::VenueId;
use smol_str::SmolStr;
use thiserror::Error;

/// A structural error in persisted market-participant settings.
///
/// The sentence names the venue — and, when the venue configures its markets apart,
/// the scope whose credential set is at fault — in canonical identity spellings. It
/// is a diagnostic: a caller that reaches a user maps this to a typed failure and
/// states the venue through the catalogue instead.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum MarketParticipantsConfigError {
    /// A credential set is stored but only partly filled.
    ///
    /// The set is one market's where the venue configures its markets apart — Binance's spot
    /// or USDⓈ-M perpetual set — and the venue's whole surface where it authenticates with one
    /// set, which is every other venue. Either way it is refused wherever it would sign a
    /// request, so a half-filled set never reaches a venue.
    #[error(
        "the credentials stored for venue {} are incomplete{}",
        .venue.as_str(),
        .scope.as_ref().map_or_else(String::new, |scope| format!(" for scope {scope}"))
    )]
    IncompleteCredentials {
        venue: VenueId,
        /// The scope whose credential set is at fault, for a venue that configures
        /// its markets apart.
        scope: Option<SmolStr>,
    },
}

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
    /// The credential set configured for one market is only partly filled.
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
    /// The stored credential set is another venue's shape.
    ///
    /// A credential set is a value of exactly one venue's shape, so this is not a
    /// set to finish filling in: it is one that can never authenticate the venue it
    /// was found under.
    #[error(
        "the credentials stored for venue {} are not that venue's shape{}",
        .venue.as_str(),
        .scope.as_ref().map_or_else(String::new, |scope| format!(" for scope {scope}"))
    )]
    MismatchedShape {
        venue: VenueId,
        scope: Option<SmolStr>,
    },
}

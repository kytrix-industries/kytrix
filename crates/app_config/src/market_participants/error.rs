use market_participants_model::identifier::VenueId;
use smol_str::SmolStr;
use thiserror::Error;

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum MarketParticipantsConfigError {
    #[error(
        "the credentials stored for venue {} are incomplete{}",
        .venue.as_str(),
        .scope.as_ref().map_or_else(String::new, |scope| format!(" for scope {scope}"))
    )]
    IncompleteCredentials {
        venue: VenueId,
        scope: Option<SmolStr>,
    },
}

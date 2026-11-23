use std::sync::Arc;

use market_participants_model::{
    error::ModelError,
    listing::route::{VenueListingKey, VenueRoute},
};
use thiserror::Error;

use crate::catalog::error::{CatalogError, SourceError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountNetworkError(Arc<crate::error::MarketParticipantError>);

impl AccountNetworkError {
    pub fn is_retryable(&self) -> bool {
        self.0.is_retryable()
    }

    pub fn error(&self) -> &crate::error::MarketParticipantError {
        &self.0
    }
}

impl std::fmt::Display for AccountNetworkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for AccountNetworkError {}

pub(crate) fn network_error(error: crate::error::MarketParticipantError) -> AccountNetworkError {
    AccountNetworkError(Arc::new(error))
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum AccountCatalogError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error("account catalog network request failed: {0}")]
    Network(#[source] AccountNetworkError),
    #[error("the catalog reported: {0}")]
    Directory(Box<CatalogError>),
    #[error("listing {listing:?} is not published on route {route}")]
    ListingNotFound {
        route: VenueRoute,
        listing: Box<VenueListingKey>,
    },
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum AccountError {
    #[error("account network request failed: {0}")]
    Network(#[source] AccountNetworkError),
    #[error("account market metadata request failed: {0}")]
    Catalog(#[source] AccountCatalogError),
    #[error("account value was rejected by the market-participants model: {0}")]
    Model(#[source] ModelError),
    #[error("the asset {asset} is not published on route {route}")]
    UnknownAsset {
        route: VenueRoute,
        asset: Box<market_participants_model::asset::AssetId>,
    },
    #[error("listing {listing:?} does not belong to route {route}")]
    ListingRouteMismatch {
        listing: Box<VenueListingKey>,
        route: VenueRoute,
    },
    #[error("account protocol returned an invalid response while performing {operation}")]
    Protocol { operation: &'static str },
}

pub(crate) fn account_error_from_market_participant(
    error: crate::error::MarketParticipantError,
) -> AccountError {
    AccountError::Network(network_error(error))
}

pub(crate) fn account_error_from_catalog(error: CatalogError) -> AccountError {
    match error {
        CatalogError::Source(error) => {
            AccountError::Catalog(AccountCatalogError::Source((*error).clone()))
        }
        error => AccountError::Catalog(AccountCatalogError::Directory(Box::new(error))),
    }
}

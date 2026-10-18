use std::sync::Arc;

use market_participants_model::{
    error::ModelError,
    listing::route::{VenueListingKey, VenueRoute},
};
use thiserror::Error;

use crate::catalog::error::{CatalogError, SourceError};

/// Network failure returned by an account operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountNetworkError(Arc<crate::error::MarketParticipantError>);

impl AccountNetworkError {
    pub fn is_retryable(&self) -> bool {
        self.0.is_retryable()
    }

    /// Returns the classified market-participant failure behind this account error.
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

/// Account-owned failures while resolving a listing or one of its values.
///
/// The catalog implementation and its market-participant error remain private to the
/// market-participants runtime. Only account-facing context crosses this boundary.
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

/// Strongly typed failures for account capability operations.
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
    /// The account query named a listing of another route.
    ///
    /// The listing and the route stay typed diagnostics: an unpublished or
    /// misaddressed listing never had a display name, so a message about it is the
    /// identity diagnostic a protocol failure is reported by.
    #[error("listing {listing:?} does not belong to route {route}")]
    ListingRouteMismatch {
        listing: Box<VenueListingKey>,
        route: VenueRoute,
    },
    /// The account protocol answered an operation with a response the adapter
    /// cannot read.
    ///
    /// The operation is a fixed name of the account capability and never a reason:
    /// the field is typed so a formatted explanation — which would carry a raw
    /// route or listing id with it — cannot be passed here at all.
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

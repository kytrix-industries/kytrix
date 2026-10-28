use std::sync::Arc;

use market_participants_model::{
    error::ModelError,
    listing::{
        listing_impl::ListingStatus,
        route::{VenueListingKey, VenueRoute},
    },
    routing::CapabilityId,
};
use smol_str::SmolStr;
use thiserror::Error;

use super::domain::DisconnectReason;
use crate::{
    catalog::error::{CatalogError, SourceError},
    error::MarketParticipantError,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketNetworkError(Arc<MarketParticipantError>);

impl MarketNetworkError {
    pub fn is_retryable(&self) -> bool {
        self.0.is_retryable()
    }

    /// Returns the classified market-participant failure behind this market error.
    pub fn error(&self) -> &MarketParticipantError {
        &self.0
    }
}

impl std::fmt::Display for MarketNetworkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for MarketNetworkError {}

impl From<MarketParticipantError> for MarketNetworkError {
    fn from(error: MarketParticipantError) -> Self {
        Self(Arc::new(error))
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum MarketError {
    #[error(transparent)]
    CatalogSource(#[from] SourceError),
    #[error("market network request failed: {0}")]
    Network(#[source] MarketNetworkError),
    #[error("the catalog reported: {0}")]
    Catalog(Box<CatalogError>),
    #[error("listing {listing:?} does not belong to route {route}")]
    ListingRouteMismatch {
        listing: Box<VenueListingKey>,
        route: VenueRoute,
    },
    #[error("capability {capability} is not supported for route {route}")]
    UnsupportedCapability {
        route: VenueRoute,
        capability: CapabilityId,
    },
    #[error("the market-participants model rejected a market value: {0}")]
    Model(Box<ModelError>),
    #[error("market protocol returned an invalid response while performing {operation}")]
    Protocol { operation: SmolStr },
    #[error("invalid market request for {operation}: {reason}")]
    InvalidRequest {
        operation: SmolStr,
        reason: MarketRequestRejection,
    },
    #[error("market stream disconnected: {0}")]
    Disconnected(DisconnectReason),
    #[error("market service is closed")]
    ServiceClosed,
    #[error("invalid market configuration: {0}")]
    Configuration(SmolStr),
    #[error("market operation {operation} timed out after {timeout_ms}ms")]
    Timeout { operation: SmolStr, timeout_ms: u64 },
}

impl MarketError {
    /// Whether the market refused because the venue's scope is not loaded.
    ///
    /// "Not loaded" and "not supported" are the two answers one scope can be refused
    /// with, and a caller acts on them differently: a market the user has not loaded —
    /// or one a shrinking commit has taken away while a request was in flight — is a
    /// wait, while a scope this build does not serve is a refusal to give up on. The
    /// answer is read here, from the typed error, so a caller does not depend on the
    /// shape the market-participant error happens to be wrapped in.
    pub fn is_venue_not_loaded(&self) -> bool {
        matches!(
            self,
            Self::Network(network)
                if matches!(network.error(), MarketParticipantError::VenueNotLoaded { .. })
        )
    }
}

impl From<MarketParticipantError> for MarketError {
    fn from(error: MarketParticipantError) -> Self {
        match error {
            MarketParticipantError::UnsupportedCapability { route, capability } => {
                Self::UnsupportedCapability { route, capability }
            }
            MarketParticipantError::MarketModel(error) => Self::Model(error),
            error => Self::Network(error.into()),
        }
    }
}

impl From<ModelError> for MarketError {
    fn from(error: ModelError) -> Self {
        Self::Model(Box::new(error))
    }
}

/// Why one market request was refused before the venue saw it.
///
/// The refusal is a closed value rather than a sentence, because the message a
/// caller draws from it is a business label a user reads: a formatted reason
/// would carry the raw listing id of the request into that label, and a listing
/// identity belongs in the typed field of an error, not in the text of a name.
/// The status is the venue's own published state, so the message states it
/// without naming the identity it belongs to.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum MarketRequestRejection {
    /// The listing is settled or delisted, so it carries no market data to read.
    #[error("the listing is not readable while it is {status:?}")]
    ListingNotReadable {
        /// The published state that makes the listing unreadable.
        status: ListingStatus,
    },
}

/// Maps one instrument-directory failure into the market capability's own error.
pub(crate) fn market_error_from_catalog(error: CatalogError) -> MarketError {
    match error {
        CatalogError::Source(error) => MarketError::CatalogSource((*error).clone()),
        error => MarketError::Catalog(Box::new(error)),
    }
}

#[cfg(test)]
mod tests {
    use market_participants_model::{
        error::{ModelError, QuantityUnitExpectation},
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
        routing::CapabilityId,
    };

    use super::{MarketError, MarketNetworkError};

    fn route() -> VenueRoute {
        VenueRoute::new(
            VenueId::Hyperliquid,
            VenueScopeId::new("core-perp").expect("the scope should be canonical"),
        )
    }

    /// An unsupported capability keeps the full route and the exact capability:
    /// a caller has to be able to name the scope that failed.
    #[test]
    fn unsupported_network_capability_maps_to_a_market_capability_error() {
        let error = crate::error::MarketParticipantError::UnsupportedCapability {
            route: route(),
            capability: CapabilityId::ForceOrder,
        };

        assert_eq!(
            MarketError::from(error),
            MarketError::UnsupportedCapability {
                route: route(),
                capability: CapabilityId::ForceOrder,
            }
        );
    }

    /// A market nothing loaded stays a market nothing loaded — and a different answer
    /// from the unsupported capability above: a caller waits for the first and gives up
    /// on the second, so the mapping must not fold them together.
    #[test]
    fn an_unloaded_market_maps_to_a_market_that_is_not_loaded() {
        let error = crate::error::MarketParticipantError::VenueNotLoaded {
            venue: route().venue,
            scope: route().scope,
        };

        let mapped = MarketError::from(error.clone());
        assert_eq!(
            mapped,
            MarketError::Network(MarketNetworkError::from(error))
        );
        let MarketError::Network(network) = &mapped else {
            panic!("a market that is not loaded must not become a refusal of the scope");
        };
        assert!(matches!(
            network.error(),
            crate::error::MarketParticipantError::VenueNotLoaded { .. }
        ));
    }

    /// A rejection the request itself produced keeps its own shape, so a caller reads the
    /// typed value it named — the interval or the adjustment convention — instead of a
    /// paraphrased sentence about a failed request.
    #[test]
    fn an_unsupported_adjustment_maps_to_a_network_error() {
        let error = crate::error::MarketParticipantError::UnsupportedAdjustment {
            route: route(),
            adjustment: "forward".into(),
        };

        let mapped = MarketError::from(error.clone());
        assert_eq!(
            mapped,
            MarketError::Network(MarketNetworkError::from(error))
        );
        let MarketError::Network(network) = &mapped else {
            panic!("a convention the route does not publish is not a refused scope");
        };
        assert!(matches!(
            network.error(),
            crate::error::MarketParticipantError::UnsupportedAdjustment { .. }
        ));
    }

    /// The predicate a pane reads is about the one answer that is a wait: the scope is
    /// not loaded. A scope this build does not serve and a venue that could not be
    /// reached are both something else — the first is settled on the scope, the second
    /// is a network failure — so neither may be answered with the notice.
    #[test]
    fn only_an_unloaded_market_is_reported_as_not_loaded() {
        let unloaded = MarketError::from(crate::error::MarketParticipantError::VenueNotLoaded {
            venue: route().venue,
            scope: route().scope,
        });
        assert!(
            unloaded.is_venue_not_loaded(),
            "a scope no load holds is the answer the notice waits on"
        );

        let unsupported = MarketError::from(
            crate::error::MarketParticipantError::UnsupportedCapability {
                route: route(),
                capability: CapabilityId::Candlesticks,
            },
        );
        assert!(
            !unsupported.is_venue_not_loaded(),
            "a scope that loaded without the capability is a refusal, not a wait"
        );

        let unreachable =
            MarketError::from(crate::error::MarketParticipantError::ConnectionFailed {
                route: route(),
                endpoint: "https://api.binance.com".into(),
                source: network::error::NetworkError::Io {
                    message: "the connection timed out".into(),
                },
            });
        assert!(
            !unreachable.is_venue_not_loaded(),
            "a venue that could not be reached is a network failure, not an unloaded market"
        );
    }

    /// A unit rejection is a model rejection, and it stays typed: a caller shows
    /// the expected unit instead of a paraphrased string.
    #[test]
    fn a_quantity_unit_mismatch_stays_a_model_error() {
        let error = MarketError::from(ModelError::QuantityUnitMismatch {
            field: "trade.quantity",
            expected: QuantityUnitExpectation::TradedQuantity,
        });

        let MarketError::Model(boxed) = error else {
            panic!("a model rejection must stay a model rejection");
        };
        assert_eq!(
            *boxed,
            ModelError::QuantityUnitMismatch {
                field: "trade.quantity",
                expected: QuantityUnitExpectation::TradedQuantity,
            }
        );
    }
}

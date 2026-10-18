use std::sync::Arc;

use market_participants_model::{
    decimal::Quantity,
    error::ModelError,
    listing::{
        listing_impl::ListingStatus,
        route::{VenueListingKey, VenueRoute},
    },
};
use smol_str::{SmolStr, format_smolstr};
use thiserror::Error;

use crate::{
    catalog::error::{CatalogError, SourceError},
    protocol::CommandKind,
};

/// Network failure returned by a trading operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TradingNetworkError(Arc<crate::error::MarketParticipantError>);

impl TradingNetworkError {
    pub fn is_retryable(&self) -> bool {
        self.0.is_retryable()
    }

    /// Returns the classified market-participant failure behind this trading error.
    pub fn error(&self) -> &crate::error::MarketParticipantError {
        &self.0
    }
}

impl std::fmt::Display for TradingNetworkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for TradingNetworkError {}

pub(crate) fn network_error(error: crate::error::MarketParticipantError) -> TradingNetworkError {
    TradingNetworkError(Arc::new(error))
}

/// Trading-owned failures while resolving a listing a command trades.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum TradingCatalogError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error("trading catalog network request failed: {0}")]
    Network(#[source] TradingNetworkError),
    #[error("the catalog reported: {0}")]
    Directory(Box<CatalogError>),
    #[error("listing {listing:?} is not published on route {route}")]
    ListingNotFound {
        route: VenueRoute,
        listing: Box<VenueListingKey>,
    },
}

/// Strongly typed failures for trading capability operations.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum TradingError {
    #[error("trading market metadata request failed: {0}")]
    Market(#[source] TradingCatalogError),
    #[error("market participant rejected the trading request ({code}): {message}")]
    Rejected {
        code: SmolStr,
        message: SmolStr,
        retryable: bool,
    },
    #[error("insufficient {asset}: required {required}, available {available} ({message})")]
    InsufficientBalance {
        asset: SmolStr,
        required: Quantity,
        available: Quantity,
        message: SmolStr,
    },
    #[error("market-participant order request failed: {reason}")]
    Order { reason: SmolStr },
    #[error("the market-participants model rejected a trading value: {0}")]
    Model(Box<ModelError>),
    #[error(
        "command {command:?} is not allowed while listing {listing:?} is {status:?} (within window: {within_window})"
    )]
    ListingActionNotAllowed {
        listing: Box<VenueListingKey>,
        status: ListingStatus,
        command: CommandKind,
        within_window: bool,
    },
    #[error("trading protocol failure while attempting to {operation}: {reason}")]
    Protocol { operation: SmolStr, reason: SmolStr },
    #[error("trading network request failed: {0}")]
    Network(#[source] TradingNetworkError),
    #[error("trading configuration is invalid: {operation}")]
    Configuration { operation: SmolStr },
}

pub(crate) fn trading_error_from_catalog(error: CatalogError) -> TradingError {
    if let CatalogError::Source(error) = error {
        return TradingError::Market(TradingCatalogError::Source((*error).clone()));
    }
    TradingError::Market(TradingCatalogError::Directory(Box::new(error)))
}

pub(crate) fn trading_error_from_market_participant(
    error: crate::error::MarketParticipantError,
) -> TradingError {
    match error {
        crate::error::MarketParticipantError::Rejected {
            code,
            message,
            retryable,
            ..
        } => TradingError::Rejected {
            code,
            message,
            retryable,
        },
        crate::error::MarketParticipantError::InsufficientBalance {
            asset,
            required,
            available,
            message,
        } => TradingError::InsufficientBalance {
            asset,
            required,
            available,
            message,
        },
        crate::error::MarketParticipantError::OrderError { kind, .. } => TradingError::Order {
            reason: format_smolstr!("{kind}"),
        },
        // A listing of another route is not a network failure: the route and the
        // listing the command named are what failed, so they are carried as the
        // catalog error that says exactly that.
        crate::error::MarketParticipantError::ListingRouteMismatch { listing, route } => {
            TradingError::Market(TradingCatalogError::ListingNotFound {
                route,
                listing: Box::new(listing),
            })
        }
        // A model rejection is not a network failure either: the value the venue
        // or the caller supplied is what the boundary refused.
        crate::error::MarketParticipantError::MarketModel(error) => TradingError::Model(error),
        // A listing whose own published status or window refuses a command is a
        // trading rule rather than a network failure, so the caller reads the
        // status, the command and the window that decided the refusal instead of
        // having to open a network error to find them.
        crate::error::MarketParticipantError::ListingActionNotAllowed {
            listing,
            status,
            command,
            within_window,
        } => TradingError::ListingActionNotAllowed {
            listing: Box::new(listing),
            status,
            command,
            within_window,
        },
        // The venue answered, but not in the protocol it claims to speak: that is
        // a protocol failure, not transport.
        crate::error::MarketParticipantError::ProtocolError {
            operation, reason, ..
        } => TradingError::Protocol { operation, reason },
        error => TradingError::Network(network_error(error)),
    }
}

#[cfg(test)]
mod tests {
    use market_participants_model::{
        error::ModelError,
        listing::{listing_impl::ListingStatus, route::VenueRoute},
    };

    use super::{TradingCatalogError, TradingError, trading_error_from_market_participant};
    use crate::{catalog::test_support, protocol::CommandKind};

    fn route() -> VenueRoute {
        test_support::spot_route()
    }

    /// A listing of another route is a catalog failure that names the route and
    /// the listing, not an anonymous network failure.
    #[test]
    fn a_listing_of_another_route_stays_a_catalog_failure() {
        let listing = test_support::listing_key(&route(), "BTCUSDT");
        let error = trading_error_from_market_participant(
            crate::error::MarketParticipantError::ListingRouteMismatch {
                listing: listing.clone(),
                route: route(),
            },
        );

        assert_eq!(
            error,
            TradingError::Market(TradingCatalogError::ListingNotFound {
                route: route(),
                listing: Box::new(listing),
            })
        );
    }

    /// A model rejection stays the typed model error it is.
    #[test]
    fn a_model_rejection_stays_a_model_error() {
        let error = trading_error_from_market_participant(
            crate::error::MarketParticipantError::MarketModel(Box::new(ModelError::ZeroValue {
                field: "quantity",
            })),
        );

        assert_eq!(
            error,
            TradingError::Model(Box::new(ModelError::ZeroValue { field: "quantity" }))
        );
    }

    /// A genuine network failure keeps its classified market-participant error reachable
    /// instead of collapsing into a message.
    #[test]
    fn a_network_failure_keeps_its_classified_error_reachable() {
        let error = trading_error_from_market_participant(
            crate::error::MarketParticipantError::ServiceClosed,
        );
        let TradingError::Network(network) = error else {
            panic!("a service closure is a network failure");
        };

        assert_eq!(
            network.error(),
            &crate::error::MarketParticipantError::ServiceClosed
        );
        assert!(!network.is_retryable());
    }

    /// A listing that refuses a command is a trading rule, not a network failure:
    /// the caller reads the status, the command and the window without having to
    /// unpack a network error to find out what happened.
    #[test]
    fn a_listing_that_refuses_a_command_is_not_a_network_failure() {
        let listing = test_support::listing_key(&route(), "BTCUSDT");
        let error = trading_error_from_market_participant(
            crate::error::MarketParticipantError::ListingActionNotAllowed {
                listing: listing.clone(),
                status: ListingStatus::Halted,
                command: CommandKind::NewTrading,
                within_window: true,
            },
        );

        assert_eq!(
            error,
            TradingError::ListingActionNotAllowed {
                listing: Box::new(listing),
                status: ListingStatus::Halted,
                command: CommandKind::NewTrading,
                within_window: true,
            }
        );
    }

    /// An answer that is not the protocol the venue claims to speak is reported as
    /// the protocol failure it is, with the operation and the reason it carried.
    #[test]
    fn a_protocol_failure_stays_a_protocol_failure() {
        let error = trading_error_from_market_participant(
            crate::error::MarketParticipantError::ProtocolError {
                operation: "decode OKX order".into(),
                reason: "the order id is missing".into(),
                raw_data: None,
            },
        );

        assert_eq!(
            error,
            TradingError::Protocol {
                operation: "decode OKX order".into(),
                reason: "the order id is missing".into(),
            }
        );
    }
}

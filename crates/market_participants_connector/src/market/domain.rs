//! Market-data facade owned by the market capability.
//!
//! Every request is listing-scoped: it names the exact `VenueListingKey` it is
//! about, and it can only be validated against the listing's own rules, status and
//! window. Which routes publish which capability is the route registry's answer,
//! so nothing in this module guesses a product from a venue.
//!
//! The request types themselves are the protocol boundary's: a market call
//! constructs the same `market_participants::model` query an adapter encodes with, so there
//! is exactly one shape of every request in the crate.

use market_participants_model::listing::route::{VenueListingKey, VenueRoute};
use smol_str::SmolStr;

use super::{
    builders::{
        AllTickersBuilder, AnalyticsSelector, CandlestickBuilder, OrderBookBuilder, TickerBuilder,
        TradeBuilder,
    },
    error::MarketError,
    provider::MarketCapability,
};
use crate::market_participants::model::TimeFrame;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisconnectReason {
    Normal,
    Network(crate::error::NetworkError),
    ServerClosed { code: u16, reason: SmolStr },
    ProxyChanged,
    AuthenticationExpired,
    HeartbeatTimeout,
}

impl From<crate::error::DisconnectReason> for DisconnectReason {
    fn from(reason: crate::error::DisconnectReason) -> Self {
        match reason {
            crate::error::DisconnectReason::Normal => Self::Normal,
            crate::error::DisconnectReason::Network(error) => Self::Network(error),
            crate::error::DisconnectReason::ServerClosed { code, reason } => {
                Self::ServerClosed { code, reason }
            }
            crate::error::DisconnectReason::ProxyChanged => Self::ProxyChanged,
            crate::error::DisconnectReason::AuthenticationExpired => Self::AuthenticationExpired,
            crate::error::DisconnectReason::HeartbeatTimeout => Self::HeartbeatTimeout,
        }
    }
}

impl From<DisconnectReason> for crate::error::DisconnectReason {
    fn from(reason: DisconnectReason) -> Self {
        match reason {
            DisconnectReason::Normal => Self::Normal,
            DisconnectReason::Network(error) => Self::Network(error),
            DisconnectReason::ServerClosed { code, reason } => Self::ServerClosed { code, reason },
            DisconnectReason::ProxyChanged => Self::ProxyChanged,
            DisconnectReason::AuthenticationExpired => Self::AuthenticationExpired,
            DisconnectReason::HeartbeatTimeout => Self::HeartbeatTimeout,
        }
    }
}

impl std::fmt::Display for DisconnectReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Normal => formatter.write_str("normal closure"),
            Self::Network(error) => write!(formatter, "network error: {error}"),
            Self::ServerClosed { code, reason } => {
                write!(formatter, "server closed connection ({code}): {reason}")
            }
            Self::ProxyChanged => formatter.write_str("proxy configuration changed"),
            Self::AuthenticationExpired => formatter.write_str("authentication expired"),
            Self::HeartbeatTimeout => formatter.write_str("heartbeat timed out"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubscriptionStatus {
    Connecting,
    Active,
    Reconnecting,
    Closed,
    Failed(MarketError),
}

impl From<crate::adapter::SubscriptionStatus> for SubscriptionStatus {
    fn from(status: crate::adapter::SubscriptionStatus) -> Self {
        match status {
            crate::adapter::SubscriptionStatus::Connecting => Self::Connecting,
            crate::adapter::SubscriptionStatus::Active => Self::Active,
            crate::adapter::SubscriptionStatus::Reconnecting => Self::Reconnecting,
            crate::adapter::SubscriptionStatus::Closed => Self::Closed,
            crate::adapter::SubscriptionStatus::Failed(error) => Self::Failed(error.into()),
        }
    }
}

/// The exact listing one market request is about.
///
/// A selector is route-scoped and a builder is listing-scoped: the key is
/// resolved inside one catalog generation, and the resolution is what carries the
/// rules, the status and the typed native identifiers a request is built from. A
/// listing whose own route is not the selector's is rejected before anything is
/// encoded.
#[derive(Clone)]
pub struct MarketSelector {
    provider: MarketCapability,
    route: VenueRoute,
}

impl MarketSelector {
    pub(crate) fn new(provider: MarketCapability, route: VenueRoute) -> Self {
        Self { provider, route }
    }

    /// Returns the route this selector is about.
    pub const fn route(&self) -> &VenueRoute {
        &self.route
    }

    pub fn candlesticks(
        &self,
        listing: VenueListingKey,
        timeframe: TimeFrame,
    ) -> CandlestickBuilder {
        CandlestickBuilder::new(
            self.provider.clone(),
            self.route.clone(),
            listing,
            timeframe,
        )
    }

    pub fn ticker(&self, listing: VenueListingKey) -> TickerBuilder {
        TickerBuilder::new(self.provider.clone(), self.route.clone(), listing)
    }

    pub fn order_book(&self, listing: VenueListingKey) -> OrderBookBuilder {
        OrderBookBuilder::new(self.provider.clone(), self.route.clone(), listing)
    }

    pub fn trades(&self, listing: VenueListingKey) -> TradeBuilder {
        TradeBuilder::new(self.provider.clone(), self.route.clone(), listing)
    }

    pub fn all_tickers(&self) -> AllTickersBuilder {
        AllTickersBuilder::new(self.provider.clone(), self.route.clone())
    }

    pub fn analytics(&self) -> AnalyticsSelector {
        AnalyticsSelector::new(self.provider.clone(), self.route.clone())
    }
}

/// Rejects a listing that does not belong to the route a selector is about.
pub(crate) fn require_listing_route(
    route: &VenueRoute,
    listing: &VenueListingKey,
) -> Result<(), MarketError> {
    if &listing.route == route {
        return Ok(());
    }
    Err(MarketError::ListingRouteMismatch {
        listing: Box::new(listing.clone()),
        route: route.clone(),
    })
}

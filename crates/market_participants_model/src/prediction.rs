//! Event-scoped prediction-market market data.
//!
//! Prediction venues publish events rather than a stable instrument directory: an
//! event owns a handful of options, each option is one outcome leg, and the
//! tradable identity of a leg is a venue-native token that is discovered with the
//! event. The model therefore keeps this vocabulary separate from the catalogued
//! instrument model: a prediction leg still carries its canonical [`OutcomeLegRef`]
//! and its [`VenueListingKey`], but nothing here is published as a catalog entry.
//!
//! The values are shared by the connector's protocol boundary and the widgets that
//! draw an event, so they are plain data: no venue name, no endpoint and no HTTP
//! shape appears in this module.

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::{
    decimal::{Fraction, Quantity, UnixMillis},
    identifier::IdentifierValue,
    listing::{
        route::{VenueListingKey, VenueRoute},
        venue::{
            HyperliquidAssetId, HyperliquidOutcomeId, PolymarketAssetId, PolymarketProtocolVersion,
        },
    },
    outcome::{OutcomeLegRef, OutcomeSideId},
};

/// The stable identity of one prediction event on one route.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PredictionEventRef {
    #[serde(rename = "route")]
    pub route: VenueRoute,
    /// The venue's own event identity.
    ///
    /// A Polymarket event id, or a HIP-4 question/outcome identity prefixed with
    /// `q:` or `o:` so the two namespaces cannot collide.
    #[serde(rename = "native_event")]
    pub native_event: IdentifierValue,
}

/// One category a prediction selector offers.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PredictionCategory {
    /// The stable category key used by filters and persisted selections.
    #[serde(rename = "key")]
    pub key: SmolStr,
    /// The category name shown to a reader.
    #[serde(rename = "label")]
    pub label: SmolStr,
}

impl PredictionCategory {
    /// Builds a category from its stable key and its display label.
    pub fn new(key: impl Into<SmolStr>, label: impl Into<SmolStr>) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
        }
    }
}

/// The stable identity of one market inside a prediction event.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PredictionMarketRef {
    #[serde(rename = "event")]
    pub event: PredictionEventRef,
    /// The venue's own market identity.
    #[serde(rename = "native_market")]
    pub native_market: IdentifierValue,
}

/// One group of markets inside a prediction event.
///
/// A sports event publishes its markets in groups such as `moneyline`, `spreads`
/// or `first_half_totals`; an event with one market publishes one group. The key
/// is stable and filterable, the label is what a reader sees.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PredictionMarketGroup {
    #[serde(rename = "key")]
    pub key: SmolStr,
    #[serde(rename = "label")]
    pub label: SmolStr,
}

impl PredictionMarketGroup {
    /// Builds a group from its stable key and display label.
    pub fn new(key: impl Into<SmolStr>, label: impl Into<SmolStr>) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
        }
    }
}

/// One option as an event card draws it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictionOptionCard {
    #[serde(rename = "market")]
    pub market: PredictionMarketRef,
    #[serde(rename = "leg")]
    pub leg: OutcomeLegRef,
    #[serde(rename = "label")]
    pub label: SmolStr,
    #[serde(rename = "probability")]
    pub probability: Option<Fraction>,
}

/// One event as a selector card draws it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictionEventCard {
    #[serde(rename = "event")]
    pub event: PredictionEventRef,
    #[serde(rename = "title")]
    pub title: SmolStr,
    #[serde(rename = "category")]
    pub category: PredictionCategory,
    #[serde(rename = "ends_at")]
    pub ends_at: Option<UnixMillis>,
    #[serde(rename = "volume")]
    pub volume: Option<Quantity>,
    #[serde(rename = "liquidity")]
    pub liquidity: Option<Quantity>,
    /// At most six options, ordered by probability descending.
    #[serde(rename = "options")]
    pub options: Vec<PredictionOptionCard>,
    /// How many options the event has beyond [`Self::options`].
    #[serde(rename = "remaining_options")]
    pub remaining_options: u16,
    /// How many markets the event publishes.
    #[serde(rename = "market_count")]
    pub market_count: u16,
    /// How many markets are not represented by [`Self::options`].
    #[serde(rename = "remaining_markets")]
    pub remaining_markets: u16,
}

/// One page of prediction events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictionEventsPage {
    #[serde(rename = "events")]
    pub events: Vec<PredictionEventCard>,
    #[serde(rename = "categories")]
    pub categories: Vec<PredictionCategory>,
    #[serde(rename = "next_cursor")]
    pub next_cursor: Option<SmolStr>,
    #[serde(rename = "total")]
    pub total: Option<u32>,
}

/// The native tradable identity of one prediction leg.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "native")]
pub enum PredictionNativeLeg {
    #[serde(rename = "polymarket")]
    Polymarket {
        #[serde(rename = "protocol")]
        protocol: PolymarketProtocolVersion,
        #[serde(rename = "asset_id")]
        asset_id: PolymarketAssetId,
    },
    #[serde(rename = "hyperliquid_outcome")]
    HyperliquidOutcome {
        #[serde(rename = "outcome")]
        outcome: HyperliquidOutcomeId,
        #[serde(rename = "side")]
        side: OutcomeSideId,
        #[serde(rename = "encoding")]
        encoding: u64,
        #[serde(rename = "coin")]
        coin: SmolStr,
        #[serde(rename = "asset_id")]
        asset_id: HyperliquidAssetId,
    },
}

/// One tradable side of one prediction market.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictionLeg {
    #[serde(rename = "leg")]
    pub leg: OutcomeLegRef,
    #[serde(rename = "label")]
    pub label: SmolStr,
    /// The latest probability the venue published with the event, when it did.
    #[serde(rename = "probability")]
    pub probability: Option<Fraction>,
    #[serde(rename = "listing")]
    pub listing: VenueListingKey,
    #[serde(rename = "native")]
    pub native: PredictionNativeLeg,
}

/// One market of a prediction event, with every side it publishes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictionMarket {
    #[serde(rename = "market")]
    pub market: PredictionMarketRef,
    /// The market's own question, shown when a reader picks a market.
    #[serde(rename = "question")]
    pub question: SmolStr,
    /// The short label an event card uses for this market.
    #[serde(rename = "label")]
    pub label: SmolStr,
    #[serde(rename = "group")]
    pub group: PredictionMarketGroup,
    /// Whether this is one of the event's headline markets.
    #[serde(rename = "headline")]
    pub headline: bool,
    /// Whether the venue still lists this market as open for trading.
    #[serde(rename = "open")]
    pub open: bool,
    /// The side that represents the market's first outcome.
    #[serde(rename = "default_leg")]
    pub default_leg: Option<OutcomeLegRef>,
    #[serde(rename = "legs")]
    pub legs: Vec<PredictionLeg>,
}

/// One event with every market and side it publishes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictionEventView {
    #[serde(rename = "event")]
    pub event: PredictionEventRef,
    #[serde(rename = "title")]
    pub title: SmolStr,
    #[serde(rename = "category")]
    pub category: PredictionCategory,
    #[serde(rename = "ends_at")]
    pub ends_at: Option<UnixMillis>,
    #[serde(rename = "markets")]
    pub markets: Vec<PredictionMarket>,
}

/// One point of a probability series.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictionPricePoint {
    #[serde(rename = "timestamp")]
    pub timestamp: UnixMillis,
    #[serde(rename = "probability")]
    pub probability: Fraction,
}

/// The probability history of one option.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictionSeries {
    #[serde(rename = "leg")]
    pub leg: OutcomeLegRef,
    #[serde(rename = "points")]
    pub points: Vec<PredictionPricePoint>,
}

/// The window a prediction series is read over.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "window")]
pub enum PredictionWindow {
    /// A bounded history of `days`, sampled at `fidelity_minutes`.
    #[serde(rename = "history")]
    History {
        #[serde(rename = "days")]
        days: u16,
        #[serde(rename = "fidelity_minutes")]
        fidelity_minutes: u16,
    },
    /// The latest probability of every requested leg.
    #[serde(rename = "latest")]
    Latest,
}

impl Default for PredictionWindow {
    fn default() -> Self {
        Self::History {
            days: 30,
            fidelity_minutes: 60,
        }
    }
}

impl PredictionWindow {
    /// The number of days this window covers, when it is a history.
    pub const fn days(self) -> Option<u16> {
        match self {
            Self::History { days, .. } => Some(days),
            Self::Latest => None,
        }
    }

    /// The sampling fidelity in minutes, when it is a history.
    pub const fn fidelity_minutes(self) -> Option<u16> {
        match self {
            Self::History {
                fidelity_minutes, ..
            } => Some(fidelity_minutes),
            Self::Latest => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PredictionEventRef, PredictionWindow};
    use crate::{
        identifier::{IdentifierValue, VenueId, VenueScopeId},
        listing::route::VenueRoute,
    };

    #[test]
    fn a_prediction_event_ref_round_trips_through_serde() {
        let event = PredictionEventRef {
            route: VenueRoute::new(
                VenueId::Polymarket,
                VenueScopeId::new("predict").expect("the scope is canonical"),
            ),
            native_event: IdentifierValue::new("1262243").expect("the event id is valid"),
        };
        let encoded = serde_json::to_string(&event).expect("the event ref serializes");
        let decoded: PredictionEventRef =
            serde_json::from_str(&encoded).expect("the event ref decodes");

        assert_eq!(decoded, event);
    }

    #[test]
    fn the_default_window_is_thirty_days_at_hourly_fidelity() {
        assert_eq!(
            PredictionWindow::default(),
            PredictionWindow::History {
                days: 30,
                fidelity_minutes: 60
            }
        );
        assert_eq!(PredictionWindow::default().days(), Some(30));
        assert_eq!(PredictionWindow::Latest.days(), None);
    }
}

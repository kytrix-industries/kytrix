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
    decimal::{Fraction, Quantity, Tick, UnixMillis},
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

/// One price level of a prediction order book.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictionBookLevel {
    #[serde(rename = "price")]
    pub price: Fraction,
    #[serde(rename = "size")]
    pub size: Quantity,
}

/// One changed price level; a zero size removes the level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictionBookLevelChange {
    #[serde(rename = "price")]
    pub price: Fraction,
    #[serde(rename = "size")]
    pub size: Quantity,
}

/// One complete prediction order book reading (REST snapshot or normalized state).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictionOrderBook {
    #[serde(rename = "leg")]
    pub leg: OutcomeLegRef,
    #[serde(rename = "tick_size")]
    pub tick_size: Tick,
    #[serde(rename = "bids")]
    pub bids: Vec<PredictionBookLevel>,
    #[serde(rename = "asks")]
    pub asks: Vec<PredictionBookLevel>,
    #[serde(rename = "last_trade_price")]
    pub last_trade_price: Option<Fraction>,
    #[serde(rename = "timestamp")]
    pub timestamp: UnixMillis,
}

/// One incremental update of a prediction order book.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "update")]
pub enum PredictionOrderBookUpdate {
    #[serde(rename = "replace")]
    Replace {
        #[serde(rename = "bids")]
        bids: Vec<PredictionBookLevel>,
        #[serde(rename = "asks")]
        asks: Vec<PredictionBookLevel>,
        #[serde(rename = "timestamp")]
        timestamp: UnixMillis,
    },
    #[serde(rename = "levels")]
    Levels {
        #[serde(rename = "bids")]
        bids: Vec<PredictionBookLevelChange>,
        #[serde(rename = "asks")]
        asks: Vec<PredictionBookLevelChange>,
        #[serde(rename = "timestamp")]
        timestamp: UnixMillis,
    },
    #[serde(rename = "tick_size")]
    TickSize {
        #[serde(rename = "tick_size")]
        tick_size: Tick,
        #[serde(rename = "timestamp")]
        timestamp: UnixMillis,
    },
    #[serde(rename = "last_trade")]
    LastTrade {
        #[serde(rename = "price")]
        price: Fraction,
        #[serde(rename = "timestamp")]
        timestamp: UnixMillis,
    },
}

/// The range a prediction series is read over.
///
/// The product offers these six ranges. The range decides both how far back the
/// series reaches and how coarsely the venue samples it; a venue that states its
/// span as an explicit timestamp pair, a preset name, or a candle count is the
/// adapter's problem, not the caller's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PredictionRange {
    #[serde(rename = "hour_1")]
    Hour1,
    #[serde(rename = "hour_6")]
    Hour6,
    #[serde(rename = "day_1")]
    Day1,
    #[serde(rename = "week_1")]
    Week1,
    #[serde(rename = "month_1")]
    Month1,
    /// The whole life of the market.
    #[default]
    #[serde(rename = "all")]
    All,
}

impl PredictionRange {
    /// Every range, in the order the range selector offers them.
    pub const ALL: [Self; 6] = [
        Self::Hour1,
        Self::Hour6,
        Self::Day1,
        Self::Week1,
        Self::Month1,
        Self::All,
    ];

    /// The span the range covers, in milliseconds; `None` for the whole life of
    /// the market.
    pub const fn span_millis(self) -> Option<i64> {
        match self {
            Self::Hour1 => Some(3_600_000),
            Self::Hour6 => Some(21_600_000),
            Self::Day1 => Some(86_400_000),
            Self::Week1 => Some(604_800_000),
            Self::Month1 => Some(2_592_000_000),
            Self::All => None,
        }
    }

    /// The sampling fidelity the chart asks for, in minutes.
    pub const fn fidelity_minutes(self) -> u16 {
        match self {
            Self::Hour1 => 1,
            Self::Hour6 => 5,
            Self::Day1 => 10,
            Self::Week1 => 60,
            Self::Month1 => 60,
            Self::All => 720,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PredictionBookLevel, PredictionBookLevelChange, PredictionEventRef, PredictionOrderBook,
        PredictionOrderBookUpdate, PredictionRange,
    };
    use crate::{
        decimal::{Fraction, Quantity, Tick},
        identifier::{IdentifierValue, VenueId, VenueScopeId},
        listing::route::VenueRoute,
        outcome::{OutcomeGroupId, OutcomeLegRef, OutcomeMemberId, OutcomeSideId},
    };

    fn leg() -> OutcomeLegRef {
        let identity = |digits: &str| format!("v1/{}", digits.repeat(64));
        OutcomeLegRef {
            group: OutcomeGroupId::from_identity_value(&identity("1"))
                .expect("the fixture group identity is valid"),
            member: OutcomeMemberId::from_identity_value(&identity("2"))
                .expect("the fixture member identity is valid"),
            side: OutcomeSideId::from_identity_value(&identity("3"))
                .expect("the fixture side identity is valid"),
        }
    }

    fn level(price: &str, size: &str) -> PredictionBookLevel {
        PredictionBookLevel {
            price: price
                .parse::<Fraction>()
                .expect("the fixture price is a fraction"),
            size: size
                .parse::<Quantity>()
                .expect("the fixture size is a quantity"),
        }
    }

    fn fixture_book() -> PredictionOrderBook {
        PredictionOrderBook {
            leg: leg(),
            tick_size: "0.01".parse::<Tick>().expect("the fixture tick is valid"),
            bids: vec![level("0.4", "10")],
            asks: vec![level("0.6", "12")],
            last_trade_price: Some(
                "0.5"
                    .parse::<Fraction>()
                    .expect("the fixture price is a fraction"),
            ),
            timestamp: 1_790_000_000_000,
        }
    }

    #[test]
    fn a_prediction_order_book_round_trips_with_its_documented_fields() {
        let book = fixture_book();
        let encoded = serde_json::to_string(&book).expect("the book serializes");
        let decoded: PredictionOrderBook =
            serde_json::from_str(&encoded).expect("the book decodes");

        assert_eq!(decoded, book);
        let value: serde_json::Value =
            serde_json::from_str(&encoded).expect("the book encodes as JSON");
        assert_eq!(value["tick_size"], "0.01");
        assert_eq!(value["bids"][0]["price"], "0.4");
        assert_eq!(value["bids"][0]["size"], "10");
        assert_eq!(value["asks"][0]["price"], "0.6");
        assert_eq!(value["last_trade_price"], "0.5");
        assert_eq!(value["timestamp"], 1_790_000_000_000_i64);
    }

    #[test]
    fn every_order_book_update_states_its_documented_tag() {
        let updates = [
            (
                PredictionOrderBookUpdate::Replace {
                    bids: vec![level("0.4", "10")],
                    asks: vec![level("0.6", "12")],
                    timestamp: 1,
                },
                "replace",
            ),
            (
                PredictionOrderBookUpdate::Levels {
                    bids: vec![PredictionBookLevelChange {
                        price: "0.4".parse::<Fraction>().expect("the fixture price"),
                        size: Quantity::ZERO,
                    }],
                    asks: Vec::new(),
                    timestamp: 2,
                },
                "levels",
            ),
            (
                PredictionOrderBookUpdate::TickSize {
                    tick_size: "0.001".parse::<Tick>().expect("the fixture tick"),
                    timestamp: 3,
                },
                "tick_size",
            ),
            (
                PredictionOrderBookUpdate::LastTrade {
                    price: "0.25".parse::<Fraction>().expect("the fixture price"),
                    timestamp: 4,
                },
                "last_trade",
            ),
        ];

        for (update, tag) in &updates {
            let value = serde_json::to_value(update).expect("the update serializes");
            assert_eq!(value["update"], *tag);
            let decoded: PredictionOrderBookUpdate =
                serde_json::from_value(value).expect("the update decodes");
            assert_eq!(&decoded, update);
        }

        let removal = serde_json::to_value(&updates[1].0).expect("the update serializes");
        assert_eq!(
            removal["bids"][0]["size"], "0",
            "a zero size states that the level is removed"
        );
    }

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
    fn the_range_table_is_the_agreed_six_in_selector_order() {
        assert_eq!(
            PredictionRange::ALL,
            [
                PredictionRange::Hour1,
                PredictionRange::Hour6,
                PredictionRange::Day1,
                PredictionRange::Week1,
                PredictionRange::Month1,
                PredictionRange::All,
            ]
        );
    }

    #[test]
    fn every_range_covers_its_span() {
        assert_eq!(PredictionRange::Hour1.span_millis(), Some(3_600_000));
        assert_eq!(PredictionRange::Hour6.span_millis(), Some(21_600_000));
        assert_eq!(PredictionRange::Day1.span_millis(), Some(86_400_000));
        assert_eq!(PredictionRange::Week1.span_millis(), Some(604_800_000));
        assert_eq!(PredictionRange::Month1.span_millis(), Some(2_592_000_000));
        assert_eq!(PredictionRange::All.span_millis(), None);
    }

    #[test]
    fn every_range_asks_for_its_sampling_fidelity() {
        assert_eq!(PredictionRange::Hour1.fidelity_minutes(), 1);
        assert_eq!(PredictionRange::Hour6.fidelity_minutes(), 5);
        assert_eq!(PredictionRange::Day1.fidelity_minutes(), 10);
        assert_eq!(PredictionRange::Week1.fidelity_minutes(), 60);
        assert_eq!(PredictionRange::Month1.fidelity_minutes(), 60);
        assert_eq!(PredictionRange::All.fidelity_minutes(), 720);
    }

    #[test]
    fn the_default_range_is_the_whole_life_of_the_market() {
        assert_eq!(PredictionRange::default(), PredictionRange::All);
    }

    #[test]
    fn every_range_round_trips_through_serde() {
        let names = [
            (PredictionRange::Hour1, "hour_1"),
            (PredictionRange::Hour6, "hour_6"),
            (PredictionRange::Day1, "day_1"),
            (PredictionRange::Week1, "week_1"),
            (PredictionRange::Month1, "month_1"),
            (PredictionRange::All, "all"),
        ];
        for (range, name) in names {
            let encoded = serde_json::to_string(&range).expect("the range serializes");
            assert_eq!(encoded, format!("\"{name}\""));
            let decoded: PredictionRange =
                serde_json::from_str(&encoded).expect("the range decodes");
            assert_eq!(decoded, range);
        }
    }
}

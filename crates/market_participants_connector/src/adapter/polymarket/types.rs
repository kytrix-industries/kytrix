//! The typed wire shapes of the Polymarket Gamma and CLOB APIs.
//!
//! Only the fields this build reads are stated. Gamma publishes a very wide event
//! document, and a field this adapter does not read is not a field the model
//! should carry: the typed shape is the contract, not a mirror of the venue's
//! whole document.

use std::{collections::BTreeMap, fmt};

use market_participants_model::decimal::UnixMillis;
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, Visitor},
};
use smol_str::SmolStr;

/// One page of Gamma's keyset event feed.
#[derive(Clone, Debug, Deserialize)]
pub struct GammaEventsKeyset {
    #[serde(default)]
    pub events: Vec<GammaEvent>,
    #[serde(default)]
    pub next_cursor: Option<SmolStr>,
}

/// One page of Gamma's public search.
#[derive(Clone, Debug, Deserialize)]
pub struct GammaSearchResponse {
    /// Gamma omits this field entirely when the search has no matches.
    #[serde(default)]
    pub events: Vec<GammaEvent>,
    #[serde(default)]
    pub pagination: Option<GammaPagination>,
}

/// The pagination envelope of a Gamma search answer.
#[derive(Clone, Debug, Deserialize)]
pub struct GammaPagination {
    #[serde(rename = "hasMore", default)]
    pub has_more: bool,
    #[serde(rename = "totalResults", default)]
    pub total_results: Option<u32>,
}

/// One Gamma event.
#[derive(Clone, Debug, Deserialize)]
pub struct GammaEvent {
    pub id: SmolStr,
    pub title: SmolStr,
    #[serde(rename = "endDate", default)]
    pub end_date: Option<SmolStr>,
    #[serde(default)]
    pub volume: Option<f64>,
    #[serde(default)]
    pub liquidity: Option<f64>,
    #[serde(rename = "parentEventId", default)]
    pub parent_event_id: Option<GammaId>,
    #[serde(default)]
    pub tags: Vec<GammaTag>,
    #[serde(default)]
    pub markets: Vec<GammaMarket>,
}

/// One Gamma tag.
#[derive(Clone, Debug, Deserialize)]
pub struct GammaTag {
    pub id: SmolStr,
}

/// A Gamma identifier that the venue publishes either as a string or as a
/// number. `parentEventId` is the field that varies between the two forms.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum GammaId {
    String(SmolStr),
    Integer(u64),
}

impl std::fmt::Display for GammaId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::String(value) => formatter.write_str(value),
            Self::Integer(value) => write!(formatter, "{value}"),
        }
    }
}

/// One Gamma market inside an event.
#[derive(Clone, Debug, Deserialize)]
pub struct GammaMarket {
    pub id: SmolStr,
    #[serde(default)]
    pub question: SmolStr,
    #[serde(rename = "conditionId", default)]
    pub condition_id: SmolStr,
    #[serde(default)]
    pub outcomes: Option<SmolStr>,
    /// The canonical outcomes of this market before a localized copy replaced
    /// [`Self::outcomes`]. Business classification reads this field, never the
    /// display text, so a translated `是` / `否` cannot change the market's shape.
    #[serde(skip)]
    pub canonical_outcomes: Option<SmolStr>,
    #[serde(rename = "outcomePrices", default)]
    pub outcome_prices: Option<SmolStr>,
    #[serde(rename = "clobTokenIds", default)]
    pub clob_token_ids: Option<GammaIdList>,
    #[serde(rename = "positionIds", default)]
    pub position_ids: Option<GammaIdList>,
    #[serde(rename = "groupItemTitle", default)]
    pub group_item_title: Option<SmolStr>,
    #[serde(rename = "sportsMarketType", default)]
    pub sports_market_type: Option<SmolStr>,
    #[serde(default)]
    pub closed: bool,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default)]
    pub version: Option<SmolStr>,
}

/// Gamma omits `active` on a few legacy documents; such a market is still live.
fn default_true() -> bool {
    true
}

impl GammaMarket {
    /// Replaces this market's display text with a localized copy.
    ///
    /// The canonical outcomes are captured before the localized outcomes replace
    /// them. Everything that decides a market's shape keeps reading
    /// [`Self::canonical_outcomes`].
    pub fn apply_localized(&mut self, localized: Self) {
        if self.canonical_outcomes.is_none() {
            self.canonical_outcomes = self.outcomes.take();
        }
        if !localized.question.is_empty() {
            self.question = localized.question;
        }
        if localized.group_item_title.is_some() {
            self.group_item_title = localized.group_item_title;
        }
        if localized.outcomes.is_some() {
            self.outcomes = localized.outcomes;
        }
    }
}

/// An id collection Gamma may publish either as a JSON array or as its string
/// encoding. The two forms occur in the same response.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum GammaIdList {
    String(SmolStr),
    List(Vec<SmolStr>),
}

impl GammaIdList {
    /// Returns the ids in either wire form.
    pub fn values(&self) -> Vec<SmolStr> {
        match self {
            Self::String(value) => serde_json::from_str(value.as_str()).unwrap_or_default(),
            Self::List(values) => values.clone(),
        }
    }
}

/// One CLOB batch history answer.
#[derive(Clone, Debug, Deserialize)]
pub struct ClobBatchHistory {
    #[serde(default)]
    pub history: BTreeMap<SmolStr, Vec<ClobHistoryPoint>>,
}

/// One point of CLOB price history.
#[derive(Clone, Debug, Deserialize)]
pub struct ClobHistoryPoint {
    /// Unix seconds.
    pub t: i64,
    pub p: f64,
}

/// The body of one CLOB batch history request.
#[derive(Clone, Debug, Serialize)]
pub struct ClobBatchHistoryRequest<'a> {
    pub markets: &'a [SmolStr],
    pub interval: &'a str,
    pub fidelity: u16,
}

/// One token of a CLOB midpoint request.
#[derive(Clone, Debug, Serialize)]
pub struct ClobMidpointRequest<'a> {
    pub token_id: &'a str,
}

/// One CLOB midpoint answer: asset id to probability.
pub type ClobMidpoints = BTreeMap<SmolStr, SmolStr>;

/// One market-channel subscription request.
#[derive(Clone, Debug, Serialize)]
pub struct PolymarketMarketSubscribe<'a> {
    pub assets_ids: &'a [SmolStr],
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub custom_feature_enabled: bool,
}

/// One market-channel unsubscribe request.
#[derive(Clone, Debug, Serialize)]
pub struct PolymarketMarketUnsubscribe<'a> {
    pub assets_ids: &'a [SmolStr],
    pub operation: &'static str,
}

/// One market-channel frame: the venue may send one event or a batch.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum PolymarketMarketFrame {
    One(PolymarketMarketEvent),
    Many(Vec<PolymarketMarketEvent>),
}

/// One event of the CLOB market channel.
///
/// Only the events that move a probability are stated; every other event the
/// venue publishes is the `Unknown` arm and is ignored.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "event_type")]
pub enum PolymarketMarketEvent {
    #[serde(rename = "book")]
    Book {
        #[serde(rename = "asset_id")]
        asset_id: SmolStr,
        #[serde(default)]
        bids: Vec<PolymarketBookLevel>,
        #[serde(default)]
        asks: Vec<PolymarketBookLevel>,
        #[serde(default, deserialize_with = "optional_millis")]
        timestamp: Option<UnixMillis>,
    },
    #[serde(rename = "price_change")]
    PriceChange {
        #[serde(default)]
        price_changes: Vec<PolymarketPriceChange>,
        #[serde(default, deserialize_with = "optional_millis")]
        timestamp: Option<UnixMillis>,
    },
    #[serde(rename = "best_bid_ask")]
    BestBidAsk {
        #[serde(rename = "asset_id")]
        asset_id: SmolStr,
        #[serde(default)]
        best_bid: Option<SmolStr>,
        #[serde(default)]
        best_ask: Option<SmolStr>,
        #[serde(default, deserialize_with = "optional_millis")]
        timestamp: Option<UnixMillis>,
    },
    #[serde(other)]
    Unknown,
}

/// One level of a CLOB book snapshot.
#[derive(Clone, Debug, Deserialize)]
pub struct PolymarketBookLevel {
    pub price: SmolStr,
    pub size: SmolStr,
}

/// One changed level of a CLOB book.
#[derive(Clone, Debug, Deserialize)]
pub struct PolymarketPriceChange {
    #[serde(rename = "asset_id")]
    pub asset_id: SmolStr,
    #[serde(default)]
    pub best_bid: Option<SmolStr>,
    #[serde(default)]
    pub best_ask: Option<SmolStr>,
}

/// Reads a timestamp the venue spells as either a decimal string or a number.
fn optional_millis<'de, D>(deserializer: D) -> Result<Option<UnixMillis>, D::Error>
where
    D: Deserializer<'de>,
{
    struct MillisVisitor;

    impl<'de> Visitor<'de> for MillisVisitor {
        type Value = Option<UnixMillis>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a millisecond timestamp as a string, an integer or null")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserializer.deserialize_any(MillisVisitor)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(None)
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            value.parse().map(Some).map_err(E::custom)
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Some(value))
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            i64::try_from(value).map(Some).map_err(E::custom)
        }
    }

    deserializer.deserialize_option(MillisVisitor)
}

#[cfg(test)]
mod tests {
    use super::{GammaEvent, GammaId, GammaSearchResponse};

    #[test]
    fn a_parent_event_id_is_read_as_either_a_string_or_a_number() {
        let numeric: GammaEvent = serde_json::from_str(
            r#"{"id":"1","title":"child","parentEventId":1182413,"markets":[]}"#,
        )
        .expect("a numeric parent id should decode");
        assert!(matches!(
            numeric.parent_event_id,
            Some(GammaId::Integer(1182413))
        ));

        let textual: GammaEvent = serde_json::from_str(
            r#"{"id":"2","title":"child","parentEventId":"1182413","markets":[]}"#,
        )
        .expect("a textual parent id should decode");
        assert!(matches!(
            textual.parent_event_id,
            Some(GammaId::String(ref id)) if id == "1182413"
        ));
    }

    #[test]
    fn a_search_answer_without_events_reads_as_an_empty_page() {
        let answer: GammaSearchResponse =
            serde_json::from_str(r#"{"pagination":{"hasMore":false,"totalResults":0}}"#)
                .expect("an empty search answer should decode");

        assert!(answer.events.is_empty());
        assert_eq!(
            answer
                .pagination
                .and_then(|pagination| pagination.total_results),
            Some(0)
        );
    }
}

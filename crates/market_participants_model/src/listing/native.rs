use serde::{Deserialize, Serialize};

use crate::{
    identifier::{
        CtfConditionId, CtfTokenId, EquityLocalCode, HyperliquidQuestionId, LocalSymbol,
        MarketParticipantSeriesId, Mic, NativeSymbol, OccSymbol, PolymarketEventId,
        PolymarketMarketId, VenueScopeId,
    },
    listing::{
        continuity_series::FutureSeriesIdentifiers,
        route::VenueListingKey,
        venue::{
            BinanceProduct, HyperliquidAssetId, HyperliquidAssetProduct, HyperliquidOutcomeId,
            OkxInstrumentType,
        },
    },
    outcome::{OutcomeEventId, OutcomeGroupId},
    routing::ListingIdentifierKind,
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "identifier")]
pub enum ListingIdentifier {
    #[serde(rename = "symbol")]
    Symbol(NativeSymbol),
    #[serde(rename = "binance_instrument")]
    BinanceInstrument {
        #[serde(rename = "symbol")]
        symbol: NativeSymbol,
        #[serde(rename = "product")]
        product: BinanceProduct,
    },
    #[serde(rename = "okx_instrument")]
    OkxInstrument {
        #[serde(rename = "instrument_id")]
        instrument_id: NativeSymbol,
        #[serde(rename = "instrument_type")]
        instrument_type: OkxInstrumentType,
    },
    #[serde(rename = "hyperliquid_asset")]
    HyperliquidAsset {
        #[serde(rename = "scope")]
        scope: VenueScopeId,
        #[serde(rename = "product")]
        product: HyperliquidAssetProduct,
        #[serde(rename = "asset_id")]
        asset_id: HyperliquidAssetId,
    },
    #[serde(rename = "hyperliquid_outcome")]
    HyperliquidOutcome {
        #[serde(rename = "scope")]
        scope: VenueScopeId,
        #[serde(rename = "outcome_id")]
        outcome_id: HyperliquidOutcomeId,
        #[serde(rename = "asset_id")]
        asset_id: HyperliquidAssetId,
    },
    #[serde(rename = "polymarket_token")]
    PolymarketToken(CtfTokenId),
    #[serde(rename = "equity_code")]
    EquityCode {
        #[serde(rename = "mic")]
        mic: Mic,
        #[serde(rename = "local_code")]
        local_code: EquityLocalCode,
    },
    #[serde(rename = "future_series")]
    FutureSeries(FutureSeriesIdentifiers),
    #[serde(rename = "occ_symbol")]
    OccSymbol(OccSymbol),
    #[serde(rename = "market_participants_series")]
    MarketParticipantSeries(MarketParticipantSeriesId),
    #[serde(rename = "option_local_symbol")]
    OptionLocalSymbol(LocalSymbol),
}

impl ListingIdentifier {
    pub fn kind(&self) -> ListingIdentifierKind {
        match self {
            Self::Symbol(_) => ListingIdentifierKind::Symbol,
            Self::BinanceInstrument { .. } => ListingIdentifierKind::BinanceInstrument,
            Self::OkxInstrument { .. } => ListingIdentifierKind::OkxInstrument,
            Self::HyperliquidAsset { .. } => ListingIdentifierKind::HyperliquidAsset,
            Self::HyperliquidOutcome { .. } => ListingIdentifierKind::HyperliquidOutcome,
            Self::PolymarketToken(_) => ListingIdentifierKind::PolymarketToken,
            Self::EquityCode { .. } => ListingIdentifierKind::EquityLocalCode,
            Self::FutureSeries(_) => ListingIdentifierKind::FutureSeries,
            Self::OccSymbol(_) => ListingIdentifierKind::OccSymbol,
            Self::MarketParticipantSeries(_) => ListingIdentifierKind::MarketParticipantSeries,
            Self::OptionLocalSymbol(_) => ListingIdentifierKind::OptionLocalSymbol,
        }
    }

    pub fn symbol_value(&self) -> Option<&str> {
        match self {
            Self::Symbol(symbol)
            | Self::BinanceInstrument { symbol, .. }
            | Self::OkxInstrument {
                instrument_id: symbol,
                ..
            } => Some(symbol.as_str()),
            Self::EquityCode { local_code, .. } => Some(local_code.as_str()),
            Self::OccSymbol(symbol) => Some(symbol.as_str()),
            Self::MarketParticipantSeries(series) => Some(series.as_str()),
            Self::OptionLocalSymbol(symbol) => Some(symbol.as_str()),
            Self::HyperliquidAsset { .. }
            | Self::HyperliquidOutcome { .. }
            | Self::PolymarketToken(_)
            | Self::FutureSeries(_) => None,
        }
    }

    pub fn scope(&self) -> Option<&VenueScopeId> {
        match self {
            Self::HyperliquidAsset { scope, .. } | Self::HyperliquidOutcome { scope, .. } => {
                Some(scope)
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "identifier")]
pub enum NativeIdentifier {
    #[serde(rename = "listing")]
    Listing(ListingIdentifier),
    #[serde(rename = "group")]
    Group(OutcomeGroupIdentifier),
    #[serde(rename = "event")]
    Event(OutcomeEventIdentifier),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "identifier")]
pub enum OutcomeGroupIdentifier {
    #[serde(rename = "hyperliquid_question")]
    HyperliquidQuestion {
        #[serde(rename = "scope")]
        scope: VenueScopeId,
        #[serde(rename = "question")]
        question: HyperliquidQuestionId,
    },
    #[serde(rename = "polymarket_market")]
    PolymarketMarket(PolymarketMarketId),
    #[serde(rename = "polymarket_condition")]
    PolymarketCondition(CtfConditionId),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "identifier")]
pub enum OutcomeEventIdentifier {
    #[serde(rename = "polymarket_event")]
    PolymarketEvent(PolymarketEventId),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "resolved")]
pub enum NativeIdentifierResolution {
    #[serde(rename = "listing")]
    Listing(VenueListingKey),
    #[serde(rename = "outcome_group")]
    OutcomeGroup(OutcomeGroupId),
    #[serde(rename = "outcome_event")]
    OutcomeEvent(OutcomeEventId),
}

impl NativeIdentifierResolution {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Listing(_) => "listing",
            Self::OutcomeGroup(_) => "outcome-group",
            Self::OutcomeEvent(_) => "outcome-event",
        }
    }
}

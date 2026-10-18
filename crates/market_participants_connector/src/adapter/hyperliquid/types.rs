//! The typed wire shapes of the Hyperliquid API.
//!
//! Every request that addresses one perpetual DEX carries the DEX name the
//! caller derived from its route scope; the empty string is Hyperliquid's own
//! spelling of "the first perp DEX", which is why the field is optional and
//! omitted for the core scope instead of being written as a placeholder.

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidSubscribe<T> {
    pub method: SmolStr,
    pub subscription: T,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidWirePost<T> {
    pub method: SmolStr,
    pub id: u64,
    pub request: HyperliquidPostRequest<T>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidPostRequest<T> {
    pub r#type: SmolStr,
    pub payload: T,
}

/// One `info` request of the Hyperliquid API.
///
/// `dex` is the name of a builder-deployed perpetual DEX; it is absent for the
/// core scope, which Hyperliquid addresses as the first perp DEX.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum HyperliquidInfoRequest {
    #[serde(rename = "candleSnapshot")]
    CandleSnapshot {
        req: HyperliquidCandleSnapshotRequest,
    },
    #[serde(rename = "recentTrades")]
    RecentTrades { coin: SmolStr },
    #[serde(rename = "l2Book")]
    L2Book { coin: SmolStr },
    #[serde(rename = "spotClearinghouseState")]
    SpotClearinghouseState { user: SmolStr },
    #[serde(rename = "clearinghouseState")]
    ClearinghouseState {
        user: SmolStr,
        #[serde(skip_serializing_if = "Option::is_none")]
        dex: Option<SmolStr>,
    },
    #[serde(rename = "frontendOpenOrders")]
    FrontendOpenOrders {
        user: SmolStr,
        #[serde(skip_serializing_if = "Option::is_none")]
        dex: Option<SmolStr>,
    },
    #[serde(rename = "historicalOrders")]
    HistoricalOrders { user: SmolStr },
    #[serde(rename = "userFills")]
    UserFills {
        user: SmolStr,
        #[serde(rename = "aggregateByTime", skip_serializing_if = "Option::is_none")]
        aggregate_by_time: Option<bool>,
    },
    #[serde(rename = "userFillsByTime")]
    UserFillsByTime {
        user: SmolStr,
        #[serde(rename = "startTime")]
        start_time: i64,
        #[serde(rename = "endTime")]
        end_time: i64,
        #[serde(rename = "aggregateByTime")]
        aggregate_by_time: bool,
    },
    #[serde(rename = "userNonFundingLedgerUpdates")]
    UserNonFundingLedgerUpdates {
        user: SmolStr,
        #[serde(rename = "startTime")]
        start_time: i64,
    },
    #[serde(rename = "spotMeta")]
    SpotMeta,
    #[serde(rename = "spotMetaAndAssetCtxs")]
    SpotMetaAndAssetCtxs,
    #[serde(rename = "meta")]
    Meta {
        #[serde(skip_serializing_if = "Option::is_none")]
        dex: Option<SmolStr>,
    },
    #[serde(rename = "metaAndAssetCtxs")]
    MetaAndAssetCtxs {
        #[serde(skip_serializing_if = "Option::is_none")]
        dex: Option<SmolStr>,
    },
    #[serde(rename = "perpDexs")]
    PerpDexs,
    #[cfg(test)]
    #[serde(rename = "outcomeMeta")]
    OutcomeMeta,
    #[cfg(test)]
    #[serde(rename = "settledOutcome")]
    SettledOutcome { outcome: u64 },
}

impl HyperliquidInfoRequest {
    /// The weight the venue charges for one request of this kind.
    ///
    /// Hyperliquid aggregates every REST request of one IP into a budget of 1200
    /// weight per minute, and publishes what each kind costs: `clearinghouseState`
    /// and `spotClearinghouseState` are 2, `l2Book` is 2, `userRole` is 60, and
    /// every other documented request is 20. The per-item surcharge the venue adds
    /// to `userFills` and its siblings is deliberately not modelled here: it is
    /// decided by the answer's size, and the budget keeps enough headroom under
    /// the venue's own cap to absorb it.
    pub(crate) const fn weight(&self) -> u32 {
        match self {
            Self::L2Book { .. }
            | Self::SpotClearinghouseState { .. }
            | Self::ClearinghouseState { .. } => 2,
            Self::CandleSnapshot { .. }
            | Self::RecentTrades { .. }
            | Self::FrontendOpenOrders { .. }
            | Self::HistoricalOrders { .. }
            | Self::UserFills { .. }
            | Self::UserFillsByTime { .. }
            | Self::UserNonFundingLedgerUpdates { .. }
            | Self::SpotMeta
            | Self::SpotMetaAndAssetCtxs
            | Self::Meta { .. }
            | Self::MetaAndAssetCtxs { .. }
            | Self::PerpDexs => 20,
            // The two requests only a test can make are not in the venue's table.
            #[cfg(test)]
            Self::OutcomeMeta | Self::SettledOutcome { .. } => 20,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidCandleSnapshotRequest {
    pub coin: SmolStr,
    pub interval: SmolStr,
    #[serde(rename = "startTime")]
    pub start_time: i64,
    #[serde(rename = "endTime")]
    pub end_time: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidCandle {
    pub t: i64,
    pub o: SmolStr,
    pub h: SmolStr,
    pub l: SmolStr,
    pub c: SmolStr,
    pub v: SmolStr,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum HyperliquidSubscription {
    #[serde(rename = "candle")]
    Candle { coin: SmolStr, interval: SmolStr },
    #[serde(rename = "trades")]
    Trades { coin: SmolStr },
    #[serde(rename = "activeAssetCtx")]
    ActiveAssetCtx { coin: SmolStr },
    #[serde(rename = "l2Book")]
    L2Book { coin: SmolStr },
    #[serde(rename = "userFills")]
    UserFills { user: SmolStr },
    #[serde(rename = "userNonFundingLedgerUpdates")]
    UserNonFundingLedgerUpdates { user: SmolStr },
    #[serde(rename = "fastAssetCtxs")]
    FastAssetCtxs,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidOrderAction {
    pub r#type: SmolStr,
    pub orders: Vec<HyperliquidOrder>,
    pub grouping: SmolStr,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidOrder {
    #[serde(rename = "a")]
    pub asset: u32,
    #[serde(rename = "b")]
    pub is_buy: bool,
    #[serde(rename = "s")]
    pub sz: SmolStr,
    #[serde(rename = "p")]
    pub limit_px: SmolStr,
    #[serde(rename = "t")]
    pub order_type: HyperliquidOrderType,
    #[serde(rename = "r")]
    pub reduce_only: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum HyperliquidOrderType {
    #[serde(rename = "limit")]
    Limit(HyperliquidLimitOrderType),
    #[serde(rename = "trigger")]
    Trigger(HyperliquidTriggerOrderType),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidLimitOrderType {
    #[serde(rename = "tif")]
    pub time_in_force: SmolStr,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidTriggerOrderType {
    #[serde(rename = "isMarket")]
    pub is_market: bool,
    #[serde(rename = "triggerPx")]
    pub trigger_price: SmolStr,
    pub tpsl: SmolStr,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidCancelAction {
    pub r#type: SmolStr,
    pub cancels: Vec<HyperliquidCancel>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidCancel {
    #[serde(rename = "a")]
    pub asset: u32,
    #[serde(rename = "o")]
    pub oid: u64,
}

/// One perpetual DEX as the venue publishes it.
///
/// The first element of the venue's answer is `null` and stands for the core
/// perpetual DEX, which is addressed as the first perp DEX rather than by name.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidPerpDex {
    pub name: SmolStr,
    #[serde(rename = "fullName", default)]
    pub full_name: Option<SmolStr>,
}

/// The perpetual metadata of one DEX: its assets, its margin tables and the
/// spot token it settles in.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidMetaResponse {
    pub universe: Vec<HyperliquidMetaAsset>,
    #[serde(rename = "marginTables", default)]
    pub margin_tables: Vec<HyperliquidMarginTableEntry>,
    /// The spot token index this DEX settles in, as the venue publishes it for
    /// builder-deployed DEXes and for the core DEX alike.
    #[serde(rename = "collateralToken", default)]
    pub collateral_token: Option<u32>,
}

/// One `[id, table]` pair of the venue's margin table list.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidMarginTableEntry(pub u32, pub HyperliquidMarginTable);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidMarginTable {
    #[serde(default)]
    pub description: SmolStr,
    #[serde(rename = "marginTiers", default)]
    pub margin_tiers: Vec<HyperliquidMarginTier>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidMarginTier {
    #[serde(rename = "lowerBound")]
    pub lower_bound: SmolStr,
    #[serde(rename = "maxLeverage")]
    pub max_leverage: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidMetaAsset {
    pub name: SmolStr,
    #[serde(rename = "szDecimals")]
    pub sz_decimals: u8,
    #[serde(rename = "maxLeverage", default)]
    pub max_leverage: u32,
    #[serde(rename = "isDelisted", default)]
    pub is_delisted: bool,
    /// How margin may be added to and removed from a position of this asset.
    #[serde(rename = "marginMode", default)]
    pub margin_mode: Option<HyperliquidMarginMode>,
    /// The venue's margin table this asset is margined by.
    #[serde(rename = "marginTableId", default)]
    pub margin_table_id: Option<u32>,
}

/// The venue's own margin-removal rule of one asset.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum HyperliquidMarginMode {
    /// Margin cannot be removed.
    #[serde(rename = "strictIsolated")]
    StrictIsolated,
    /// Only isolated margin is allowed.
    #[serde(rename = "noCross")]
    NoCross,
    #[serde(rename = "normal")]
    Normal,
}

/// The spot metadata of the venue: its tokens and its tradable pairs.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidSpotMetaResponse {
    pub tokens: Vec<HyperliquidSpotToken>,
    pub universe: Vec<HyperliquidSpotPairWire>,
}

/// One spot token, as the venue indexes it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidSpotToken {
    pub name: SmolStr,
    #[serde(rename = "szDecimals")]
    pub sz_decimals: u8,
    pub index: u32,
}

/// One spot pair, named by the two token indices it exchanges.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidSpotPairWire {
    pub name: SmolStr,
    pub tokens: [u32; 2],
    pub index: u32,
}

/// The market numbers Hyperliquid publishes for one instrument.
///
/// The same shape arrives from the `activeAssetCtx` subscription and from the
/// `metaAndAssetCtxs` and `spotMetaAndAssetCtxs` queries. A price the market does
/// not quote is reported as null, and the venue documents the traded base amount
/// for some markets only, so every number here is optional.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct HyperliquidActiveContextValues {
    #[serde(rename = "prevDayPx")]
    pub prev_day_px: Option<SmolStr>,
    #[serde(rename = "dayNtlVlm")]
    pub day_ntl_vlm: Option<SmolStr>,
    #[serde(rename = "dayBaseVlm")]
    pub day_base_vlm: Option<SmolStr>,
    #[serde(rename = "markPx")]
    pub mark_px: Option<SmolStr>,
    #[serde(rename = "midPx")]
    pub mid_px: Option<SmolStr>,
}

/// One outcome of the venue's outcome metadata.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg(test)]
pub struct HyperliquidOutcomeSpec {
    pub outcome: u64,
    pub name: SmolStr,
    pub description: SmolStr,
    #[serde(rename = "sideSpecs")]
    pub side_specs: Vec<HyperliquidOutcomeSideSpec>,
}

/// One tradable side of an outcome, named by the venue.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg(test)]
pub struct HyperliquidOutcomeSideSpec {
    pub name: SmolStr,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg(test)]
pub struct HyperliquidOutcomeMetaResponse {
    pub outcomes: Vec<HyperliquidOutcomeSpec>,
}

/// The settlement of one outcome, as the venue reports it once it is settled.
///
/// The quote token is published here and only here: an unsettled outcome's
/// settlement asset cannot be read from the outcome metadata, so it is not
/// invented.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg(test)]
pub struct HyperliquidSettledOutcome {
    pub spec: HyperliquidSettledOutcomeSpec,
    #[serde(rename = "settleFraction")]
    pub settle_fraction: SmolStr,
    #[serde(default)]
    pub details: Option<SmolStr>,
}

/// The settled spec of one outcome, which repeats its metadata and adds the
/// token the outcome settles in.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg(test)]
pub struct HyperliquidSettledOutcomeSpec {
    pub outcome: u64,
    pub name: SmolStr,
    pub description: SmolStr,
    #[serde(rename = "sideSpecs")]
    pub side_specs: Vec<HyperliquidOutcomeSideSpec>,
    #[serde(rename = "quoteToken")]
    pub quote_token: SmolStr,
}

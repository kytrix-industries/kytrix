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
    #[serde(rename = "outcomeMeta")]
    OutcomeMeta,
    #[serde(rename = "outcomeTemplates")]
    OutcomeTemplates,
    #[serde(rename = "allMids")]
    AllMids,
    #[cfg(test)]
    #[serde(rename = "settledOutcome")]
    SettledOutcome { outcome: u64 },
}

impl HyperliquidInfoRequest {
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
            | Self::PerpDexs
            | Self::OutcomeMeta
            | Self::OutcomeTemplates => 20,
            Self::AllMids => 2,
            #[cfg(test)]
            Self::SettledOutcome { .. } => 20,
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
    #[serde(rename = "allMids")]
    AllMids,
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidPerpDex {
    pub name: SmolStr,
    #[serde(rename = "fullName", default)]
    pub full_name: Option<SmolStr>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidMetaResponse {
    pub universe: Vec<HyperliquidMetaAsset>,
    #[serde(rename = "marginTables", default)]
    pub margin_tables: Vec<HyperliquidMarginTableEntry>,
    #[serde(rename = "collateralToken", default)]
    pub collateral_token: Option<u32>,
}

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
    #[serde(rename = "marginMode", default)]
    pub margin_mode: Option<HyperliquidMarginMode>,
    #[serde(rename = "marginTableId", default)]
    pub margin_table_id: Option<u32>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum HyperliquidMarginMode {
    #[serde(rename = "strictIsolated")]
    StrictIsolated,
    #[serde(rename = "noCross")]
    NoCross,
    #[serde(rename = "normal")]
    Normal,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidSpotMetaResponse {
    pub tokens: Vec<HyperliquidSpotToken>,
    pub universe: Vec<HyperliquidSpotPairWire>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidSpotToken {
    pub name: SmolStr,
    #[serde(rename = "szDecimals")]
    pub sz_decimals: u8,
    pub index: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidSpotPairWire {
    pub name: SmolStr,
    pub tokens: [u32; 2],
    pub index: u32,
}

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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidOutcomeSpec {
    pub outcome: u64,
    pub name: SmolStr,
    pub description: SmolStr,
    #[serde(rename = "sideSpecs")]
    pub side_specs: Vec<HyperliquidOutcomeSideSpec>,
    #[serde(rename = "quoteToken", default)]
    pub quote_token: Option<SmolStr>,
    #[serde(default)]
    pub venue: Option<SmolStr>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidOutcomeSideSpec {
    pub name: SmolStr,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidOutcomeQuestion {
    pub question: u64,
    pub name: SmolStr,
    pub description: SmolStr,
    #[serde(rename = "fallbackOutcome")]
    pub fallback_outcome: u64,
    #[serde(rename = "namedOutcomes", default)]
    pub named_outcomes: Vec<u64>,
    #[serde(rename = "settledNamedOutcomes", default)]
    pub settled_named_outcomes: Vec<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidOutcomeMetaResponse {
    #[serde(default)]
    pub outcomes: Vec<HyperliquidOutcomeSpec>,
    #[serde(default)]
    pub questions: Vec<HyperliquidOutcomeQuestion>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidOutcomeTemplate {
    pub id: SmolStr,
    pub role: HyperliquidOutcomeTemplateRole,
    pub name: SmolStr,
    #[serde(default)]
    pub description: SmolStr,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum HyperliquidOutcomeTemplateRole {
    Kind(SmolStr),
    Detailed {
        #[serde(rename = "standaloneOutcome", default)]
        standalone_outcome: Option<HyperliquidStandaloneOutcomeRole>,
        #[serde(rename = "questionOutcome", default)]
        question_outcome: Option<HyperliquidQuestionOutcomeRole>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidStandaloneOutcomeRole {
    #[serde(rename = "sideNames", default)]
    pub side_names: Vec<SmolStr>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperliquidQuestionOutcomeRole {
    #[serde(default)]
    pub parent: SmolStr,
}

pub type HyperliquidAllMids = std::collections::BTreeMap<SmolStr, SmolStr>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg(test)]
pub struct HyperliquidSettledOutcome {
    pub spec: HyperliquidSettledOutcomeSpec,
    #[serde(rename = "settleFraction")]
    pub settle_fraction: SmolStr,
    #[serde(default)]
    pub details: Option<SmolStr>,
}

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

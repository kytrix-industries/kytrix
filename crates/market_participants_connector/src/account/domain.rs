use std::num::NonZeroU8;

use market_participants_model::{
    asset::AssetId,
    decimal::{Price, ProfitOrLoss, Quantity, SignedQuantity, UnixMillis},
    identifier::WalletAddress,
    listing::route::VenueListingKey,
    market::{
        account::AssetAmount,
        price::MarketPrice,
        units::{MarketQuantity, PositionSide, Side},
    },
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::account::shared::{
    AccountTradeId, HistoricalPositionStatus, HyperliquidFillDirection, LiquidityRole, MarginMode,
    OrderId, OrderKind, OrderStatus, PositionDirection, PositionId,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    pub listing: VenueListingKey,
    pub id: OrderId,
    pub side: Side,
    pub position_side: Option<PositionSide>,
    pub status: OrderStatus,
    pub kind: OrderKind,
    pub quantity: MarketQuantity,
    pub filled_quantity: MarketQuantity,
    pub average_filled_price: Option<MarketPrice>,
    pub reduce_only: bool,
    pub margin_mode: Option<MarginMode>,
    pub leverage: Option<NonZeroU8>,
    pub create_timestamp: UnixMillis,
    pub update_timestamp: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub listing: VenueListingKey,
    pub id: Option<PositionId>,
    pub position_side: PositionSide,
    pub direction: PositionDirection,
    pub margin_mode: Option<MarginMode>,
    pub leverage: Option<NonZeroU8>,
    pub quantity: MarketQuantity,
    pub entry_price: MarketPrice,
    pub break_even_price: Option<MarketPrice>,
    pub mark_price: MarketPrice,
    pub unrealized_profit_or_loss: AssetAmount,
    pub liquidation_price: Option<MarketPrice>,
    pub notional: MarketQuantity,
    pub initial_margin: AssetAmount,
    pub maintenance_margin: Option<AssetAmount>,
    pub update_timestamp: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountTrade {
    pub listing: VenueListingKey,
    pub id: AccountTradeId,
    pub order_id: OrderId,
    pub side: Side,
    pub price: MarketPrice,
    pub quantity: MarketQuantity,
    pub fee: AssetAmount,
    pub liquidity: Option<LiquidityRole>,
    pub timestamp: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalPosition {
    pub listing: VenueListingKey,
    pub id: PositionId,
    pub direction: PositionDirection,
    pub status: HistoricalPositionStatus,
    pub margin_mode: Option<MarginMode>,
    pub leverage: Option<NonZeroU8>,
    pub quantity: MarketQuantity,
    pub maximum_quantity: MarketQuantity,
    pub closed_quantity: MarketQuantity,
    pub entry_price: MarketPrice,
    pub exit_price: MarketPrice,
    pub realized_profit_or_loss: AssetAmount,
    pub realized_return: Option<ProfitOrLoss>,
    pub opened_timestamp: UnixMillis,
    pub closed_timestamp: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MutationOutcome<T> {
    Confirmed(T),
    Rejected { code: SmolStr, message: SmolStr },
    Unknown { operation: SmolStr, reason: SmolStr },
}

impl<T> MutationOutcome<T> {
    pub fn confirmed(value: T) -> Self {
        Self::Confirmed(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperliquidSpotBalance {
    pub asset: AssetId,
    pub total: SignedQuantity,
    pub hold: SignedQuantity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperliquidSpotState {
    pub balances: Vec<HyperliquidSpotBalance>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperliquidSpotPair {
    pub coin: SmolStr,
    pub base_token: AssetId,
    pub quote_token: AssetId,
    pub asset_id: market_participants_model::listing::venue::HyperliquidAssetId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperliquidAssetContext {
    pub listing: VenueListingKey,
    pub mark_price: Option<MarketPrice>,
    pub mid_price: Option<MarketPrice>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperliquidPerpetualPosition {
    pub listing: VenueListingKey,
    pub size: MarketQuantity,
    pub direction: PositionDirection,
    pub entry_price: Option<MarketPrice>,
    pub mark_price: Option<MarketPrice>,
    pub liquidation_price: Option<MarketPrice>,
    pub position_value: Option<AssetAmount>,
    pub unrealized_pnl: AssetAmount,
    pub margin: Option<AssetAmount>,
    pub funding_since_open: AssetAmount,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperliquidPerpetualState {
    pub positions: Vec<HyperliquidPerpetualPosition>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperliquidOpenOrder {
    pub listing: VenueListingKey,
    pub side: Side,
    pub quantity: MarketQuantity,
    pub price: MarketPrice,
    pub status: OrderStatus,
    pub kind: OrderKind,
    pub reduce_only: bool,
    pub timestamp: UnixMillis,
    pub order_id: SmolStr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperliquidHistoricalOrder {
    pub listing: VenueListingKey,
    pub side: Side,
    pub quantity: MarketQuantity,
    pub price: MarketPrice,
    pub kind: OrderKind,
    pub reduce_only: bool,
    pub status: SmolStr,
    pub timestamp: UnixMillis,
    pub order_id: SmolStr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperliquidUserFill {
    pub address: WalletAddress,
    pub listing: VenueListingKey,
    pub side: Side,
    pub quantity: MarketQuantity,
    pub price: MarketPrice,
    pub timestamp: UnixMillis,
    pub start_position: SignedQuantity,
    pub closed_pnl: AssetAmount,
    pub fee: AssetAmount,
    pub hash: Option<SmolStr>,
    pub tid: u64,
    pub liquidation: bool,
    #[serde(default)]
    pub direction: Option<HyperliquidFillDirection>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperliquidLedgerEvent {
    pub address: WalletAddress,
    pub timestamp: UnixMillis,
    pub hash: SmolStr,
    pub asset: Option<AssetId>,
    pub amount: Option<AssetAmount>,
    pub kind: SmolStr,
    pub value: Option<AssetAmount>,
    pub transfer_direction: Option<HyperliquidTransferDirection>,
    pub related_wallet: Option<WalletAddress>,
    pub liquidated_notional: Option<AssetAmount>,
    pub liquidated_positions: Vec<HyperliquidLiquidatedPosition>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HyperliquidTransferDirection {
    Incoming,
    Outgoing,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperliquidLiquidatedPosition {
    pub listing: VenueListingKey,
    pub size: MarketQuantity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HyperliquidWalletUpdate {
    UserFills {
        is_snapshot: bool,
        fills: Vec<HyperliquidUserFill>,
    },
    UserLedger {
        is_snapshot: bool,
        events: Vec<HyperliquidLedgerEvent>,
    },
    FastAssetContexts {
        contexts: Vec<HyperliquidAssetContext>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountUnits {
    quote_asset: AssetId,
    basis: market_participants_model::listing::venue::PriceBasis,
    quantity: market_participants_model::market::units::QuantityUnit,
}

impl AccountUnits {
    pub(crate) fn of(rules: &market_participants_model::listing::rules::TradingRules) -> Self {
        Self {
            quote_asset: rules.common.price.quote_asset.clone(),
            basis: rules.common.price.basis.clone(),
            quantity: rules.common.quantity.unit.clone(),
        }
    }

    pub(crate) fn price(&self, value: Price) -> MarketPrice {
        MarketPrice {
            value,
            quote_asset: self.quote_asset.clone(),
            basis: self.basis.clone(),
        }
    }

    pub(crate) fn optional_price(&self, value: Option<Price>) -> Option<MarketPrice> {
        value.map(|value| self.price(value))
    }

    pub(crate) fn quantity(
        &self,
        value: Quantity,
    ) -> Result<MarketQuantity, market_participants_model::error::ModelError> {
        MarketQuantity::traded(value, self.quantity.clone())
    }

    pub(crate) fn notional(
        &self,
        value: Quantity,
    ) -> Result<MarketQuantity, market_participants_model::error::ModelError> {
        MarketQuantity::amount(
            value,
            market_participants_model::market::units::QuantityUnit::Notional {
                asset: self.quote_asset.clone(),
            },
        )
    }
    pub(crate) fn amount(&self, value: SignedQuantity) -> AssetAmount {
        AssetAmount {
            asset: self.quote_asset.clone(),
            value,
        }
    }
}

use std::num::NonZeroU8;

use market_participants_model::{
    asset::AssetId,
    decimal::{Price, ProfitOrLoss, Quantity, UnixMillis},
    listing::route::VenueListingKey,
    market::units::{PositionSide, Side},
};

use crate::account::shared::{
    AccountTradeId, HistoricalPositionStatus, LiquidityRole, MarginMode, OrderId, OrderKind,
    OrderStatus, PositionDirection, PositionId,
};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeOrder {
    pub listing: VenueListingKey,
    pub id: OrderId,
    pub side: Side,
    pub position_side: Option<PositionSide>,
    pub status: OrderStatus,
    pub kind: OrderKind,
    pub quantity: Quantity,
    pub filled_quantity: Quantity,
    pub average_filled_price: Option<Price>,
    pub reduce_only: bool,
    pub margin_mode: Option<MarginMode>,
    pub leverage: Option<NonZeroU8>,
    pub create_timestamp: UnixMillis,
    pub update_timestamp: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativePosition {
    pub listing: VenueListingKey,
    pub id: Option<PositionId>,
    pub position_side: PositionSide,
    pub direction: PositionDirection,
    pub margin_mode: Option<MarginMode>,
    pub leverage: Option<NonZeroU8>,
    pub quantity: Quantity,
    pub entry_price: Price,
    pub break_even_price: Option<Price>,
    pub mark_price: Price,
    pub unrealized_profit_or_loss: ProfitOrLoss,
    pub liquidation_price: Option<Price>,
    pub notional: Quantity,
    pub initial_margin: ProfitOrLoss,
    pub maintenance_margin: Option<ProfitOrLoss>,
    pub update_timestamp: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeAccountTrade {
    pub listing: VenueListingKey,
    pub id: AccountTradeId,
    pub order_id: OrderId,
    pub side: Side,
    pub price: Price,
    pub quantity: Quantity,
    pub fee_asset: AssetId,
    pub fee: ProfitOrLoss,
    pub liquidity: Option<LiquidityRole>,
    pub timestamp: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeBalance {
    pub asset: AssetId,
    pub total: Quantity,
    pub available: Quantity,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeHistoricalPosition {
    pub listing: VenueListingKey,
    pub id: PositionId,
    pub direction: PositionDirection,
    pub status: HistoricalPositionStatus,
    pub margin_mode: Option<MarginMode>,
    pub leverage: Option<NonZeroU8>,
    pub quantity: Quantity,
    pub maximum_quantity: Quantity,
    pub closed_quantity: Quantity,
    pub entry_price: Price,
    pub exit_price: Price,
    pub realized_profit_or_loss: ProfitOrLoss,
    pub realized_return: Option<ProfitOrLoss>,
    pub opened_timestamp: UnixMillis,
    pub closed_timestamp: UnixMillis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeOrderBookLevel {
    pub price: Price,
    pub quantity: Quantity,
}

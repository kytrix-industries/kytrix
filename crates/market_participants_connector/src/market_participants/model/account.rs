//! Protocol-level account payloads.
//!
//! Every payload keeps the venue's own value shape: a symbol the wire named, a
//! price, a quantity and the venue's own order-state label. Which listing a
//! payload is about is decided by the request that asked for it (or, for a
//! bulk event, by the coarse native identifier on the wrapper), so a payload
//! never carries a listing key it could contradict.

use std::num::NonZeroU8;

use market_participants_model::{
    asset::AssetId,
    decimal::{Price, ProfitOrLoss, Quantity, UnixMillis},
    listing::route::VenueListingKey,
    market::units::{PositionSide, Side},
};

use super::query::invalid_field;
use crate::account::shared::{
    AccountTradeId, HistoricalPositionStatus, LiquidityRole, MarginMode, OrderId, OrderKind,
    OrderStatus, PositionDirection, PositionId,
};

/// One protocol order.
///
/// An order is always about one listing, so it carries the key the venue's own
/// identifier resolved to; the account is the query's identity, not the
/// payload's.
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

/// One protocol position.
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

/// One protocol account trade.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeAccountTrade {
    pub listing: VenueListingKey,
    pub id: AccountTradeId,
    pub order_id: OrderId,
    pub side: Side,
    pub price: Price,
    pub quantity: Quantity,
    /// The canonical asset the fee was charged in.
    pub fee_asset: AssetId,
    pub fee: ProfitOrLoss,
    pub liquidity: Option<LiquidityRole>,
    pub timestamp: UnixMillis,
}

/// One protocol balance entry.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeBalance {
    /// The canonical asset this balance is held in.
    pub asset: AssetId,
    pub total: Quantity,
    pub available: Quantity,
}

/// One protocol historical position.
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

/// One native order-book level, before any unit is attached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeOrderBookLevel {
    pub price: Price,
    pub quantity: Quantity,
}

/// Validates that a native order-book level was published.
pub fn require_order_book_level(
    price: Price,
    quantity: Quantity,
) -> Result<NativeOrderBookLevel, market_participants_model::error::ModelError> {
    if price.is_zero() {
        return Err(invalid_field("order_book.price", "zero"));
    }
    if quantity.is_zero() {
        return Err(invalid_field("order_book.quantity", "zero"));
    }
    Ok(NativeOrderBookLevel { price, quantity })
}

//! OKX account payload decoders.
//!
//! Every decoder answers with the venue's own numbers paired with the canonical
//! asset or listing the venue named: a balance or a fee carries the asset its
//! currency code names, and an order, position or fill resolves its own
//! instrument to the listing key the rest of the crate works with.

use std::num::NonZeroU8;

use market_participants_model::{
    decimal::{Price, ProfitOrLoss, Quantity, SignedQuantity},
    listing::{
        route::{VenueListingKey, VenueRoute},
        venue::OkxInstrumentType,
    },
    market::units::{PositionSide, Side},
};
use serde::Deserialize;
use smol_str::SmolStr;

use super::CODEC;
use crate::{
    account::shared::{
        AccountTradeId, HistoricalPositionStatus, LiquidityRole, MarginMode, OrderId, OrderKind,
        OrderRoute, OrderStatus, PositionDirection, PositionId, PositionMode, TimeInForce,
        TriggerPriceKind,
    },
    adapter::ProtocolEvent,
    error::MarketParticipantError,
    market_participants::model::{
        AuthenticatedQuery, NativeAccountTrade, NativeBalance, NativeHistoricalPosition,
        NativeOrder, NativePosition,
    },
};

pub(super) const PAGE_LIMIT: u16 = 100;
const MAX_HISTORY_LIMIT: u16 = 10_000;

#[derive(Deserialize)]
pub(super) struct HttpResponse<T> {
    pub code: SmolStr,
    pub msg: Option<SmolStr>,
    pub data: Vec<T>,
}

#[derive(Deserialize)]
pub(super) struct ConfigRow {
    #[serde(rename = "posMode")]
    pub pos_mode: Option<SmolStr>,
}

/// Validates one authenticated history window and its row limit.
pub(super) fn validate_history(
    query: &AuthenticatedQuery,
    operation: &'static str,
) -> Result<(), MarketParticipantError> {
    query.validate().map_err(|error| {
        MarketParticipantError::Configuration(smol_str::format_smolstr!("{error}"))
    })?;
    if query.limit > MAX_HISTORY_LIMIT {
        return Err(MarketParticipantError::InvalidLimit {
            operation: operation.into(),
            requested: query.limit,
            maximum: MAX_HISTORY_LIMIT,
        });
    }
    Ok(())
}

/// Decodes the balance account of one OKX balance answer.
pub(super) fn decode_balances(
    accounts: &[OkxBalanceAccount],
) -> Result<Vec<NativeBalance>, MarketParticipantError> {
    let [account] = accounts else {
        return Err(protocol(
            "decode OKX balances",
            "expected exactly one balance account",
        ));
    };
    let mut balances = account
        .details
        .iter()
        .map(|item| {
            Ok(NativeBalance {
                asset: super::adapter::asset(&item.ccy).asset_id(),
                total: parse(&item.cash_bal, "decode OKX balance")?,
                available: optional_decimal(item.avail_bal.as_deref(), "decode OKX balance")?
                    .or(optional_decimal(
                        item.avail_eq.as_deref(),
                        "decode OKX balance",
                    )?)
                    .unwrap_or(Quantity::ZERO),
            })
        })
        .collect::<Result<Vec<_>, MarketParticipantError>>()?;
    balances.retain(|item| !item.total.is_zero() || !item.available.is_zero());
    balances.sort_unstable_by(|left, right| left.asset.cmp(&right.asset));
    Ok(balances)
}

/// Decodes a page of OKX orders, each under its own instrument's listing.
pub(super) fn decode_orders(
    route: &VenueRoute,
    instrument_type: OkxInstrumentType,
    order_route: OrderRoute,
    rows: Vec<OkxOrder>,
) -> Result<Vec<NativeOrder>, MarketParticipantError> {
    rows.into_iter()
        .map(|item| decode_order(route, instrument_type, order_route, item))
        .collect()
}

fn decode_order(
    route: &VenueRoute,
    instrument_type: OkxInstrumentType,
    order_route: OrderRoute,
    item: OkxOrder,
) -> Result<NativeOrder, MarketParticipantError> {
    let listing = CODEC.listing_key(route, instrument_type, item.inst_id.as_str())?;
    let raw_id = required(
        match order_route {
            OrderRoute::Normal => item.ord_id.as_deref(),
            OrderRoute::Algo => item.algo_id.as_deref(),
        }
        .unwrap_or_default(),
        "decode OKX order ID",
    )?;
    Ok(NativeOrder {
        listing,
        id: model(OrderId::new(raw_id, order_route), "decode OKX order ID")?,
        side: decode_side(&item.side)?,
        position_side: item
            .pos_side
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(decode_position_side)
            .transpose()?,
        status: decode_order_status(&item.state)?,
        kind: decode_order_kind(order_route, &item)?,
        quantity: parse(
            item.sz.as_deref().unwrap_or_default(),
            "decode OKX order quantity",
        )?,
        filled_quantity: optional_decimal(
            match order_route {
                OrderRoute::Normal => item.acc_fill_sz.as_deref(),
                OrderRoute::Algo => item.actual_sz.as_deref(),
            },
            "decode OKX filled quantity",
        )?
        .unwrap_or(Quantity::ZERO),
        average_filled_price: optional_decimal(
            match order_route {
                OrderRoute::Normal => item.avg_px.as_deref(),
                OrderRoute::Algo => item.actual_px.as_deref(),
            },
            "decode OKX average order price",
        )?
        .filter(|price: &Price| !price.is_zero()),
        reduce_only: decode_optional_bool(item.reduce_only.as_deref())?,
        margin_mode: decode_order_margin_mode(route, item.td_mode.as_deref())?,
        leverage: optional_nonzero(item.lever.as_deref())?,
        create_timestamp: parse(&item.c_time, "decode OKX order create timestamp")?,
        update_timestamp: parse(&item.u_time, "decode OKX order update timestamp")?,
    })
}

/// The margin mode one order was placed under.
///
/// A spot order carries OKX's cash mode, which is the absence of a perpetual
/// margin mode rather than a third one.
fn decode_order_margin_mode(
    route: &VenueRoute,
    value: Option<&str>,
) -> Result<Option<MarginMode>, MarketParticipantError> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if CODEC.scope_instrument_type(&route.scope) == Some(OkxInstrumentType::Spot) && value == "cash"
    {
        return Ok(None);
    }
    decode_margin_mode(value).map(Some)
}

fn decode_order_kind(
    order_route: OrderRoute,
    item: &OkxOrder,
) -> Result<OrderKind, MarketParticipantError> {
    let normalized = required(&item.ord_type, "decode OKX order kind")?.to_ascii_uppercase();
    let price = optional_decimal::<Price>(
        match order_route {
            OrderRoute::Normal => item.px.as_deref(),
            OrderRoute::Algo => item.actual_px.as_deref(),
        },
        "decode OKX order price",
    )?
    .filter(|price| !price.is_zero());
    let time_in_force = item
        .time_in_force
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(decode_time_in_force)
        .transpose()?
        .unwrap_or(TimeInForce::GoodTillCancelled);
    match normalized.as_str() {
        "MARKET" => Ok(OrderKind::Market),
        "LIMIT" | "POST_ONLY" | "IOC" | "FOK" => Ok(OrderKind::Limit {
            price: price.ok_or_else(|| protocol("decode OKX limit price", "missing price"))?,
            time_in_force: match normalized.as_str() {
                "POST_ONLY" => TimeInForce::PostOnly,
                "IOC" => TimeInForce::ImmediateOrCancel,
                "FOK" => TimeInForce::FillOrKill,
                _ => time_in_force,
            },
        }),
        "CONDITIONAL" | "TRIGGER" => {
            let (take_profit, trigger, order_price, trigger_kind) =
                decode_conditional_terms(&normalized, item, price)?;
            match (take_profit, order_price) {
                (false, None) => Ok(OrderKind::StopMarket {
                    trigger_price: trigger,
                    trigger_price_kind: trigger_kind,
                }),
                (false, Some(price)) => Ok(OrderKind::StopLimit {
                    price,
                    trigger_price: trigger,
                    trigger_price_kind: trigger_kind,
                    time_in_force,
                }),
                (true, None) => Ok(OrderKind::TakeProfitMarket {
                    trigger_price: trigger,
                    trigger_price_kind: trigger_kind,
                }),
                (true, Some(price)) => Ok(OrderKind::TakeProfitLimit {
                    price,
                    trigger_price: trigger,
                    trigger_price_kind: trigger_kind,
                    time_in_force,
                }),
            }
        }
        _ => Err(protocol("decode OKX order kind", "unknown order type")),
    }
}

/// The trigger terms of one conditional OKX order.
///
/// A take-profit order names its trigger in the `tp*` fields, a stop order in the
/// `sl*` fields, and a plain trigger order in the unsuffixed ones; OKX leaves the
/// fields a branch does not use empty.
fn decode_conditional_terms(
    normalized: &str,
    item: &OkxOrder,
    price: Option<Price>,
) -> Result<(bool, Price, Option<Price>, TriggerPriceKind), MarketParticipantError> {
    if let Some(trigger) =
        optional_decimal::<Price>(item.tp_trigger_px.as_deref(), "decode OKX trigger price")?
    {
        return Ok((
            true,
            trigger,
            conditional_price(item.tp_ord_px.as_deref(), price)?,
            decode_trigger_kind(required(
                item.tp_trigger_px_type.as_deref().unwrap_or_default(),
                "decode OKX trigger kind",
            )?)?,
        ));
    }
    if normalized == "TRIGGER" {
        let trigger =
            optional_decimal::<Price>(item.trigger_px.as_deref(), "decode OKX trigger price")?
                .ok_or_else(|| protocol("decode OKX trigger price", "missing price"))?;
        return Ok((
            false,
            trigger,
            conditional_price(item.ord_px.as_deref(), price)?,
            decode_trigger_kind(required(
                item.trigger_px_type.as_deref().unwrap_or_default(),
                "decode OKX trigger kind",
            )?)?,
        ));
    }
    if let Some(trigger) =
        optional_decimal::<Price>(item.sl_trigger_px.as_deref(), "decode OKX trigger price")?
    {
        return Ok((
            false,
            trigger,
            conditional_price(item.sl_ord_px.as_deref(), price)?,
            decode_trigger_kind(required(
                item.sl_trigger_px_type.as_deref().unwrap_or_default(),
                "decode OKX trigger kind",
            )?)?,
        ));
    }
    let trigger =
        optional_decimal::<Price>(item.trigger_px.as_deref(), "decode OKX trigger price")?
            .ok_or_else(|| protocol("decode OKX trigger price", "missing price"))?;
    Ok((
        false,
        trigger,
        price,
        decode_trigger_kind(required(
            item.trigger_px_type.as_deref().unwrap_or_default(),
            "decode OKX trigger kind",
        )?)?,
    ))
}

fn conditional_price(
    value: Option<&str>,
    fallback: Option<Price>,
) -> Result<Option<Price>, MarketParticipantError> {
    match value {
        None | Some("") => Ok(fallback),
        Some("-1") => Ok(None),
        Some(value) => value
            .parse()
            .map(Some)
            .map_err(|_| protocol("decode OKX conditional price", "invalid price")),
    }
}

/// Decodes OKX positions, each under its own instrument's listing.
pub(super) fn decode_positions(
    route: &VenueRoute,
    instrument_type: OkxInstrumentType,
    rows: Vec<OkxPosition>,
) -> Result<Vec<NativePosition>, MarketParticipantError> {
    let mut positions = Vec::with_capacity(rows.len());
    for item in rows {
        let listing = CODEC.listing_key(route, instrument_type, item.inst_id.as_str())?;
        let position_side =
            decode_position_side(required(&item.pos_side, "decode OKX position side")?)?;
        let signed: SignedQuantity = parse(&item.pos, "decode OKX position quantity")?;
        let entry_price: Price = parse(&item.avg_px, "decode OKX position entry price")?;
        let mark_price: Price = parse(&item.mark_px, "decode OKX position mark price")?;
        let quantity = Quantity::from_inner(signed.inner().unsigned_abs());
        let direction = match position_side {
            PositionSide::Long => PositionDirection::Long,
            PositionSide::Short => PositionDirection::Short,
            PositionSide::Net if signed.inner().is_negative() => PositionDirection::Short,
            PositionSide::Net => PositionDirection::Long,
        };
        positions.push(NativePosition {
            listing,
            id: item
                .pos_id
                .as_deref()
                .filter(|value| !value.is_empty())
                .map(|value| model(PositionId::new(value), "decode OKX position ID"))
                .transpose()?,
            position_side,
            direction,
            margin_mode: item
                .mgn_mode
                .as_deref()
                .filter(|value| !value.is_empty())
                .map(decode_margin_mode)
                .transpose()?,
            leverage: optional_nonzero(item.lever.as_deref())?,
            quantity,
            entry_price,
            break_even_price: optional_decimal(
                item.be_px.as_deref(),
                "decode OKX break-even price",
            )?
            .filter(|price: &Price| !price.is_zero()),
            mark_price,
            unrealized_profit_or_loss: optional_decimal(
                item.upl.as_deref(),
                "decode OKX unrealized profit",
            )?
            .unwrap_or(ProfitOrLoss::ZERO),
            liquidation_price: optional_decimal(
                item.liq_px.as_deref(),
                "decode OKX liquidation price",
            )?
            .filter(|price: &Price| !price.is_zero()),
            notional: optional_decimal(item.notional_usd.as_deref(), "decode OKX notional")?
                .unwrap_or_else(|| {
                    mark_price
                        .checked_mul_quantity(quantity)
                        .unwrap_or(Quantity::ZERO)
                }),
            initial_margin: optional_decimal(item.imr.as_deref(), "decode OKX initial margin")?
                .unwrap_or(ProfitOrLoss::ZERO),
            maintenance_margin: optional_decimal(
                item.mmr.as_deref(),
                "decode OKX maintenance margin",
            )?,
            update_timestamp: item
                .u_time
                .as_deref()
                .or(item.p_time.as_deref())
                .map(|value| parse(value, "decode OKX position timestamp"))
                .transpose()?
                .unwrap_or_default(),
        });
    }
    Ok(positions)
}

/// Decodes a page of OKX fills, each under its own instrument's listing.
pub(super) fn decode_account_trades(
    route: &VenueRoute,
    instrument_type: OkxInstrumentType,
    rows: Vec<OkxTrade>,
) -> Result<Vec<NativeAccountTrade>, MarketParticipantError> {
    let mut trades = Vec::with_capacity(rows.len());
    for item in rows {
        let listing = CODEC.listing_key(route, instrument_type, item.inst_id.as_str())?;
        trades.push(NativeAccountTrade {
            listing,
            id: model(
                AccountTradeId::new(required(&item.trade_id, "decode OKX trade ID")?),
                "decode OKX trade ID",
            )?,
            order_id: model(
                OrderId::normal(required(&item.ord_id, "decode OKX order ID")?),
                "decode OKX order ID",
            )?,
            side: decode_side(&item.side)?,
            price: parse(&item.fill_px, "decode OKX trade price")?,
            quantity: parse(&item.fill_sz, "decode OKX trade size")?,
            fee_asset: super::adapter::asset(&item.fee_ccy).asset_id(),
            fee: parse(&item.fee, "decode OKX trade fee")?,
            liquidity: match item.exec_type.as_str() {
                "M" => Some(LiquidityRole::Maker),
                "T" => Some(LiquidityRole::Taker),
                _ => {
                    return Err(protocol(
                        "decode OKX trade execution type",
                        "unknown execution type",
                    ));
                }
            },
            timestamp: parse(&item.fill_time, "decode OKX trade timestamp")?,
        });
    }
    trades.sort_unstable_by_key(|trade| trade.timestamp);
    Ok(trades)
}

/// Decodes a page of closed OKX positions, each under its own instrument's listing.
pub(super) fn decode_position_history(
    route: &VenueRoute,
    instrument_type: OkxInstrumentType,
    rows: Vec<OkxHistoricalPosition>,
) -> Result<Vec<NativeHistoricalPosition>, MarketParticipantError> {
    let mut history = Vec::with_capacity(rows.len());
    for item in rows {
        let listing = CODEC.listing_key(route, instrument_type, item.inst_id.as_str())?;
        let position_side =
            decode_position_side(required(&item.pos_side, "decode OKX historical side")?)?;
        let direction = decode_historical_direction(item.direction.as_deref(), position_side)?;
        let maximum_quantity: Quantity =
            parse(&item.open_max_pos, "decode OKX historical quantity")?;
        let closed_quantity: Quantity =
            parse(&item.close_total_pos, "decode OKX historical quantity")?;
        history.push(NativeHistoricalPosition {
            listing,
            id: model(
                PositionId::new(required(
                    item.pos_id.as_deref().unwrap_or_default(),
                    "decode OKX historical ID",
                )?),
                "decode OKX historical ID",
            )?,
            direction,
            status: match item.kind.as_str() {
                "1" => HistoricalPositionStatus::PartiallyClosed,
                "2" => HistoricalPositionStatus::Closed,
                "3" => HistoricalPositionStatus::Liquidated,
                "4" => HistoricalPositionStatus::PartiallyLiquidated,
                "5" | "6" => HistoricalPositionStatus::AutoDeleveraged,
                _ => {
                    return Err(protocol("decode OKX historical status", "unknown status"));
                }
            },
            margin_mode: item
                .mgn_mode
                .as_deref()
                .filter(|value| !value.is_empty())
                .map(decode_margin_mode)
                .transpose()?,
            leverage: optional_nonzero(item.lever.as_deref())?,
            quantity: maximum_quantity
                .checked_sub(closed_quantity)
                .unwrap_or(Quantity::ZERO),
            maximum_quantity,
            closed_quantity,
            entry_price: optional_decimal(item.open_avg_px.as_deref(), "decode OKX entry price")?
                .unwrap_or(Price::ZERO),
            exit_price: optional_decimal(item.close_avg_px.as_deref(), "decode OKX exit price")?
                .unwrap_or(Price::ZERO),
            realized_profit_or_loss: optional_decimal(
                item.realized_pnl.as_deref(),
                "decode OKX realized PnL",
            )?
            .unwrap_or(ProfitOrLoss::ZERO),
            realized_return: optional_decimal(
                item.pnl_ratio.as_deref(),
                "decode OKX realized return",
            )?,
            opened_timestamp: parse(&item.c_time, "decode OKX open timestamp")?,
            closed_timestamp: parse(&item.u_time, "decode OKX close timestamp")?,
        });
    }
    history.sort_unstable_by_key(|position| std::cmp::Reverse(position.closed_timestamp));
    Ok(history)
}

/// The direction of one closed position.
///
/// A hedged account names the direction directly; a net account only names the
/// side it was held on, so the direction is the one that side means.
fn decode_historical_direction(
    value: Option<&str>,
    position_side: PositionSide,
) -> Result<PositionDirection, MarketParticipantError> {
    match value {
        Some("long") => Ok(PositionDirection::Long),
        Some("short") => Ok(PositionDirection::Short),
        Some("") | Some("net") | None => match position_side {
            PositionSide::Long => Ok(PositionDirection::Long),
            PositionSide::Short => Ok(PositionDirection::Short),
            PositionSide::Net => Err(protocol(
                "decode OKX historical direction",
                "missing direction",
            )),
        },
        Some(_) => Err(protocol(
            "decode OKX historical direction",
            "unknown direction",
        )),
    }
}

/// Decodes the account-wide position mode OKX reports.
pub(super) fn decode_position_mode(
    rows: &[ConfigRow],
) -> Result<PositionMode, MarketParticipantError> {
    let [row] = rows else {
        return Err(protocol(
            "decode OKX position mode",
            "expected one config row",
        ));
    };
    match row.pos_mode.as_deref() {
        Some("long_short_mode") => Ok(PositionMode::Hedge),
        Some("net_mode") => Ok(PositionMode::OneWay),
        _ => Err(protocol("decode OKX position mode", "unknown mode")),
    }
}

/// Decodes the margin mode and leverage OKX reports for one listing.
pub(super) fn decode_leverage(
    rows: &[OkxLeverage],
) -> Result<(MarginMode, NonZeroU8), MarketParticipantError> {
    let [row] = rows else {
        return Err(protocol("decode OKX leverage", "expected one leverage row"));
    };
    Ok((
        decode_margin_mode(row.mgn_mode.as_deref().unwrap_or_default())?,
        nonzero(&row.lever)?,
    ))
}

/// Decodes one OKX order push addressed to an authenticated orders subscription.
///
/// Every order of the frame keeps its own instrument's listing, and a
/// subscription narrowed to one listing publishes only that one.
pub(super) fn decode_order_frame(
    payload: &[u8],
    route: &VenueRoute,
    instrument_type: OkxInstrumentType,
    order_route: OrderRoute,
    listing: Option<&VenueListingKey>,
) -> Result<Vec<ProtocolEvent>, MarketParticipantError> {
    let envelope: PrivateEventEnvelope<OkxOrder> =
        serde_json::from_slice(payload).map_err(|_| {
            protocol(
                "decode OKX private order event",
                "malformed order event payload",
            )
        })?;
    if envelope.data.is_empty() {
        return Err(protocol(
            "decode OKX private order event",
            "order event omitted data",
        ));
    }
    let orders = decode_orders(route, instrument_type, order_route, envelope.data)?;
    Ok(orders
        .into_iter()
        .filter(|order| listing.is_none_or(|expected| &order.listing == expected))
        .map(ProtocolEvent::Order)
        .collect())
}

/// Decodes one OKX position push addressed to an authenticated positions
/// subscription.
///
/// A position the venue closed is pushed with a zero quantity, which is the
/// event that reports the closure: its own numbers are published as they are.
pub(super) fn decode_position_frame(
    payload: &[u8],
    route: &VenueRoute,
    instrument_type: OkxInstrumentType,
    listing: Option<&VenueListingKey>,
) -> Result<Vec<ProtocolEvent>, MarketParticipantError> {
    let envelope: PrivateEventEnvelope<OkxPosition> =
        serde_json::from_slice(payload).map_err(|_| {
            protocol(
                "decode OKX private position event",
                "malformed position event payload",
            )
        })?;
    if envelope.data.is_empty() {
        return Err(protocol(
            "decode OKX private position event",
            "position event omitted data",
        ));
    }
    let positions = decode_positions(route, instrument_type, envelope.data)?;
    Ok(positions
        .into_iter()
        .filter(|position| listing.is_none_or(|expected| &position.listing == expected))
        .map(ProtocolEvent::Position)
        .collect())
}

fn decode_side(value: &str) -> Result<Side, MarketParticipantError> {
    match value {
        "buy" => Ok(Side::Buy),
        "sell" => Ok(Side::Sell),
        _ => Err(protocol("decode OKX order side", "unknown side")),
    }
}

fn decode_position_side(value: &str) -> Result<PositionSide, MarketParticipantError> {
    match value {
        "net" => Ok(PositionSide::Net),
        "long" => Ok(PositionSide::Long),
        "short" => Ok(PositionSide::Short),
        _ => Err(protocol("decode OKX position side", "unknown side")),
    }
}

fn decode_margin_mode(value: &str) -> Result<MarginMode, MarketParticipantError> {
    match value {
        "isolated" => Ok(MarginMode::Isolated),
        "cross" => Ok(MarginMode::Cross),
        _ => Err(protocol("decode OKX margin mode", "unknown mode")),
    }
}

fn decode_order_status(value: &str) -> Result<OrderStatus, MarketParticipantError> {
    match value.to_ascii_lowercase().as_str() {
        "pending" | "pending_new" | "working" | "triggered" | "pause" => Ok(OrderStatus::Pending),
        "new" | "live" | "effective" => Ok(OrderStatus::Open),
        "partially_filled" | "partially_effective" => Ok(OrderStatus::PartiallyFilled),
        "filled" | "finished" => Ok(OrderStatus::Filled),
        "canceled" | "cancelled" | "mmp_canceled" => Ok(OrderStatus::Cancelled),
        "order_failed" | "partially_failed" | "failed" => Ok(OrderStatus::Rejected),
        "expired" => Ok(OrderStatus::Expired),
        _ => Err(protocol("decode OKX order status", "unknown status")),
    }
}

fn decode_time_in_force(value: &str) -> Result<TimeInForce, MarketParticipantError> {
    match value.to_ascii_uppercase().as_str() {
        "GTC" => Ok(TimeInForce::GoodTillCancelled),
        "IOC" => Ok(TimeInForce::ImmediateOrCancel),
        "FOK" => Ok(TimeInForce::FillOrKill),
        "POST_ONLY" => Ok(TimeInForce::PostOnly),
        _ => Err(protocol("decode OKX time in force", "unknown value")),
    }
}

fn decode_trigger_kind(value: &str) -> Result<TriggerPriceKind, MarketParticipantError> {
    match value {
        "last" => Ok(TriggerPriceKind::Last),
        "mark" => Ok(TriggerPriceKind::Mark),
        "index" => Ok(TriggerPriceKind::Index),
        _ => Err(protocol("decode OKX trigger price kind", "unknown kind")),
    }
}

fn decode_optional_bool(value: Option<&str>) -> Result<bool, MarketParticipantError> {
    match value {
        None | Some("") => Ok(false),
        Some("true") | Some("1") => Ok(true),
        Some("false") | Some("0") => Ok(false),
        Some(_) => Err(protocol("decode OKX reduce-only flag", "invalid boolean")),
    }
}

fn optional_nonzero(value: Option<&str>) -> Result<Option<NonZeroU8>, MarketParticipantError> {
    value
        .filter(|value| !value.is_empty())
        .map(nonzero)
        .transpose()
}

fn nonzero(value: &str) -> Result<NonZeroU8, MarketParticipantError> {
    value
        .parse::<u8>()
        .ok()
        .and_then(NonZeroU8::new)
        .ok_or_else(|| protocol("decode OKX leverage", "invalid leverage"))
}

fn parse<T: std::str::FromStr>(
    value: &str,
    operation: &'static str,
) -> Result<T, MarketParticipantError> {
    required(value, operation)?
        .parse()
        .map_err(|_| protocol(operation, "invalid numeric value"))
}

fn optional_decimal<T: std::str::FromStr>(
    value: Option<&str>,
    operation: &'static str,
) -> Result<Option<T>, MarketParticipantError> {
    value
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse()
                .map_err(|_| protocol(operation, "invalid numeric value"))
        })
        .transpose()
}

fn required<'a>(
    value: &'a str,
    operation: &'static str,
) -> Result<&'a str, MarketParticipantError> {
    (!value.is_empty())
        .then_some(value)
        .ok_or_else(|| protocol(operation, "required field is empty"))
}

fn model<T>(
    value: Result<T, market_participants_model::error::ModelError>,
    operation: &'static str,
) -> Result<T, MarketParticipantError> {
    value.map_err(|error| protocol(operation, error.to_string()))
}

fn protocol(operation: &'static str, reason: impl Into<SmolStr>) -> MarketParticipantError {
    MarketParticipantError::ProtocolError {
        operation: operation.into(),
        reason: reason.into(),
        raw_data: None,
    }
}

#[derive(Deserialize)]
pub(super) struct OkxBalanceAccount {
    pub details: Vec<OkxBalance>,
}
#[derive(Deserialize)]
pub(super) struct OkxBalance {
    ccy: SmolStr,
    #[serde(rename = "cashBal")]
    cash_bal: SmolStr,
    #[serde(rename = "availBal")]
    avail_bal: Option<SmolStr>,
    #[serde(rename = "availEq")]
    avail_eq: Option<SmolStr>,
}
#[derive(Deserialize)]
pub(super) struct OkxOrder {
    #[serde(rename = "instId")]
    inst_id: SmolStr,
    #[serde(rename = "ordId")]
    ord_id: Option<SmolStr>,
    #[serde(rename = "algoId")]
    algo_id: Option<SmolStr>,
    side: SmolStr,
    state: SmolStr,
    #[serde(rename = "ordType")]
    ord_type: SmolStr,
    sz: Option<SmolStr>,
    #[serde(rename = "accFillSz")]
    acc_fill_sz: Option<SmolStr>,
    #[serde(rename = "actualSz")]
    actual_sz: Option<SmolStr>,
    px: Option<SmolStr>,
    #[serde(rename = "actualPx")]
    actual_px: Option<SmolStr>,
    #[serde(rename = "avgPx")]
    avg_px: Option<SmolStr>,
    #[serde(rename = "tpTriggerPx")]
    tp_trigger_px: Option<SmolStr>,
    #[serde(rename = "tpOrdPx")]
    tp_ord_px: Option<SmolStr>,
    #[serde(rename = "tpTriggerPxType")]
    tp_trigger_px_type: Option<SmolStr>,
    #[serde(rename = "slTriggerPx")]
    sl_trigger_px: Option<SmolStr>,
    #[serde(rename = "slOrdPx")]
    sl_ord_px: Option<SmolStr>,
    #[serde(rename = "slTriggerPxType")]
    sl_trigger_px_type: Option<SmolStr>,
    #[serde(rename = "triggerPx")]
    trigger_px: Option<SmolStr>,
    #[serde(rename = "triggerPxType")]
    trigger_px_type: Option<SmolStr>,
    #[serde(rename = "ordPx")]
    ord_px: Option<SmolStr>,
    #[serde(rename = "tdMode")]
    td_mode: Option<SmolStr>,
    #[serde(rename = "reduceOnly")]
    reduce_only: Option<SmolStr>,
    #[serde(rename = "posSide")]
    pos_side: Option<SmolStr>,
    lever: Option<SmolStr>,
    #[serde(rename = "cTime")]
    c_time: SmolStr,
    #[serde(rename = "uTime")]
    u_time: SmolStr,
    #[serde(rename = "timeInForce")]
    time_in_force: Option<SmolStr>,
}
#[derive(Deserialize)]
pub(super) struct OkxPosition {
    #[serde(rename = "instId")]
    inst_id: SmolStr,
    #[serde(rename = "posId")]
    pos_id: Option<SmolStr>,
    pos: SmolStr,
    #[serde(rename = "posSide")]
    pos_side: SmolStr,
    #[serde(rename = "avgPx")]
    avg_px: SmolStr,
    #[serde(rename = "markPx")]
    mark_px: SmolStr,
    #[serde(rename = "mgnMode")]
    mgn_mode: Option<SmolStr>,
    lever: Option<SmolStr>,
    #[serde(rename = "bePx")]
    be_px: Option<SmolStr>,
    upl: Option<SmolStr>,
    #[serde(rename = "liqPx")]
    liq_px: Option<SmolStr>,
    #[serde(rename = "notionalUsd")]
    notional_usd: Option<SmolStr>,
    imr: Option<SmolStr>,
    mmr: Option<SmolStr>,
    #[serde(rename = "uTime")]
    u_time: Option<SmolStr>,
    #[serde(rename = "pTime")]
    p_time: Option<SmolStr>,
}
#[derive(Deserialize)]
pub(super) struct OkxTrade {
    #[serde(rename = "tradeId")]
    trade_id: SmolStr,
    #[serde(rename = "ordId")]
    ord_id: SmolStr,
    #[serde(rename = "instId")]
    inst_id: SmolStr,
    side: SmolStr,
    #[serde(rename = "fillPx")]
    fill_px: SmolStr,
    #[serde(rename = "fillSz")]
    fill_sz: SmolStr,
    #[serde(rename = "feeCcy")]
    fee_ccy: SmolStr,
    fee: SmolStr,
    #[serde(rename = "execType")]
    exec_type: SmolStr,
    #[serde(rename = "fillTime")]
    fill_time: SmolStr,
}
#[derive(Deserialize)]
pub(super) struct OkxHistoricalPosition {
    #[serde(rename = "instId")]
    inst_id: SmolStr,
    #[serde(rename = "posId")]
    pos_id: Option<SmolStr>,
    #[serde(rename = "posSide")]
    pos_side: SmolStr,
    direction: Option<SmolStr>,
    #[serde(rename = "type")]
    kind: SmolStr,
    #[serde(rename = "mgnMode")]
    mgn_mode: Option<SmolStr>,
    lever: Option<SmolStr>,
    #[serde(rename = "openMaxPos")]
    open_max_pos: SmolStr,
    #[serde(rename = "closeTotalPos")]
    close_total_pos: SmolStr,
    #[serde(rename = "openAvgPx")]
    open_avg_px: Option<SmolStr>,
    #[serde(rename = "closeAvgPx")]
    close_avg_px: Option<SmolStr>,
    #[serde(rename = "realizedPnl")]
    realized_pnl: Option<SmolStr>,
    #[serde(rename = "pnlRatio")]
    pnl_ratio: Option<SmolStr>,
    #[serde(rename = "cTime")]
    c_time: SmolStr,
    #[serde(rename = "uTime")]
    u_time: SmolStr,
}
#[derive(Deserialize)]
pub(super) struct OkxLeverage {
    #[serde(rename = "mgnMode")]
    pub mgn_mode: Option<SmolStr>,
    pub lever: SmolStr,
}
#[derive(Deserialize)]
struct PrivateEventEnvelope<T> {
    data: Vec<T>,
}

/// The channel one private data frame belongs to, or none for a control frame.
pub(super) fn private_frame_channel(payload: &[u8]) -> Option<SmolStr> {
    #[derive(Deserialize)]
    struct Header {
        event: Option<SmolStr>,
        op: Option<SmolStr>,
        arg: ChannelArg,
    }
    #[derive(Deserialize)]
    struct ChannelArg {
        channel: SmolStr,
    }
    let header: Header = serde_json::from_slice(payload).ok()?;
    if header.event.is_some() || header.op.is_some() {
        return None;
    }
    Some(header.arg.channel)
}

#[cfg(test)]
mod tests {
    use market_participants_model::{
        decimal::{Price, Quantity},
        identifier::{VenueId, VenueScopeId},
        listing::{route::VenueRoute, venue::OkxInstrumentType},
        market::units::PositionSide,
    };

    use super::*;
    use crate::{adapter::ProtocolEvent, error::AuthError};

    fn perpetual_route() -> VenueRoute {
        VenueRoute::new(
            VenueId::Okx,
            VenueScopeId::new(crate::routing::SWAP_SCOPE).expect("the scope id is canonical"),
        )
    }

    fn swap_listing(instrument_id: &str) -> VenueListingKey {
        CODEC
            .listing_key(
                &perpetual_route(),
                OkxInstrumentType::Perpetual,
                instrument_id,
            )
            .expect("the fixture instrument id is a stable identity")
    }

    #[tokio::test]
    async fn signed_request_rejects_missing_account_identity_with_typed_authentication_error() {
        let transport = std::sync::Arc::new(network::transport::hub::TransportHub::new(
            network::transport::hub::TransportPolicy::default(),
        ));
        let pool =
            std::sync::Arc::new(crate::connection::ConnectionPool::with_transport(transport));
        let identity = crate::protocol::RequestIdentity::new(
            perpetual_route(),
            market_participants_model::routing::CapabilityId::AuthenticatedTrading,
        );
        let origins = app_config::market_participants::origins::OriginPair {
            http: "https://openapi.okx.com"
                .parse()
                .expect("the origin parses"),
            websocket: "wss://ws.okx.com".parse().expect("the origin parses"),
        };
        let context =
            crate::adapter::ExecutionContext::new(identity, origins, reqwest::Client::new(), pool);

        let error = super::super::adapter::signed_get::<serde::de::IgnoredAny>(
            &context,
            "/api/v5/account/balance",
            Vec::new(),
        )
        .await
        .expect_err("missing identity must be rejected before network access");

        assert!(matches!(
            error,
            MarketParticipantError::AuthenticationFailed {
                account: None,
                source: AuthError::AccountIdentityMissing,
                ..
            }
        ));
    }

    const NORMAL_ORDER: &[u8] = br#"{
  "arg": {"channel": "orders", "instId": "BTC-USDT-SWAP"},
  "data": [{
    "instId": "BTC-USDT-SWAP",
    "ordId": "123",
    "algoId": "",
    "side": "buy",
    "state": "live",
    "ordType": "limit",
    "sz": "1",
    "accFillSz": "0.2",
    "actualSz": "0",
    "px": "50000",
    "actualPx": "",
    "avgPx": "0",
    "tdMode": "cross",
    "reduceOnly": "false",
    "posSide": "long",
    "lever": "10",
    "cTime": "1700000000000",
    "uTime": "1700000001000",
    "timeInForce": "GTC"
  }]
}"#;

    const ALGO_ORDER: &[u8] = br#"{
  "arg": {"channel": "orders-algo", "instId": "BTC-USDT-SWAP"},
  "data": [{
    "instId": "BTC-USDT-SWAP",
    "ordId": "",
    "algoId": "algo-1",
    "side": "sell",
    "state": "live",
    "ordType": "conditional",
    "sz": "2",
    "accFillSz": "0",
    "actualSz": "0",
    "px": "0",
    "actualPx": "0",
    "avgPx": "0",
    "triggerPx": "51000",
    "triggerPxType": "last",
    "ordPx": "-1",
    "tdMode": "cross",
    "reduceOnly": "true",
    "posSide": "short",
    "lever": "5",
    "cTime": "1700000000000",
    "uTime": "1700000001000",
    "timeInForce": "GTC"
  }]
}"#;

    const NONZERO_POSITION: &[u8] = br#"{
  "arg": {"channel": "positions", "instId": "BTC-USDT-SWAP"},
  "data": [{
    "instId": "BTC-USDT-SWAP",
    "posId": "position-1",
    "pos": "1",
    "posSide": "long",
    "avgPx": "50000",
    "markPx": "50100",
    "mgnMode": "cross",
    "lever": "10",
    "bePx": "50000",
    "upl": "100",
    "liqPx": "25000",
    "notionalUsd": "50100",
    "imr": "5010",
    "mmr": "100",
    "uTime": "1700000002000",
    "pTime": "1700000001000"
  }]
}"#;

    const ZERO_POSITION: &[u8] = br#"{
  "arg": {"channel": "positions", "instId": "BTC-USDT-SWAP"},
  "data": [{
    "instId": "BTC-USDT-SWAP",
    "posId": "position-1",
    "pos": "0",
    "posSide": "long",
    "avgPx": "0",
    "markPx": "0",
    "mgnMode": "cross",
    "lever": "10",
    "bePx": "0",
    "upl": "0",
    "liqPx": "0",
    "notionalUsd": "0",
    "imr": "0",
    "mmr": "0",
    "uTime": "1700000003000",
    "pTime": "1700000002000"
  }]
}"#;

    #[test]
    fn decodes_normal_private_order_event() {
        let events = decode_order_frame(
            NORMAL_ORDER,
            &perpetual_route(),
            OkxInstrumentType::Perpetual,
            OrderRoute::Normal,
            None,
        )
        .expect("normal order frame");
        let [ProtocolEvent::Order(order)] = events.as_slice() else {
            panic!("expected one order event");
        };
        assert_eq!(order.listing, swap_listing("BTC-USDT-SWAP"));
        assert_eq!(order.id.as_str(), "123");
        assert_eq!(order.id.route(), OrderRoute::Normal);
        assert_eq!(order.quantity, Quantity::from(1_u64));
        assert_eq!(
            order.filled_quantity,
            "0.2".parse::<Quantity>().expect("quantity")
        );
    }

    #[test]
    fn decodes_algo_private_order_event() {
        let events = decode_order_frame(
            ALGO_ORDER,
            &perpetual_route(),
            OkxInstrumentType::Perpetual,
            OrderRoute::Algo,
            None,
        )
        .expect("algo order frame");
        let [ProtocolEvent::Order(order)] = events.as_slice() else {
            panic!("expected one order event");
        };
        assert_eq!(order.id.as_str(), "algo-1");
        assert_eq!(order.id.route(), OrderRoute::Algo);
        assert!(matches!(
            order.kind,
            crate::account::shared::OrderKind::StopMarket { .. }
        ));
    }

    /// A subscription narrowed to one listing publishes that listing and leaves
    /// the orders of every other instrument of the account alone.
    #[test]
    fn a_narrowed_order_subscription_leaves_other_instruments_alone() {
        let events = decode_order_frame(
            NORMAL_ORDER,
            &perpetual_route(),
            OkxInstrumentType::Perpetual,
            OrderRoute::Normal,
            Some(&swap_listing("ETH-USDT-SWAP")),
        )
        .expect("the frame itself is well formed");

        assert!(events.is_empty());
    }

    #[test]
    fn decodes_nonzero_private_position_event() {
        let events = decode_position_frame(
            NONZERO_POSITION,
            &perpetual_route(),
            OkxInstrumentType::Perpetual,
            None,
        )
        .expect("nonzero position frame");
        let [ProtocolEvent::Position(position)] = events.as_slice() else {
            panic!("expected one position event");
        };
        assert_eq!(position.listing, swap_listing("BTC-USDT-SWAP"));
        assert_eq!(
            position.id.as_ref().map(PositionId::as_str),
            Some("position-1")
        );
        assert_eq!(position.quantity, Quantity::from(1_u64));
        assert_eq!(
            position.mark_price,
            "50100".parse::<Price>().expect("price")
        );
        assert_eq!(position.update_timestamp, 1_700_000_002_000);
    }

    /// The venue reports a closed position as a push of quantity zero; its own
    /// numbers are what the event carries.
    #[test]
    fn decodes_zero_private_position_event_for_removal() {
        let events = decode_position_frame(
            ZERO_POSITION,
            &perpetual_route(),
            OkxInstrumentType::Perpetual,
            None,
        )
        .expect("zero position frame");
        let [ProtocolEvent::Position(position)] = events.as_slice() else {
            panic!("expected one position event");
        };
        assert!(position.quantity.is_zero());
        assert_eq!(position.position_side, PositionSide::Long);
        assert_eq!(position.update_timestamp, 1_700_000_003_000);
    }

    /// The account-wide position mode and leverage answers are the venue's own
    /// spellings, and an unknown one is refused instead of being read as a default.
    #[test]
    fn decodes_account_config_and_leverage_answers() {
        assert_eq!(
            decode_position_mode(&[ConfigRow {
                pos_mode: Some("net_mode".into())
            }])
            .expect("net mode decodes"),
            PositionMode::OneWay
        );
        assert_eq!(
            decode_position_mode(&[ConfigRow {
                pos_mode: Some("long_short_mode".into())
            }])
            .expect("hedge mode decodes"),
            PositionMode::Hedge
        );
        assert!(decode_position_mode(&[ConfigRow { pos_mode: None }]).is_err());

        let (margin_mode, leverage) = decode_leverage(&[OkxLeverage {
            mgn_mode: Some("isolated".into()),
            lever: "10".into(),
        }])
        .expect("leverage decodes");
        assert_eq!(margin_mode, MarginMode::Isolated);
        assert_eq!(leverage.get(), 10);
    }

    /// One balance answer keeps the venue's token codes for the account boundary
    /// to resolve, and drops the entries that hold nothing.
    #[test]
    fn decodes_balance_entries_with_the_venues_token_codes() {
        let balances = decode_balances(&[OkxBalanceAccount {
            details: vec![
                OkxBalance {
                    ccy: "USDT".into(),
                    cash_bal: "100".into(),
                    avail_bal: Some("80".into()),
                    avail_eq: None,
                },
                OkxBalance {
                    ccy: "BTC".into(),
                    cash_bal: "0".into(),
                    avail_bal: Some("0".into()),
                    avail_eq: None,
                },
            ],
        }])
        .expect("balances decode");

        assert_eq!(balances.len(), 1);
        assert_eq!(
            balances[0].asset,
            super::super::adapter::asset("USDT").asset_id(),
            "a balance carries the canonical asset its currency code names"
        );
        assert_eq!(balances[0].total, Quantity::from(100_u64));
        assert_eq!(balances[0].available, Quantity::from(80_u64));
    }
}

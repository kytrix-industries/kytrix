use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use market_participants_model::{
    decimal::{Price, ProfitOrLoss, Quantity, SignedQuantity},
    identifier::NativeSymbol,
    listing::{
        route::{VenueListingKey, VenueRoute},
        venue::BinanceProduct,
    },
    market::units::{PositionSide, Side},
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::{
    account::shared::{
        AccountTradeId, LiquidityRole, MarginMode, OrderId, OrderKind, OrderRoute, OrderStatus,
        PositionDirection, PositionMode, TimeInForce, TradingSettings, TriggerPriceKind,
    },
    adapter::{ProtocolEvent, binance::adapter::venue_asset},
    error::MarketParticipantError,
    market_participants::model::{NativeAccountTrade, NativeBalance, NativeOrder, NativePosition},
};

pub(super) const RECV_WINDOW_MS: u64 = 5_000;
/// The largest history page the venue serves in one answer.
const HISTORY_PAGE_LIMIT: u16 = 1_000;
const MAX_HISTORY_LIMIT: u16 = 10_000;

/// Keeps one account's listen key alive for as long as its connection lives.
///
/// The key belongs to one route, so every control call reports that route when it
/// fails instead of collapsing it into the venue.
pub(super) fn spawn_listen_key_refresh(
    manager: super::ListenKeyManager,
    client: reqwest::Client,
    listen_key: SmolStr,
    route: VenueRoute,
    connection: Arc<crate::connection::Connection>,
    interval_duration: Duration,
) {
    let refresh_cancel = connection.cancellation();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(interval_duration);
        interval.tick().await;
        loop {
            tokio::select! {
                biased;
                () = refresh_cancel.cancelled() => {
                    let _ = manager.delete(&client, &listen_key, &route).await;
                    break;
                }
                _ = interval.tick() => {
                    if refresh_cancel.is_cancelled() {
                        continue;
                    }
                    if manager.refresh(&client, &listen_key, &route).await.is_err() {
                        connection.close();
                        let _ = manager.delete(&client, &listen_key, &route).await;
                        break;
                    }
                }
            }
        }
    });
}

pub(super) fn validate_history(
    query: &crate::market_participants::model::AuthenticatedQuery,
    operation: &'static str,
) -> Result<(), MarketParticipantError> {
    query.validate().map_err(|error| {
        MarketParticipantError::Configuration(smol_str::format_smolstr!("{}", error))
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

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum AccountIdentifier {
    Number(u64),
    Text(SmolStr),
}

impl AccountIdentifier {
    fn into_string(self, operation: &'static str) -> Result<String, MarketParticipantError> {
        let value = match self {
            Self::Number(value) => value.to_string(),
            Self::Text(value) => value.to_string(),
        };
        (!value.is_empty())
            .then_some(value)
            .ok_or_else(|| protocol(operation, "identifier is empty"))
    }
}

#[derive(Deserialize)]
struct SpotBalances {
    balances: Vec<SpotBalance>,
}

#[derive(Deserialize)]
struct SpotBalance {
    asset: SmolStr,
    free: SmolStr,
    locked: SmolStr,
}

#[derive(Deserialize)]
struct PerpetualBalance {
    asset: SmolStr,
    balance: SmolStr,
    #[serde(rename = "availableBalance")]
    available_balance: SmolStr,
}

/// Decodes one balance answer of `product`.
///
/// The venue names an asset by its own code, and the code is what this adapter
/// turns into a canonical asset before anything else sees the balance: the code
/// is the only form in which the venue ever names it, so resolving it anywhere
/// else would mean keeping a second table of what a code means.
pub(crate) fn decode_balances(
    product: BinanceProduct,
    body: &[u8],
) -> Result<Vec<NativeBalance>, MarketParticipantError> {
    let mut balances = match product {
        BinanceProduct::Spot => {
            let response: SpotBalances = decode_json(body, "decode Binance spot balances")?;
            response
                .balances
                .into_iter()
                .map(|item| {
                    let free = parse::<Quantity>(&item.free, "decode Binance free balance")?;
                    let locked = parse::<Quantity>(&item.locked, "decode Binance locked balance")?;
                    Ok(NativeBalance {
                        asset: venue_asset(&item.asset)?.asset_id(),
                        total: free
                            .checked_add(locked)
                            .ok_or_else(|| protocol("sum Binance balance", "overflow"))?,
                        available: free,
                    })
                })
                .collect::<Result<Vec<_>, MarketParticipantError>>()?
        }
        BinanceProduct::Perpetual => {
            let response: Vec<PerpetualBalance> =
                decode_json(body, "decode Binance perpetual balances")?;
            response
                .into_iter()
                .map(|item| {
                    Ok(NativeBalance {
                        asset: venue_asset(&item.asset)?.asset_id(),
                        total: parse(&item.balance, "decode Binance balance")?,
                        available: parse(&item.available_balance, "decode Binance balance")?,
                    })
                })
                .collect::<Result<Vec<_>, MarketParticipantError>>()?
        }
        BinanceProduct::Future | BinanceProduct::Option => {
            return Err(protocol(
                "decode Binance balances",
                "unsupported product for account balances",
            ));
        }
    };
    balances.retain(|item| !item.total.is_zero() || !item.available.is_zero());
    balances.sort_unstable_by(|left, right| left.asset.cmp(&right.asset));
    Ok(balances)
}

#[derive(Deserialize)]
struct AccountOrder {
    symbol: SmolStr,
    #[serde(rename = "orderId")]
    order_id: Option<AccountIdentifier>,
    #[serde(rename = "algoId")]
    algo_id: Option<AccountIdentifier>,
    side: SmolStr,
    #[serde(rename = "positionSide")]
    position_side: Option<SmolStr>,
    status: Option<SmolStr>,
    #[serde(rename = "algoStatus")]
    algo_status: Option<SmolStr>,
    #[serde(rename = "type")]
    kind: Option<SmolStr>,
    #[serde(rename = "orderType")]
    order_type: Option<SmolStr>,
    #[serde(rename = "origQty")]
    orig_qty: Option<SmolStr>,
    quantity: Option<SmolStr>,
    #[serde(rename = "executedQty")]
    executed_qty: Option<SmolStr>,
    price: Option<SmolStr>,
    #[serde(rename = "avgPrice")]
    avg_price: Option<SmolStr>,
    #[serde(rename = "timeInForce")]
    time_in_force: Option<SmolStr>,
    #[serde(rename = "workingType")]
    working_type: Option<SmolStr>,
    #[serde(rename = "triggerPrice")]
    trigger_price: Option<SmolStr>,
    #[serde(rename = "stopPrice")]
    stop_price: Option<SmolStr>,
    #[serde(rename = "reduceOnly")]
    reduce_only: Option<bool>,
    #[serde(rename = "marginType")]
    margin_type: Option<SmolStr>,
    leverage: Option<SmolStr>,
    time: Option<AccountIdentifier>,
    #[serde(rename = "createTime")]
    create_time: Option<AccountIdentifier>,
    #[serde(rename = "updateTime")]
    update_time: AccountIdentifier,
}

pub(crate) fn decode_orders(
    route: &market_participants_model::listing::route::VenueRoute,
    product: BinanceProduct,
    order_route: OrderRoute,
    body: &[u8],
) -> Result<Vec<NativeOrder>, MarketParticipantError> {
    let items: Vec<AccountOrder> = decode_json(body, "decode Binance orders")?;
    items
        .into_iter()
        .map(|item| decode_order(route, product, order_route, item))
        .collect()
}

fn decode_order(
    route: &market_participants_model::listing::route::VenueRoute,
    product: BinanceProduct,
    order_route: OrderRoute,
    item: AccountOrder,
) -> Result<NativeOrder, MarketParticipantError> {
    let symbol = required(&item.symbol, "decode Binance order symbol")?;
    let raw_id = match order_route {
        OrderRoute::Normal => item.order_id.clone(),
        OrderRoute::Algo => item.algo_id.clone(),
    }
    .ok_or_else(|| protocol("decode Binance order ID", "missing order ID"))?
    .into_string("decode Binance order ID")?;
    let quantity = parse(
        required(
            match order_route {
                OrderRoute::Normal => item.orig_qty.as_deref(),
                OrderRoute::Algo => item.quantity.as_deref(),
            }
            .unwrap_or_default(),
            "decode Binance order quantity",
        )?,
        "decode Binance order quantity",
    )?;
    let filled_quantity = parse(
        required(
            item.executed_qty.as_deref().unwrap_or_default(),
            "decode Binance filled quantity",
        )?,
        "decode Binance filled quantity",
    )?;
    let create_timestamp = match order_route {
        OrderRoute::Normal => item.time.clone(),
        OrderRoute::Algo => item.create_time.clone(),
    }
    .ok_or_else(|| protocol("decode Binance order create timestamp", "missing timestamp"))?
    .into_string("decode Binance order create timestamp")?
    .parse()
    .map_err(|_| protocol("decode Binance order create timestamp", "invalid timestamp"))?;
    let update_timestamp = item
        .update_time
        .clone()
        .into_string("decode Binance order update timestamp")?
        .parse()
        .map_err(|_| protocol("decode Binance order update timestamp", "invalid timestamp"))?;
    let listing = super::BinanceCodec::listing_key(route, product, symbol).map_err(|error| {
        protocol(
            "decode Binance order listing",
            smol_str::format_smolstr!("{}", error),
        )
    })?;
    let id = model(OrderId::new(raw_id, order_route), "decode Binance order ID")?;
    Ok(NativeOrder {
        listing,
        id,
        side: decode_side(&item.side)?,
        position_side: item
            .position_side
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(decode_position_side)
            .transpose()?,
        status: decode_order_status(
            match order_route {
                OrderRoute::Normal => item.status.as_deref(),
                OrderRoute::Algo => item.algo_status.as_deref(),
            }
            .unwrap_or_default(),
        )?,
        kind: decode_order_kind(order_route, &item)?,
        quantity,
        filled_quantity,
        average_filled_price: optional_decimal(
            item.avg_price.as_deref(),
            "decode average order price",
        )?
        .filter(|price: &Price| !price.is_zero()),
        reduce_only: item.reduce_only.unwrap_or(false),
        margin_mode: item
            .margin_type
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(decode_margin_mode)
            .transpose()?,
        leverage: optional_nonzero(item.leverage.as_deref())?,
        create_timestamp,
        update_timestamp,
    })
}

fn decode_order_kind(
    order_route: OrderRoute,
    item: &AccountOrder,
) -> Result<OrderKind, MarketParticipantError> {
    let kind = required(
        match order_route {
            OrderRoute::Normal => item.kind.as_deref(),
            OrderRoute::Algo => item.order_type.as_deref(),
        }
        .unwrap_or_default(),
        "decode Binance order kind",
    )?
    .to_ascii_uppercase();
    let price = optional_decimal::<Price>(item.price.as_deref(), "decode Binance order price")?
        .filter(|value| !value.is_zero());
    let tif = item
        .time_in_force
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(decode_time_in_force)
        .transpose()?
        .unwrap_or(TimeInForce::GoodTillCancelled);
    match kind.as_str() {
        "MARKET" => Ok(OrderKind::Market),
        "LIMIT" | "LIMIT_MAKER" => Ok(OrderKind::Limit {
            price: price.ok_or_else(|| protocol("decode Binance limit price", "missing price"))?,
            time_in_force: if kind == "LIMIT_MAKER" {
                TimeInForce::PostOnly
            } else {
                tif
            },
        }),
        "STOP" | "STOP_MARKET" | "TAKE_PROFIT" | "TAKE_PROFIT_MARKET" => {
            let trigger = optional_decimal::<Price>(
                item.trigger_price.as_deref().or(item.stop_price.as_deref()),
                "decode Binance trigger price",
            )?
            .ok_or_else(|| protocol("decode Binance trigger price", "missing trigger"))?;
            let trigger_kind = decode_trigger_kind(
                item.working_type
                    .as_deref()
                    .ok_or_else(|| protocol("decode Binance trigger kind", "missing kind"))?,
            )?;
            let take_profit = kind.starts_with("TAKE_PROFIT");
            match (take_profit, price) {
                (false, None) => Ok(OrderKind::StopMarket {
                    trigger_price: trigger,
                    trigger_price_kind: trigger_kind,
                }),
                (false, Some(price)) => Ok(OrderKind::StopLimit {
                    price,
                    trigger_price: trigger,
                    trigger_price_kind: trigger_kind,
                    time_in_force: tif,
                }),
                (true, None) => Ok(OrderKind::TakeProfitMarket {
                    trigger_price: trigger,
                    trigger_price_kind: trigger_kind,
                }),
                (true, Some(price)) => Ok(OrderKind::TakeProfitLimit {
                    price,
                    trigger_price: trigger,
                    trigger_price_kind: trigger_kind,
                    time_in_force: tif,
                }),
            }
        }
        _ => Err(protocol("decode Binance order kind", "unknown order type")),
    }
}

#[derive(Deserialize)]
struct PositionRow {
    symbol: SmolStr,
    #[serde(rename = "positionAmt")]
    position_amt: SmolStr,
    #[serde(rename = "positionSide")]
    position_side: SmolStr,
    #[serde(rename = "entryPrice")]
    entry_price: SmolStr,
    #[serde(rename = "markPrice")]
    mark_price: SmolStr,
    #[serde(rename = "marginType")]
    margin_type: Option<SmolStr>,
    leverage: Option<SmolStr>,
    #[serde(rename = "breakEvenPrice")]
    break_even_price: Option<SmolStr>,
    #[serde(rename = "unRealizedProfit")]
    unrealized_profit: Option<SmolStr>,
    #[serde(rename = "liquidationPrice")]
    liquidation_price: Option<SmolStr>,
    notional: Option<SmolStr>,
    #[serde(rename = "initialMargin")]
    initial_margin: Option<SmolStr>,
    #[serde(rename = "maintMargin")]
    maintenance_margin: Option<SmolStr>,
    #[serde(rename = "updateTime")]
    update_time: Option<AccountIdentifier>,
}

pub(crate) fn decode_positions(
    route: &market_participants_model::listing::route::VenueRoute,
    product: BinanceProduct,
    body: &[u8],
) -> Result<Vec<NativePosition>, MarketParticipantError> {
    let items: Vec<PositionRow> = decode_json(body, "decode Binance positions")?;
    let mut positions = Vec::with_capacity(items.len());
    for item in items {
        let symbol = required(&item.symbol, "decode Binance position symbol")?;
        let position_side = decode_position_side(required(
            &item.position_side,
            "decode Binance position side",
        )?)?;
        let signed: SignedQuantity = parse(&item.position_amt, "decode Binance position quantity")?;
        if signed.is_zero() {
            continue;
        }
        let entry_price = parse(&item.entry_price, "decode Binance position entry price")?;
        let mark_price = parse(&item.mark_price, "decode Binance position mark price")?;
        let listing =
            super::BinanceCodec::listing_key(route, product, symbol).map_err(|error| {
                protocol(
                    "decode Binance position listing",
                    smol_str::format_smolstr!("{}", error),
                )
            })?;
        let direction = match position_side {
            PositionSide::Long => PositionDirection::Long,
            PositionSide::Short => PositionDirection::Short,
            PositionSide::Net if signed.inner().is_negative() => PositionDirection::Short,
            PositionSide::Net => PositionDirection::Long,
        };
        positions.push(NativePosition {
            listing,
            id: None,
            position_side,
            direction,
            margin_mode: item
                .margin_type
                .as_deref()
                .filter(|value| !value.is_empty())
                .map(decode_margin_mode)
                .transpose()?,
            leverage: optional_nonzero(item.leverage.as_deref())?,
            quantity: Quantity::from_inner(signed.inner().unsigned_abs()),
            entry_price,
            break_even_price: optional_decimal(
                item.break_even_price.as_deref(),
                "decode Binance break-even price",
            )?
            .filter(|value: &Price| !value.is_zero()),
            mark_price,
            unrealized_profit_or_loss: optional_decimal(
                item.unrealized_profit.as_deref(),
                "decode Binance unrealized profit",
            )?
            .unwrap_or(ProfitOrLoss::ZERO),
            liquidation_price: optional_decimal(
                item.liquidation_price.as_deref(),
                "decode Binance liquidation price",
            )?
            .filter(|value: &Price| !value.is_zero()),
            notional: optional_decimal::<SignedQuantity>(
                item.notional.as_deref(),
                "decode Binance notional",
            )?
            .map(|value| Quantity::from_inner(value.inner().unsigned_abs()))
            .unwrap_or(Quantity::ZERO),
            initial_margin: optional_decimal(
                item.initial_margin.as_deref(),
                "decode Binance initial margin",
            )?
            .unwrap_or(ProfitOrLoss::ZERO),
            maintenance_margin: optional_decimal(
                item.maintenance_margin.as_deref(),
                "decode Binance maintenance margin",
            )?,
            update_timestamp: item
                .update_time
                .map(|value| {
                    value
                        .into_string("decode Binance position timestamp")?
                        .parse()
                        .map_err(|_| {
                            protocol("decode Binance position timestamp", "invalid timestamp")
                        })
                })
                .transpose()?
                .unwrap_or_default(),
        });
    }
    Ok(positions)
}

#[derive(Deserialize)]
struct AccountTradeRow {
    id: AccountIdentifier,
    #[serde(rename = "orderId")]
    order_id: AccountIdentifier,
    symbol: SmolStr,
    #[serde(rename = "isBuyer")]
    is_buyer: bool,
    price: SmolStr,
    qty: SmolStr,
    #[serde(rename = "commissionAsset")]
    commission_asset: SmolStr,
    commission: SmolStr,
    #[serde(rename = "isMaker")]
    is_maker: Option<bool>,
    time: AccountIdentifier,
}

pub(crate) fn decode_account_trades(
    route: &market_participants_model::listing::route::VenueRoute,
    product: BinanceProduct,
    body: &[u8],
) -> Result<Vec<NativeAccountTrade>, MarketParticipantError> {
    let items: Vec<AccountTradeRow> = decode_json(body, "decode Binance account trades")?;
    let mut trades = Vec::with_capacity(items.len());
    for item in items {
        let symbol = required(&item.symbol, "decode Binance account trade symbol")?;
        let listing =
            super::BinanceCodec::listing_key(route, product, symbol).map_err(|error| {
                protocol(
                    "decode Binance account trade listing",
                    smol_str::format_smolstr!("{}", error),
                )
            })?;
        let id = model(
            AccountTradeId::new(item.id.into_string("decode Binance account trade ID")?),
            "decode Binance account trade ID",
        )?;
        let order_id = model(
            OrderId::normal(
                item.order_id
                    .into_string("decode Binance account trade order ID")?,
            ),
            "decode Binance account trade order ID",
        )?;
        trades.push(NativeAccountTrade {
            listing,
            id,
            order_id,
            side: if item.is_buyer { Side::Buy } else { Side::Sell },
            price: parse(&item.price, "decode Binance account trade price")?,
            quantity: parse(&item.qty, "decode Binance account trade quantity")?,
            fee_asset: venue_asset(&item.commission_asset)?.asset_id(),
            fee: parse(&item.commission, "decode Binance account trade fee")?,
            liquidity: item.is_maker.map(|maker| {
                if maker {
                    LiquidityRole::Maker
                } else {
                    LiquidityRole::Taker
                }
            }),
            timestamp: item
                .time
                .into_string("decode Binance account trade timestamp")?
                .parse()
                .map_err(|_| {
                    protocol(
                        "decode Binance account trade timestamp",
                        "invalid timestamp",
                    )
                })?,
        });
    }
    trades.sort_unstable_by_key(|trade| trade.timestamp);
    Ok(trades)
}

#[derive(Deserialize)]
struct PositionModeResponse {
    #[serde(rename = "dualSidePosition")]
    dual_side_position: bool,
}

#[derive(Deserialize)]
struct PositionSetting {
    #[serde(rename = "marginType")]
    margin_type: SmolStr,
    leverage: SmolStr,
}

pub(crate) fn decode_trading_settings(
    mode_body: &[u8],
    positions_body: &[u8],
) -> Result<TradingSettings, MarketParticipantError> {
    let mode: PositionModeResponse = decode_json(mode_body, "decode Binance position mode")?;
    let positions: Vec<PositionSetting> =
        decode_json(positions_body, "decode Binance position settings")?;
    let [position] = positions.as_slice() else {
        return Err(protocol(
            "decode Binance position settings",
            "expected one position",
        ));
    };
    Ok(TradingSettings {
        position_mode: if mode.dual_side_position {
            PositionMode::Hedge
        } else {
            PositionMode::OneWay
        },
        margin_mode: decode_margin_mode(&position.margin_type)?,
        leverage: nonzero(&position.leverage)?,
    })
}

fn decode_side(value: &str) -> Result<Side, MarketParticipantError> {
    match value {
        "BUY" | "buy" => Ok(Side::Buy),
        "SELL" | "sell" => Ok(Side::Sell),
        _ => Err(protocol("decode Binance order side", "unknown side")),
    }
}

fn decode_position_side(value: &str) -> Result<PositionSide, MarketParticipantError> {
    match value {
        "BOTH" | "net" => Ok(PositionSide::Net),
        "LONG" | "long" => Ok(PositionSide::Long),
        "SHORT" | "short" => Ok(PositionSide::Short),
        _ => Err(protocol("decode Binance position side", "unknown side")),
    }
}

fn decode_margin_mode(value: &str) -> Result<MarginMode, MarketParticipantError> {
    match value {
        "isolated" | "ISOLATED" => Ok(MarginMode::Isolated),
        "cross" | "crossed" | "CROSS" | "CROSSED" => Ok(MarginMode::Cross),
        _ => Err(protocol("decode Binance margin mode", "unknown mode")),
    }
}

fn decode_order_status(value: &str) -> Result<OrderStatus, MarketParticipantError> {
    match value.to_ascii_uppercase().as_str() {
        "PENDING" | "PENDING_NEW" | "TO_NEW" | "WORKING" | "TRIGGERED" => Ok(OrderStatus::Pending),
        "NEW" | "LIVE" | "EFFECTIVE" => Ok(OrderStatus::Open),
        "PARTIALLY_FILLED" | "PARTIALLY_EFFECTIVE" => Ok(OrderStatus::PartiallyFilled),
        "FILLED" | "FINISHED" => Ok(OrderStatus::Filled),
        "CANCELED" | "CANCELLED" => Ok(OrderStatus::Cancelled),
        "REJECTED" | "ORDER_FAILED" | "FAILED" => Ok(OrderStatus::Rejected),
        "EXPIRED" => Ok(OrderStatus::Expired),
        _ => Err(protocol("decode Binance order status", "unknown status")),
    }
}

fn decode_time_in_force(value: &str) -> Result<TimeInForce, MarketParticipantError> {
    match value.to_ascii_uppercase().as_str() {
        "GTC" => Ok(TimeInForce::GoodTillCancelled),
        "IOC" => Ok(TimeInForce::ImmediateOrCancel),
        "FOK" => Ok(TimeInForce::FillOrKill),
        "GTX" | "POST_ONLY" => Ok(TimeInForce::PostOnly),
        _ => Err(protocol("decode Binance time in force", "unknown value")),
    }
}

fn decode_trigger_kind(value: &str) -> Result<TriggerPriceKind, MarketParticipantError> {
    match value {
        "CONTRACT_PRICE" | "last" => Ok(TriggerPriceKind::Last),
        "MARK_PRICE" | "mark" => Ok(TriggerPriceKind::Mark),
        "INDEX_PRICE" | "index" => Ok(TriggerPriceKind::Index),
        _ => Err(protocol(
            "decode Binance trigger price kind",
            "unknown kind",
        )),
    }
}

fn decode_json<T: serde::de::DeserializeOwned>(
    body: &[u8],
    operation: &'static str,
) -> Result<T, MarketParticipantError> {
    serde_json::from_slice(body).map_err(|_| protocol(operation, "malformed JSON"))
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

fn optional_nonzero(
    value: Option<&str>,
) -> Result<Option<std::num::NonZeroU8>, MarketParticipantError> {
    value
        .filter(|value| !value.is_empty())
        .map(nonzero)
        .transpose()
}

fn nonzero(value: &str) -> Result<std::num::NonZeroU8, MarketParticipantError> {
    value
        .parse::<u8>()
        .ok()
        .and_then(std::num::NonZeroU8::new)
        .ok_or_else(|| protocol("decode Binance leverage", "invalid leverage"))
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

#[derive(Clone, Copy)]
pub(super) enum PrivateSubscriptionKind {
    Orders,
    Positions,
}

#[derive(Deserialize)]
struct PrivateHeader {
    #[serde(rename = "e")]
    event: Option<SmolStr>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum WireId {
    Number(u64),
    Text(SmolStr),
}

#[derive(Deserialize, Serialize)]
struct DecoderOrder {
    symbol: SmolStr,
    #[serde(rename = "orderId")]
    order_id: WireId,
    side: SmolStr,
    status: Option<SmolStr>,
    #[serde(rename = "type")]
    kind: Option<SmolStr>,
    #[serde(rename = "orderType")]
    order_type: Option<SmolStr>,
    #[serde(rename = "origQty")]
    orig_qty: Option<SmolStr>,
    quantity: Option<SmolStr>,
    #[serde(rename = "executedQty")]
    executed_qty: Option<SmolStr>,
    price: Option<SmolStr>,
    #[serde(rename = "avgPrice")]
    avg_price: Option<SmolStr>,
    #[serde(rename = "timeInForce")]
    time_in_force: Option<SmolStr>,
    #[serde(rename = "workingType")]
    working_type: Option<SmolStr>,
    #[serde(rename = "triggerPrice")]
    trigger_price: Option<SmolStr>,
    #[serde(rename = "reduceOnly")]
    reduce_only: Option<bool>,
    #[serde(rename = "positionSide")]
    position_side: Option<SmolStr>,
    time: Option<WireId>,
    #[serde(rename = "createTime")]
    create_time: Option<WireId>,
    #[serde(rename = "updateTime")]
    update_time: WireId,
    #[serde(rename = "algoId", skip_serializing_if = "Option::is_none")]
    algo_id: Option<WireId>,
    #[serde(rename = "algoStatus", skip_serializing_if = "Option::is_none")]
    algo_status: Option<SmolStr>,
}

#[derive(Deserialize)]
struct SpotOrderEvent {
    #[serde(rename = "s")]
    symbol: SmolStr,
    #[serde(rename = "i")]
    order_id: WireId,
    #[serde(rename = "S")]
    side: SmolStr,
    #[serde(rename = "X")]
    status: SmolStr,
    #[serde(rename = "o")]
    kind: SmolStr,
    #[serde(rename = "q")]
    quantity: SmolStr,
    #[serde(rename = "z")]
    executed_quantity: SmolStr,
    #[serde(rename = "p")]
    price: SmolStr,
    #[serde(rename = "P")]
    trigger_price: Option<SmolStr>,
    #[serde(rename = "f")]
    time_in_force: Option<SmolStr>,
    #[serde(rename = "ap")]
    average_price: Option<SmolStr>,
    #[serde(rename = "R")]
    reduce_only: Option<bool>,
    #[serde(rename = "O")]
    create_time: WireId,
    #[serde(rename = "E")]
    update_time: WireId,
}

#[derive(Deserialize)]
struct FuturesEnvelope {
    #[serde(rename = "E")]
    event_time: WireId,
    #[serde(rename = "o")]
    order: FuturesOrderEvent,
}

#[derive(Deserialize)]
struct FuturesOrderEvent {
    #[serde(rename = "s")]
    symbol: SmolStr,
    #[serde(rename = "i")]
    order_id: WireId,
    #[serde(rename = "S")]
    side: SmolStr,
    #[serde(rename = "X")]
    status: SmolStr,
    #[serde(rename = "o")]
    kind: SmolStr,
    #[serde(rename = "q")]
    quantity: SmolStr,
    #[serde(rename = "z")]
    executed_quantity: SmolStr,
    #[serde(rename = "p")]
    price: SmolStr,
    #[serde(rename = "sp")]
    trigger_price: Option<SmolStr>,
    #[serde(rename = "wt")]
    working_type: Option<SmolStr>,
    #[serde(rename = "f")]
    time_in_force: Option<SmolStr>,
    #[serde(rename = "ap")]
    average_price: Option<SmolStr>,
    #[serde(rename = "R")]
    reduce_only: Option<bool>,
    #[serde(rename = "ps")]
    position_side: Option<SmolStr>,
    #[serde(rename = "T")]
    create_time: Option<WireId>,
}

#[derive(Deserialize)]
struct AlgoEnvelope {
    #[serde(rename = "o")]
    order: AlgoOrderEvent,
}

#[derive(Deserialize)]
struct AlgoOrderEvent {
    #[serde(rename = "s")]
    symbol: SmolStr,
    #[serde(rename = "aid")]
    algo_id: WireId,
    #[serde(rename = "S")]
    side: SmolStr,
    #[serde(rename = "algoStatus")]
    status: SmolStr,
    #[serde(rename = "orderType")]
    kind: SmolStr,
    #[serde(rename = "quantity")]
    quantity: SmolStr,
    #[serde(rename = "executedQty")]
    executed_quantity: SmolStr,
    #[serde(rename = "price")]
    price: Option<SmolStr>,
    #[serde(rename = "triggerPrice")]
    trigger_price: Option<SmolStr>,
    #[serde(rename = "workingType")]
    working_type: Option<SmolStr>,
    #[serde(rename = "avgPrice")]
    average_price: Option<SmolStr>,
    #[serde(rename = "createTime")]
    create_time: WireId,
    #[serde(rename = "updateTime")]
    update_time: WireId,
}

/// Decodes one private-stream payload into the events a subscriber asked for.
///
/// `filter` is the listing the subscription named, when it named one: an order
/// of another listing of the same account is not an event of that subscription.
pub(super) fn decode_private_events(
    payload: &[u8],
    route: &market_participants_model::listing::route::VenueRoute,
    product: BinanceProduct,
    kind: PrivateSubscriptionKind,
    filter: Option<&VenueListingKey>,
) -> Result<Vec<ProtocolEvent>, MarketParticipantError> {
    let header: PrivateHeader = match serde_json::from_slice(payload) {
        Ok(header) => header,
        Err(_) => return Ok(Vec::new()),
    };
    let event = header.event.as_deref();
    match (kind, product, event) {
        (PrivateSubscriptionKind::Orders, BinanceProduct::Spot, Some("executionReport")) => {
            let value: SpotOrderEvent = serde_json::from_slice(payload).map_err(|_| {
                MarketParticipantError::ProtocolError {
                    operation: "decode Binance spot order event".into(),
                    reason: "malformed executionReport payload".into(),
                    raw_data: Some(Bytes::copy_from_slice(payload)),
                }
            })?;
            decode_event_order(
                route,
                product,
                filter,
                OrderRoute::Normal,
                DecoderOrder {
                    symbol: value.symbol,
                    order_id: value.order_id,
                    side: value.side,
                    status: Some(value.status),
                    kind: Some(value.kind),
                    order_type: None,
                    orig_qty: Some(value.quantity),
                    quantity: None,
                    executed_qty: Some(value.executed_quantity),
                    price: Some(value.price),
                    avg_price: value.average_price,
                    time_in_force: value.time_in_force,
                    working_type: None,
                    trigger_price: value.trigger_price,
                    reduce_only: value.reduce_only,
                    position_side: None,
                    time: Some(value.create_time),
                    create_time: None,
                    update_time: value.update_time,
                    algo_id: None,
                    algo_status: None,
                },
            )
        }
        (
            PrivateSubscriptionKind::Orders,
            BinanceProduct::Perpetual,
            Some("ORDER_TRADE_UPDATE"),
        ) => {
            let envelope: FuturesEnvelope = serde_json::from_slice(payload).map_err(|_| {
                MarketParticipantError::ProtocolError {
                    operation: "decode Binance futures order event".into(),
                    reason: "malformed ORDER_TRADE_UPDATE payload".into(),
                    raw_data: Some(Bytes::copy_from_slice(payload)),
                }
            })?;
            let value = envelope.order;
            decode_event_order(
                route,
                product,
                filter,
                OrderRoute::Normal,
                DecoderOrder {
                    symbol: value.symbol,
                    order_id: value.order_id,
                    side: value.side,
                    status: Some(value.status),
                    kind: Some(value.kind),
                    order_type: None,
                    orig_qty: Some(value.quantity),
                    quantity: None,
                    executed_qty: Some(value.executed_quantity),
                    price: Some(value.price),
                    avg_price: value.average_price,
                    time_in_force: value.time_in_force,
                    working_type: value.working_type,
                    trigger_price: value.trigger_price,
                    reduce_only: value.reduce_only,
                    position_side: value.position_side,
                    time: value.create_time,
                    create_time: None,
                    update_time: envelope.event_time,
                    algo_id: None,
                    algo_status: None,
                },
            )
        }
        (PrivateSubscriptionKind::Orders, BinanceProduct::Perpetual, Some("ALGO_UPDATE")) => {
            let envelope: AlgoEnvelope = serde_json::from_slice(payload).map_err(|_| {
                MarketParticipantError::ProtocolError {
                    operation: "decode Binance algo order event".into(),
                    reason: "malformed ALGO_UPDATE payload".into(),
                    raw_data: Some(Bytes::copy_from_slice(payload)),
                }
            })?;
            let value = envelope.order;
            let algo_id = value.algo_id.clone();
            decode_event_order(
                route,
                product,
                filter,
                OrderRoute::Algo,
                DecoderOrder {
                    symbol: value.symbol,
                    order_id: algo_id.clone(),
                    side: value.side,
                    status: None,
                    kind: None,
                    order_type: Some(value.kind),
                    orig_qty: None,
                    quantity: Some(value.quantity),
                    executed_qty: Some(value.executed_quantity),
                    price: value.price,
                    avg_price: value.average_price,
                    time_in_force: None,
                    working_type: value.working_type,
                    trigger_price: value.trigger_price,
                    reduce_only: None,
                    position_side: None,
                    time: None,
                    create_time: Some(value.create_time),
                    update_time: value.update_time,
                    algo_id: Some(algo_id),
                    algo_status: Some(value.status),
                },
            )
        }
        _ => Ok(Vec::new()),
    }
}

pub(super) fn is_account_update(payload: &[u8]) -> bool {
    serde_json::from_slice::<PrivateHeader>(payload)
        .ok()
        .and_then(|header| header.event)
        .is_some_and(|event| event == "ACCOUNT_UPDATE")
}

fn decode_event_order(
    route: &market_participants_model::listing::route::VenueRoute,
    product: BinanceProduct,
    filter: Option<&VenueListingKey>,
    order_route: OrderRoute,
    order: DecoderOrder,
) -> Result<Vec<ProtocolEvent>, MarketParticipantError> {
    let body = serde_json::to_vec(&[order]).map_err(|_| MarketParticipantError::ProtocolError {
        operation: "encode Binance order event".into(),
        reason: "failed to prepare decoder input".into(),
        raw_data: None,
    })?;
    let orders = decode_orders(route, product, order_route, &body)?;
    Ok(orders
        .into_iter()
        .filter(|value| filter.is_none_or(|expected| &value.listing == expected))
        .map(ProtocolEvent::Order)
        .collect())
}

/// The query parameters of one history page of one listing.
///
/// The venue serves at most [`HISTORY_PAGE_LIMIT`] rows per answer, so the window
/// is paged by that bound and the caller truncates to its own limit.
pub(super) fn history_parameters(
    symbol: &NativeSymbol,
    query: &crate::market_participants::model::AuthenticatedQuery,
) -> Vec<(String, String)> {
    let mut parameters = vec![
        ("symbol".into(), symbol.as_str().to_owned()),
        ("limit".into(), HISTORY_PAGE_LIMIT.to_string()),
    ];
    if let Some(start) = query.start_timestamp {
        parameters.push(("startTime".into(), start.to_string()));
    }
    if let Some(end) = query.end_timestamp {
        parameters.push(("endTime".into(), end.to_string()));
    }
    parameters
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use app_config::{market_participants::origins::OriginPair, network::ResolvedProxy};
    use market_participants_model::{
        decimal::Quantity,
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
        market::units::PositionSide,
    };
    use network::transport::hub::{TransportHub, TransportPolicy};
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
        sync::mpsc,
    };
    use url::Url;

    use super::{super::BinanceAdapter, *};
    use crate::{
        account::shared::OrderRoute,
        adapter::{ExecutionContext, ProtocolEvent},
        connection::{
            ConnectionPool,
            physical::{ConnectionProtocol, no_response_id},
        },
        error::AuthError,
        protocol::RequestIdentity,
    };

    fn route(scope: &str) -> VenueRoute {
        VenueRoute::new(
            VenueId::Binance,
            VenueScopeId::new(scope).expect("the scope should be canonical"),
        )
    }

    fn context(route: VenueRoute) -> ExecutionContext {
        let transport = Arc::new(TransportHub::new(TransportPolicy::default()));
        let pool = Arc::new(ConnectionPool::with_transport(transport));
        let identity = RequestIdentity::new(
            route,
            market_participants_model::routing::CapabilityId::AuthenticatedTrading,
        );
        let origins = OriginPair {
            http: "https://api.binance.com"
                .parse()
                .expect("the origin parses"),
            websocket: "wss://stream.binance.com:9443"
                .parse()
                .expect("the origin parses"),
        };
        ExecutionContext::new(identity, origins, reqwest::Client::new(), pool).with_proxy(
            ResolvedProxy::NoProxy,
            app_config::network::NetworkRouteGeneration::default(),
        )
    }

    #[tokio::test]
    async fn listen_key_refresh_stops_without_refreshing_after_connection_close() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (methods, mut received) = mpsc::channel(2);
        let request_count = Arc::new(AtomicUsize::new(0));
        let server_count = Arc::clone(&request_count);
        tokio::spawn(async move {
            while let Ok(Ok((socket, _))) =
                tokio::time::timeout(Duration::from_millis(100), listener.accept()).await
            {
                server_count.fetch_add(1, Ordering::Relaxed);
                let mut reader = BufReader::new(socket);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let _ = methods.send(line).await;
                let mut socket = reader.into_inner();
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                    .await
                    .unwrap();
            }
        });
        let (outbound, _receiver) = tokio::sync::mpsc::channel(1);
        let (subscription_outbound, _subscription_receiver) =
            tokio::sync::mpsc::unbounded_channel();
        let connection = crate::connection::Connection::with_outbound(
            crate::connection::ConnectionKey::new(
                Url::parse("wss://example.test/ws").unwrap(),
                ResolvedProxy::NoProxy,
                None,
                0,
                &ConnectionProtocol::new(no_response_id),
            ),
            crate::connection::ConnectionState::stateless(),
            outbound,
            subscription_outbound,
        );
        let manager = super::super::ListenKeyManager::new(
            smol_str::format_smolstr!("http://{address}/api/v3/userDataStream"),
            "api-key",
        );
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        spawn_listen_key_refresh(
            manager,
            client,
            "listen-key-secret".into(),
            route("spot"),
            Arc::clone(&connection),
            Duration::from_millis(5),
        );
        connection.close();
        let method = tokio::time::timeout(Duration::from_secs(1), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(method.starts_with("DELETE "));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(request_count.load(Ordering::Relaxed), 1);
    }

    /// A signed request without a configured account is refused before it reaches
    /// the network, and it says which route failed.
    #[tokio::test]
    async fn signed_request_rejects_missing_account_identity_with_typed_authentication_error() {
        let adapter = BinanceAdapter::new();
        let context = context(route("spot"));

        let error = adapter
            .signed_get(&context, "/api/v3/account", Vec::new())
            .await
            .expect_err("missing identity must be rejected before network access");

        assert!(matches!(
            error,
            MarketParticipantError::AuthenticationFailed {
                account: None,
                source: AuthError::AccountIdentityMissing,
                route: failed,
            } if failed == context.route()
        ));
    }

    const SPOT_EXECUTION_REPORT: &[u8] = br#"{
  "e": "executionReport",
  "E": 1700000001000,
  "s": "BTCUSDT",
  "S": "BUY",
  "o": "LIMIT",
  "f": "GTC",
  "q": "0.01",
  "p": "50000",
  "P": "0",
  "x": "NEW",
  "X": "NEW",
  "i": 123,
  "z": "0",
  "ap": "0",
  "O": 1700000000000,
  "R": false
}"#;

    const FUTURES_ORDER_UPDATE: &[u8] = br#"{
  "e": "ORDER_TRADE_UPDATE",
  "E": 1700000001000,
  "o": {
    "s": "BTCUSDT",
    "S": "BUY",
    "o": "LIMIT",
    "f": "GTC",
    "q": "0.01",
    "p": "50000",
    "ap": "0",
    "X": "NEW",
    "i": 456,
    "z": "0",
    "T": 1700000000000,
    "R": false,
    "wt": "MARK_PRICE",
    "ps": "LONG"
  }
}"#;

    const ACCOUNT_UPDATE: &[u8] = br#"{
  "e": "ACCOUNT_UPDATE",
  "E": 1700000001000,
  "T": 1700000001000,
  "a": {"m": "ORDER", "B": [], "P": []}
}"#;

    #[test]
    fn decodes_spot_execution_report_into_order() {
        let route = route("spot");
        let events = decode_private_events(
            SPOT_EXECUTION_REPORT,
            &route,
            BinanceProduct::Spot,
            PrivateSubscriptionKind::Orders,
            None,
        )
        .unwrap();
        let [ProtocolEvent::Order(order)] = events.as_slice() else {
            panic!("expected one order event");
        };
        assert_eq!(order.id.as_str(), "123");
        assert_eq!(order.id.route(), OrderRoute::Normal);
        assert_eq!(
            order.quantity,
            "0.01".parse::<Quantity>().expect("quantity")
        );
        assert_eq!(order.update_timestamp, 1_700_000_001_000);
        assert_eq!(
            order.listing,
            super::super::BinanceCodec::listing_key(&route, BinanceProduct::Spot, "BTCUSDT")
                .expect("the spot symbol names a listing")
        );
    }

    #[test]
    fn decodes_futures_order_update_into_order() {
        let route = route(crate::routing::PERPETUAL_SCOPE);
        let events = decode_private_events(
            FUTURES_ORDER_UPDATE,
            &route,
            BinanceProduct::Perpetual,
            PrivateSubscriptionKind::Orders,
            None,
        )
        .unwrap();
        let [ProtocolEvent::Order(order)] = events.as_slice() else {
            panic!("expected one order event");
        };
        assert_eq!(order.id.as_str(), "456");
        assert_eq!(order.position_side, Some(PositionSide::Long));
        assert_eq!(order.update_timestamp, 1_700_000_001_000);
        assert_eq!(
            order.listing.route.scope.as_str(),
            crate::routing::PERPETUAL_SCOPE
        );
    }

    #[test]
    fn a_private_order_event_of_another_listing_is_not_published() {
        let route = route("spot");
        let other =
            super::super::BinanceCodec::listing_key(&route, BinanceProduct::Spot, "ETHUSDT")
                .expect("the other symbol names a listing");
        let events = decode_private_events(
            SPOT_EXECUTION_REPORT,
            &route,
            BinanceProduct::Spot,
            PrivateSubscriptionKind::Orders,
            Some(&other),
        )
        .unwrap();

        assert!(events.is_empty());
    }

    #[test]
    fn recognizes_futures_account_update_for_position_refresh() {
        assert!(is_account_update(ACCOUNT_UPDATE));
        assert!(!is_account_update(SPOT_EXECUTION_REPORT));
    }
}

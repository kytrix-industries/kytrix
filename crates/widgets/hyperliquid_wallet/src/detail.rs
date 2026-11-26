use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use app_config::settings::Config;
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use market_participants_connector::account::shared::{OrderKind, PositionDirection};
use market_participants_model::{
    asset::AssetId,
    decimal::{Price, ProfitOrLoss, Quantity, SignedQuantity},
    listing::route::VenueListingKey,
    market::{
        account::AssetAmount,
        price::MarketPrice,
        units::{MarketQuantity, Side},
    },
};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    app::TimerId,
    error::EngineError,
    math::{LogicalPx, px},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::{style::StylePatch, style_builder::StyleBuilder},
    text::TextStyle,
};
use ui_kit::{
    kit::{
        list::SelectionMode,
        spinner::Spinner,
        table::{Column, ColumnAlign, Table, TableOptions},
    },
    theme::{handle::ThemeHandle, role::Role},
};
use widget::{cx::WidgetCx, time_zone::data_time_zone};

use super::{
    model::{
        DetailData, DetailSnapshot, DetailTab, DisplayPrecision, SpotBalanceRow, WalletDataError,
        absolute_time, now_millis,
    },
    timeline::age_text,
};

const AGE_TICK: Duration = Duration::from_secs(1);

fn row_height(theme: &ThemeHandle) -> LogicalPx {
    theme.tokens().medium.height
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CellTone {
    Default,
    Muted,
    Positive,
    Negative,
}

const fn tone_role(tone: CellTone) -> Role {
    match tone {
        CellTone::Default | CellTone::Muted => Role::FgDefault,
        CellTone::Positive => Role::MarketUp,
        CellTone::Negative => Role::MarketDown,
    }
}

#[derive(Clone, Debug)]
struct DisplayCell {
    text: SmolStr,
    suffix: Option<SmolStr>,
    tone: CellTone,
}

impl DisplayCell {
    fn text(value: impl Into<SmolStr>) -> Self {
        Self {
            text: value.into(),
            suffix: None,
            tone: CellTone::Default,
        }
    }

    fn missing() -> Self {
        Self::text(SmolStr::new_static("-")).tone(CellTone::Muted)
    }

    fn tone(mut self, tone: CellTone) -> Self {
        self.tone = tone;
        self
    }

    fn suffix(mut self, value: impl Into<SmolStr>) -> Self {
        self.suffix = Some(value.into());
        self
    }

    fn age(now: i64, timestamp: i64, translate: &Translate) -> Self {
        Self {
            text: age_text(now, timestamp, translate),
            suffix: None,
            tone: CellTone::Muted,
        }
    }
}

fn pnl_tone(value: ProfitOrLoss) -> CellTone {
    match value.inner().cmp(&0) {
        std::cmp::Ordering::Less => CellTone::Negative,
        std::cmp::Ordering::Equal => CellTone::Muted,
        std::cmp::Ordering::Greater => CellTone::Positive,
    }
}

fn amount_tone(value: SignedQuantity) -> CellTone {
    pnl_tone(ProfitOrLoss::from_inner(value.inner()))
}

fn position_cell(direction: PositionDirection, translate: &Translate) -> DisplayCell {
    match direction {
        PositionDirection::Long => {
            DisplayCell::text(t_smol!(translate, "widget.hyperliquid_wallet.long"))
                .tone(CellTone::Positive)
        }
        PositionDirection::Short => {
            DisplayCell::text(t_smol!(translate, "widget.hyperliquid_wallet.short"))
                .tone(CellTone::Negative)
        }
    }
}

fn side_cell(side: Side, translate: &Translate) -> DisplayCell {
    match side {
        Side::Buy => DisplayCell::text(t_smol!(translate, "widget.hyperliquid_wallet.buy"))
            .tone(CellTone::Positive),
        Side::Sell => DisplayCell::text(t_smol!(translate, "widget.hyperliquid_wallet.sell"))
            .tone(CellTone::Negative),
    }
}

fn boolean_cell(value: bool, translate: &Translate) -> DisplayCell {
    DisplayCell::text(if value {
        t_smol!(translate, "common.yes")
    } else {
        t_smol!(translate, "common.no")
    })
}

fn trigger_price(kind: &OrderKind) -> Option<Price> {
    match kind {
        OrderKind::StopMarket { trigger_price, .. }
        | OrderKind::StopLimit { trigger_price, .. }
        | OrderKind::TakeProfitMarket { trigger_price, .. }
        | OrderKind::TakeProfitLimit { trigger_price, .. } => Some(*trigger_price),
        OrderKind::Market | OrderKind::Limit { .. } => None,
    }
}

fn listing_cell(
    precision: &DisplayPrecision,
    listing: &VenueListingKey,
    translate: &Translate,
) -> DisplayCell {
    DisplayCell::text(precision.listing_label(listing, translate))
}

fn collateral(
    precision: &DisplayPrecision,
    listing: &VenueListingKey,
    translate: &Translate,
) -> DisplayCell {
    DisplayCell::text(precision.listing_collateral_label(listing, translate))
}

fn money_amount(precision: &DisplayPrecision, value: Option<&AssetAmount>) -> DisplayCell {
    value.map_or_else(DisplayCell::missing, |value| {
        DisplayCell::text(precision.format_money_signed_quantity(value.value))
    })
}

fn market_quantity(
    precision: &DisplayPrecision,
    listing: &VenueListingKey,
    quantity: &MarketQuantity,
    translate: &Translate,
) -> Result<DisplayCell, WalletDataError> {
    let value = DisplayCell::text(precision.format_listing_quantity(listing, quantity.value)?);
    Ok(
        match precision.quantity_unit_label(&quantity.unit, translate) {
            Some(unit) if !quantity.unit.is_notional() => DisplayCell {
                suffix: Some(unit),
                ..value
            },
            Some(_) | None => value,
        },
    )
}

fn market_price_cell(
    precision: &DisplayPrecision,
    listing: &VenueListingKey,
    price: &MarketPrice,
    translate: &Translate,
) -> Result<DisplayCell, WalletDataError> {
    Ok(DisplayCell {
        suffix: Some(precision.asset_label(&price.quote_asset, translate)),
        ..DisplayCell::text(precision.format_listing_price(listing, price.value)?)
    })
}

fn optional_market_price(
    precision: &DisplayPrecision,
    listing: &VenueListingKey,
    price: Option<&MarketPrice>,
    translate: &Translate,
) -> Result<DisplayCell, WalletDataError> {
    price.map_or_else(
        || Ok(DisplayCell::missing()),
        |price| market_price_cell(precision, listing, price, translate),
    )
}

fn balance_price_cell(
    precision: &DisplayPrecision,
    row: &SpotBalanceRow,
    price: &MarketPrice,
    translate: &Translate,
) -> Result<DisplayCell, WalletDataError> {
    match row.price_listing.as_ref() {
        Some(listing) => market_price_cell(precision, listing, price, translate),
        None => Ok(DisplayCell {
            suffix: Some(precision.asset_label(&row.asset, translate)),
            ..DisplayCell::text(precision.format_asset_price(&row.asset, price.value)?)
        }),
    }
}

fn asset_quantity_cell(
    precision: &DisplayPrecision,
    asset: &AssetId,
    value: SignedQuantity,
    translate: &Translate,
) -> Result<DisplayCell, WalletDataError> {
    Ok(DisplayCell {
        suffix: Some(precision.asset_label(asset, translate)),
        ..DisplayCell::text(precision.format_asset_quantity(asset, value)?)
    })
}

fn order_notional(quantity: Quantity, price: &MarketPrice) -> Option<AssetAmount> {
    quantity
        .checked_mul_price(price.value)
        .map(|value| AssetAmount {
            asset: price.quote_asset.clone(),
            value: SignedQuantity::from_inner(value.inner() as i128),
        })
}

fn spot_cell(
    snapshot: &DetailSnapshot,
    row: usize,
    column: usize,
    _now: i64,
    translate: &Translate,
) -> Result<DisplayCell, WalletDataError> {
    let DetailData::Spot(data) = &snapshot.data else {
        return Ok(DisplayCell::missing());
    };
    let Some(row) = data.rows.get(row) else {
        return Ok(DisplayCell::missing());
    };
    let precision = &snapshot.precision;
    Ok(match column {
        0 => DisplayCell::text(precision.asset_label(&row.asset, translate)),
        1 => row
            .asset_share
            .clone()
            .map_or_else(DisplayCell::missing, DisplayCell::text),
        2 => asset_quantity_cell(precision, &row.asset, row.total, translate)?.tone(
            if row.total.inner() < 0 {
                CellTone::Negative
            } else {
                CellTone::Default
            },
        ),
        3 => row.value.map_or_else(DisplayCell::missing, |value| {
            DisplayCell::text(precision.format_money_signed_quantity(value)).tone(
                if value.inner() < 0 {
                    CellTone::Negative
                } else {
                    CellTone::Default
                },
            )
        }),
        4 => row.price.as_ref().map_or_else(
            || Ok(DisplayCell::missing()),
            |price| balance_price_cell(precision, row, price, translate),
        )?,
        _ => DisplayCell::missing(),
    })
}

fn perpetual_cell(
    snapshot: &DetailSnapshot,
    row: usize,
    column: usize,
    _now: i64,
    translate: &Translate,
) -> Result<DisplayCell, WalletDataError> {
    let DetailData::Perpetual(state) = &snapshot.data else {
        return Ok(DisplayCell::missing());
    };
    let Some(position) = state.positions.get(row) else {
        return Ok(DisplayCell::missing());
    };
    let precision = &snapshot.precision;
    let priced = position.entry_price.is_some() && position.mark_price.is_some();
    Ok(match column {
        0 => listing_cell(precision, &position.listing, translate),
        1 => position_cell(position.direction, translate),
        2 => money_amount(precision, position.position_value.as_ref()),
        3 => {
            let pnl = position.unrealized_pnl.value;
            if priced {
                DisplayCell::text(precision.format_money_profit(pnl)).tone(amount_tone(pnl))
            } else {
                DisplayCell::missing()
            }
        }
        4 => optional_market_price(
            precision,
            &position.listing,
            position.entry_price.as_ref(),
            translate,
        )?,
        5 => optional_market_price(
            precision,
            &position.listing,
            position.mark_price.as_ref(),
            translate,
        )?,
        6 => optional_market_price(
            precision,
            &position.listing,
            position.liquidation_price.as_ref(),
            translate,
        )?,
        7 => money_amount(precision, position.margin.as_ref()),
        8 => {
            let funding = position.funding_since_open.value;
            DisplayCell::text(precision.format_money_profit(funding)).tone(amount_tone(funding))
        }
        _ => DisplayCell::missing(),
    })
}

fn open_order_cell(
    snapshot: &DetailSnapshot,
    row: usize,
    column: usize,
    now: i64,
    translate: &Translate,
) -> Result<DisplayCell, WalletDataError> {
    let DetailData::OpenOrders(orders) = &snapshot.data else {
        return Ok(DisplayCell::missing());
    };
    let Some(order) = orders.get(row) else {
        return Ok(DisplayCell::missing());
    };
    let precision = &snapshot.precision;
    Ok(match column {
        0 => listing_cell(precision, &order.listing, translate),
        1 => collateral(precision, &order.listing, translate),
        2 => side_cell(order.side, translate),
        3 => market_quantity(precision, &order.listing, &order.quantity, translate)?,
        4 => money_amount(
            precision,
            order_notional(order.quantity.value, &order.price).as_ref(),
        ),
        5 => market_price_cell(precision, &order.listing, &order.price, translate)?,
        6 => trigger_price(&order.kind).map_or_else(
            || Ok(DisplayCell::missing()),
            |price| {
                market_price_cell(
                    precision,
                    &order.listing,
                    &MarketPrice {
                        value: price,
                        quote_asset: order.price.quote_asset.clone(),
                        basis: order.price.basis.clone(),
                    },
                    translate,
                )
            },
        )?,
        7 => boolean_cell(order.reduce_only, translate),
        8 => DisplayCell::age(now, order.timestamp, translate),
        _ => DisplayCell::missing(),
    })
}

fn trade_cell(
    snapshot: &DetailSnapshot,
    row: usize,
    column: usize,
    now: i64,
    translate: &Translate,
) -> Result<DisplayCell, WalletDataError> {
    let DetailData::Trades(fills) = &snapshot.data else {
        return Ok(DisplayCell::missing());
    };
    let Some(fill) = fills.get(row) else {
        return Ok(DisplayCell::missing());
    };
    let precision = &snapshot.precision;
    Ok(match column {
        0 => listing_cell(precision, &fill.listing, translate),
        1 => side_cell(fill.side, translate),
        2 => market_quantity(precision, &fill.listing, &fill.quantity, translate)?,
        3 => DisplayCell::text(
            precision.format_listing_signed_quantity(&fill.listing, fill.start_position)?,
        )
        .tone(if fill.start_position.inner() < 0 {
            CellTone::Negative
        } else {
            CellTone::Positive
        }),
        4 => market_price_cell(precision, &fill.listing, &fill.price, translate)?,
        5 => {
            let closed_pnl = fill.closed_pnl.value;
            DisplayCell::text(precision.format_money_profit(closed_pnl))
                .tone(amount_tone(closed_pnl))
        }
        6 => {
            let fee = fill.fee.value;
            DisplayCell::text(precision.format_money_profit(fee))
                .suffix(precision.asset_label(&fill.fee.asset, translate))
                .tone(amount_tone(fee))
        }
        7 => DisplayCell::age(now, fill.timestamp, translate),
        _ => DisplayCell::missing(),
    })
}

fn historical_order_cell(
    snapshot: &DetailSnapshot,
    row: usize,
    column: usize,
    now: i64,
    translate: &Translate,
) -> Result<DisplayCell, WalletDataError> {
    let DetailData::HistoricalOrders(orders) = &snapshot.data else {
        return Ok(DisplayCell::missing());
    };
    let Some(order) = orders.get(row) else {
        return Ok(DisplayCell::missing());
    };
    let precision = &snapshot.precision;
    Ok(match column {
        0 => listing_cell(precision, &order.listing, translate),
        1 => side_cell(order.side, translate),
        2 => market_quantity(precision, &order.listing, &order.quantity, translate)?,
        3 => money_amount(
            precision,
            order_notional(order.quantity.value, &order.price).as_ref(),
        ),
        4 => market_price_cell(precision, &order.listing, &order.price, translate)?,
        5 => trigger_price(&order.kind).map_or_else(
            || Ok(DisplayCell::missing()),
            |price| {
                market_price_cell(
                    precision,
                    &order.listing,
                    &MarketPrice {
                        value: price,
                        quote_asset: order.price.quote_asset.clone(),
                        basis: order.price.basis.clone(),
                    },
                    translate,
                )
            },
        )?,
        6 => boolean_cell(order.reduce_only, translate),
        7 => DisplayCell::text(order.status.clone()),
        8 => DisplayCell::age(now, order.timestamp, translate),
        _ => DisplayCell::missing(),
    })
}

fn ledger_cell(
    snapshot: &DetailSnapshot,
    row: usize,
    column: usize,
    now: i64,
    translate: &Translate,
) -> Result<DisplayCell, WalletDataError> {
    let DetailData::Ledger(events) = &snapshot.data else {
        return Ok(DisplayCell::missing());
    };
    let Some(event) = events.get(row) else {
        return Ok(DisplayCell::missing());
    };
    let precision = &snapshot.precision;
    Ok(match column {
        0 => event
            .asset
            .as_ref()
            .map_or_else(DisplayCell::missing, |asset| {
                DisplayCell::text(precision.asset_label(asset, translate))
            }),
        1 => DisplayCell::text(event.kind.clone()),
        2 => money_amount(precision, event.amount.as_ref()),
        3 => money_amount(precision, event.value.as_ref()),
        4 => DisplayCell::age(now, event.timestamp, translate),
        _ => DisplayCell::missing(),
    })
}

fn tab_cell(
    tab: DetailTab,
    snapshot: &DetailSnapshot,
    row: usize,
    column: usize,
    now: i64,
    translate: &Translate,
) -> Result<DisplayCell, WalletDataError> {
    match tab {
        DetailTab::Spot => spot_cell(snapshot, row, column, now, translate),
        DetailTab::Perpetual => perpetual_cell(snapshot, row, column, now, translate),
        DetailTab::OpenOrders => open_order_cell(snapshot, row, column, now, translate),
        DetailTab::Trades => trade_cell(snapshot, row, column, now, translate),
        DetailTab::HistoricalOrders => historical_order_cell(snapshot, row, column, now, translate),
        DetailTab::Ledger => ledger_cell(snapshot, row, column, now, translate),
    }
}

fn displayed_cell(
    tab: DetailTab,
    snapshot: Option<&DetailSnapshot>,
    row: usize,
    column: usize,
    now: i64,
    translate: &Translate,
) -> DisplayCell {
    let Some(snapshot) = snapshot else {
        return DisplayCell::missing();
    };
    tab_cell(tab, snapshot, row, column, now, translate).unwrap_or_else(|_| DisplayCell::missing())
}

fn tab_rows(tab: DetailTab, snapshot: &DetailSnapshot) -> usize {
    match (tab, &snapshot.data) {
        (DetailTab::Spot, DetailData::Spot(data)) => data.rows.len(),
        (DetailTab::Perpetual, DetailData::Perpetual(state)) => state.positions.len(),
        (DetailTab::OpenOrders, DetailData::OpenOrders(orders)) => orders.len(),
        (DetailTab::Trades, DetailData::Trades(fills)) => fills.len(),
        (DetailTab::HistoricalOrders, DetailData::HistoricalOrders(orders)) => orders.len(),
        (DetailTab::Ledger, DetailData::Ledger(events)) => events.len(),
        _ => 0,
    }
}

fn time_column(tab: DetailTab) -> Option<usize> {
    match tab {
        DetailTab::OpenOrders => Some(8),
        DetailTab::Trades => Some(7),
        DetailTab::HistoricalOrders => Some(8),
        DetailTab::Ledger => Some(4),
        DetailTab::Spot | DetailTab::Perpetual => None,
    }
}

fn moment_of(tab: DetailTab, snapshot: &DetailSnapshot, row: usize) -> Option<i64> {
    match (tab, &snapshot.data) {
        (DetailTab::OpenOrders, DetailData::OpenOrders(orders)) => {
            orders.get(row).map(|order| order.timestamp)
        }
        (DetailTab::Trades, DetailData::Trades(fills)) => fills.get(row).map(|fill| fill.timestamp),
        (DetailTab::HistoricalOrders, DetailData::HistoricalOrders(orders)) => {
            orders.get(row).map(|order| order.timestamp)
        }
        (DetailTab::Ledger, DetailData::Ledger(events)) => {
            events.get(row).map(|event| event.timestamp)
        }
        _ => None,
    }
}

fn moment_label(
    tab: DetailTab,
    snapshot: Option<&DetailSnapshot>,
    row: usize,
    zone: time::UtcOffset,
) -> Option<SmolStr> {
    let snapshot = snapshot?;
    moment_of(tab, snapshot, row).map(|timestamp| absolute_time(timestamp, zone))
}

struct ColumnSpec {
    header: &'static str,
    weight: f32,
    align: ColumnAlign,
}

const SPOT_COLUMNS: [ColumnSpec; 5] = [
    ColumnSpec {
        header: "widget.hyperliquid_wallet.asset",
        weight: 0.15,
        align: ColumnAlign::Start,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.asset_share",
        weight: 0.18,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.quantity",
        weight: 0.22,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.value",
        weight: 0.22,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "common.price",
        weight: 0.23,
        align: ColumnAlign::End,
    },
];

const PERPETUAL_COLUMNS: [ColumnSpec; 9] = [
    ColumnSpec {
        header: "widget.hyperliquid_wallet.coin",
        weight: 0.08,
        align: ColumnAlign::Start,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.side",
        weight: 0.08,
        align: ColumnAlign::Center,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.position_value",
        weight: 0.12,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.unrealized_pnl",
        weight: 0.12,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.entry",
        weight: 0.12,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.mark",
        weight: 0.12,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.liquidation",
        weight: 0.12,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.margin",
        weight: 0.12,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.funding",
        weight: 0.12,
        align: ColumnAlign::End,
    },
];

const OPEN_ORDER_COLUMNS: [ColumnSpec; 9] = [
    ColumnSpec {
        header: "widget.hyperliquid_wallet.coin",
        weight: 0.09,
        align: ColumnAlign::Start,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.collateral",
        weight: 0.09,
        align: ColumnAlign::Start,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.side",
        weight: 0.09,
        align: ColumnAlign::Center,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.quantity",
        weight: 0.13,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.value",
        weight: 0.13,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "common.price",
        weight: 0.13,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.trigger_price",
        weight: 0.13,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.reduce_only",
        weight: 0.10,
        align: ColumnAlign::Center,
    },
    ColumnSpec {
        header: "common.time",
        weight: 0.11,
        align: ColumnAlign::End,
    },
];

const TRADE_COLUMNS: [ColumnSpec; 8] = [
    ColumnSpec {
        header: "widget.hyperliquid_wallet.coin",
        weight: 0.09,
        align: ColumnAlign::Start,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.side",
        weight: 0.09,
        align: ColumnAlign::Center,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.quantity",
        weight: 0.14,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.position",
        weight: 0.14,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "common.price",
        weight: 0.14,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.realized_pnl",
        weight: 0.14,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.fee",
        weight: 0.15,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "common.time",
        weight: 0.11,
        align: ColumnAlign::End,
    },
];

const HISTORICAL_ORDER_COLUMNS: [ColumnSpec; 9] = [
    ColumnSpec {
        header: "widget.hyperliquid_wallet.coin",
        weight: 0.09,
        align: ColumnAlign::Start,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.side",
        weight: 0.09,
        align: ColumnAlign::Center,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.quantity",
        weight: 0.13,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.value",
        weight: 0.13,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "common.price",
        weight: 0.13,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.trigger_price",
        weight: 0.13,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.reduce_only",
        weight: 0.09,
        align: ColumnAlign::Center,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.status",
        weight: 0.11,
        align: ColumnAlign::Start,
    },
    ColumnSpec {
        header: "common.time",
        weight: 0.10,
        align: ColumnAlign::End,
    },
];

const LEDGER_COLUMNS: [ColumnSpec; 5] = [
    ColumnSpec {
        header: "widget.hyperliquid_wallet.coin",
        weight: 0.16,
        align: ColumnAlign::Start,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.kind",
        weight: 0.18,
        align: ColumnAlign::Center,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.quantity",
        weight: 0.24,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "widget.hyperliquid_wallet.value",
        weight: 0.24,
        align: ColumnAlign::End,
    },
    ColumnSpec {
        header: "common.time",
        weight: 0.18,
        align: ColumnAlign::End,
    },
];

fn specs(tab: DetailTab) -> &'static [ColumnSpec] {
    match tab {
        DetailTab::Spot => &SPOT_COLUMNS,
        DetailTab::Perpetual => &PERPETUAL_COLUMNS,
        DetailTab::OpenOrders => &OPEN_ORDER_COLUMNS,
        DetailTab::Trades => &TRADE_COLUMNS,
        DetailTab::HistoricalOrders => &HISTORICAL_ORDER_COLUMNS,
        DetailTab::Ledger => &LEDGER_COLUMNS,
    }
}

fn columns(tab: DetailTab, translate: &Translate) -> Vec<Column> {
    specs(tab)
        .iter()
        .map(|spec| {
            Column::flex(
                t_smol!(translate, spec.header),
                spec.weight,
                LogicalPx::ZERO,
            )
            .align(spec.align)
        })
        .collect()
}

#[derive(Clone)]
struct CellContext {
    tab: DetailTab,
    readings: [Signal<Option<Arc<DetailSnapshot>>>; 6],
    nows: [Signal<i64>; 6],
    config: Signal<Config>,
    language: Signal<LanguageCode>,
    theme: ThemeHandle,
    translate: Translate,
    font_size: LogicalPx,
}

fn build_cell(
    tree: &mut NodeTree,
    parent: NodeId,
    row: usize,
    column: usize,
    context: &CellContext,
) {
    let tab = context.tab;
    let tag = tab.index();
    let reading = context.readings[tag];
    let language = context.language;
    let nows = context.nows;
    let translate = context.translate.clone();
    let value = NodeBuilder::text(SmolStr::default(), TextStyle::new(context.font_size));
    let value = value.bind_text(move |read| {
        let _language = *read.read(&language);
        let snapshot = read.read(&reading);
        let now = if Some(column) == time_column(tab) {
            *read.read(&nows[tag])
        } else {
            0
        };
        displayed_cell(tab, snapshot.as_deref(), row, column, now, &translate).text
    });
    let tone_reading = reading;
    let tone_translate = context.translate.clone();
    let theme = context.theme.clone();
    let value = value.bind_style(move |read| {
        let snapshot = read.read(&tone_reading);
        let tone = displayed_cell(tab, snapshot.as_deref(), row, column, 0, &tone_translate).tone;
        StylePatch {
            foreground: Some(theme.role(read, tone_role(tone))),
            ..StylePatch::default()
        }
    });
    let value = match time_column(tab) {
        Some(time) if time == column => {
            let config = context.config;
            value.bind_a11y(move |read| {
                let _language = *read.read(&language);
                let zone = data_time_zone(&read.read(&config));
                let snapshot = read.read(&reading);
                A11yPatch {
                    label: moment_label(tab, snapshot.as_deref(), row, zone),
                    ..A11yPatch::default()
                }
            })
        }
        _ => value,
    };
    let _ = value.build(tree, parent);

    let suffix_reading = reading;
    let suffix_language = language;
    let suffix_translate = context.translate.clone();
    let suffix_theme = context.theme.clone();
    let _ = NodeBuilder::text(SmolStr::default(), TextStyle::new(context.font_size))
        .bind_text(move |read| {
            let _language = *read.read(&suffix_language);
            let snapshot = read.read(&suffix_reading);
            displayed_cell(tab, snapshot.as_deref(), row, column, 0, &suffix_translate)
                .suffix
                .unwrap_or_default()
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(suffix_theme.role(read, Role::FgDefault)),
            ..StylePatch::default()
        })
        .build(tree, parent);
}

pub(super) struct DetailTables {
    area: NodeId,
    stack: NodeId,
    spinner_slot: NodeId,
    tab: Signal<DetailTab>,
    readings: [Signal<Option<Arc<DetailSnapshot>>>; 6],
    nows: [Signal<i64>; 6],
    page: Signal<bool>,
    overlay_visible: Signal<bool>,
    config: Signal<Config>,
    language: Signal<LanguageCode>,
    theme: ThemeHandle,
    translate: Translate,
    tables: RefCell<Vec<Table>>,
    wrappers: RefCell<Vec<NodeId>>,
    spinner: RefCell<Option<Spinner>>,
    loading: Cell<[bool; 6]>,
    seen_language: Cell<LanguageCode>,
    timer: RefCell<Option<TimerId>>,
    timer_fired: Rc<Cell<bool>>,
}

impl DetailTables {
    pub(super) fn build(
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        page: Signal<bool>,
    ) -> Result<Self, EngineError> {
        let theme = cx.theme().clone();
        let language = cx.language();
        let config = cx.services().config;
        let translate = cx.translate().clone();
        let tab = cx.signal(DetailTab::Spot);
        let readings: [Signal<Option<Arc<DetailSnapshot>>>; 6] =
            std::array::from_fn(|_| cx.signal(None));
        let now = now_millis().unwrap_or(0);
        let nows: [Signal<i64>; 6] = std::array::from_fn(|_| cx.signal(now));
        let overlay_visible = cx.signal(false);
        let seen_language = *cx.read(&language);

        let area = NodeBuilder::boxed()
            .id("TEST_HYPERLIQUID_WALLET_DETAIL")
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .flex_col()
                    .size_full()
                    .min_width_0()
                    .min_height_0()
                    .relative()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        let stack = NodeBuilder::boxed()
            .id("TEST_HYPERLIQUID_WALLET_DETAIL_STACK")
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .min_width_0()
                    .min_height_0()
                    .relative()
                    .build(),
            )
            .build(cx.tree_mut(), area)?;
        let overlay = NodeBuilder::boxed()
            .id("TEST_HYPERLIQUID_WALLET_DETAIL_LOADING")
            .style(
                StyleBuilder::new()
                    .absolute()
                    .top(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .left(px(0.0))
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(6.0))
                    .build(),
            )
            .a11y(A11yOverride {
                role: Some(A11yRole::ProgressIndicator),
                ..A11yOverride::default()
            })
            .bind_style(move |read| StylePatch {
                visible: Some(*read.read(&overlay_visible)),
                ..StylePatch::default()
            })
            .bind_a11y({
                let translate = translate.clone();
                move |read| {
                    let _language = *read.read(&language);
                    let visible = *read.read(&overlay_visible);
                    A11yPatch {
                        hidden: Some(!visible),
                        label: Some(t_smol!(translate, "common.loading")),
                        ..A11yPatch::default()
                    }
                }
            })
            .build(cx.tree_mut(), area)?;
        let spinner_slot = NodeBuilder::boxed()
            .id("TEST_HYPERLIQUID_WALLET_DETAIL_SPINNER")
            .style(StyleBuilder::new().flex_none().build())
            .build(cx.tree_mut(), overlay)?;
        let _ = NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(theme.tokens().medium.font_size),
        )
        .id("TEST_HYPERLIQUID_WALLET_DETAIL_WAITING")
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        })
        .bind_text({
            let translate = translate.clone();
            move |read| {
                let _language = *read.read(&language);
                t_smol!(translate, "common.loading")
            }
        })
        .bind_style({
            let theme = theme.clone();
            move |read| StylePatch {
                foreground: Some(theme.role(read, Role::FgMuted)),
                ..StylePatch::default()
            }
        })
        .build(cx.tree_mut(), overlay)?;

        let tables = Self {
            area,
            stack,
            spinner_slot,
            tab,
            readings,
            nows,
            page,
            overlay_visible,
            config,
            language,
            theme,
            translate,
            tables: RefCell::new(Vec::with_capacity(DetailTab::ALL.len())),
            wrappers: RefCell::new(Vec::with_capacity(DetailTab::ALL.len())),
            spinner: RefCell::new(None),
            loading: Cell::new([false; 6]),
            seen_language: Cell::new(seen_language),
            timer: RefCell::new(None),
            timer_fired: Rc::new(Cell::new(false)),
        };
        tables.build_tables(cx)?;
        Ok(tables)
    }

    pub(super) fn accept(
        &self,
        tab: DetailTab,
        snapshot: Arc<DetailSnapshot>,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let count = tab_rows(tab, &snapshot);
        cx.write(&self.nows[tab.index()], now_millis().unwrap_or(0));
        cx.write(&self.readings[tab.index()], Some(snapshot));
        if let Some(table) = self.tables.borrow().get(tab.index()) {
            table.set_rows(cx.app_mut(), count)?;
        }
        let mut flags = self.loading.get();
        flags[tab.index()] = false;
        self.loading.set(flags);
        self.sync_loading(cx)
    }

    pub(super) fn show(&self, tab: DetailTab, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if *cx.read(&self.tab) != tab {
            cx.write(&self.tab, tab);
            cx.write(&self.nows[tab.index()], now_millis().unwrap_or(0));
        }
        self.sync_loading(cx)
    }

    pub(super) fn clear(&self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        for (index, reading) in self.readings.iter().enumerate() {
            cx.write(reading, None);
            if let Some(table) = self.tables.borrow().get(index) {
                table.set_rows(cx.app_mut(), 0)?;
            }
        }
        self.loading.set([false; 6]);
        self.sync_loading(cx)
    }

    pub(super) fn discard(&self, tab: DetailTab, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        cx.write(&self.readings[tab.index()], None);
        if let Some(table) = self.tables.borrow().get(tab.index()) {
            table.set_rows(cx.app_mut(), 0)?;
        }
        let mut flags = self.loading.get();
        flags[tab.index()] = false;
        self.loading.set(flags);
        self.sync_loading(cx)
    }

    pub(super) fn set_loading(
        &self,
        tab: DetailTab,
        loading: bool,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let mut flags = self.loading.get();
        flags[tab.index()] = loading;
        self.loading.set(flags);
        self.sync_loading(cx)
    }

    pub(super) fn sync(&self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        if language != self.seen_language.get() {
            self.seen_language.set(language);
            self.rebuild(cx)?;
        }
        self.sync_age_clock(cx);
        if let Some(spinner) = self.spinner.borrow().as_ref() {
            spinner.sync(cx.app_mut())?;
        }
        for table in self.tables.borrow().iter() {
            table.sync(cx.app_mut())?;
        }
        Ok(())
    }

    pub(super) fn release(&self, cx: &mut WidgetCx<'_>) {
        if let Some(timer) = self.timer.borrow_mut().take() {
            cx.app_mut().cancel_timer(&timer);
        }
        if let Some(spinner) = self.spinner.borrow_mut().take() {
            let window = cx.window();
            if let Some(node) = spinner.node() {
                let _ = cx.app_mut().remove_node(window, node);
            }
            spinner.release(cx.app_mut());
        }
        for table in self.tables.borrow_mut().drain(..) {
            table.release(cx.app_mut());
        }
        self.wrappers.borrow_mut().clear();
    }

    #[cfg(test)]
    pub(super) fn cell_node(
        &self,
        tab: DetailTab,
        row: usize,
        column: usize,
        app: &ui_core::app::App,
    ) -> Option<NodeId> {
        self.tables
            .borrow()
            .get(tab.index())
            .and_then(|table| table.cell_node(app, row, column))
    }

    #[cfg(test)]
    pub(super) fn row_count(&self, tab: DetailTab) -> usize {
        self.tables.borrow().get(tab.index()).map_or(0, Table::rows)
    }

    #[cfg(test)]
    pub(super) fn overlay_visible(&self, app: &ui_core::app::App) -> bool {
        *app.read(&self.overlay_visible)
    }

    #[cfg(test)]
    pub(super) fn age_clock_armed(&self) -> bool {
        self.timer.borrow().is_some()
    }

    #[cfg(test)]
    pub(super) fn area_node(&self) -> NodeId {
        self.area
    }

    fn build_tables(&self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let window = cx.window();
        let font_size = self.theme.tokens().medium.font_size;
        let options = TableOptions::fixed(row_height(&self.theme)).selection(SelectionMode::None);
        let mut tables = self.tables.borrow_mut();
        let mut wrappers = self.wrappers.borrow_mut();
        for tab in DetailTab::ALL {
            let wrapper = NodeBuilder::boxed()
                .id(format_smolstr!(
                    "TEST_HYPERLIQUID_WALLET_DETAIL_TAB_{}",
                    tab.index()
                ))
                .style(
                    StyleBuilder::new()
                        .absolute()
                        .top(px(0.0))
                        .right(px(0.0))
                        .bottom(px(0.0))
                        .left(px(0.0))
                        .flex_col()
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .bind_style({
                    let shown = self.tab;
                    move |read| StylePatch {
                        visible: Some(*read.read(&shown) == tab),
                        ..StylePatch::default()
                    }
                })
                .bind_a11y({
                    let shown = self.tab;
                    move |read| A11yPatch {
                        hidden: Some(*read.read(&shown) != tab),
                        ..A11yPatch::default()
                    }
                })
                .build(cx.tree_mut(), self.stack)?;
            let rows = cx
                .read(&self.readings[tab.index()])
                .as_deref()
                .map_or(0, |snapshot| tab_rows(tab, snapshot));
            let table = self.make_table(cx, tab, rows, font_size, options.clone());
            table.mount(cx.app_mut(), window, wrapper)?;
            wrappers.push(wrapper);
            tables.push(table);
        }
        Ok(())
    }

    fn make_table(
        &self,
        cx: &mut WidgetCx<'_>,
        tab: DetailTab,
        rows: usize,
        font_size: LogicalPx,
        options: TableOptions,
    ) -> Table {
        let context = CellContext {
            tab,
            readings: self.readings,
            nows: self.nows,
            config: self.config,
            language: self.language,
            theme: self.theme.clone(),
            translate: self.translate.clone(),
            font_size,
        };
        let key = cx.unique_key(&format_smolstr!(
            "hyperliquid_wallet.detail.{}",
            tab.index()
        ));
        let cells = move |tree: &mut NodeTree, parent: NodeId, row: usize, column: usize| {
            build_cell(tree, parent, row, column, &context);
        };
        let table = Table::new(
            cx.app_mut(),
            self.theme.clone(),
            key,
            columns(tab, &self.translate),
            options,
            cells,
            t_smol!(self.translate, "common.no_data"),
        );
        table.set_initial_rows(cx.app_mut(), rows);
        table
    }

    fn rebuild(&self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let window = cx.window();
        for wrapper in self.wrappers.borrow_mut().drain(..) {
            let _ = cx.app_mut().remove_node(window, wrapper);
        }
        for table in self.tables.borrow_mut().drain(..) {
            table.release(cx.app_mut());
        }
        self.build_tables(cx)
    }

    fn sync_loading(&self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let shown = *cx.read(&self.tab);
        let visible = self.loading.get()[shown.index()];
        cx.write(&self.overlay_visible, visible);
        let built = self.spinner.borrow().is_some();
        if visible && !built {
            let spinning = !*cx.read(&cx.services().reduce_motion);
            let key = cx.unique_key("hyperliquid_wallet.detail.loading");
            let theme = self.theme.clone();
            let spinner = Spinner::new(cx.app_mut(), theme, key)
                .with_role(Role::FgMuted)
                .spinning(spinning);
            let window = cx.window();
            spinner.build(cx.app_mut(), window, self.spinner_slot)?;
            *self.spinner.borrow_mut() = Some(spinner);
        } else if !visible && built {
            let Some(spinner) = self.spinner.borrow_mut().take() else {
                return Ok(());
            };
            let window = cx.window();
            if let Some(node) = spinner.node() {
                let _ = cx.app_mut().remove_node(window, node);
            }
            spinner.release(cx.app_mut());
        }
        Ok(())
    }

    fn sync_age_clock(&self, cx: &mut WidgetCx<'_>) {
        if self.timer_fired.get() {
            self.timer_fired.set(false);
            self.timer.borrow_mut().take();
        }
        let tab = *cx.read(&self.tab);
        let shown = *cx.read(&self.page);
        let armed =
            shown && time_column(tab).is_some() && cx.read(&self.readings[tab.index()]).is_some();
        if !armed {
            if let Some(timer) = self.timer.borrow_mut().take() {
                cx.app_mut().cancel_timer(&timer);
            }
            return;
        }
        if self.timer.borrow().is_some() {
            return;
        }
        let fired = Rc::clone(&self.timer_fired);
        let nows = self.nows;
        let tab_signal = self.tab;
        let window = cx.window();
        let scope = self.area;
        match cx
            .app_mut()
            .after_scoped(window, scope, AGE_TICK, move |app| {
                fired.set(true);
                let index = app.read(&tab_signal).index();
                app.write(&nows[index], now_millis().unwrap_or(0));
            }) {
            Ok(timer) => *self.timer.borrow_mut() = Some(timer),
            Err(error) => log::warn!("the Hyperliquid wallet ages could not be timed: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use i18n::{language::codes::LanguageCode, translate::Translate};
    use market_participants_connector::{
        account::{
            domain::{
                HyperliquidOpenOrder, HyperliquidPerpetualPosition, HyperliquidPerpetualState,
                HyperliquidSpotBalance,
            },
            shared::{TimeInForce, TriggerPriceKind},
        },
        catalog::batch::CatalogEntry,
    };
    use market_participants_model::market::units::QuantityUnit;
    use smol_str::SmolStr;
    use ui_core::node::signal::SignalScope;
    use widget::test_support::WidgetTestApp;

    use super::*;
    use crate::{
        model::SpotSnapshot,
        test_support::{
            address, context, deposit, entry, fill, historical_order, money, open_order,
            pair_listing, perp_instrument, perpetual_route, position, precision_of, spot_entry,
            spot_pair, spot_route, spot_state, token, wallet_balance,
        },
    };

    fn translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    fn row(
        tab: DetailTab,
        snapshot: &DetailSnapshot,
        index: usize,
        now: i64,
        translate: &Translate,
    ) -> Vec<DisplayCell> {
        (0..specs(tab).len())
            .map(|column| displayed_cell(tab, Some(snapshot), index, column, now, translate))
            .collect()
    }

    fn texts(row: &[DisplayCell]) -> Vec<&str> {
        row.iter().map(|cell| cell.text.as_str()).collect()
    }

    fn suffix(row: &[DisplayCell], index: usize) -> Option<&str> {
        row[index].suffix.as_deref()
    }

    fn perp_listing() -> VenueListingKey {
        perp_entry().listing.key
    }

    fn perp_entry() -> CatalogEntry {
        entry(
            &perpetual_route(),
            perp_instrument("ETH"),
            "ETH",
            "0.1",
            "0.001",
        )
    }

    fn spot_listing() -> VenueListingKey {
        pair_listing("HYPE", "USDC")
    }

    fn perpetual_precision() -> DisplayPrecision {
        precision_of(&perpetual_route(), vec![perp_entry()])
    }

    fn spot_precision() -> DisplayPrecision {
        precision_of(
            &spot_route(),
            vec![
                spot_entry(&spot_route(), "HYPE", "USDC", "0.001", "0.001"),
                spot_entry(&spot_route(), "SOL", "USDC", "0.001", "0.001"),
            ],
        )
    }

    fn spot_reading(balances: Vec<HyperliquidSpotBalance>) -> DetailSnapshot {
        let precision = spot_precision();
        DetailSnapshot::new(
            DetailData::Spot(
                SpotSnapshot::new(
                    spot_state(balances),
                    vec![spot_pair("HYPE", "USDC")],
                    Vec::new(),
                    &precision,
                )
                .expect("the reading should value"),
            ),
            precision,
        )
        .expect("a published listing should validate")
    }

    fn perpetual_reading(positions: Vec<HyperliquidPerpetualPosition>) -> DetailSnapshot {
        DetailSnapshot::new(
            DetailData::Perpetual(HyperliquidPerpetualState { positions }),
            perpetual_precision(),
        )
        .expect("a published listing should validate")
    }

    #[test]
    fn a_spot_row_should_use_the_assets_own_ticks_and_its_share() {
        let translate = translate(LanguageCode::En);
        let mut reading = spot_reading(vec![
            wallet_balance("USDC", "10"),
            wallet_balance("HYPE", "-2"),
        ]);
        if let DetailData::Spot(snapshot) = &mut reading.data {
            let precision = spot_precision();
            snapshot
                .apply_contexts(&[context(&spot_listing(), Some("2"), None)], &precision)
                .expect("a context should apply");
        }

        let rows = row(DetailTab::Spot, &reading, 0, 0, &translate);

        assert_eq!(
            texts(&rows),
            ["USDC", "166.66%", "10.00", "10.00", "1.00"],
            "a stablecoin is money, so its own balance and price are read as money"
        );
        assert_eq!(suffix(&rows, 2), Some("USDC"));
        let rows = row(DetailTab::Spot, &reading, 1, 0, &translate);
        assert_eq!(
            texts(&rows),
            ["HYPE", "-66.66%", "-2.000", "-4.00", "2.000"]
        );
        assert_eq!(suffix(&rows, 2), Some("HYPE"));
        assert_eq!(rows[2].tone, CellTone::Negative);
        assert_eq!(rows[3].tone, CellTone::Negative);
    }

    #[test]
    fn a_perpetual_row_should_carry_its_direction_value_and_profit() {
        let translate = translate(LanguageCode::En);
        let mut reading = perpetual_reading(vec![position(&perp_listing(), "2", Some("100"))]);
        reading
            .apply_contexts(&[context(&perp_listing(), Some("110"), None)])
            .expect("a context should apply");

        let rows = row(DetailTab::Perpetual, &reading, 0, 0, &translate);

        assert_eq!(
            texts(&rows),
            [
                "ETH", "Long", "220.00", "+20.00", "100.0", "110.0", "-", "-", "0"
            ]
        );
        assert_eq!(rows[1].tone, CellTone::Positive);
        assert_eq!(rows[3].tone, CellTone::Positive);
        assert_eq!(
            rows[6].tone,
            CellTone::Muted,
            "a value the venue did not report is not a number"
        );
        assert_eq!(
            suffix(&rows, 4),
            Some("USDC"),
            "a mark price says which asset it is quoted in"
        );
    }

    #[test]
    fn a_short_position_should_be_read_as_a_short() {
        let translate = translate(LanguageCode::En);
        let reading = perpetual_reading(vec![position(&perp_listing(), "-3", None)]);

        let rows = row(DetailTab::Perpetual, &reading, 0, 0, &translate);

        assert_eq!(
            rows[1].text.as_str(),
            "Short",
            "a negative size is a position on the short side"
        );
        assert_eq!(rows[1].tone, CellTone::Negative);
        assert_eq!(
            rows[3].tone,
            CellTone::Muted,
            "a position without an entry has no profit to report"
        );
    }

    #[test]
    fn a_short_position_should_be_valued_from_the_side_it_is_held() {
        let translate = translate(LanguageCode::En);
        let mut reading = perpetual_reading(vec![position(&perp_listing(), "-2", Some("100"))]);
        reading
            .apply_contexts(&[context(&perp_listing(), Some("90"), None)])
            .expect("a context should apply");

        let rows = row(DetailTab::Perpetual, &reading, 0, 0, &translate);

        assert_eq!(
            texts(&rows),
            [
                "ETH", "Short", "180.00", "+20.00", "100.0", "90.0", "-", "-", "0"
            ],
            "a short the price fell away from is up what a long would be down"
        );
        assert_eq!(rows[3].tone, CellTone::Positive);
    }

    #[test]
    fn every_tab_should_draw_the_rows_of_its_reading() {
        let translate = translate(LanguageCode::En);
        let wallet = address("1");
        let precision = perpetual_precision();
        let reading = |data: DetailData| {
            DetailSnapshot::new(data, precision.clone())
                .expect("a published listing should validate")
        };

        let spot = spot_reading(vec![wallet_balance("USDC", "1")]);
        let perpetual = perpetual_reading(vec![position(&perp_listing(), "1", None)]);
        let orders = reading(DetailData::OpenOrders(vec![
            open_order(&perp_listing(), "7", 1),
            open_order(&perp_listing(), "8", 2),
        ]));
        let trades = reading(DetailData::Trades(vec![fill(
            &wallet,
            &perp_listing(),
            Side::Buy,
            "1",
            "100",
            1,
            1,
        )]));
        let history = reading(DetailData::HistoricalOrders(vec![historical_order(
            &perp_listing(),
            "8",
            1,
        )]));
        let ledger =
            DetailSnapshot::new(DetailData::Ledger(vec![deposit(&wallet, "5", 1)]), money())
                .expect("a ledger validates");

        assert_eq!(
            [
                tab_rows(DetailTab::Spot, &spot),
                tab_rows(DetailTab::Perpetual, &perpetual),
                tab_rows(DetailTab::OpenOrders, &orders),
                tab_rows(DetailTab::Trades, &trades),
                tab_rows(DetailTab::HistoricalOrders, &history),
                tab_rows(DetailTab::Ledger, &ledger),
            ],
            [1, 1, 2, 1, 1, 1],
            "every tab draws one row per thing its reading holds"
        );
        for index in 0..2 {
            let rows = row(DetailTab::OpenOrders, &orders, index, 0, &translate);
            assert_eq!(rows.len(), specs(DetailTab::OpenOrders).len());
        }

        let spot = spot_reading(Vec::new());
        let perpetual = perpetual_reading(Vec::new());
        let orders = reading(DetailData::OpenOrders(Vec::new()));
        let trades = reading(DetailData::Trades(Vec::new()));
        let history = reading(DetailData::HistoricalOrders(Vec::new()));
        let ledger = DetailSnapshot::new(DetailData::Ledger(Vec::new()), money())
            .expect("an empty ledger validates");

        assert_eq!(
            [
                tab_rows(DetailTab::Spot, &spot),
                tab_rows(DetailTab::Perpetual, &perpetual),
                tab_rows(DetailTab::OpenOrders, &orders),
                tab_rows(DetailTab::Trades, &trades),
                tab_rows(DetailTab::HistoricalOrders, &history),
                tab_rows(DetailTab::Ledger, &ledger),
            ],
            [0, 0, 0, 0, 0, 0],
            "a reading with nothing in it draws no row at all"
        );
    }

    #[test]
    fn an_order_quantity_should_carry_the_listings_own_unit() {
        let translate = translate(LanguageCode::En);
        let order = open_order(&perp_listing(), "7", 1);
        let reading =
            DetailSnapshot::new(DetailData::OpenOrders(vec![order]), perpetual_precision())
                .expect("a published listing should validate");

        let rows = row(DetailTab::OpenOrders, &reading, 0, 0, &translate);

        assert_eq!(
            suffix(&rows, 3),
            Some("contracts"),
            "the venue published the order in contracts, so that is what it is read as"
        );
    }

    #[test]
    fn a_position_without_an_entry_should_report_no_profit() {
        let translate = translate(LanguageCode::En);
        let reading = perpetual_reading(vec![position(&perp_listing(), "3", None)]);

        let rows = row(DetailTab::Perpetual, &reading, 0, 0, &translate);

        assert_eq!(
            rows[1].text.as_str(),
            "Long",
            "a position the venue published with a size is a long by the size it carries"
        );
        assert_eq!(
            rows[3].tone,
            CellTone::Muted,
            "a position without an entry has no profit to report"
        );
    }

    #[test]
    fn an_open_order_row_should_show_its_trigger_and_its_collateral() {
        let translate = translate(LanguageCode::En);
        let order = HyperliquidOpenOrder {
            reduce_only: true,
            kind: OrderKind::StopLimit {
                price: "100".parse().expect("a price should be valid"),
                trigger_price: "95".parse().expect("a price should be valid"),
                trigger_price_kind: TriggerPriceKind::Mark,
                time_in_force: TimeInForce::GoodTillCancelled,
            },
            ..open_order(&perp_listing(), "7", 1)
        };
        let reading =
            DetailSnapshot::new(DetailData::OpenOrders(vec![order]), perpetual_precision())
                .expect("a published listing should validate");

        let rows = row(DetailTab::OpenOrders, &reading, 0, 0, &translate);

        assert_eq!(
            texts(&rows),
            [
                "ETH", "USDC", "Buy", "2.000", "200.00", "100.0", "95.0", "Yes", "0s ago"
            ]
        );
        assert_eq!(rows[2].tone, CellTone::Positive);
    }

    #[test]
    fn a_trade_row_should_suffix_the_fee_with_its_token() {
        let translate = translate(LanguageCode::En);
        let wallet = address("1");
        let mut trade = fill(&wallet, &perp_listing(), Side::Sell, "2", "100", 1, 1);
        trade.fee = AssetAmount {
            asset: token("USDC"),
            value: "-0.1"
                .parse::<SignedQuantity>()
                .expect("a fee should be valid"),
        };
        trade.closed_pnl = AssetAmount {
            asset: token("USDC"),
            value: "3".parse().expect("a profit should be valid"),
        };
        let reading = DetailSnapshot::new(DetailData::Trades(vec![trade]), perpetual_precision())
            .expect("a published listing should validate");

        let rows = row(DetailTab::Trades, &reading, 0, 0, &translate);

        assert_eq!(rows[5].text.as_str(), "+3.00");
        assert_eq!(rows[5].tone, CellTone::Positive);
        assert_eq!(rows[6].text.as_str(), "-0.10");
        assert_eq!(suffix(&rows, 6), Some("USDC"));
        assert_eq!(rows[6].tone, CellTone::Negative);
    }

    #[test]
    fn a_historical_order_row_should_keep_the_venues_own_status() {
        let translate = translate(LanguageCode::En);
        let mut order = historical_order(&perp_listing(), "8", 1);
        order.status = SmolStr::new_static("filledByLiquidation");
        let reading = DetailSnapshot::new(
            DetailData::HistoricalOrders(vec![order]),
            perpetual_precision(),
        )
        .expect("a published listing should validate");

        let rows = row(DetailTab::HistoricalOrders, &reading, 0, 0, &translate);

        assert_eq!(rows[7].text.as_str(), "filledByLiquidation");
        assert_eq!(rows[6].text.as_str(), "Yes");
    }

    #[test]
    fn a_ledger_row_should_write_a_missing_value_as_a_dash() {
        let translate = translate(LanguageCode::En);
        let wallet = address("1");
        let mut event = deposit(&wallet, "5", 1);
        event.asset = None;
        event.amount = None;
        event.value = None;
        let reading = DetailSnapshot::new(DetailData::Ledger(vec![event]), money())
            .expect("a ledger validates");

        let rows = row(DetailTab::Ledger, &reading, 0, 0, &translate);

        assert_eq!(
            &texts(&rows)[..4],
            ["-", "deposit", "-", "-"],
            "a value the venue did not report is not a zero"
        );
        assert_eq!(rows[0].tone, CellTone::Muted);
    }

    #[test]
    fn a_reading_of_another_tab_should_leave_its_table_empty() {
        let translate = translate(LanguageCode::En);
        let reading = DetailSnapshot::new(DetailData::Ledger(Vec::new()), money())
            .expect("a ledger validates");

        assert_eq!(tab_rows(DetailTab::Spot, &reading), 0);
        assert_eq!(tab_rows(DetailTab::OpenOrders, &reading), 0);
        assert_eq!(
            displayed_cell(DetailTab::Spot, Some(&reading), 0, 0, 0, &translate).text,
            "-",
            "a reading of another tab draws no cell of this one"
        );
    }

    #[test]
    fn an_age_should_follow_the_second_the_clock_is_in() {
        let translate = translate(LanguageCode::En);
        let wallet = address("1");
        let reading = DetailSnapshot::new(
            DetailData::Trades(vec![fill(
                &wallet,
                &perp_listing(),
                Side::Buy,
                "1",
                "100",
                1_000,
                1,
            )]),
            perpetual_precision(),
        )
        .expect("a published listing should validate");

        let first = displayed_cell(DetailTab::Trades, Some(&reading), 0, 7, 1_000, &translate);
        assert_eq!(first.text.as_str(), "0s ago");
        assert_eq!(
            displayed_cell(DetailTab::Trades, Some(&reading), 0, 7, 1_500, &translate).text,
            first.text,
            "a moment inside the second the cell was drawn at does not move the age"
        );

        let later = displayed_cell(DetailTab::Trades, Some(&reading), 0, 7, 61_000, &translate);
        assert_ne!(later.text, first.text, "the age follows the clock");
        assert_eq!(
            moment_of(DetailTab::Trades, &reading, 0),
            Some(1_000),
            "the row keeps the moment it is about, which is what its name is read from"
        );
    }

    #[test]
    fn a_balance_price_should_be_read_in_the_tick_of_the_pair_that_priced_it() {
        let translate = translate(LanguageCode::En);
        let precision = precision_of(
            &spot_route(),
            vec![
                spot_entry(&spot_route(), "HYPE", "USDC", "0.001", "0.001"),
                spot_entry(&spot_route(), "HYPE", "USDH", "0.01", "0.001"),
            ],
        );
        let usdh = pair_listing("HYPE", "USDH");
        let snapshot = SpotSnapshot::new(
            spot_state(vec![wallet_balance("HYPE", "2")]),
            vec![spot_pair("HYPE", "USDC"), spot_pair("HYPE", "USDH")],
            vec![context(&usdh, Some("1.0004"), None)],
            &precision,
        )
        .expect("a balance of a two-quote asset should value");
        let rows = row(
            DetailTab::Spot,
            &DetailSnapshot::new(DetailData::Spot(snapshot), precision)
                .expect("the reading should validate"),
            0,
            0,
            &translate,
        );

        assert_eq!(
            rows[4].text.as_str(),
            "1.00",
            "the tick of the pair that priced the balance draws it, not the other pair's"
        );
        assert_eq!(rows[4].suffix.as_deref(), Some("USDH"));
    }

    #[test]
    fn a_notional_quantity_should_not_carry_an_asset_suffix() {
        let translate = translate(LanguageCode::En);
        let precision = perpetual_precision();
        let notional = MarketQuantity::amount(
            market_participants_model::decimal::Quantity::from(5_u8),
            QuantityUnit::Notional {
                asset: token("USDC"),
            },
        )
        .expect("a notional is an amount");

        let cell = market_quantity(&precision, &perp_listing(), &notional, &translate)
            .expect("a published listing should format an amount");

        assert_eq!(cell.text.as_str(), "5.000");
        assert_eq!(
            cell.suffix, None,
            "an amount is money, so the asset it is in is not repeated"
        );
    }

    #[test]
    fn every_tables_columns_should_fill_the_width_it_is_given() {
        let translate = translate(LanguageCode::En);
        for tab in DetailTab::ALL {
            let total = specs(tab).iter().map(|spec| spec.weight).sum::<f32>();
            assert!(
                (total - 1.0).abs() < 0.001,
                "{}'s columns should fill the table, not {total}",
                tab.index()
            );
            assert_eq!(
                columns(tab, &translate).len(),
                specs(tab).len(),
                "a tab declares a column for every weight"
            );
        }
    }

    fn tables(harness: &mut WidgetTestApp) -> DetailTables {
        let root = harness.tree().root();
        harness.with_cx(|cx| {
            let page = cx.signal(true);
            DetailTables::build(cx, root, page).expect("the tables build")
        })
    }

    fn settle(harness: &mut WidgetTestApp, tables: &DetailTables) {
        for _ in 0..4 {
            harness.with_cx(|cx| tables.sync(cx).expect("the tables sync"));
            harness.drain();
        }
    }

    fn table(tables: &DetailTables, tab: DetailTab) -> Table {
        tables.tables.borrow()[tab.index()].clone()
    }

    fn loading_overlay(harness: &WidgetTestApp, tables: &DetailTables) -> NodeId {
        harness
            .find_by_debug_prefix(tables.area, "TEST_HYPERLIQUID_WALLET_DETAIL_LOADING")
            .expect("the loading overlay is built with the tables")
    }

    fn cell_text(
        harness: &mut WidgetTestApp,
        tables: &DetailTables,
        tab: DetailTab,
        row: usize,
        column: usize,
    ) -> String {
        settle(harness, tables);
        let node = tables
            .cell_node(tab, row, column, harness.app())
            .expect("the cell is mounted");
        harness
            .text_of(node)
            .lines()
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn header_text(
        harness: &mut WidgetTestApp,
        tables: &DetailTables,
        tab: DetailTab,
        column: usize,
    ) -> String {
        settle(harness, tables);
        let node = table(tables, tab)
            .header_node(harness.app(), column)
            .expect("the header cell is mounted");
        harness.text_of(node)
    }

    fn one_position() -> Arc<DetailSnapshot> {
        Arc::new(perpetual_reading(vec![position(
            &perp_listing(),
            "2",
            Some("100"),
        )]))
    }

    #[test]
    fn an_accepted_reading_should_reach_its_table() {
        let mut harness = WidgetTestApp::with_translate(translate(LanguageCode::En));
        let tables = tables(&mut harness);
        assert_eq!(tables.row_count(DetailTab::Perpetual), 0);

        harness.with_cx(|cx| {
            tables
                .accept(DetailTab::Perpetual, one_position(), cx)
                .expect("the reading is accepted")
        });

        assert_eq!(
            tables.row_count(DetailTab::Perpetual),
            1,
            "the reading's rows reach the table it belongs to"
        );
        assert_eq!(
            tables.row_count(DetailTab::Trades),
            0,
            "a reading of one tab leaves the others empty"
        );
    }

    #[test]
    fn showing_a_tab_should_hide_the_other_five() {
        let mut harness = WidgetTestApp::with_translate(translate(LanguageCode::En));
        let tables = tables(&mut harness);
        let spot = tables.wrappers.borrow()[DetailTab::Spot.index()];
        let trades = tables.wrappers.borrow()[DetailTab::Trades.index()];
        settle(&mut harness, &tables);
        assert!(harness.visible(spot), "the first tab is the one drawn");
        assert!(!harness.visible(trades));

        harness.with_cx(|cx| tables.show(DetailTab::Trades, cx).expect("the tab shows"));
        settle(&mut harness, &tables);

        assert!(harness.visible(trades), "the tab that was chosen is drawn");
        assert!(!harness.visible(spot), "the tab that was left is hidden");
        assert_eq!(
            tables.tables.borrow()[DetailTab::Trades.index()].rows(),
            0,
            "the table was there all along rather than built on the switch"
        );
    }

    #[test]
    fn the_headers_and_the_cells_should_follow_the_language() {
        let mut harness = WidgetTestApp::with_translate(translate(LanguageCode::En));
        let tables = tables(&mut harness);
        harness.with_cx(|cx| {
            tables
                .show(DetailTab::Perpetual, cx)
                .expect("the tab shows");
            tables
                .accept(DetailTab::Perpetual, one_position(), cx)
                .expect("the reading is accepted")
        });
        assert_eq!(
            header_text(&mut harness, &tables, DetailTab::Perpetual, 1),
            "Side"
        );
        assert_eq!(
            cell_text(&mut harness, &tables, DetailTab::Perpetual, 0, 1),
            "Long"
        );

        harness.switch_language(LanguageCode::ZhCn);
        settle(&mut harness, &tables);

        assert_eq!(
            header_text(&mut harness, &tables, DetailTab::Perpetual, 1),
            "方向",
            "the rebuilt header is read in the language showing now"
        );
        assert_eq!(
            cell_text(&mut harness, &tables, DetailTab::Perpetual, 0, 1),
            "多",
            "the cell reads the language signal rather than a built-in word"
        );
        assert_eq!(
            tables.row_count(DetailTab::Perpetual),
            1,
            "the rebuild keeps the accepted reading"
        );
    }

    #[test]
    fn moving_the_display_time_zone_should_redraw_the_time_cells() {
        let mut harness = WidgetTestApp::with_translate(translate(LanguageCode::En));
        let tables = tables(&mut harness);
        let wallet = address("1");
        let reading = Arc::new(
            DetailSnapshot::new(
                DetailData::Trades(vec![fill(
                    &wallet,
                    &perp_listing(),
                    Side::Buy,
                    "1",
                    "100",
                    1_000,
                    1,
                )]),
                perpetual_precision(),
            )
            .expect("a published listing should validate"),
        );
        harness.with_cx(|cx| {
            tables.show(DetailTab::Trades, cx).expect("the tab shows");
            tables
                .accept(DetailTab::Trades, Arc::clone(&reading), cx)
                .expect("the reading is accepted")
        });
        settle(&mut harness, &tables);
        let age = cell_text(&mut harness, &tables, DetailTab::Trades, 0, 7);
        let requests = harness.frame_requests();

        let config_signal = harness.services().config;
        let mut config = (*harness.app().read(&config_signal)).clone();
        config.data_time_zone =
            app_config::time_zone::DataTimeZone::from_hours(8).expect("UTC+8 is offered");
        assert!(
            harness.app_mut().write(&config_signal, config),
            "the setting move is a change"
        );
        assert!(
            harness.frame_requests() > requests,
            "the time cells read the settings signal, so moving the zone asks for a frame"
        );

        let utc = moment_label(DetailTab::Trades, Some(&reading), 0, time::UtcOffset::UTC);
        let east = moment_label(
            DetailTab::Trades,
            Some(&reading),
            0,
            time::UtcOffset::from_hms(8, 0, 0).expect("UTC+8 is an offset"),
        );
        assert_eq!(utc.as_deref(), Some("1970-01-01 00:00:01"));
        assert_eq!(
            east.as_deref(),
            Some("1970-01-01 08:00:01"),
            "the moment a time cell names is read in the zone the settings name"
        );
        assert_eq!(
            cell_text(&mut harness, &tables, DetailTab::Trades, 0, 7),
            age,
            "the age itself is a relative time, which no zone moves"
        );
    }

    #[test]
    fn an_empty_reading_should_draw_the_tables_empty_state() {
        let mut harness = WidgetTestApp::with_translate(translate(LanguageCode::En));
        let tables = tables(&mut harness);
        let empty = Arc::new(perpetual_reading(Vec::new()));
        harness.with_cx(|cx| {
            tables
                .show(DetailTab::Perpetual, cx)
                .expect("the tab shows");
            tables
                .accept(DetailTab::Perpetual, empty, cx)
                .expect("the empty reading is accepted")
        });
        settle(&mut harness, &tables);

        let perpetual_table = table(&tables, DetailTab::Perpetual);
        assert_eq!(
            perpetual_table.rows(),
            0,
            "nothing to draw is no row at all"
        );
        let empty_node = perpetual_table
            .empty_node()
            .expect("the table draws its empty state");
        assert_eq!(
            harness.text_of(empty_node),
            "No data",
            "the empty body says so in the language showing now"
        );
        assert_eq!(
            header_text(&mut harness, &tables, DetailTab::Perpetual, 0),
            "Coin",
            "an empty table keeps its headers"
        );

        harness.switch_language(LanguageCode::ZhCn);
        settle(&mut harness, &tables);
        let empty_node = table(&tables, DetailTab::Perpetual)
            .empty_node()
            .expect("the rebuilt table draws its empty state");
        assert_eq!(
            harness.text_of(empty_node),
            "暂无数据",
            "the rebuilt empty sentence is read in the language showing now"
        );
    }

    #[test]
    fn a_tab_with_no_reading_should_be_waited_for_under_a_spinner() {
        let mut harness = WidgetTestApp::with_translate(translate(LanguageCode::En));
        let tables = tables(&mut harness);
        harness.with_cx(|cx| {
            tables.show(DetailTab::Spot, cx).expect("the tab shows");
            tables
                .set_loading(DetailTab::Spot, true, cx)
                .expect("the wait is announced");
        });
        settle(&mut harness, &tables);
        let overlay = loading_overlay(&harness, &tables);

        assert!(harness.visible(overlay), "the wait is drawn");
        assert!(
            harness.has_canvas(tables.spinner_slot),
            "the wait is a kit spinner rather than a sentence alone"
        );
        assert!(
            harness.has_text(overlay, "Loading"),
            "the wait says what it is waiting for"
        );
        assert!(tables.spinner.borrow().is_some(), "the spinner is mounted");

        harness.with_cx(|cx| {
            tables
                .accept(
                    DetailTab::Spot,
                    Arc::new(spot_reading(vec![wallet_balance("USDC", "1")])),
                    cx,
                )
                .expect("the reading is accepted")
        });
        settle(&mut harness, &tables);

        assert!(!harness.visible(overlay), "the wait is over");
        assert!(
            tables.spinner.borrow().is_none(),
            "the spinner is released with the wait, so it stops asking for frames"
        );
    }

    #[test]
    fn a_tab_that_is_read_again_should_be_waited_for_over_its_reading() {
        let mut harness = WidgetTestApp::with_translate(translate(LanguageCode::En));
        let tables = tables(&mut harness);
        harness.with_cx(|cx| {
            tables.show(DetailTab::Spot, cx).expect("the tab shows");
            tables
                .accept(
                    DetailTab::Spot,
                    Arc::new(spot_reading(vec![wallet_balance("USDC", "1")])),
                    cx,
                )
                .expect("the reading is accepted");
        });
        settle(&mut harness, &tables);
        let overlay = loading_overlay(&harness, &tables);
        assert!(
            !harness.visible(overlay),
            "a reading in hand is not waited for"
        );

        harness.with_cx(|cx| {
            tables
                .set_loading(DetailTab::Spot, true, cx)
                .expect("the reading is read again");
        });
        settle(&mut harness, &tables);
        assert!(harness.visible(overlay), "the newer reading is waited for");
        assert_eq!(
            tables.row_count(DetailTab::Spot),
            1,
            "the reading on hand stays under the wait"
        );

        harness.with_cx(|cx| {
            tables
                .set_loading(DetailTab::Spot, false, cx)
                .expect("the wait is over");
        });
        settle(&mut harness, &tables);
        assert!(!harness.visible(overlay), "the wait ends with the answer");
    }

    #[test]
    fn clearing_the_tables_should_drop_every_reading() {
        let mut harness = WidgetTestApp::with_translate(translate(LanguageCode::En));
        let tables = tables(&mut harness);
        harness.with_cx(|cx| {
            tables
                .accept(
                    DetailTab::Spot,
                    Arc::new(spot_reading(vec![wallet_balance("USDC", "1")])),
                    cx,
                )
                .expect("the spot reading is accepted");
            tables
                .accept(DetailTab::Perpetual, one_position(), cx)
                .expect("the perpetual reading is accepted");
        });
        settle(&mut harness, &tables);
        assert_eq!(tables.row_count(DetailTab::Spot), 1);
        assert_eq!(tables.row_count(DetailTab::Perpetual), 1);

        harness.with_cx(|cx| tables.clear(cx).expect("the tables clear"));
        settle(&mut harness, &tables);

        assert_eq!(
            tables.row_count(DetailTab::Spot),
            0,
            "a cleared table holds no rows"
        );
        assert_eq!(tables.row_count(DetailTab::Perpetual), 0);
        assert!(
            tables
                .cell_node(DetailTab::Spot, 0, 0, harness.app())
                .is_none(),
            "and no cell of the reading that was dropped"
        );
    }

    #[test]
    fn a_steady_sync_should_ask_for_no_frame() {
        let mut harness = WidgetTestApp::with_translate(translate(LanguageCode::En));
        let tables = tables(&mut harness);
        harness.with_cx(|cx| {
            tables
                .show(DetailTab::Perpetual, cx)
                .expect("the tab shows");
            tables
                .accept(DetailTab::Perpetual, one_position(), cx)
                .expect("the reading is accepted")
        });
        settle(&mut harness, &tables);
        let requests = harness.frame_requests();

        harness.with_cx(|cx| tables.sync(cx).expect("the tables sync"));
        harness.with_cx(|cx| tables.sync(cx).expect("the tables sync"));
        assert_eq!(
            harness.frame_requests(),
            requests,
            "a steady sync writes no signal and asks for no frame"
        );
    }

    #[test]
    fn the_ages_should_advance_once_per_second_of_engine_time() {
        let mut harness = WidgetTestApp::with_translate(translate(LanguageCode::En));
        let tables = tables(&mut harness);
        let wallet = address("1");
        let reading = Arc::new(
            DetailSnapshot::new(
                DetailData::Trades(vec![fill(
                    &wallet,
                    &perp_listing(),
                    Side::Buy,
                    "1",
                    "100",
                    1,
                    1,
                )]),
                perpetual_precision(),
            )
            .expect("a published listing should validate"),
        );
        harness.with_cx(|cx| {
            tables.show(DetailTab::Trades, cx).expect("the tab shows");
            tables
                .accept(DetailTab::Trades, reading, cx)
                .expect("the reading is accepted")
        });
        settle(&mut harness, &tables);
        assert!(
            tables.timer.borrow().is_some(),
            "the tick is armed while a reading is on screen"
        );
        let requests = harness.frame_requests();

        harness.advance(Duration::from_millis(500));
        assert_eq!(
            harness.frame_requests(),
            requests,
            "halfway through the second the tick has not fired and nothing asks for a frame"
        );

        harness.advance(AGE_TICK);
        settle(&mut harness, &tables);
        assert!(
            harness.frame_requests() > requests,
            "the second that passed asked for the frame that rewrites the ages"
        );
        assert!(
            tables.timer.borrow().is_some(),
            "the timer re-arms itself for the next second"
        );
    }

    #[test]
    fn the_ages_clock_should_stop_when_its_page_is_hidden() {
        let mut harness = WidgetTestApp::with_translate(translate(LanguageCode::En));
        let page = harness.with_cx(|cx| cx.signal(true));
        let root = harness.tree().root();
        let tables =
            harness.with_cx(|cx| DetailTables::build(cx, root, page).expect("the tables build"));
        let wallet = address("1");
        let reading = Arc::new(
            DetailSnapshot::new(
                DetailData::Trades(vec![fill(
                    &wallet,
                    &perp_listing(),
                    Side::Buy,
                    "1",
                    "100",
                    1,
                    1,
                )]),
                perpetual_precision(),
            )
            .expect("a published listing should validate"),
        );
        harness.with_cx(|cx| {
            tables.show(DetailTab::Trades, cx).expect("the tab shows");
            tables
                .accept(DetailTab::Trades, reading, cx)
                .expect("the reading is accepted")
        });
        settle(&mut harness, &tables);
        assert!(
            tables.age_clock_armed(),
            "the ages of the detail page move while it is on screen"
        );

        harness.app_mut().write(&page, false);
        settle(&mut harness, &tables);
        assert!(
            !tables.age_clock_armed(),
            "a page nobody is looking at asks for no frames to move its ages"
        );

        harness.app_mut().write(&page, true);
        settle(&mut harness, &tables);
        assert!(tables.age_clock_armed(), "showing the page starts it again");

        harness.with_cx(|cx| tables.show(DetailTab::Spot, cx).expect("the tab shows"));
        settle(&mut harness, &tables);
        assert!(
            !tables.age_clock_armed(),
            "a tab with no time column has no age to move"
        );
    }

    #[test]
    fn releasing_the_tables_should_reclaim_every_value_slot() {
        let mut harness = WidgetTestApp::with_translate(translate(LanguageCode::En));
        let baseline = harness.signal_slot_stats();
        let mut mounted_slots = None;

        for _ in 0..3 {
            let scope = harness.app_mut().signal_scope();
            let window = harness.window();
            let root = harness.tree().root();
            let tables = with_scope(&mut harness, scope, |cx| {
                let page = cx.signal(true);
                DetailTables::build(cx, root, page).expect("the tables build")
            });
            with_scope(&mut harness, scope, |cx| {
                tables.show(DetailTab::Spot, cx).expect("the tab shows");
                tables
                    .accept(
                        DetailTab::Spot,
                        Arc::new(spot_reading(vec![wallet_balance("USDC", "1")])),
                        cx,
                    )
                    .expect("the reading is accepted");
                tables.show(DetailTab::Trades, cx).expect("the tab shows");
                tables
                    .set_loading(DetailTab::Trades, true, cx)
                    .expect("the other tab is being read");
            });
            settle(&mut harness, &tables);
            assert!(
                harness.signal_slot_stats().live > baseline.live,
                "the tables mint their own values"
            );
            assert!(
                tables.spinner.borrow().is_some(),
                "the widest state the tables build includes a mounted spinner"
            );
            let area = tables.area;

            with_scope(&mut harness, scope, |cx| tables.release(cx));
            drop(tables);
            let _ = harness.app_mut().remove_node(window, area);
            harness.app_mut().release_scope(scope);

            assert_eq!(
                harness.signal_slot_stats().live,
                baseline.live,
                "releasing the tables returns every live value slot"
            );
            let used = harness.signal_slot_stats().slots;
            assert_eq!(
                *mounted_slots.get_or_insert(used),
                used,
                "a mount reuses the slots it was given rather than minting afresh"
            );
        }
    }

    fn with_scope<R>(
        harness: &mut WidgetTestApp,
        scope: SignalScope,
        body: impl FnOnce(&mut WidgetCx<'_>) -> R,
    ) -> R {
        let services = Rc::clone(harness.services_rc());
        let window = harness.window();
        let mut cx = WidgetCx::new(harness.app_mut(), window, &services, scope);
        body(&mut cx)
    }
}

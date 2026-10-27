//! The six views of one account, each as a table.
//!
//! # Why the rows are formatted once
//!
//! A cell's text is a decimal rounded to a tick and grouped, which is the expensive part of
//! drawing a wallet. It is therefore produced when the reading is accepted rather than while
//! the table is drawn, so a reading that cannot be displayed is reported once, by the scope
//! that accepted it, instead of per cell per frame.
//!
//! # Why the ages are the exception
//!
//! An age moves while nothing else does, so a time cell keeps the moment it is about and has
//! only its text rewritten when the second it was formatted at changes. That is the one thing
//! a frame can change about a reading the pane already holds, which is why a table is an
//! entity: the pane hands it the current moment while it draws it.
//!
//! # One type per tab
//!
//! The shared table is generic over its column count, and a tab is a column count plus the
//! meaning of its cells. Two tabs with the same count would be the same type if that meaning
//! were a field, so every tab has its own type instead: what a cell means is then the code
//! that produced it rather than a dispatch at draw time.

use std::sync::Arc;

use base_ui::{
    empty_state::EmptyState,
    style::{
        roles::MarketRoles,
        theme::{Theme, ThemeExt},
    },
    table::{Table, TableDataSet},
};
use gpui_kit::{
    AnyElement, App, AppContext, ElementId, Empty, Entity, InteractiveElement, IntoElement, Length,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, base::Align, div,
    prelude::FluentBuilder, relative,
};
use i18n::t;
use market_participants_connector::account::shared::{OrderKind, PositionDirection};
use market_participants_model::{
    asset::AssetId,
    decimal::{Price, ProfitOrLoss, SignedQuantity},
    listing::route::VenueListingKey,
    market::{
        account::AssetAmount,
        price::MarketPrice,
        units::{MarketQuantity, Side},
    },
};
use widget::time_zone::data_time_zone;

use super::{
    model::{
        DetailData, DetailSnapshot, DetailTab, DisplayPrecision, SpotBalanceRow, WalletDataError,
        absolute_time,
    },
    timeline::age_text,
};

/// The colour a cell is read in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CellTone {
    Default,
    Muted,
    Positive,
    Negative,
}

/// One formatted cell.
#[derive(Clone, Debug)]
struct DisplayCell {
    text: SharedString,
    /// A second word after the text, drawn in the table's own colour: the token a fee is paid
    /// in.
    suffix: Option<SharedString>,
    tone: CellTone,
    /// The moment this cell is about, when it is about one.
    ///
    /// A cell that carries it has its text rewritten when the second changes, which is what
    /// keeps an age an age.
    timestamp: Option<i64>,
}

impl DisplayCell {
    fn text(value: impl Into<SharedString>) -> Self {
        Self {
            text: value.into(),
            suffix: None,
            tone: CellTone::Default,
            timestamp: None,
        }
    }

    /// A cell with nothing to show, which is how a value the venue did not report is written.
    fn missing() -> Self {
        Self::text(SharedString::new_static("-")).tone(CellTone::Muted)
    }

    fn tone(mut self, tone: CellTone) -> Self {
        self.tone = tone;
        self
    }

    fn suffix(mut self, value: impl Into<SharedString>) -> Self {
        self.suffix = Some(value.into());
        self
    }

    /// A cell that shows how long ago something happened, and remembers when it was.
    fn age(now: i64, timestamp: i64, cx: &App) -> Self {
        Self {
            text: age_text(cx, now, timestamp),
            suffix: None,
            tone: CellTone::Muted,
            timestamp: Some(timestamp),
        }
    }
}

/// One formatted row.
#[derive(Clone, Debug)]
struct DisplayRow<const N: usize> {
    cells: [DisplayCell; N],
}

/// The rows of one table, with what a frame needs to draw them.
struct DetailRows<const N: usize> {
    rows: Arc<[DisplayRow<N>]>,
    /// The market colours the tone of a row's own reading is drawn in.
    ///
    /// The shared table draws a cell without an application context, so what a gain and a loss
    /// are read in has to be resolved before the cell is asked for. A cell that is neither
    /// carries no colour of its own: the table already draws every cell in the theme's content
    /// colour. A number's direction is a market reading, not a chart series (§8.4).
    market: MarketRoles,
    /// The second the ages were last written at.
    formatted_second: i64,
}

impl<const N: usize> DetailRows<N> {
    fn new(market: MarketRoles) -> Self {
        Self {
            rows: Arc::from([]),
            market,
            formatted_second: i64::MIN,
        }
    }

    fn len(&self) -> usize {
        self.rows.len()
    }

    fn set(&mut self, rows: Vec<DisplayRow<N>>, now: i64) {
        self.rows = rows.into();
        self.formatted_second = now / 1_000;
    }

    /// Rewrites the ages of a reading that is still on screen.
    ///
    /// It does nothing until a whole second has passed, so a frame that changed something else
    /// does not re-format a hundred rows for nothing.
    fn refresh(&mut self, now: i64, cx: &App) {
        let second = now / 1_000;
        if second == self.formatted_second {
            return;
        }
        self.formatted_second = second;
        let rows = std::mem::take(&mut self.rows);
        self.rows = rows
            .iter()
            .map(|row| DisplayRow {
                cells: row.cells.clone().map(|cell| match cell.timestamp {
                    Some(timestamp) => DisplayCell {
                        text: age_text(cx, now, timestamp),
                        ..cell
                    },
                    None => cell,
                }),
            })
            .collect::<Vec<_>>()
            .into();
    }

    fn cell(&self, row_index: usize, column_index: usize, cx: &App) -> AnyElement {
        let Some(cell) = self
            .rows
            .get(row_index)
            .and_then(|row| row.cells.get(column_index))
        else {
            return Empty.into_any_element();
        };
        let color = match cell.tone {
            CellTone::Default | CellTone::Muted => None,
            CellTone::Positive => Some(self.market.up.hsla),
            CellTone::Negative => Some(self.market.down.hsla),
        };
        let content = match cell.suffix.as_ref() {
            Some(suffix) => div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .when_some(color, |this, color| this.text_color(color))
                        .child(cell.text.clone()),
                )
                .child(div().child(suffix.clone()))
                .into_any_element(),
            None => div()
                .when_some(color, |this, color| this.text_color(color))
                .child(cell.text.clone())
                .into_any_element(),
        };
        match cell.timestamp {
            Some(timestamp) => div()
                .id(ElementId::Name(SharedString::new_static(
                    "HyperliquidWalletTimeCell",
                )))
                .aria_label(absolute_time(timestamp, data_time_zone(cx)))
                .child(content)
                .into_any_element(),
            None => content,
        }
    }

    /// A wallet that holds none of what the selected tab lists draws an empty table,
    /// which is a fact rather than a state: the body says so instead of leaving the
    /// room under its headers blank. Every tab answers with this, because a tab is
    /// drawn on its own and the sentence for "none of these" is the same one.
    fn empty(cx: &App) -> Option<AnyElement> {
        Some(EmptyState::new("DetailTableEmpty", t!(cx, "common.no_data")).into_any_element())
    }
}

/// Whether a value is a gain, a loss, or nothing at all.
fn pnl_tone(value: ProfitOrLoss) -> CellTone {
    match value.inner().cmp(&0) {
        std::cmp::Ordering::Less => CellTone::Negative,
        std::cmp::Ordering::Equal => CellTone::Muted,
        std::cmp::Ordering::Greater => CellTone::Positive,
    }
}

/// The tone of one signed amount the venue published.
fn amount_tone(value: SignedQuantity) -> CellTone {
    pnl_tone(ProfitOrLoss::from_inner(value.inner()))
}

/// Which way a position points, as the venue's own signed size said.
fn position_cell(direction: PositionDirection, cx: &App) -> DisplayCell {
    match direction {
        PositionDirection::Long => {
            DisplayCell::text(t!(cx, "widget.hyperliquid_wallet_monitor.long"))
                .tone(CellTone::Positive)
        }
        PositionDirection::Short => {
            DisplayCell::text(t!(cx, "widget.hyperliquid_wallet_monitor.short"))
                .tone(CellTone::Negative)
        }
    }
}

fn side_cell(side: Side, cx: &App) -> DisplayCell {
    match side {
        Side::Buy => DisplayCell::text(t!(cx, "widget.hyperliquid_wallet_monitor.buy"))
            .tone(CellTone::Positive),
        Side::Sell => DisplayCell::text(t!(cx, "widget.hyperliquid_wallet_monitor.sell"))
            .tone(CellTone::Negative),
    }
}

fn boolean_cell(value: bool, cx: &App) -> DisplayCell {
    DisplayCell::text(if value {
        t!(cx, "common.yes")
    } else {
        t!(cx, "common.no")
    })
}

/// The trigger price of an order, when it has one.
fn trigger_price(kind: &OrderKind) -> Option<Price> {
    match kind {
        OrderKind::StopMarket { trigger_price, .. }
        | OrderKind::StopLimit { trigger_price, .. }
        | OrderKind::TakeProfitMarket { trigger_price, .. }
        | OrderKind::TakeProfitLimit { trigger_price, .. } => Some(*trigger_price),
        OrderKind::Market | OrderKind::Limit { .. } => None,
    }
}

/// The name of one listing, as a reader knows it.
fn listing_cell(precision: &DisplayPrecision, listing: &VenueListingKey, cx: &App) -> DisplayCell {
    DisplayCell::text(SharedString::new(
        precision.listing_label(listing, cx).as_str(),
    ))
}

/// The asset one order is collateralised in.
fn collateral(precision: &DisplayPrecision, listing: &VenueListingKey, cx: &App) -> DisplayCell {
    DisplayCell::text(SharedString::new(
        precision.listing_collateral_label(listing, cx).as_str(),
    ))
}

/// A monetary amount of one asset, drawn to the money tick.
fn money_amount(precision: &DisplayPrecision, value: Option<&AssetAmount>) -> DisplayCell {
    value.map_or_else(DisplayCell::missing, |value| {
        DisplayCell::text(SharedString::new(
            precision.format_money_signed_quantity(value.value).as_str(),
        ))
    })
}

/// A quantity the venue published with its unit.
///
/// The value is drawn to the tick of the listing it belongs to and is followed by the asset the
/// unit is denominated in, so a balance in one asset and a balance in another are not read as
/// two numbers of the same thing.
fn market_quantity(
    precision: &DisplayPrecision,
    listing: &VenueListingKey,
    quantity: &MarketQuantity,
    cx: &App,
) -> Result<DisplayCell, WalletDataError> {
    let value = DisplayCell::text(SharedString::new(
        precision
            .format_listing_quantity(listing, quantity.value)?
            .as_str(),
    ));
    Ok(match precision.quantity_unit_label(&quantity.unit, cx) {
        Some(unit) if !quantity.unit.is_notional() => DisplayCell {
            suffix: Some(SharedString::new(unit.as_str())),
            ..value
        },
        Some(_) | None => value,
    })
}

/// A price the venue published with its quote asset and basis.
fn market_price_cell(
    precision: &DisplayPrecision,
    listing: &VenueListingKey,
    price: &MarketPrice,
    cx: &App,
) -> Result<DisplayCell, WalletDataError> {
    Ok(DisplayCell {
        suffix: Some(SharedString::new(
            precision.asset_label(&price.quote_asset, cx).as_str(),
        )),
        ..DisplayCell::text(SharedString::new(
            precision
                .format_listing_price(listing, price.value)?
                .as_str(),
        ))
    })
}

fn optional_market_price(
    precision: &DisplayPrecision,
    listing: &VenueListingKey,
    price: Option<&MarketPrice>,
    cx: &App,
) -> Result<DisplayCell, WalletDataError> {
    price.map_or_else(
        || Ok(DisplayCell::missing()),
        |price| market_price_cell(precision, listing, price, cx),
    )
}

/// A balance's price, drawn in the market it was read on.
///
/// A price is a market's rather than an asset's: the tick and the quote asset that draw it belong
/// to the pair it was read on, and a base quoted against two assets has one of each per pair. It
/// is drawn from that pair whenever there is one. A stablecoin is the one balance whose price no
/// market quotes — money is one unit of itself — so it is drawn in its own tick; a balance that
/// was priced without a pair of its own despite not being money is left out rather than drawn as
/// something it is not.
fn balance_price_cell(
    precision: &DisplayPrecision,
    row: &SpotBalanceRow,
    price: &MarketPrice,
    cx: &App,
) -> Result<DisplayCell, WalletDataError> {
    match row.price_listing.as_ref() {
        Some(listing) => market_price_cell(precision, listing, price, cx),
        None => Ok(DisplayCell {
            suffix: Some(SharedString::new(
                precision.asset_label(&row.asset, cx).as_str(),
            )),
            ..DisplayCell::text(SharedString::new(
                precision
                    .format_asset_price(&row.asset, price.value)?
                    .as_str(),
            ))
        }),
    }
}

/// One asset's own quantity, drawn in the asset's own tick.
fn asset_quantity_cell(
    precision: &DisplayPrecision,
    asset: &AssetId,
    value: SignedQuantity,
    cx: &App,
) -> Result<DisplayCell, WalletDataError> {
    Ok(DisplayCell {
        suffix: Some(SharedString::new(precision.asset_label(asset, cx).as_str())),
        ..DisplayCell::text(SharedString::new(
            precision.format_asset_quantity(asset, value)?.as_str(),
        ))
    })
}

/// The widths of one table's columns, as fractions of the width it is given.
fn widths<const N: usize>(fractions: [f32; N]) -> [Length; N] {
    fractions.map(|fraction| Length::Definite(relative(fraction)))
}

/// One spot balance, valued against the wallet's other balances.
///
/// A balance is a quantity of one asset rather than a position on one listing, so its asset is
/// a column of its own: the same balance can be quoted by several pairs of the route.
fn spot_rows(
    snapshot: &DetailSnapshot,
    _now: i64,
    cx: &App,
) -> Result<Vec<DisplayRow<5>>, WalletDataError> {
    let DetailData::Spot(data) = &snapshot.data else {
        return Ok(Vec::new());
    };
    let precision = &snapshot.precision;
    data.rows
        .iter()
        .map(|row| {
            Ok(DisplayRow {
                cells: [
                    DisplayCell::text(SharedString::new(
                        precision.asset_label(&row.asset, cx).as_str(),
                    )),
                    row.asset_share
                        .clone()
                        .map_or_else(DisplayCell::missing, |share| {
                            DisplayCell::text(SharedString::new(share.as_str()))
                        }),
                    asset_quantity_cell(precision, &row.asset, row.total, cx)?.tone(
                        if row.total.inner() < 0 {
                            CellTone::Negative
                        } else {
                            CellTone::Default
                        },
                    ),
                    row.value.map_or_else(DisplayCell::missing, |value| {
                        DisplayCell::text(SharedString::new(
                            precision.format_money_signed_quantity(value).as_str(),
                        ))
                        .tone(if value.inner() < 0 {
                            CellTone::Negative
                        } else {
                            CellTone::Default
                        })
                    }),
                    row.price.as_ref().map_or_else(
                        || Ok(DisplayCell::missing()),
                        |price| balance_price_cell(precision, row, price, cx),
                    )?,
                ],
            })
        })
        .collect()
}

fn perpetual_rows(
    snapshot: &DetailSnapshot,
    _now: i64,
    cx: &App,
) -> Result<Vec<DisplayRow<9>>, WalletDataError> {
    let DetailData::Perpetual(state) = &snapshot.data else {
        return Ok(Vec::new());
    };
    let precision = &snapshot.precision;
    state
        .positions
        .iter()
        .map(|position| {
            let priced = position.entry_price.is_some() && position.mark_price.is_some();
            let pnl = position.unrealized_pnl.value;
            let funding = position.funding_since_open.value;
            Ok(DisplayRow {
                cells: [
                    listing_cell(precision, &position.listing, cx),
                    position_cell(position.direction, cx),
                    money_amount(precision, position.position_value.as_ref()),
                    if priced {
                        DisplayCell::text(SharedString::new(
                            precision.format_money_profit(pnl).as_str(),
                        ))
                        .tone(amount_tone(pnl))
                    } else {
                        DisplayCell::missing()
                    },
                    optional_market_price(
                        precision,
                        &position.listing,
                        position.entry_price.as_ref(),
                        cx,
                    )?,
                    optional_market_price(
                        precision,
                        &position.listing,
                        position.mark_price.as_ref(),
                        cx,
                    )?,
                    optional_market_price(
                        precision,
                        &position.listing,
                        position.liquidation_price.as_ref(),
                        cx,
                    )?,
                    money_amount(precision, position.margin.as_ref()),
                    DisplayCell::text(SharedString::new(
                        precision.format_money_profit(funding).as_str(),
                    ))
                    .tone(amount_tone(funding)),
                ],
            })
        })
        .collect()
}

fn open_order_rows(
    snapshot: &DetailSnapshot,
    now: i64,
    cx: &App,
) -> Result<Vec<DisplayRow<9>>, WalletDataError> {
    let DetailData::OpenOrders(orders) = &snapshot.data else {
        return Ok(Vec::new());
    };
    let precision = &snapshot.precision;
    orders
        .iter()
        .map(|order| {
            let notional = order
                .quantity
                .value
                .checked_mul_price(order.price.value)
                .map(|value| AssetAmount {
                    asset: order.price.quote_asset.clone(),
                    value: SignedQuantity::from_inner(value.inner() as i128),
                });
            Ok(DisplayRow {
                cells: [
                    listing_cell(precision, &order.listing, cx),
                    collateral(precision, &order.listing, cx),
                    side_cell(order.side, cx),
                    market_quantity(precision, &order.listing, &order.quantity, cx)?,
                    money_amount(precision, notional.as_ref()),
                    market_price_cell(precision, &order.listing, &order.price, cx)?,
                    trigger_price(&order.kind).map_or_else(
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
                                cx,
                            )
                        },
                    )?,
                    boolean_cell(order.reduce_only, cx),
                    DisplayCell::age(now, order.timestamp, cx),
                ],
            })
        })
        .collect()
}

fn trade_rows(
    snapshot: &DetailSnapshot,
    now: i64,
    cx: &App,
) -> Result<Vec<DisplayRow<8>>, WalletDataError> {
    let DetailData::Trades(fills) = &snapshot.data else {
        return Ok(Vec::new());
    };
    let precision = &snapshot.precision;
    fills
        .iter()
        .map(|fill| {
            let closed_pnl = fill.closed_pnl.value;
            let fee = fill.fee.value;
            Ok(DisplayRow {
                cells: [
                    listing_cell(precision, &fill.listing, cx),
                    side_cell(fill.side, cx),
                    market_quantity(precision, &fill.listing, &fill.quantity, cx)?,
                    DisplayCell::text(SharedString::new(
                        precision
                            .format_listing_signed_quantity(&fill.listing, fill.start_position)?
                            .as_str(),
                    ))
                    .tone(if fill.start_position.inner() < 0 {
                        CellTone::Negative
                    } else {
                        CellTone::Positive
                    }),
                    market_price_cell(precision, &fill.listing, &fill.price, cx)?,
                    DisplayCell::text(SharedString::new(
                        precision.format_money_profit(closed_pnl).as_str(),
                    ))
                    .tone(amount_tone(closed_pnl)),
                    DisplayCell::text(SharedString::new(
                        precision.format_money_profit(fee).as_str(),
                    ))
                    .suffix(SharedString::new(
                        precision.asset_label(&fill.fee.asset, cx).as_str(),
                    ))
                    .tone(amount_tone(fee)),
                    DisplayCell::age(now, fill.timestamp, cx),
                ],
            })
        })
        .collect()
}

fn historical_order_rows(
    snapshot: &DetailSnapshot,
    now: i64,
    cx: &App,
) -> Result<Vec<DisplayRow<9>>, WalletDataError> {
    let DetailData::HistoricalOrders(orders) = &snapshot.data else {
        return Ok(Vec::new());
    };
    let precision = &snapshot.precision;
    orders
        .iter()
        .map(|order| {
            let notional = order
                .quantity
                .value
                .checked_mul_price(order.price.value)
                .map(|value| AssetAmount {
                    asset: order.price.quote_asset.clone(),
                    value: SignedQuantity::from_inner(value.inner() as i128),
                });
            Ok(DisplayRow {
                cells: [
                    listing_cell(precision, &order.listing, cx),
                    side_cell(order.side, cx),
                    market_quantity(precision, &order.listing, &order.quantity, cx)?,
                    money_amount(precision, notional.as_ref()),
                    market_price_cell(precision, &order.listing, &order.price, cx)?,
                    trigger_price(&order.kind).map_or_else(
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
                                cx,
                            )
                        },
                    )?,
                    boolean_cell(order.reduce_only, cx),
                    DisplayCell::text(SharedString::new(order.status.as_str())),
                    DisplayCell::age(now, order.timestamp, cx),
                ],
            })
        })
        .collect()
}

fn ledger_rows(
    snapshot: &DetailSnapshot,
    now: i64,
    cx: &App,
) -> Result<Vec<DisplayRow<5>>, WalletDataError> {
    let DetailData::Ledger(events) = &snapshot.data else {
        return Ok(Vec::new());
    };
    let precision = &snapshot.precision;
    Ok(events
        .iter()
        .map(|event| DisplayRow {
            cells: [
                event
                    .asset
                    .as_ref()
                    .map_or_else(DisplayCell::missing, |asset| {
                        DisplayCell::text(SharedString::new(
                            precision.asset_label(asset, cx).as_str(),
                        ))
                    }),
                DisplayCell::text(SharedString::new(event.kind.as_str())),
                money_amount(precision, event.amount.as_ref()),
                money_amount(precision, event.value.as_ref()),
                DisplayCell::age(now, event.timestamp, cx),
            ],
        })
        .collect())
}

/// One tab's table: the rows it was given, and the columns it draws them in.
pub(super) struct SpotTable(DetailRows<5>);
pub(super) struct PerpetualTable(DetailRows<9>);
pub(super) struct OpenOrdersTable(DetailRows<9>);
pub(super) struct TradesTable(DetailRows<8>);
pub(super) struct HistoricalOrdersTable(DetailRows<9>);
pub(super) struct LedgerTable(DetailRows<5>);

impl SpotTable {
    fn new(theme: &Theme) -> Self {
        Self(DetailRows::new(theme.roles.market))
    }
}

impl PerpetualTable {
    fn new(theme: &Theme) -> Self {
        Self(DetailRows::new(theme.roles.market))
    }
}

impl OpenOrdersTable {
    fn new(theme: &Theme) -> Self {
        Self(DetailRows::new(theme.roles.market))
    }
}

impl TradesTable {
    fn new(theme: &Theme) -> Self {
        Self(DetailRows::new(theme.roles.market))
    }
}

impl HistoricalOrdersTable {
    fn new(theme: &Theme) -> Self {
        Self(DetailRows::new(theme.roles.market))
    }
}

impl LedgerTable {
    fn new(theme: &Theme) -> Self {
        Self(DetailRows::new(theme.roles.market))
    }
}

impl TableDataSet<5> for SpotTable {
    fn rows_count(&self) -> usize {
        self.0.len()
    }

    fn headers(&self, cx: &App) -> [SharedString; 5] {
        [
            t!(cx, "widget.hyperliquid_wallet_monitor.asset"),
            t!(cx, "widget.hyperliquid_wallet_monitor.asset_share"),
            t!(cx, "widget.hyperliquid_wallet_monitor.quantity"),
            t!(cx, "widget.hyperliquid_wallet_monitor.value"),
            t!(cx, "common.price"),
        ]
    }

    fn column_aligns(&self) -> [Align; 5] {
        [Align::Start, Align::End, Align::End, Align::End, Align::End]
    }

    fn column_widths(&self) -> [Length; 5] {
        widths([0.15, 0.18, 0.22, 0.22, 0.23])
    }

    fn cell(&self, row_index: usize, column_index: usize, cx: &App) -> impl IntoElement {
        self.0.cell(row_index, column_index, cx)
    }

    fn empty(&self, cx: &App) -> Option<AnyElement> {
        DetailRows::<5>::empty(cx)
    }
}

impl TableDataSet<9> for PerpetualTable {
    fn rows_count(&self) -> usize {
        self.0.len()
    }

    fn headers(&self, cx: &App) -> [SharedString; 9] {
        [
            t!(cx, "widget.hyperliquid_wallet_monitor.coin"),
            t!(cx, "widget.hyperliquid_wallet_monitor.side"),
            t!(cx, "widget.hyperliquid_wallet_monitor.position_value"),
            t!(cx, "widget.hyperliquid_wallet_monitor.unrealized_pnl"),
            t!(cx, "widget.hyperliquid_wallet_monitor.entry"),
            t!(cx, "widget.hyperliquid_wallet_monitor.mark"),
            t!(cx, "widget.hyperliquid_wallet_monitor.liquidation"),
            t!(cx, "widget.hyperliquid_wallet_monitor.margin"),
            t!(cx, "widget.hyperliquid_wallet_monitor.funding"),
        ]
    }

    fn column_aligns(&self) -> [Align; 9] {
        [
            Align::Start,
            Align::Center,
            Align::End,
            Align::End,
            Align::End,
            Align::End,
            Align::End,
            Align::End,
            Align::End,
        ]
    }

    fn column_widths(&self) -> [Length; 9] {
        widths([0.08, 0.08, 0.12, 0.12, 0.12, 0.12, 0.12, 0.12, 0.12])
    }

    fn cell(&self, row_index: usize, column_index: usize, cx: &App) -> impl IntoElement {
        self.0.cell(row_index, column_index, cx)
    }

    fn empty(&self, cx: &App) -> Option<AnyElement> {
        DetailRows::<9>::empty(cx)
    }
}

impl TableDataSet<9> for OpenOrdersTable {
    fn rows_count(&self) -> usize {
        self.0.len()
    }

    fn headers(&self, cx: &App) -> [SharedString; 9] {
        [
            t!(cx, "widget.hyperliquid_wallet_monitor.coin"),
            t!(cx, "widget.hyperliquid_wallet_monitor.collateral"),
            t!(cx, "widget.hyperliquid_wallet_monitor.side"),
            t!(cx, "widget.hyperliquid_wallet_monitor.quantity"),
            t!(cx, "widget.hyperliquid_wallet_monitor.value"),
            t!(cx, "common.price"),
            t!(cx, "widget.hyperliquid_wallet_monitor.trigger_price"),
            t!(cx, "widget.hyperliquid_wallet_monitor.reduce_only"),
            t!(cx, "common.time"),
        ]
    }

    fn column_aligns(&self) -> [Align; 9] {
        [
            Align::Start,
            Align::Start,
            Align::Center,
            Align::End,
            Align::End,
            Align::End,
            Align::End,
            Align::Center,
            Align::End,
        ]
    }

    fn column_widths(&self) -> [Length; 9] {
        widths([0.09, 0.09, 0.09, 0.13, 0.13, 0.13, 0.13, 0.10, 0.11])
    }

    fn cell(&self, row_index: usize, column_index: usize, cx: &App) -> impl IntoElement {
        self.0.cell(row_index, column_index, cx)
    }

    fn empty(&self, cx: &App) -> Option<AnyElement> {
        DetailRows::<9>::empty(cx)
    }
}

impl TableDataSet<8> for TradesTable {
    fn rows_count(&self) -> usize {
        self.0.len()
    }

    fn headers(&self, cx: &App) -> [SharedString; 8] {
        [
            t!(cx, "widget.hyperliquid_wallet_monitor.coin"),
            t!(cx, "widget.hyperliquid_wallet_monitor.side"),
            t!(cx, "widget.hyperliquid_wallet_monitor.quantity"),
            t!(cx, "widget.hyperliquid_wallet_monitor.position"),
            t!(cx, "common.price"),
            t!(cx, "widget.hyperliquid_wallet_monitor.realized_pnl"),
            t!(cx, "widget.hyperliquid_wallet_monitor.fee"),
            t!(cx, "common.time"),
        ]
    }

    fn column_aligns(&self) -> [Align; 8] {
        [
            Align::Start,
            Align::Center,
            Align::End,
            Align::End,
            Align::End,
            Align::End,
            Align::End,
            Align::End,
        ]
    }

    fn column_widths(&self) -> [Length; 8] {
        widths([0.09, 0.09, 0.14, 0.14, 0.14, 0.14, 0.15, 0.11])
    }

    fn cell(&self, row_index: usize, column_index: usize, cx: &App) -> impl IntoElement {
        self.0.cell(row_index, column_index, cx)
    }

    fn empty(&self, cx: &App) -> Option<AnyElement> {
        DetailRows::<8>::empty(cx)
    }
}

impl TableDataSet<9> for HistoricalOrdersTable {
    fn rows_count(&self) -> usize {
        self.0.len()
    }

    fn headers(&self, cx: &App) -> [SharedString; 9] {
        [
            t!(cx, "widget.hyperliquid_wallet_monitor.coin"),
            t!(cx, "widget.hyperliquid_wallet_monitor.side"),
            t!(cx, "widget.hyperliquid_wallet_monitor.quantity"),
            t!(cx, "widget.hyperliquid_wallet_monitor.value"),
            t!(cx, "common.price"),
            t!(cx, "widget.hyperliquid_wallet_monitor.trigger_price"),
            t!(cx, "widget.hyperliquid_wallet_monitor.reduce_only"),
            t!(cx, "widget.hyperliquid_wallet_monitor.status"),
            t!(cx, "common.time"),
        ]
    }

    fn column_aligns(&self) -> [Align; 9] {
        [
            Align::Start,
            Align::Center,
            Align::End,
            Align::End,
            Align::End,
            Align::End,
            Align::Center,
            Align::Start,
            Align::End,
        ]
    }

    fn column_widths(&self) -> [Length; 9] {
        widths([0.09, 0.09, 0.13, 0.13, 0.13, 0.13, 0.09, 0.11, 0.10])
    }

    fn cell(&self, row_index: usize, column_index: usize, cx: &App) -> impl IntoElement {
        self.0.cell(row_index, column_index, cx)
    }

    fn empty(&self, cx: &App) -> Option<AnyElement> {
        DetailRows::<9>::empty(cx)
    }
}

impl TableDataSet<5> for LedgerTable {
    fn rows_count(&self) -> usize {
        self.0.len()
    }

    fn headers(&self, cx: &App) -> [SharedString; 5] {
        [
            t!(cx, "widget.hyperliquid_wallet_monitor.coin"),
            t!(cx, "widget.hyperliquid_wallet_monitor.kind"),
            t!(cx, "widget.hyperliquid_wallet_monitor.quantity"),
            t!(cx, "widget.hyperliquid_wallet_monitor.value"),
            t!(cx, "common.time"),
        ]
    }

    fn column_aligns(&self) -> [Align; 5] {
        [
            Align::Start,
            Align::Center,
            Align::End,
            Align::End,
            Align::End,
        ]
    }

    fn column_widths(&self) -> [Length; 5] {
        widths([0.16, 0.18, 0.24, 0.24, 0.18])
    }

    fn cell(&self, row_index: usize, column_index: usize, cx: &App) -> impl IntoElement {
        self.0.cell(row_index, column_index, cx)
    }

    fn empty(&self, cx: &App) -> Option<AnyElement> {
        DetailRows::<5>::empty(cx)
    }
}

/// Which table one detail tab is drawn by, and the entity that holds it.
pub(super) enum DetailTable {
    Spot(Entity<SpotTable>),
    Perpetual(Entity<PerpetualTable>),
    OpenOrders(Entity<OpenOrdersTable>),
    Trades(Entity<TradesTable>),
    HistoricalOrders(Entity<HistoricalOrdersTable>),
    Ledger(Entity<LedgerTable>),
}

impl DetailTable {
    /// Creates the table one tab is drawn by.
    pub(super) fn new<C: AppContext>(tab: DetailTab, cx: &mut C) -> Self {
        match tab {
            DetailTab::Spot => Self::Spot(cx.new(|cx| SpotTable::new(cx.theme()))),
            DetailTab::Perpetual => Self::Perpetual(cx.new(|cx| PerpetualTable::new(cx.theme()))),
            DetailTab::OpenOrders => {
                Self::OpenOrders(cx.new(|cx| OpenOrdersTable::new(cx.theme())))
            }
            DetailTab::Trades => Self::Trades(cx.new(|cx| TradesTable::new(cx.theme()))),
            DetailTab::HistoricalOrders => {
                Self::HistoricalOrders(cx.new(|cx| HistoricalOrdersTable::new(cx.theme())))
            }
            DetailTab::Ledger => Self::Ledger(cx.new(|cx| LedgerTable::new(cx.theme()))),
        }
    }

    /// Hands the table a reading that was already validated against its catalog.
    ///
    /// Formatting happens here, so a reading this cannot display is refused here rather than
    /// drawn as its raw value.
    pub(super) fn accept<C: AppContext>(
        &self,
        snapshot: &DetailSnapshot,
        now: i64,
        cx: &mut C,
    ) -> Result<(), WalletDataError> {
        match self {
            Self::Spot(table) => table.update(cx, |table, cx| {
                let rows = spot_rows(snapshot, now, cx)?;
                table.0.set(rows, now);
                Ok(())
            }),
            Self::Perpetual(table) => table.update(cx, |table, cx| {
                let rows = perpetual_rows(snapshot, now, cx)?;
                table.0.set(rows, now);
                Ok(())
            }),
            Self::OpenOrders(table) => table.update(cx, |table, cx| {
                let rows = open_order_rows(snapshot, now, cx)?;
                table.0.set(rows, now);
                Ok(())
            }),
            Self::Trades(table) => table.update(cx, |table, cx| {
                let rows = trade_rows(snapshot, now, cx)?;
                table.0.set(rows, now);
                Ok(())
            }),
            Self::HistoricalOrders(table) => table.update(cx, |table, cx| {
                let rows = historical_order_rows(snapshot, now, cx)?;
                table.0.set(rows, now);
                Ok(())
            }),
            Self::Ledger(table) => table.update(cx, |table, cx| {
                let rows = ledger_rows(snapshot, now, cx)?;
                table.0.set(rows, now);
                Ok(())
            }),
        }
    }

    /// Rewrites the ages of the reading this table is showing.
    pub(super) fn refresh<C: AppContext>(&self, now: i64, cx: &mut C) {
        match self {
            Self::Spot(table) => table.update(cx, |table, cx| table.0.refresh(now, cx)),
            Self::Perpetual(table) => table.update(cx, |table, cx| table.0.refresh(now, cx)),
            Self::OpenOrders(table) => table.update(cx, |table, cx| table.0.refresh(now, cx)),
            Self::Trades(table) => table.update(cx, |table, cx| table.0.refresh(now, cx)),
            Self::HistoricalOrders(table) => table.update(cx, |table, cx| table.0.refresh(now, cx)),
            Self::Ledger(table) => table.update(cx, |table, cx| table.0.refresh(now, cx)),
        }
    }

    /// The table, drawn at the id the pane gives it so its scroll position survives a rebuild.
    pub(super) fn render(&self, id: ElementId) -> AnyElement {
        match self {
            Self::Spot(table) => Table::new(id, table).into_any_element(),
            Self::Perpetual(table) => Table::new(id, table).into_any_element(),
            Self::OpenOrders(table) => Table::new(id, table).into_any_element(),
            Self::Trades(table) => Table::new(id, table).into_any_element(),
            Self::HistoricalOrders(table) => Table::new(id, table).into_any_element(),
            Self::Ledger(table) => Table::new(id, table).into_any_element(),
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{App, TestAppContext};
    use market_participants_connector::{
        account::{
            domain::{
                HyperliquidOpenOrder, HyperliquidPerpetualPosition, HyperliquidPerpetualState,
                HyperliquidSpotBalance,
            },
            shared::TimeInForce,
        },
        catalog::batch::CatalogEntry,
    };
    use market_participants_model::{
        decimal::{Quantity, SignedQuantity},
        market::units::{MarketQuantity, QuantityUnit, Side},
    };
    use smol_str::SmolStr;

    use super::*;
    use crate::{
        model::SpotSnapshot,
        test_support::{
            address, context, deposit, entry, fill, historical_order, money, open_order,
            pair_listing, perp_instrument, perpetual_route, position, precision_of, spot_entry,
            spot_pair, spot_route, spot_state, token, wallet_balance,
        },
    };

    /// Installs what a formatted row reads, which is the style and the translations.
    fn install(cx: &mut App) {
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
    }

    /// What one row says, cell by cell.
    fn texts<const N: usize>(row: &DisplayRow<N>) -> Vec<&str> {
        row.cells
            .iter()
            .map(|cell| cell.text.as_ref())
            .collect::<Vec<_>>()
    }

    /// What one cell says after the text it follows.
    fn suffix<const N: usize>(row: &DisplayRow<N>, index: usize) -> Option<&str> {
        row.cells[index].suffix.as_deref()
    }

    /// The perpetual listing the test readings are about, as the fixture publishes it.
    fn perp_listing() -> VenueListingKey {
        perp_entry().listing.key
    }

    /// The perpetual listing entry the test precision is built from.
    ///
    /// One entry serves both the precision and the readings: a listing identity derived in a
    /// test is a different listing from the one a catalog holds.
    fn perp_entry() -> CatalogEntry {
        entry(
            &perpetual_route(),
            perp_instrument("ETH"),
            "ETH",
            "0.1",
            "0.001",
        )
    }

    /// The spot listing the test balances are quoted by.
    fn spot_listing() -> VenueListingKey {
        pair_listing("HYPE", "USDC")
    }

    /// The precision of the test perpetual route's listings.
    fn perpetual_precision() -> DisplayPrecision {
        perpetual_of(vec![perp_entry()])
    }

    /// The precision of a given set of perpetual listings of the test route.
    fn perpetual_of(entries: Vec<CatalogEntry>) -> DisplayPrecision {
        precision_of(&perpetual_route(), entries)
    }

    /// The precision of the test spot route's listings.
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

    /// A perpetual reading of one position.
    fn perpetual_reading(positions: Vec<HyperliquidPerpetualPosition>) -> DetailSnapshot {
        DetailSnapshot::new(
            DetailData::Perpetual(HyperliquidPerpetualState { positions }),
            perpetual_precision(),
        )
        .expect("a published listing should validate")
    }

    #[gpui_kit::test]
    fn a_spot_row_should_use_the_assets_own_ticks_and_its_share(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(cx);
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

            let rows = spot_rows(&reading, 0, cx).expect("the row should format");

            assert_eq!(
                texts(&rows[0]),
                ["USDC", "166.66%", "10.00", "10.00", "1.00"],
                "a stablecoin is money, so its own balance and price are read as money"
            );
            assert_eq!(suffix(&rows[0], 2), Some("USDC"));
            assert_eq!(
                texts(&rows[1]),
                ["HYPE", "-66.66%", "-2.000", "-4.00", "2.000"]
            );
            assert_eq!(suffix(&rows[1], 2), Some("HYPE"));
            assert_eq!(rows[1].cells[2].tone, CellTone::Negative);
            assert_eq!(rows[1].cells[3].tone, CellTone::Negative);
        });
    }

    #[gpui_kit::test]
    fn a_perpetual_row_should_carry_its_direction_value_and_profit(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(cx);
            let mut reading = perpetual_reading(vec![position(&perp_listing(), "2", Some("100"))]);
            reading
                .apply_contexts(&[context(&perp_listing(), Some("110"), None)])
                .expect("a context should apply");

            let rows = perpetual_rows(&reading, 0, cx).expect("the row should format");

            assert_eq!(
                texts(&rows[0]),
                [
                    "ETH", "Long", "220.00", "+20.00", "100.0", "110.0", "-", "-", "0"
                ]
            );
            assert_eq!(rows[0].cells[1].tone, CellTone::Positive);
            assert_eq!(rows[0].cells[3].tone, CellTone::Positive);
            assert_eq!(
                rows[0].cells[6].tone,
                CellTone::Muted,
                "a value the venue did not report is not a number"
            );
            assert_eq!(
                suffix(&rows[0], 4),
                Some("USDC"),
                "a mark price says which asset it is quoted in"
            );
        });
    }

    /// A position the venue published as a negative size is a short, and the row says so in the
    /// words and the colour of the side it is held from rather than reading as a long.
    #[gpui_kit::test]
    fn a_short_position_should_be_read_as_a_short(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(cx);
            let reading = perpetual_reading(vec![position(&perp_listing(), "-3", None)]);

            let rows = perpetual_rows(&reading, 0, cx).expect("the row should format");

            assert_eq!(
                rows[0].cells[1].text.as_ref(),
                "Short",
                "a negative size is a position on the short side"
            );
            assert_eq!(rows[0].cells[1].tone, CellTone::Negative);
            assert_eq!(
                rows[0].cells[3].tone,
                CellTone::Muted,
                "a position without an entry has no profit to report"
            );
        });
    }

    /// A short's own profit is the other side of a long's: the same fall in the price is what a
    /// short gains and what a long loses, and the row is read in that direction.
    #[gpui_kit::test]
    fn a_short_position_should_be_valued_from_the_side_it_is_held(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(cx);
            let mut reading = perpetual_reading(vec![position(&perp_listing(), "-2", Some("100"))]);
            reading
                .apply_contexts(&[context(&perp_listing(), Some("90"), None)])
                .expect("a context should apply");

            let rows = perpetual_rows(&reading, 0, cx).expect("the row should format");

            assert_eq!(
                texts(&rows[0]),
                [
                    "ETH", "Short", "180.00", "+20.00", "100.0", "90.0", "-", "-", "0"
                ],
                "a short the price fell away from is up what a long would be down"
            );
            assert_eq!(rows[0].cells[3].tone, CellTone::Positive);
        });
    }

    /// Every tab draws exactly the rows its reading holds: a row is what the tab formats from
    /// the reading, so what a reading holds and what the table shows cannot disagree.
    #[gpui_kit::test]
    fn every_tab_should_draw_the_rows_of_its_reading(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(cx);
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
                    spot_rows(&spot, 0, cx)
                        .expect("the rows should format")
                        .len(),
                    perpetual_rows(&perpetual, 0, cx)
                        .expect("the rows should format")
                        .len(),
                    open_order_rows(&orders, 0, cx)
                        .expect("the rows should format")
                        .len(),
                    trade_rows(&trades, 0, cx)
                        .expect("the rows should format")
                        .len(),
                    historical_order_rows(&history, 0, cx)
                        .expect("the rows should format")
                        .len(),
                    ledger_rows(&ledger, 0, cx)
                        .expect("the rows should format")
                        .len(),
                ],
                [1, 1, 2, 1, 1, 1],
                "every tab draws one row per thing its reading holds"
            );

            let spot = spot_reading(Vec::new());
            let perpetual = perpetual_reading(Vec::new());
            let orders = reading(DetailData::OpenOrders(Vec::new()));
            let trades = reading(DetailData::Trades(Vec::new()));
            let history = reading(DetailData::HistoricalOrders(Vec::new()));
            let ledger = DetailSnapshot::new(DetailData::Ledger(Vec::new()), money())
                .expect("an empty ledger validates");

            assert_eq!(
                [
                    spot_rows(&spot, 0, cx)
                        .expect("an empty reading formats")
                        .len(),
                    perpetual_rows(&perpetual, 0, cx)
                        .expect("an empty reading formats")
                        .len(),
                    open_order_rows(&orders, 0, cx)
                        .expect("an empty reading formats")
                        .len(),
                    trade_rows(&trades, 0, cx)
                        .expect("an empty reading formats")
                        .len(),
                    historical_order_rows(&history, 0, cx)
                        .expect("an empty reading formats")
                        .len(),
                    ledger_rows(&ledger, 0, cx)
                        .expect("an empty reading formats")
                        .len(),
                ],
                [0, 0, 0, 0, 0, 0],
                "a reading with nothing in it draws no row at all"
            );
        });
    }

    /// A quantity is written in the unit its listing publishes it in, not in a unit this
    /// widget picked: a contract count is not a number of coins.
    #[gpui_kit::test]
    fn an_order_quantity_should_carry_the_listings_own_unit(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(cx);
            let order = open_order(&perp_listing(), "7", 1);
            let reading =
                DetailSnapshot::new(DetailData::OpenOrders(vec![order]), perpetual_precision())
                    .expect("a published listing should validate");

            let rows = open_order_rows(&reading, 0, cx).expect("the row should format");

            assert_eq!(
                suffix(&rows[0], 3),
                Some("contracts"),
                "the venue published the order in contracts, so that is what it is read as"
            );
        });
    }

    /// A position carries its size as a market quantity rather than a signed one, so what a row
    /// reads from it is the size and the profit the venue reported — never a direction the venue
    /// did not send.
    #[gpui_kit::test]
    fn a_position_without_an_entry_should_report_no_profit(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(cx);
            let reading = perpetual_reading(vec![position(&perp_listing(), "3", None)]);

            let rows = perpetual_rows(&reading, 0, cx).expect("the row should format");

            assert_eq!(
                rows[0].cells[1].text.as_ref(),
                "Long",
                "a position the venue published with a size is a long by the size it carries"
            );
            assert_eq!(
                rows[0].cells[3].tone,
                CellTone::Muted,
                "a position without an entry has no profit to report"
            );
        });
    }

    #[gpui_kit::test]
    fn an_open_order_row_should_show_its_trigger_and_its_collateral(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(cx);
            let order = HyperliquidOpenOrder {
                reduce_only: true,
                kind: market_participants_connector::account::shared::OrderKind::StopLimit {
                    price: "100".parse().expect("a price should be valid"),
                    trigger_price: "95".parse().expect("a price should be valid"),
                    trigger_price_kind:
                        market_participants_connector::account::shared::TriggerPriceKind::Mark,
                    time_in_force: TimeInForce::GoodTillCancelled,
                },
                ..open_order(&perp_listing(), "7", 1)
            };
            let reading =
                DetailSnapshot::new(DetailData::OpenOrders(vec![order]), perpetual_precision())
                    .expect("a published listing should validate");

            let rows = open_order_rows(&reading, 0, cx).expect("the row should format");

            assert_eq!(
                texts(&rows[0]),
                [
                    "ETH", "USDC", "Buy", "2.000", "200.00", "100.0", "95.0", "Yes", "0s ago"
                ]
            );
            assert_eq!(rows[0].cells[2].tone, CellTone::Positive);
        });
    }

    #[gpui_kit::test]
    fn a_trade_row_should_suffix_the_fee_with_its_token(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(cx);
            let wallet = address("1");
            let mut trade = fill(&wallet, &perp_listing(), Side::Sell, "2", "100", 1, 1);
            trade.fee = market_participants_model::market::account::AssetAmount {
                asset: token("USDC"),
                value: "-0.1"
                    .parse::<SignedQuantity>()
                    .expect("a fee should be valid"),
            };
            trade.closed_pnl = market_participants_model::market::account::AssetAmount {
                asset: token("USDC"),
                value: "3".parse().expect("a profit should be valid"),
            };
            let reading =
                DetailSnapshot::new(DetailData::Trades(vec![trade]), perpetual_precision())
                    .expect("a published listing should validate");

            let rows = trade_rows(&reading, 0, cx).expect("the row should format");

            assert_eq!(rows[0].cells[5].text.as_ref(), "+3.00");
            assert_eq!(rows[0].cells[5].tone, CellTone::Positive);
            assert_eq!(rows[0].cells[6].text.as_ref(), "-0.10");
            assert_eq!(suffix(&rows[0], 6), Some("USDC"));
            assert_eq!(rows[0].cells[6].tone, CellTone::Negative);
        });
    }

    #[gpui_kit::test]
    fn a_historical_order_row_should_keep_the_venues_own_status(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(cx);
            let mut order = historical_order(&perp_listing(), "8", 1);
            order.status = SmolStr::new_static("filledByLiquidation");
            let reading = DetailSnapshot::new(
                DetailData::HistoricalOrders(vec![order]),
                perpetual_precision(),
            )
            .expect("a published listing should validate");

            let rows = historical_order_rows(&reading, 0, cx).expect("the row should format");

            assert_eq!(rows[0].cells[7].text.as_ref(), "filledByLiquidation");
            assert_eq!(rows[0].cells[6].text.as_ref(), "Yes");
        });
    }

    #[gpui_kit::test]
    fn a_ledger_row_should_write_a_missing_value_as_a_dash(cx: &mut App) {
        install(cx);
        let wallet = address("1");
        let mut event = deposit(&wallet, "5", 1);
        event.asset = None;
        event.amount = None;
        event.value = None;
        let reading = DetailSnapshot::new(DetailData::Ledger(vec![event]), money())
            .expect("a ledger validates");

        let rows = ledger_rows(&reading, 0, cx).expect("the row should format");

        assert_eq!(
            &texts(&rows[0])[..4],
            ["-", "deposit", "-", "-"],
            "a value the venue did not report is not a zero"
        );
        assert_eq!(rows[0].cells[0].tone, CellTone::Muted);
    }

    #[gpui_kit::test]
    fn a_reading_of_another_tab_should_leave_its_table_empty(cx: &mut App) {
        install(cx);
        let reading = DetailSnapshot::new(DetailData::Ledger(Vec::new()), money())
            .expect("a ledger validates");

        assert!(
            spot_rows(&reading, 0, cx)
                .expect("it should be refused quietly")
                .is_empty()
        );
        assert!(
            open_order_rows(&reading, 0, cx)
                .expect("it should be refused quietly")
                .is_empty()
        );
    }

    #[gpui_kit::test]
    fn an_age_should_be_rewritten_when_the_second_changes(cx: &mut App) {
        install(cx);
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
        let mut rows = DetailRows::<8>::new(cx.theme().roles.market);
        rows.set(
            trade_rows(&reading, 1_000, cx).expect("the row should format"),
            1_000,
        );
        let first = rows.rows[0].cells[7].text.clone();

        // Nothing moves inside the second the row was formatted at…
        rows.refresh(1_500, cx);
        assert_eq!(rows.rows[0].cells[7].text, first);

        // …and the age follows the clock once it does.
        rows.refresh(61_000, cx);
        assert_ne!(rows.rows[0].cells[7].text, first);
        assert_eq!(rows.rows[0].cells[7].timestamp, Some(1_000));
    }

    /// A balance's price is drawn in the tick of the pair that priced it and says which asset it
    /// is quoted in: one base quoted against two assets has two markets, and the balance is
    /// valued in one of them.
    #[gpui_kit::test]
    fn a_balance_price_should_be_read_in_the_tick_of_the_pair_that_priced_it(cx: &mut App) {
        install(cx);
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
        let rows = spot_rows(
            &DetailSnapshot::new(DetailData::Spot(snapshot), precision)
                .expect("the reading should validate"),
            0,
            cx,
        )
        .expect("the spot rows should format");

        assert_eq!(
            rows[0].cells[4].text.as_ref(),
            "1.00",
            "the tick of the pair that priced the balance draws it, not the other pair's"
        );
        assert_eq!(rows[0].cells[4].suffix.as_deref(), Some("USDH"));
    }

    /// A quantity the route publishes as an amount rather than as a number of coins is not
    /// suffixed with the asset it is denominated in: the amount is already money.
    #[gpui_kit::test]
    fn a_notional_quantity_should_not_carry_an_asset_suffix(cx: &mut App) {
        install(cx);
        let precision = perpetual_precision();
        let notional = MarketQuantity::amount(
            Quantity::from(5_u8),
            QuantityUnit::Notional {
                asset: token("USDC"),
            },
        )
        .expect("a notional is an amount");

        let cell = market_quantity(&precision, &perp_listing(), &notional, cx)
            .expect("a published listing should format an amount");

        assert_eq!(cell.text.as_ref(), "5.000");
        assert_eq!(
            cell.suffix, None,
            "an amount is money, so the asset it is in is not repeated"
        );
    }

    #[test]
    fn every_tables_columns_should_fill_the_width_it_is_given() {
        let theme = Theme::default();
        for widths in [
            SpotTable::new(&theme).column_widths().to_vec(),
            PerpetualTable::new(&theme).column_widths().to_vec(),
            OpenOrdersTable::new(&theme).column_widths().to_vec(),
            TradesTable::new(&theme).column_widths().to_vec(),
            HistoricalOrdersTable::new(&theme).column_widths().to_vec(),
            LedgerTable::new(&theme).column_widths().to_vec(),
        ] {
            let total = widths
                .iter()
                .map(|width| match width {
                    Length::Definite(gpui_kit::DefiniteLength::Fraction(fraction)) => *fraction,
                    _ => panic!("a column width should be a fraction of the table"),
                })
                .sum::<f32>();
            assert!(
                (total - 1.0).abs() < 0.001,
                "the columns should fill the table, not {total}"
            );
        }
    }
}

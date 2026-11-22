//! The six views of one account, each as a kit table.
//!
//! # Why a cell is formatted on the frame that draws it
//!
//! A cell's text is a decimal rounded to a tick and grouped, which is the expensive part of
//! drawing a wallet. The kit table's row factory has no application context — no language, no
//! clock, no theme — so a cell is a pair of text nodes whose text and colour are read on the
//! frames that draw them: the text node binds over the reading, the settings and the language,
//! and asks the pure functions here for the one cell it draws. A reading that cannot be
//! displayed is therefore refused once, by the scope that accepted it, rather than per cell per
//! frame: every reading is validated against its catalog before it reaches a table
//! ([`DetailSnapshot::new`]), and the formatting here consequently only ever confirms what the
//! validation promised.
//!
//! # Why the ages are the exception
//!
//! An age moves while nothing else does, so every tab keeps its own clock as a signal: a
//! one-shot engine timer (E10) advances the shown tab's clock once a second while that tab's
//! reading is on screen, the tab's time cells read it, and the write that moves it is what asks
//! for the frame that rewrites the ages. A tick that leaves the ages where they were asks for no
//! frame, and neither the tabs that are hidden nor the tabs with no age in them are rewritten by
//! a tick the shown tab's clock made.
//!
//! # One cell function per tab
//!
//! A tab is a column count plus the meaning of its cells. Two tabs with the same count would be
//! the same type if that meaning were a field, so every tab has its own cell function instead:
//! what a cell means is then the code that produced it rather than a dispatch at draw time. The
//! column declarations (headers, widths, alignment) sit beside those functions in one table per
//! tab, so what a tab shows and how it is laid out cannot drift apart.
//!
//! # The tables are built once
//!
//! The body is built once and then updated through bindings (AGENTS §1-3): the six tables are
//! built under one container and the shown tab is a signal each table's wrapper container reads.
//! A tab switch is therefore one signal write, and the tables keep their node identity, their
//! scroll position and their accepted readings.
//!
//! The kit table takes its column headers and its empty-state sentence as resolved text when it
//! is built (AGENTS §15.1), so the language is the one value the static copy cannot bind: the
//! language is memoised and the six tables are rebuilt under the same parent and keys when it
//! moves. The accepted readings live in signals this module owns, so the rebuild keeps the data
//! the user is looking at.
//!
//! # A reading belongs to one wallet
//!
//! The six tables outlive the wallet they were read for, so the pane says when they are no
//! longer about what is on screen: opening another wallet, or a scope the readings were not
//! read in, clears them. A cleared table is an empty table — the pane then says it is reading
//! the tab it is about to draw, rather than leaving the last wallet's rows under a spinner or
//! beneath a failure banner.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use app_config::settings::Config;
use base_ui::{
    kit::{
        list::SelectionMode,
        spinner::Spinner,
        table::{Column, ColumnAlign, Table, TableOptions},
    },
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};
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
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::style::StylePatch,
    text::TextStyle,
};
use widget::{cx::WidgetCx, time_zone::data_time_zone};

use super::{
    model::{
        DetailData, DetailSnapshot, DetailTab, DisplayPrecision, SpotBalanceRow, WalletDataError,
        absolute_time, now_millis,
    },
    timeline::age_text,
};

/// How often the ages of a reading that is on screen are rewritten.
///
/// One second is the resolution an age is read at ([`age_text`]): a tick that ran more often
/// would write a value no cell reads.
const AGE_TICK: Duration = Duration::from_secs(1);

/// The row height of every detail table.
///
/// It is the kit's medium control height, which is what the old shared table used as well: a
/// row of numbers is a control-height line, not a paragraph.
fn row_height(theme: &ThemeHandle) -> f32 {
    theme.tokens().medium.height
}

/// The colour a cell is read in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CellTone {
    Default,
    Muted,
    Positive,
    Negative,
}

/// The colour a cell's tone is drawn in.
///
/// A cell that is neither a gain nor a loss carries no colour of its own: the theme's content
/// colour is what the shared table drew every cell in, and a muted cell — one the venue did not
/// report — is read in it too rather than drawn more quietly. A number's direction is a market
/// reading, not a chart series (§8.4).
const fn tone_role(tone: CellTone) -> Role {
    match tone {
        CellTone::Default | CellTone::Muted => Role::FgDefault,
        CellTone::Positive => Role::MarketUp,
        CellTone::Negative => Role::MarketDown,
    }
}

/// One formatted cell.
#[derive(Clone, Debug)]
struct DisplayCell {
    text: SmolStr,
    /// A second word after the text, drawn in the table's own colour: the token a fee is paid
    /// in.
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

    /// A cell with nothing to show, which is how a value the venue did not report is written.
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

    /// A cell that shows how long ago something happened.
    ///
    /// The moment it is about is the row's rather than the cell's: it is what the time cell's
    /// accessible name is read from ([`moment_label`]), and what the clock the cell reads is
    /// compared against.
    fn age(now: i64, timestamp: i64, translate: &Translate) -> Self {
        Self {
            text: age_text(now, timestamp, translate),
            suffix: None,
            tone: CellTone::Muted,
        }
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
fn listing_cell(
    precision: &DisplayPrecision,
    listing: &VenueListingKey,
    translate: &Translate,
) -> DisplayCell {
    DisplayCell::text(precision.listing_label(listing, translate))
}

/// The asset one order is collateralised in.
fn collateral(
    precision: &DisplayPrecision,
    listing: &VenueListingKey,
    translate: &Translate,
) -> DisplayCell {
    DisplayCell::text(precision.listing_collateral_label(listing, translate))
}

/// A monetary amount of one asset, drawn to the money tick.
fn money_amount(precision: &DisplayPrecision, value: Option<&AssetAmount>) -> DisplayCell {
    value.map_or_else(DisplayCell::missing, |value| {
        DisplayCell::text(precision.format_money_signed_quantity(value.value))
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

/// A price the venue published with its quote asset and basis.
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

/// One asset's own quantity, drawn in the asset's own tick.
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

/// The notional value of one order, in the asset its price is quoted in.
///
/// The venue reports an order's size and its price, not what the two are worth together; a size
/// it cannot multiply is not a value, so an order whose notional overflows is drawn without one.
fn order_notional(quantity: Quantity, price: &MarketPrice) -> Option<AssetAmount> {
    quantity
        .checked_mul_price(price.value)
        .map(|value| AssetAmount {
            asset: price.quote_asset.clone(),
            value: SignedQuantity::from_inner(value.inner() as i128),
        })
}

/// One spot balance, valued against the wallet's other balances.
///
/// A balance is a quantity of one asset rather than a position on one listing, so its asset is
/// a column of its own: the same balance can be quoted by several pairs of the route.
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

/// One perpetual position, with the profit it carries against its entry.
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

/// One resting order, with the trigger it waits for and the moment it was placed at.
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

/// One fill, with the position it started from and the fee it paid.
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

/// One order the venue has already taken out of its book, with the state it ended in.
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

/// One account event, as the venue's own ledger records it.
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

/// The cell one row of one tab draws in one column.
///
/// A reading of another tab holds none of this tab's rows, so it draws no cell at all rather
/// than a row the tab does not mean ([`tab_rows`] answers the same question).
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

/// The cell a text node draws, with the missing value standing in for anything that cannot be
/// formatted.
///
/// Every accepted reading has been validated against its catalog ([`DetailSnapshot::new`]), so
/// the formatting inside [`tab_cell`] confirms what the validation promised; this is the
/// defensive path a frame takes rather than one the user is meant to reach. It is also what a
/// cell draws in a row the reading no longer holds — the tab's rows and the table's row count
/// are written from the same reading, so a frame does not see the two disagree.
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

/// How many rows one reading holds, as the tab draws them.
///
/// A reading of another tab holds none of this tab's rows, which is how a tab that has not been
/// read yet draws an empty table rather than one invented from somebody else's data.
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

/// The column a tab writes the age of a moment in, when it has one.
fn time_column(tab: DetailTab) -> Option<usize> {
    match tab {
        DetailTab::OpenOrders => Some(8),
        DetailTab::Trades => Some(7),
        DetailTab::HistoricalOrders => Some(8),
        DetailTab::Ledger => Some(4),
        DetailTab::Spot | DetailTab::Perpetual => None,
    }
}

/// The moment a row of a tab is about, when it is about one.
///
/// A row that carries it is a moment rather than a duration: the time cell's accessible name is
/// that moment read in the display time zone ([`moment_label`]), while the text the table draws
/// is how long ago it was.
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

/// The moment a time cell is about, read in `zone`.
///
/// An age says how long ago something happened without saying when; this is the name assistive
/// technology reads instead, and it is the reason a time cell depends on the display time zone.
fn moment_label(
    tab: DetailTab,
    snapshot: Option<&DetailSnapshot>,
    row: usize,
    zone: time::UtcOffset,
) -> Option<SmolStr> {
    let snapshot = snapshot?;
    moment_of(tab, snapshot, row).map(|timestamp| absolute_time(timestamp, zone))
}

/// One column of one tab, as the tables declare them.
struct ColumnSpec {
    /// The i18n key of the header, resolved when the table is built.
    header: &'static str,
    /// The column's share of the table's width (the old `widths(..)` fractions, as weights).
    weight: f32,
    align: ColumnAlign,
}

/// The columns of the spot tab: a balance is a quantity of an asset, quoted by its pairs.
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

/// The columns of the perpetual tab: one position, its numbers and its profit.
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

/// The columns of the open-orders tab: an order that has not been filled yet.
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

/// The columns of the trades tab: one fill, what it closed and what it cost.
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

/// The columns of the historical-orders tab: an order the venue has taken out of its book.
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

/// The columns of the ledger tab: one account event, and the value it moved.
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

/// The columns one tab declares, in header order.
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

/// The columns a tab's table declares, with their headers resolved in the language showing now.
///
/// The kit table takes its headers as text rather than as a binding (AGENTS §15.1), so this is
/// what a language switch rebuilds the tables for.
fn columns(tab: DetailTab, translate: &Translate) -> Vec<Column> {
    specs(tab)
        .iter()
        .map(|spec| Column::flex(t_smol!(translate, spec.header), spec.weight).align(spec.align))
        .collect()
}

/// What one built cell reads on the frames that draw it.
///
/// The kit table's row factory is called without an application context, so every dynamic value
/// inside a cell comes from a binding over these signals rather than from the factory.
#[derive(Clone)]
struct CellContext {
    tab: DetailTab,
    /// One reading per tab, indexed by [`DetailTab::index`]: the cell draws its own tab's.
    readings: [Signal<Option<Arc<DetailSnapshot>>>; 6],
    /// One clock per tab, indexed the same way: only the shown tab's is advanced, so a tick
    /// rewrites the ages of the tab a reader is looking at and no others.
    nows: [Signal<i64>; 6],
    /// The settings snapshot, for the display time zone a moment's name is read in.
    config: Signal<Config>,
    language: Signal<LanguageCode>,
    theme: ThemeHandle,
    translate: Translate,
    font_size: f32,
}

/// Builds one cell of one tab.
///
/// The kit table calls this without an application context, so what it builds is text nodes
/// whose text and colour are read on the frames that draw them: the cell registers its
/// dependencies inside `bind_text` / `bind_style` / `bind_a11y` rather than resolving anything
/// here (AGENTS §4.4 / §9).
///
/// A cell draws in a theme foreground. The engine's `TextStyle` colour is `BLACK` by default and
/// the cell box's `foreground` does not inherit into the text node, so a cell that bound no
/// colour would be unreadable on a dark appearance (and differ from the old table, which
/// resolved `theme.roles.fg.default` for every cell).
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
        // Only a time cell reads a clock: the five tabs and the other columns of a tab with an
        // age in it hold no moving text, so a tick does not rewrite them.
        let now = if Some(column) == time_column(tab) {
            *read.read(&nows[tag])
        } else {
            0
        };
        displayed_cell(tab, snapshot.as_deref(), row, column, now, &translate).text
    });
    // The colour is the tone of what the row holds, which no clock moves: it is read in its own
    // pass, so the second that rewrites an age does not recompute the colours of the table.
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
    // A time cell is also a moment: its accessible name is when it happened, read in the
    // display time zone, because an age says how long ago without saying when.
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

    // The unit the value is read in, drawn after it in the table's own colour.
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

/// The six kit tables, one per tab, built once behind the tab strip.
///
/// # Who removes what
///
/// The container, the stack of tables and the loading overlay are built by this module under the
/// pane's content slot, and they stay inside the framework's surface subtree: the **framework**
/// removes them when the pane is released (`AnyWidget::release` → E16: nodes first, then the
/// scope), so this module must not. What this module does remove is its own: the six table
/// subtrees when a language switch rebuilds them, and the spinner's node when the loading window
/// closes (a spinner left mounted keeps asking for frames).
pub(super) struct DetailTables {
    /// The container built under the pane's body: the stack of tables and the loading overlay.
    area: NodeId,
    /// Where the six tables live; a language switch rebuilds this node's children.
    stack: NodeId,
    /// Where the spinner's canvas goes while the overlay is drawn.
    ///
    /// The spinner itself is not a field: it is built for the window it is shown in and released
    /// when the wait ends (I3), so the only node this module needs to keep is its slot.
    spinner_slot: NodeId,
    /// The tab whose table is drawn. Each wrapper container reads it.
    tab: Signal<DetailTab>,
    /// One reading per tab, indexed by [`DetailTab::index`], as the cells read it.
    readings: [Signal<Option<Arc<DetailSnapshot>>>; 6],
    /// Each tab's clock, advanced once a second while that tab's ages are on screen.
    nows: [Signal<i64>; 6],
    /// Whether the detail page is the one the pane is showing: the ages of a hidden page are
    /// not read, so its clocks do not run.
    page: Signal<bool>,
    /// Whether the loading overlay is drawn.
    overlay_visible: Signal<bool>,
    /// The settings snapshot, for the display time zone a moment's name is read in.
    config: Signal<Config>,
    language: Signal<LanguageCode>,
    theme: ThemeHandle,
    translate: Translate,
    /// The six tables, in [`DetailTab::ALL`] order; rebuilt on a language switch.
    tables: RefCell<Vec<Table>>,
    /// The wrapper container of each table, in the same order; rebuilt with the tables.
    wrappers: RefCell<Vec<NodeId>>,
    /// The spinner, while the loading window is open.
    spinner: RefCell<Option<Spinner>>,
    /// Which tabs the pane is reading, so the overlay follows the shown one.
    loading: Cell<[bool; 6]>,
    /// The language the tables' static copy was built in.
    seen_language: Cell<LanguageCode>,
    /// The one-shot timer that advances the shown tab's clock.
    timer: RefCell<Option<TimerId>>,
    /// Whether that timer has fired and the `sync` that sees it must arm the next one.
    timer_fired: Rc<Cell<bool>>,
}

impl DetailTables {
    /// Builds the container under `parent` and all six tables under it.
    ///
    /// The container is the pane's whole detail area: a stack the six tables share (the shown
    /// tab is the one whose wrapper is visible) and the loading overlay drawn over it. The
    /// tables are built with no rows; [`DetailTables::accept`] hands each one its reading. The
    /// pane's page signal is what says whether these ages are on screen at all: the clock only
    /// runs for a page a reader is looking at.
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
                    .top(0.0)
                    .right(0.0)
                    .bottom(0.0)
                    .left(0.0)
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(6.0)
                    .build(),
            )
            // The waiting area is one loading indicator to assistive technology, named in the
            // language showing now: the sentence under the spinner is what a reader sees, and
            // this is what a screen reader announces instead of the shape.
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
        // The sentence is drawn for the reader; the overlay carries the same words as its
        // accessible name, so the text itself is decorative to assistive technology.
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

    /// Hands one tab's accepted reading to its table.
    ///
    /// What the table is told is the reading — the cells format themselves from it on the frame
    /// that draws them — and how many rows it holds. A reading of another tab holds none of this
    /// tab's rows, which is how a tab nobody has read yet draws an empty table. The reading is
    /// also the answer the tab was waiting for, so the wait ends with it.
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

    /// Shows one tab and hides the other five.
    ///
    /// The six tables keep their nodes: the tab is a signal each wrapper container reads, so a
    /// switch is one write rather than a rebuild. The tab being shown gets the moment its ages
    /// are read at: the clocks of the hidden tabs were not the ones running.
    pub(super) fn show(&self, tab: DetailTab, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if *cx.read(&self.tab) != tab {
            cx.write(&self.tab, tab);
            cx.write(&self.nows[tab.index()], now_millis().unwrap_or(0));
        }
        self.sync_loading(cx)
    }

    /// Drops every tab's reading: the tables the pane shows belong to one wallet, and a
    /// reading of another wallet is not one this page may draw.
    ///
    /// A cleared table is an empty table, and the wait that follows is the overlay's: the pane
    /// says it is reading the tab it is about to draw rather than leaving the last wallet's
    /// rows under a spinner.
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

    /// Drops one tab's reading: a tab whose reading the venue refused is a tab with nothing to
    /// draw, not one that keeps showing the reading before it.
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

    /// Says whether the pane is reading one tab, which is what draws the wait over it.
    ///
    /// A tab read again is a wait over the reading already on screen: the answer is what the
    /// table is for, and the old pane said the same in its table area.
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

    /// Per-frame work: the static copy a language switch rebuilds, the shown tab's ages' clock,
    /// the table component syncs.
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

    /// Releases the six tables and the spinner.
    ///
    /// The container this module built stays in the tree: it is inside the framework's surface
    /// subtree, which the framework removes after this hook returns (E16). The ages' timer is
    /// cancelled here because it is the one registration of this module's that outlives the
    /// nodes: the framework would cancel it with them, but a pane being torn down should not
    /// leave a tick armed until then.
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

    /// One tab's cell node, for the pane's own tests.
    ///
    /// The view tests what the pane draws («the time cell redraws when the settings move», «an
    /// empty table keeps its header») without owning the tables, so the two reads they need are
    /// opened here rather than making the tables public.
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

    /// The rows one tab's table was last given, for the pane's own tests.
    #[cfg(test)]
    pub(super) fn row_count(&self, tab: DetailTab) -> usize {
        self.tables.borrow().get(tab.index()).map_or(0, Table::rows)
    }

    /// Whether the loading overlay is drawn, for the pane's own tests.
    #[cfg(test)]
    pub(super) fn overlay_visible(&self, app: &ui_core::app::App) -> bool {
        *app.read(&self.overlay_visible)
    }

    /// Whether the ages' timer is armed, for the pane's own tests.
    #[cfg(test)]
    pub(super) fn age_clock_armed(&self) -> bool {
        self.timer.borrow().is_some()
    }

    /// The container the six tables are built under, for the pane's own tests.
    #[cfg(test)]
    pub(super) fn area_node(&self) -> NodeId {
        self.area
    }

    /// Builds a table and its wrapper container into the stack.
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
                        .top(0.0)
                        .right(0.0)
                        .bottom(0.0)
                        .left(0.0)
                        .flex_col()
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                // The shown tab is the visible one; the other five stay in the tree, keeping
                // their scroll position and their nodes, and are hidden from both the frame and
                // assistive technology (R-28: a hidden node is not one an AT should announce).
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

    /// Builds one tab's table over the rows its reading holds.
    fn make_table(
        &self,
        cx: &mut WidgetCx<'_>,
        tab: DetailTab,
        rows: usize,
        font_size: f32,
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

    /// Rebuilds the six tables after the language moved.
    ///
    /// The kit table's headers and empty sentence are resolved text rather than bindings
    /// (AGENTS §15.1), so that is the one thing a language switch has to rebuild — and only
    /// that: the wrapper containers, the container and the readings are this module's state and
    /// survive. The nodes go before the components (E16): the old table's subtree is removed
    /// first, then its scope and its sort handler are handed back, then the new table is built
    /// under the same parent and the same key.
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

    /// Draws the loading overlay while the shown tab is being read.
    ///
    /// A reading already on screen stays under the wait: the answer is what the table is for,
    /// so a tab being read again is waited for over the reading it still shows rather than
    /// drawn as if the round were over. The spinner is built for the window it is shown in and
    /// released when the wait ends (I3): a hidden spinner would keep asking for frames for as
    /// long as the pane lives.
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

    /// Advances the shown tab's clock once a second while its ages are on screen.
    ///
    /// The engine's timer is one-shot (E10), so the `sync` that sees it fired arms the next one.
    /// The callback's write is what asks for the frame that rewrites the ages: a tick that moved
    /// no age writes a value no cell reads and draws nothing. A page that is not showing and a tab
    /// with no age in it run no clock at all (I3): a list page has no ages to move.
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

    /// The translator the product ships, in the language a test asks for.
    fn translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::global::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    /// What one row says, cell by cell.
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

    /// What one row says, cell by cell.
    fn texts(row: &[DisplayCell]) -> Vec<&str> {
        row.iter().map(|cell| cell.text.as_str()).collect()
    }

    /// What one cell says after the text it follows.
    fn suffix(row: &[DisplayCell], index: usize) -> Option<&str> {
        row[index].suffix.as_deref()
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
        precision_of(&perpetual_route(), vec![perp_entry()])
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

    // ------------------------------------------------------------- formatting

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

    /// A position the venue published as a negative size is a short, and the row says so in the
    /// words and the colour of the side it is held from rather than reading as a long.
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

    /// A short's own profit is the other side of a long's: the same fall in the price is what a
    /// short gains and what a long loses, and the row is read in that direction.
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

    /// Every tab draws exactly the rows its reading holds: a row is what the tab formats from
    /// the reading, so what a reading holds and what the table shows cannot disagree.
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
        // Both rows of the open-orders reading format, so the count is not the only thing the
        // tab answers.
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

    /// A quantity is written in the unit its listing publishes it in, not in a unit this
    /// widget picked: a contract count is not a number of coins.
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

    /// A position carries its size as a market quantity rather than a signed one, so what a row
    /// reads from it is the size and the profit the venue reported — never a direction the venue
    /// did not send.
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

    /// An age is what the row says about how long ago it happened, and it follows the clock the
    /// cell is drawn at — the table itself is handed the moment once a second, not a text it
    /// would have to keep rewriting.
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

    /// A balance's price is drawn in the tick of the pair that priced it and says which asset it
    /// is quoted in: one base quoted against two assets has two markets, and the balance is
    /// valued in one of them.
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

    /// A quantity the route publishes as an amount rather than as a number of coins is not
    /// suffixed with the asset it is denominated in: the amount is already money.
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

    /// The columns of every tab fill the width the table is given: a fixed set of weights that
    /// did not add up would leave a strip of the table undrawn.
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

    // ----------------------------------------------------------------- tables

    /// Builds the six tables under the window root, where the layout gives them a size.
    fn tables(harness: &mut WidgetTestApp) -> DetailTables {
        let root = harness.tree().root();
        harness.with_cx(|cx| {
            let page = cx.signal(true);
            DetailTables::build(cx, root, page).expect("the tables build")
        })
    }

    /// Runs the tables' per-frame work and a frame, the way the pane's own `sync` does.
    fn settle(harness: &mut WidgetTestApp, tables: &DetailTables) {
        for _ in 0..4 {
            harness.with_cx(|cx| tables.sync(cx).expect("the tables sync"));
            harness.drain();
        }
    }

    /// One table of the six, by tab.
    fn table(tables: &DetailTables, tab: DetailTab) -> Table {
        tables.tables.borrow()[tab.index()].clone()
    }

    /// The loading overlay this module built under the detail area.
    fn loading_overlay(harness: &WidgetTestApp, tables: &DetailTables) -> NodeId {
        harness
            .find_by_debug_prefix(tables.area, "TEST_HYPERLIQUID_WALLET_DETAIL_LOADING")
            .expect("the loading overlay is built with the tables")
    }

    /// The text one cell of one tab draws, one line per non-empty text node.
    ///
    /// A cell that carries a unit draws two text nodes; the unit's node is always built (the
    /// unit is the row's, not the column's), so an empty line is what a cell without one draws.
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

    /// The text one header cell of one tab draws.
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

    /// A perpetual reading of one long position, which every table test starts from.
    fn one_position() -> Arc<DetailSnapshot> {
        Arc::new(perpetual_reading(vec![position(
            &perp_listing(),
            "2",
            Some("100"),
        )]))
    }

    /// Accepting a reading reaches the tab's table, and the table's rows are what the reading
    /// holds.
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

    /// The shown tab is the visible one: switching tabs writes the tab signal the wrapper
    /// containers read, so no table is rebuilt and no reading is lost.
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

    /// The cells and the headers follow the language: a cell reads the language signal inside
    /// its binding, while the headers and the empty sentence — the kit table's build-time copy
    /// — are rebuilt in the language showing now (AGENTS §15.1).
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

    /// The display time zone is a settings read: moving it redraws the time cells, whose
    /// accessible name is the moment read in that zone, while the age they draw stays relative.
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

    /// An empty reading is a reading: the table keeps its headers and says, in the table body,
    /// that the wallet holds none of what the tab lists (AGENTS §6-6).
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

    /// A tab being read with nothing on screen is waited for under a spinner, and the wait ends
    /// when the reading arrives: the spinner is released, so a hidden spinner never asks for
    /// frames.
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

    /// A reading that is being read again is waited for in the table area: the reading already on
    /// screen stays under the overlay, and the overlay is what says a newer one is coming.
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

    /// The readings of one wallet are not the readings of another: clearing the tables is how the
    /// pane stops drawing a wallet it is no longer about.
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

    /// A steady sync asks for no frame (I3): the clock tick is a timer, not a wall-clock read in
    /// `sync`, so two syncs in a row write nothing and draw nothing.
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
        // What must not happen is the tables scheduling a frame: a write would ask for one, and
        // an idle round would then be followed by a drawn one. The round the host runs itself is
        // not part of the claim — the kit list repaints a row it re-measured without scheduling a
        // frame, which is the kit's business (AGENTS §11: `frame_requests` is the I3 face).
        assert_eq!(
            harness.frame_requests(),
            requests,
            "a steady sync writes no signal and asks for no frame"
        );
    }

    /// The ages are advanced by the engine's one-shot timer: one second of engine time rewrites
    /// them, and the pane asks for no frame before that second is up.
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

        // The second is up: the tick writes the clock, the write asks for the frame that
        // rewrites the ages, and the `sync` of that frame arms the next one.
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

    /// The ages of a page nobody is looking at are not moved: hiding the detail page stops the
    /// clock, and a tab with no age in it never runs one.
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

    /// The dialog-style release: every value slot the tables minted comes back, so mounting and
    /// releasing a pane repeatedly reuses the slots instead of minting fresh ones.
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
                // The other tab is being read with nothing on screen yet, which is the one
                // state that mounts the spinner.
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
            // The framework removes the pane's subtree before it releases the scope (E16); a
            // test that released the scope first would leave bindings reading freed slots.
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

    /// Runs `body` with a context over a scope the caller owns, the way the framework mounts a
    /// pane.
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

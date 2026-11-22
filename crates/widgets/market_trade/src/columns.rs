//! The tape's columns: the units its headers name and the cells its rows draw.
//!
//! A price without its quote currency is not readable, and the unit a traded
//! quantity is counted in is not the same on every product, so the first two headers
//! name the unit the newest retained trade actually carries. The names are the display
//! codes the published generation decided on — resolved through the directory's
//! catalogue — not the venue's own symbols; an asset that generation names nothing for
//! reads as the localized unknown, because a reader cannot use a canonical identity and
//! a name invented here would disagree with every other pane.
//!
//! The kit table builds its cells **without an application context**, so every dynamic
//! value in a row is a `bind_*` closure over the signals the pane hands in: the retained
//! trades (for the row), the settings snapshot (for the display time zone) and the theme
//! (for the colours). Nothing here needs an `App` at build time (AGENTS §9).

use app_config::settings::Config;
use base_ui::{
    kit::{
        list::SelectionMode,
        table::{Column, ColumnAlign, TableOptions},
    },
    theme::{handle::ThemeHandle, role::Role},
};
use i18n::{t_smol, translate::Translate};
use market_participants_connector::catalog::{display::catalog::DisplaySnapshot, store::Catalog};
use market_participants_model::{
    asset::AssetId,
    market::units::{QuantityUnit, Side},
};
use smol_str::SmolStr;
use time::UtcOffset;
use ui_core::{
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::style::StylePatch,
    text::TextStyle,
};
use widget::time_zone::data_time_zone;

use super::model::{MarketTradeRow, MarketTradeSnapshot};

/// The number of columns the tape has: price, amount, time.
pub(super) const COLUMN_COUNT: usize = 3;

/// The width weight of each column, in header order.
///
/// The kit table distributes the remaining width by these weights, which is the
/// engine's spelling of the relative widths the shared table used to declare: the price
/// column is the widest because it is the column a tape is read by.
const COLUMN_WEIGHTS: [f32; COLUMN_COUNT] = [0.4, 0.35, 0.25];

/// The units the columns of the tape are named with.
///
/// A price is quoted in an asset and a traded quantity is counted in a unit, and both
/// are carried by every trade rather than by the pair the user picked. The newest
/// trade is what the headers name; a tape that has printed nothing has no unit to name
/// either, so its headers stay generic.
#[derive(Debug)]
pub(super) struct MarketTradeUnits {
    price: SmolStr,
    quantity: SmolStr,
}

/// The units of the newest trade in `snapshot`, as the published generation names them.
///
/// One capture of the display generation names both headers, so they are read from the
/// same publication. It is taken when the table's static copy is built or compared, not
/// once per frame: what the table keeps is the copy it was built with, and a snapshot
/// that changes the units is what makes the pane rebuild it.
pub(super) fn units_of(
    snapshot: &MarketTradeSnapshot,
    catalog: &Catalog,
    translate: &Translate,
) -> Option<MarketTradeUnits> {
    let trade = snapshot.rows().last()?.trade();
    let display = catalog.cached_display();

    Some(MarketTradeUnits {
        price: asset_display(&display, &trade.price.quote_asset, translate),
        quantity: quantity_unit_label(&display, &trade.quantity.unit, translate),
    })
}

/// The display code of one asset, as the captured generation published it.
///
/// Only the display catalogue knows what an asset is called: the canonical identity of
/// a token is a digest, and the code the catalogue publishes is the whole name of the
/// asset. An asset the captured generation does not hold is the localized unknown
/// rather than an identity digest, because a reader cannot use a hash and a name
/// invented here would disagree with every other pane.
fn asset_display(display: &DisplaySnapshot, asset: &AssetId, translate: &Translate) -> SmolStr {
    display.resolver().asset_code(asset).map_or_else(
        |_| t_smol!(translate, "error.display.unknown_asset"),
        |code| SmolStr::new(code.as_str()),
    )
}

/// The label of one quantity unit, named the way that unit is measured.
fn quantity_unit_label(
    display: &DisplaySnapshot,
    unit: &QuantityUnit,
    translate: &Translate,
) -> SmolStr {
    match unit {
        QuantityUnit::AssetUnits { asset } | QuantityUnit::Notional { asset } => {
            asset_display(display, asset, translate)
        }
        QuantityUnit::Contracts => t_smol!(translate, "widget.market_trade.contracts"),
        QuantityUnit::OutcomeTokens { .. } => {
            t_smol!(translate, "widget.market_trade.outcome_tokens")
        }
    }
}

/// The headers the table declares, in column order.
///
/// Built when the table is built, because the kit table takes its headers as resolved
/// text; a language switch or a snapshot that changes the units rebuilds it.
pub(super) fn headers(
    units: Option<&MarketTradeUnits>,
    translate: &Translate,
) -> [SmolStr; COLUMN_COUNT] {
    units.map_or_else(
        || {
            [
                t_smol!(translate, "common.price"),
                t_smol!(translate, "common.amount"),
                t_smol!(translate, "common.time"),
            ]
        },
        |units| {
            [
                t_smol!(
                    translate,
                    "widget.market_trade.price_with_unit",
                    unit = units.price
                ),
                t_smol!(
                    translate,
                    "widget.market_trade.amount_with_unit",
                    unit = units.quantity
                ),
                t_smol!(translate, "common.time"),
            ]
        },
    )
}

/// The columns the table declares, in header order: text on the left, numbers on the right.
pub(super) fn columns(headers: [SmolStr; COLUMN_COUNT]) -> Vec<Column> {
    headers
        .into_iter()
        .zip(COLUMN_WEIGHTS)
        .map(|(header, weight)| Column::flex(header, weight).align(ColumnAlign::End))
        .collect()
}

/// The options the tape needs from the kit table: a fixed row height (the dataset
/// declares one) and **no row selection**.
///
/// The old table drew rows to read, not rows to pick: it had no selection state, no
/// click handling and no selected-row styling. The kit table defaults to
/// [`SelectionMode::Multi`], so the pane turns selection off explicitly here — the one
/// place the decision is made.
pub(super) fn table_options(row_height: f32) -> TableOptions {
    TableOptions::fixed(row_height).selection(SelectionMode::None)
}

/// The retained row a table row index displays: the newest trade first.
///
/// The table draws the retained trades with the newest at the top, so its row index
/// counts backwards through the snapshot's chronological order.
pub(super) fn displayed_row(snapshot: &MarketTradeSnapshot, row: usize) -> Option<&MarketTradeRow> {
    let index = snapshot.len().checked_sub(row.checked_add(1)?)?;
    snapshot.rows().get(index).map(|row| &**row)
}

/// The text one cell draws, from the row and column it was built for.
///
/// The numbers keep the increments of the listing the tape follows; the time is read in
/// the zone the settings display data in. A cell that has no row (the table was asked
/// for a row the retained tape no longer holds) draws nothing.
pub(super) fn cell_text(row: Option<&MarketTradeRow>, column: usize, zone: UtcOffset) -> SmolStr {
    let Some(row) = row else {
        return SmolStr::default();
    };
    let display = row.display(zone);

    match column {
        0 => display.price.clone(),
        1 => display.quantity.clone(),
        2 => display.time.clone(),
        _ => SmolStr::default(),
    }
}

/// Builds one cell of the tape.
///
/// The kit table calls this without an application context, so what it builds is a
/// text node whose text and colour are read on the frames that draw it: the cell
/// registers its dependencies inside `bind_text` / `bind_style` rather than resolving
/// anything here.
///
/// Every cell draws in a theme foreground. The engine's `TextStyle` colour is `BLACK` by
/// default and the cell box's `foreground` does not inherit into the text node, so a
/// cell that bound no colour would be unreadable on a dark appearance. The price column
/// overrides it with the direction colour of the side that crossed the spread, because a
/// tape is read by which side printed; the shared table could not tint a whole row, so
/// the tint belongs to the cell.
pub(super) fn build_cell(
    tree: &mut NodeTree,
    parent: NodeId,
    row: usize,
    column: usize,
    rows: Signal<MarketTradeSnapshot>,
    config: Signal<Config>,
    theme: ThemeHandle,
    font_size: f32,
) {
    let text = NodeBuilder::text(SmolStr::default(), TextStyle::new(font_size));

    // The time is the one value read in the display time zone, which is a settings read:
    // the cell resolves it on the frames that draw it, so moving the setting redraws it.
    let text = if column == 2 {
        text.bind_text(move |read| {
            let zone = {
                let config = read.read(&config);
                data_time_zone(&config)
            };
            let snapshot = read.read(&rows);
            cell_text(displayed_row(&snapshot, row), column, zone)
        })
    } else {
        text.bind_text(move |read| {
            let snapshot = read.read(&rows);
            cell_text(displayed_row(&snapshot, row), column, UtcOffset::UTC)
        })
    };

    let text = text.bind_style(move |read| {
        let role = if column == 0 {
            let snapshot = read.read(&rows);
            displayed_row(&snapshot, row).map_or(Role::FgDefault, |row| {
                if row.trade().side == Side::Buy {
                    Role::MarketUp
                } else {
                    Role::MarketDown
                }
            })
        } else {
            Role::FgDefault
        };
        StylePatch {
            foreground: Some(theme.role(read, role)),
            ..StylePatch::default()
        }
    });
    let _ = text.build(tree, parent);
}

#[cfg(test)]
mod tests {
    use i18n::{language::codes::LanguageCode, translate::Translate};
    use market_participants_connector::catalog::test_support;
    use market_participants_model::{
        asset::{CurrencyId, CurrencyTerms, Iso4217Code},
        decimal::{Price, Quantity, Tick, UnixMillis},
        listing::{route::VenueListingKey, venue::PriceBasis},
        market::{
            book::ListingTrade,
            price::MarketPrice,
            units::{MarketQuantity, MarketTradeId, QuantityUnit},
        },
    };

    use super::*;
    use crate::model::MarketTradeState;

    /// The route and listing every fixture is built on.
    fn listing() -> VenueListingKey {
        test_support::listing_key(&test_support::spot_route(), "BTCUSDT")
    }

    fn product_translate(language: LanguageCode) -> Translate {
        Translate::new(
            std::rc::Rc::new(i18n::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    fn price(value: u64) -> MarketPrice {
        MarketPrice {
            value: Price::from(value),
            quote_asset: test_support::token("binance", "USDT"),
            basis: PriceBasis::PerAssetUnit,
        }
    }

    fn quantity(value: u64, unit: QuantityUnit) -> MarketQuantity {
        MarketQuantity {
            value: Quantity::from(value),
            unit,
        }
    }

    fn trade(id: u64, timestamp: UnixMillis, value: u64) -> ListingTrade {
        ListingTrade {
            listing: listing(),
            id: MarketTradeId::Numeric(id),
            price: price(value),
            quantity: quantity(
                2,
                QuantityUnit::AssetUnits {
                    asset: test_support::token("binance", "BTC"),
                },
            ),
            side: Side::Buy,
            timestamp,
        }
    }

    fn precision() -> market_participants_model::listing::rules::ListingPrecision {
        market_participants_model::listing::rules::ListingPrecision {
            price_tick: "0.01".parse::<Tick>().expect("a price tick should parse"),
            quantity_tick: "0.001"
                .parse::<Tick>()
                .expect("a quantity step should parse"),
        }
    }

    fn snapshot_of(trades: impl IntoIterator<Item = ListingTrade>) -> MarketTradeSnapshot {
        let mut state = MarketTradeState::new(8, precision());
        state.merge(trades);
        state.snapshot()
    }

    fn snapshot() -> MarketTradeSnapshot {
        snapshot_of([trade(1, 1, 100)])
    }

    /// The sequence a price tick of one cent and a quantity step of one thousandth
    /// write, so a label that keeps the listing's precision is told apart from the
    /// canonical form that drops trailing zeroes.
    #[test]
    fn a_tape_row_should_name_its_three_values() {
        let snapshot = snapshot();
        let row = displayed_row(&snapshot, 0);

        assert_eq!(
            [
                cell_text(row, 0, UtcOffset::UTC),
                cell_text(row, 1, UtcOffset::UTC),
                cell_text(row, 2, UtcOffset::UTC),
            ],
            [
                SmolStr::new_static("100.00"),
                SmolStr::new_static("2.000"),
                SmolStr::new_static("00:00:00.001"),
            ]
        );
        assert_eq!(cell_text(None, 0, UtcOffset::UTC), SmolStr::default());
    }

    /// The newest trade is displayed first, so display row 0 is the last chronologically.
    #[test]
    fn the_display_order_should_be_the_newest_trade_first() {
        let snapshot = snapshot_of([trade(1, 10, 100), trade(2, 20, 101)]);

        assert_eq!(
            displayed_row(&snapshot, 0).map(|row| row.trade().price.value),
            Some(Price::from(101_u64))
        );
        assert_eq!(
            displayed_row(&snapshot, 1).map(|row| row.trade().price.value),
            Some(Price::from(100_u64))
        );
        assert!(displayed_row(&snapshot, 2).is_none());
    }

    /// The column headers name the units the trades are measured in, which is what makes
    /// a price column readable on its own. The names are the display codes the published
    /// generation decided on, not the venue's own symbols.
    #[test]
    fn the_headers_should_name_the_units_of_the_followed_listing() {
        let fixture = test_support::Fixture::new(vec![test_support::entry(
            &test_support::spot_route(),
            test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
            "BTCUSDT",
        )]);
        let snapshot = snapshot();
        let translate = product_translate(LanguageCode::En);
        let units = units_of(&snapshot, fixture.catalog(), &translate);

        assert_eq!(
            headers(units.as_ref(), &translate),
            [
                SmolStr::new_static("Price (USDT)"),
                SmolStr::new_static("Amount (BTC)"),
                SmolStr::new_static("Time"),
            ]
        );
    }

    /// A quantity counted in contracts is labelled as the unit it actually carries, not
    /// as base volume.
    #[test]
    fn the_amount_header_should_name_the_unit_the_trades_carry() {
        let fixture = test_support::Fixture::new(vec![test_support::entry(
            &test_support::spot_route(),
            test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
            "BTCUSDT",
        )]);
        let mut contract = trade(1, 1, 100);
        contract.quantity = quantity(3, QuantityUnit::Contracts);
        let snapshot = snapshot_of([contract]);
        let translate = product_translate(LanguageCode::En);
        let units = units_of(&snapshot, fixture.catalog(), &translate);

        assert_eq!(
            headers(units.as_ref(), &translate)[1],
            SmolStr::new_static("Amount (Contracts)"),
            "the contracts label is the unit's own, not the base asset the tape would have \
             named if it assumed every listing is measured in it"
        );
    }

    /// A tape that has printed nothing keeps the generic headers: it has no unit to name.
    #[test]
    fn a_tape_without_trades_should_name_no_unit() {
        let translate = product_translate(LanguageCode::En);

        assert_eq!(
            headers(None, &translate),
            [
                SmolStr::new_static("Price"),
                SmolStr::new_static("Amount"),
                SmolStr::new_static("Time"),
            ]
        );
    }

    /// An asset the published generation does not name is the localized unknown: a
    /// reader cannot use the canonical identity, and a name invented here would disagree
    /// with every other pane about the same asset.
    #[test]
    fn an_unpublished_asset_should_be_named_as_unknown() {
        let fixture = test_support::Fixture::new(Vec::new());
        let display = fixture.catalog().cached_display();
        // A currency the fixture vocabulary never registers. The code is a valid ISO
        // 4217 code, so the only reason it has no name here is that this generation does
        // not carry the asset.
        let absent = AssetId::Currency(CurrencyId::of_terms(&CurrencyTerms {
            code: Iso4217Code::new("CHF").expect("CHF is canonical"),
        }));
        let translate = product_translate(LanguageCode::En);

        assert!(
            display.resolver().asset_code(&absent).is_err(),
            "the fixture generation does not register {absent}"
        );
        assert_eq!(
            asset_display(&display, &absent, &translate),
            SmolStr::new_static("Unknown asset")
        );
    }

    /// The table draws the three columns end-aligned on the declared weights and offers no
    /// row selection: the old dataset was a table to read, not a table to pick.
    #[test]
    fn the_table_should_keep_the_three_column_dataset_semantics() {
        let translate = product_translate(LanguageCode::En);
        let columns = columns(headers(None, &translate));

        assert_eq!(columns.len(), COLUMN_COUNT);
        assert_eq!(
            columns
                .iter()
                .map(|column| column.align)
                .collect::<Vec<_>>(),
            [ColumnAlign::End, ColumnAlign::End, ColumnAlign::End]
        );
        assert!(
            columns.iter().all(|column| !column.sortable),
            "the old dataset had no sorting"
        );

        let options = table_options(24.0);
        assert_eq!(options.selection, SelectionMode::None);
    }
}

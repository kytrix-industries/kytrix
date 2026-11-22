//! The two tables a watch list draws: their columns, their cells and the slots
//! their per-row controls are built into.
//!
//! # The tables are built once and kept up to date
//!
//! [`TablePane::build`] builds a kit table once, in the widget's body. The rows
//! reach it through [`Table::set_rows`] — which rebuilds the mounted row window and
//! keeps the scroll offset — and every dynamic value in a row is a `bind_text` /
//! `bind_style` closure over the signals the pane owns: the accepted rows, the
//! published catalogue, the language and the theme.
//!
//! The kit table's row factory runs **without an application context** (AGENTS §9),
//! so a cell cannot read the settings, the theme or a kit component while it is
//! built. What a cell needs therefore arrives as signals (rows / catalogue revision /
//! language) or as the captured theme handle, and the only things the factory does
//! that need an `App` are the two structural decisions:
//!
//! - a row waiting for its market spans the columns its values would have taken —
//!   the pane sets the per-row span callback from its own mirror of the rows;
//! - a row's controls are kit buttons, so the factory builds an empty slot node and
//!   the widget fills it from [`WidgetView::sync`](widget::view::WidgetView::sync)
//!   (the same split `solana_wallet` uses for its list rows).
//!
//! # Every value is drawn with the unit it came with
//!
//! A listing publishes the traded quantity of its session in one unit and, when it
//! publishes one at all, a traded notional in an asset. Both are the listing's own,
//! so both are named where the row is drawn, and a value the listing does not publish
//! is drawn as missing rather than as zero or as the other unit.

use std::{borrow::Cow, cell::RefCell, collections::BTreeMap, rc::Rc, sync::Arc};

use ahash::HashMap;
use base_ui::{
    kit::{
        list::SelectionMode,
        markdown::{parse::parse_document, source::MarkdownAssets},
        table::{Column, ColumnAlign, Table, TableOptions},
    },
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use market_participants_connector::catalog::{display::catalog::DisplaySnapshot, store::Catalog};
use market_participants_model::{
    asset::AssetId,
    market::units::{MarketQuantity, QuantityUnit},
};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::style::StylePatch,
    text::TextStyle,
};
use widget::{
    cx::WidgetCx,
    error::{ListingErrorPresentation, TranslatedError},
    venue::{VenueDisplay, route_display_string},
};

use super::model::{
    PriceDirection, WatchListMarket, WatchListRow, WatchListRowState, WatchListStatus,
    format_volume,
};

/// The number of columns a table has: route, instrument, price, the four statistics
/// the market's heading names, and the row's own controls.
pub(super) const COLUMN_COUNT: usize = 8;

/// The width weight of each column, in header order.
///
/// The kit table distributes the remaining width by these weights. They are the old
/// dataset's shares, with the controls column widened from `0.05` to `0.06`: a kit
/// ghost button carries a padding the old icon button did not, and two of them do not
/// fit in five percent of a narrow pane.
const COLUMN_WEIGHTS: [f32; COLUMN_COUNT] = [0.09, 0.18, 0.12, 0.12, 0.12, 0.155, 0.155, 0.06];

/// How each column is aligned, in column order: words on the left, numbers on the right.
const COLUMN_ALIGNS: [ColumnAlign; COLUMN_COUNT] = [
    ColumnAlign::Start,
    ColumnAlign::Start,
    ColumnAlign::End,
    ColumnAlign::End,
    ColumnAlign::End,
    ColumnAlign::End,
    ColumnAlign::End,
    ColumnAlign::End,
];

/// The column a row waiting for its market begins its one cell in.
///
/// Everything between the route and the column of controls of such a row is one cell:
/// the name of the listing, the price and the statistics of its table. The route and
/// the controls stay where they are, because the first is not a value of the market
/// behind the row — it names the scope the user picked — and the second is the user's
/// own: a listing whose market was never loaded is exactly the one they may want to
/// take back out, and a button is not a value of the market either.
pub(super) const MISSING_MARKET_COLUMN: usize = 1;

/// The column a row's own controls are drawn in.
pub(super) const ACTIONS_COLUMN: usize = COLUMN_COUNT - 1;

/// What a column says when the listing publishes no value for it.
pub(super) const MISSING_VALUE: &str = "—";

/// The rows one table draws, as the cells read them.
///
/// Equality is **identity**, not content: the pane publishes a fresh value exactly
/// when what the rows draw changed, and the write that publishes it is what makes the
/// cells that read it re-evaluate. Comparing the rows themselves would walk every
/// row on every write for no gain.
#[derive(Clone, Debug)]
pub(super) struct TableRows(Arc<[Arc<WatchListRow>]>);

impl TableRows {
    pub(super) fn empty() -> Self {
        Self(Arc::from(Vec::new()))
    }

    pub(super) fn new(rows: Arc<[Arc<WatchListRow>]>) -> Self {
        Self(rows)
    }

    pub(super) fn len(&self) -> usize {
        self.0.len()
    }

    pub(super) fn row(&self, index: usize) -> Option<&Arc<WatchListRow>> {
        self.0.get(index)
    }

    pub(super) fn as_slice(&self) -> &[Arc<WatchListRow>] {
        &self.0
    }
}

impl PartialEq for TableRows {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for TableRows {}

/// The nodes the per-row controls are built into, by row index.
///
/// The row factory builds the slot (it owns the row's cells); the widget builds the
/// kit buttons into it from `sync`, where it has an application context. The map is
/// shared between the two, and is what tells the widget which rows are mounted.
pub(super) type ActionSlots = Rc<RefCell<BTreeMap<usize, NodeId>>>;

/// The signals and handles both tables of one pane read.
///
/// The catalogue is a mirror rather than a signal because the generation it publishes
/// is not `PartialEq`; [`TableSignals::catalog_revision`] is the signal that says it
/// moved.
#[derive(Clone)]
pub(super) struct TableSignals {
    pub catalog: Rc<RefCell<Arc<Catalog>>>,
    pub catalog_revision: Signal<u64>,
    pub language: Signal<LanguageCode>,
    pub translate: Translate,
}

/// What one cell build reads that the row factory cannot resolve itself.
struct CellSink {
    table_key: SmolStr,
    rows: Signal<TableRows>,
    row_mirror: Rc<RefCell<TableRows>>,
    catalog: Rc<RefCell<Arc<Catalog>>>,
    catalog_revision: Signal<u64>,
    language: Signal<LanguageCode>,
    translate: Translate,
    theme: ThemeHandle,
    font_size: f32,
    small_font_size: f32,
    slots: ActionSlots,
}

impl CellSink {
    /// Builds one cell, logging the one failure a cell build can report.
    fn build(&self, tree: &mut NodeTree, parent: NodeId, row: usize, column: usize) {
        if let Err(error) = self.build_cell(tree, parent, row, column) {
            log::error!("a watch-list cell could not be built: {error}");
        }
    }

    fn build_cell(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
        column: usize,
    ) -> Result<(), EngineError> {
        // What the row is *doing* is a structural fact read off the pane's mirror:
        // the factory has no signals to read, and a row whose market is missing draws
        // one cell where its values would be.
        if self.is_awaiting_market(row) && column == MISSING_MARKET_COLUMN {
            return self.build_missing_market(tree, parent, row);
        }
        match column {
            0 => self.build_route(tree, parent, row),
            1 => self.build_instrument(tree, parent, row),
            2 => self.build_price(tree, parent, row),
            3 => self.build_number(tree, parent, row, NumberColumn::Change),
            4 => self.build_number(tree, parent, row, NumberColumn::ChangePercent),
            5 => self.build_traded(tree, parent, row, TradedColumn::Quantity),
            6 => self.build_traded(tree, parent, row, TradedColumn::Notional),
            ACTIONS_COLUMN => self.build_actions_slot(tree, parent, row),
            _ => Ok(()),
        }
    }

    fn is_awaiting_market(&self, row: usize) -> bool {
        self.row_mirror
            .borrow()
            .row(row)
            .is_some_and(|row| matches!(row.state(), WatchListRowState::AwaitingMarket))
    }

    /// The whole user-visible route of the row.
    ///
    /// The watch list is the one surface that follows listings of several routes, so
    /// this states the whole route and not the brand: two rows can carry the same
    /// name, and `Binance/SPOT` is what tells them apart.
    fn build_route(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
    ) -> Result<(), EngineError> {
        let rows = self.rows;
        let language = self.language;
        let translate = self.translate.clone();
        let theme = self.theme.clone();
        NodeBuilder::text(SmolStr::default(), TextStyle::new(self.font_size))
            .bind_text(move |read| {
                let _language = *read.read(&language);
                let rows = read.read(&rows);
                rows.row(row).map_or_else(SmolStr::default, |row| {
                    route_display_string(&row.key().route, &translate)
                })
            })
            .bind_style(move |read| StylePatch {
                foreground: Some(theme.role(read, Role::FgDefault)),
                ..StylePatch::default()
            })
            .build(tree, parent)?;
        Ok(())
    }

    /// The listing and, beside it when it has something to say, what its stream is
    /// doing.
    ///
    /// A row that is reconnecting keeps showing the prices it has, so the status is
    /// only shown next to a ticker; a failed row says so whether or not it still has
    /// one. A row whose market is not loaded never reaches here: what it draws stands
    /// in for this column as well.
    fn build_instrument(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
    ) -> Result<(), EngineError> {
        let cell = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .width_full()
                    .min_width_0()
                    .build(),
            )
            .build(tree, parent)?;

        let rows = self.rows;
        let catalog = Rc::clone(&self.catalog);
        let revision = self.catalog_revision;
        let language = self.language;
        let translate = self.translate.clone();
        let theme = self.theme.clone();
        NodeBuilder::text(SmolStr::default(), TextStyle::new(self.font_size))
            .bind_text(move |read| {
                let _language = *read.read(&language);
                let _catalog = *read.read(&revision);
                let catalog = Arc::clone(&catalog.borrow());
                let rows = read.read(&rows);
                rows.row(row).map_or_else(SmolStr::default, |row| {
                    listing_label(&catalog, row, &translate)
                })
            })
            .bind_style(move |read| StylePatch {
                foreground: Some(theme.role(read, Role::FgDefault)),
                ..StylePatch::default()
            })
            .build(tree, cell)?;

        let rows = self.rows;
        let language = self.language;
        let translate = self.translate.clone();
        let theme = self.theme.clone();
        NodeBuilder::text(SmolStr::default(), TextStyle::new(self.small_font_size))
            .bind_text(move |read| {
                let _language = *read.read(&language);
                let rows = read.read(&rows);
                rows.row(row)
                    .map_or_else(SmolStr::default, |row| status_text(row, &translate))
            })
            .bind_style(move |read| {
                let rows = read.read(&rows);
                let visible = rows.row(row).is_some_and(|row| status_visible(row));
                let role = status_role(rows.row(row));
                StylePatch {
                    foreground: Some(theme.role(read, role)),
                    visible: Some(visible),
                    ..StylePatch::default()
                }
            })
            .build(tree, cell)?;
        Ok(())
    }

    /// The price, written with the asset it is quoted in and coloured by its
    /// direction.
    fn build_price(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
    ) -> Result<(), EngineError> {
        let rows = self.rows;
        let catalog = Rc::clone(&self.catalog);
        let revision = self.catalog_revision;
        let language = self.language;
        let translate = self.translate.clone();
        let theme = self.theme.clone();
        NodeBuilder::text(SmolStr::default(), TextStyle::new(self.font_size))
            .bind_text(move |read| {
                let _language = *read.read(&language);
                let _catalog = *read.read(&revision);
                let catalog = Arc::clone(&catalog.borrow());
                let rows = read.read(&rows);
                rows.row(row).map_or_else(SmolStr::default, |row| {
                    price_text(row, &catalog, &translate)
                })
            })
            .bind_style(move |read| {
                let rows = read.read(&rows);
                let role = rows.row(row).map_or(Role::FgMuted, |row| price_role(row));
                StylePatch {
                    foreground: Some(theme.role(read, role)),
                    ..StylePatch::default()
                }
            })
            .build(tree, parent)?;
        Ok(())
    }

    /// One number of a row, coloured by the direction it moved in when it describes a
    /// price.
    fn build_number(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
        column: NumberColumn,
    ) -> Result<(), EngineError> {
        let rows = self.rows;
        let language = self.language;
        let theme = self.theme.clone();
        NodeBuilder::text(SmolStr::default(), TextStyle::new(self.font_size))
            .bind_text(move |read| {
                let _language = *read.read(&language);
                let rows = read.read(&rows);
                rows.row(row)
                    .map_or_else(SmolStr::default, |row| number_text(row, column))
            })
            .bind_style(move |read| {
                let rows = read.read(&rows);
                let role = rows
                    .row(row)
                    .map_or(Role::FgDefault, |row| number_role(row, column));
                StylePatch {
                    foreground: Some(theme.role(read, role)),
                    ..StylePatch::default()
                }
            })
            .build(tree, parent)?;
        Ok(())
    }

    /// A traded value and the unit it is counted in.
    fn build_traded(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
        column: TradedColumn,
    ) -> Result<(), EngineError> {
        let rows = self.rows;
        let catalog = Rc::clone(&self.catalog);
        let revision = self.catalog_revision;
        let language = self.language;
        let translate = self.translate.clone();
        let theme = self.theme.clone();
        NodeBuilder::text(SmolStr::default(), TextStyle::new(self.font_size))
            .bind_text(move |read| {
                let _language = *read.read(&language);
                let _catalog = *read.read(&revision);
                let catalog = Arc::clone(&catalog.borrow());
                let rows = read.read(&rows);
                rows.row(row).map_or_else(SmolStr::default, |row| {
                    traded_text(row, column, &catalog, &translate)
                })
            })
            .bind_style(move |read| StylePatch {
                foreground: Some(theme.role(read, Role::FgDefault)),
                ..StylePatch::default()
            })
            .build(tree, parent)?;
        Ok(())
    }

    /// The one thing a row says while the market participant behind it is not loaded.
    ///
    /// The sentence is the catalogue's own: it names the market participant and asks
    /// for its metadata. It is drawn as the text the catalogue writes rather than as
    /// its source — the markup belongs to the sentence, and a cell that drew the
    /// source would show a reader the punctuation. The merged cell is centred in the
    /// room the values would have taken, which is the row's own alignment rather than
    /// any single column's.
    fn build_missing_market(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
    ) -> Result<(), EngineError> {
        let Some(venue) = self
            .row_mirror
            .borrow()
            .row(row)
            .map(|row| row.key().route.venue)
        else {
            return Ok(());
        };

        let cell = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .width_full()
                    .min_width_0()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .build(),
            )
            .build(tree, parent)?;
        let translate = self.translate.clone();
        let language = self.language;
        let rows = self.rows;
        let cache: Rc<RefCell<HashMap<LanguageCode, SmolStr>>> =
            Rc::new(RefCell::new(HashMap::default()));
        let text_cache = Rc::clone(&cache);
        let text_translate = translate.clone();
        let theme = self.theme.clone();
        NodeBuilder::text(
            market_not_loaded_text(&translate, venue),
            TextStyle::new(self.font_size),
        )
        .bind_text(move |read| {
            let language = *read.read(&language);
            let _rows = read.read(&rows);
            let mut cache = text_cache.borrow_mut();
            cache
                .entry(language)
                .or_insert_with(|| market_not_loaded_text(&text_translate, venue))
                .clone()
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(theme.role(read, Role::FgMuted)),
            ..StylePatch::default()
        })
        .build(tree, cell)?;
        Ok(())
    }

    /// The empty node the row's controls are built into from the widget's `sync`.
    fn build_actions_slot(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
    ) -> Result<(), EngineError> {
        let slot = NodeBuilder::boxed()
            .id(format_smolstr!("{}:actions:{row}", self.table_key))
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap(2.0)
                    .build(),
            )
            .build(tree, parent)?;
        self.slots.borrow_mut().insert(row, slot);
        Ok(())
    }
}

/// The span a row's cell takes.
///
/// A row waiting for its market says one thing across the columns its values would
/// have taken; every other row draws one cell per column. The route and the controls
/// keep their own columns in both cases.
pub(super) fn column_span(rows: &TableRows, row: usize, column: usize) -> usize {
    let awaiting_market = rows
        .row(row)
        .is_some_and(|row| matches!(row.state(), WatchListRowState::AwaitingMarket));
    if awaiting_market && column == MISSING_MARKET_COLUMN {
        COLUMN_COUNT - MISSING_MARKET_COLUMN - 1
    } else {
        1
    }
}

/// A number of the row a `bind_text` cell draws.
#[derive(Clone, Copy)]
enum NumberColumn {
    Change,
    ChangePercent,
}

/// A traded column of the row a `bind_text` cell draws.
#[derive(Clone, Copy)]
enum TradedColumn {
    Quantity,
    Notional,
}

fn number_text(row: &WatchListRow, column: NumberColumn) -> SmolStr {
    let Some(display) = row.display() else {
        return SmolStr::new_static(MISSING_VALUE);
    };
    match column {
        NumberColumn::Change => display.change.clone(),
        NumberColumn::ChangePercent => display.change_percent.clone(),
    }
}

fn number_role(row: &WatchListRow, column: NumberColumn) -> Role {
    match (row.display(), column) {
        (Some(display), NumberColumn::Change | NumberColumn::ChangePercent) => {
            direction_role(display.direction)
        }
        _ => Role::FgDefault,
    }
}

fn direction_role(direction: PriceDirection) -> Role {
    match direction {
        PriceDirection::Up => Role::MarketUp,
        PriceDirection::Flat => Role::MarketFlat,
        PriceDirection::Down => Role::MarketDown,
    }
}

/// The price of a row, written with the asset it is quoted in.
///
/// Before the first ticker the row says what it is waiting for instead of showing a
/// number it does not have.
fn price_text(row: &WatchListRow, catalog: &Catalog, translate: &Translate) -> SmolStr {
    let Some(display) = row.display() else {
        return pending_text(row.status(), translate);
    };
    let generation = catalog.cached_display();
    format_smolstr!(
        "{} {}",
        display.price,
        asset_display(&generation, &display.quote_asset, translate)
    )
}

fn price_role(row: &WatchListRow) -> Role {
    match row.display() {
        Some(display) => direction_role(display.direction),
        None => Role::FgMuted,
    }
}

/// A traded value and the unit it is counted in.
///
/// A listing that publishes no traded quantity — or no traded notional — has nothing
/// for that column: it says the value is missing instead of falling back to the other
/// unit or to zero.
fn traded_text(
    row: &WatchListRow,
    column: TradedColumn,
    catalog: &Catalog,
    translate: &Translate,
) -> SmolStr {
    let Some(display) = row.display() else {
        return SmolStr::new_static(MISSING_VALUE);
    };
    let quantity: Option<&MarketQuantity> = match column {
        TradedColumn::Quantity => display.traded_quantity.as_ref(),
        TradedColumn::Notional => display.traded_notional.as_ref(),
    };
    let Some(quantity) = quantity else {
        return SmolStr::new_static(MISSING_VALUE);
    };
    let generation = catalog.cached_display();
    format_smolstr!(
        "{} {}",
        format_volume(quantity.value),
        unit_label(&generation, &quantity.unit, translate)
    )
}

/// The display code of one asset, as the captured generation published it.
///
/// Only the display catalogue knows what an asset is called: the canonical identity
/// of a token is a digest, and the code the catalogue publishes is the whole name of
/// the asset. An asset the captured generation does not hold is the localized unknown
/// — never an identity digest, and never a name invented here, which would disagree
/// with every other pane about the same asset.
fn asset_display(display: &DisplaySnapshot, asset: &AssetId, translate: &Translate) -> SmolStr {
    display.resolver().asset_code(asset).map_or_else(
        |_| t_smol!(translate, "error.display.unknown_asset"),
        |code| SmolStr::new(code.as_str()),
    )
}

/// The label of one quantity unit, named the way that unit is measured.
///
/// The three traded units read differently — asset units are counted in the asset
/// they are denominated in, contracts are counted in whole contracts, and outcome
/// tokens in the shares of one outcome — so none of them may be labelled as another.
fn unit_label(display: &DisplaySnapshot, unit: &QuantityUnit, translate: &Translate) -> SmolStr {
    match unit {
        QuantityUnit::AssetUnits { asset } | QuantityUnit::Notional { asset } => {
            asset_display(display, asset, translate)
        }
        QuantityUnit::Contracts => t_smol!(translate, "widget.watch_list.contracts"),
        QuantityUnit::OutcomeTokens { .. } => {
            t_smol!(translate, "widget.watch_list.outcome_tokens")
        }
    }
}

/// The name one listing is drawn and spoken about by.
///
/// The published name is the whole label: nothing is prefixed to it and nothing is
/// assembled beside it. A listing the published generation does not name is the
/// localized unknown, so a row can never be labelled with another listing's symbol
/// and never with an identity digest. The route a row follows is stated in its own
/// column.
fn listing_label(catalog: &Catalog, row: &WatchListRow, translate: &Translate) -> SmolStr {
    ListingErrorPresentation::of(catalog.cached_listing_instrument(row.key()))
        .translated_message(translate)
}

/// What a row says while it has nothing to show yet.
fn pending_text(status: WatchListStatus, translate: &Translate) -> SmolStr {
    match status {
        WatchListStatus::Connecting => t_smol!(translate, "common.connecting"),
        WatchListStatus::LoadingSnapshot => {
            t_smol!(translate, "widget.watch_list.loading_snapshot")
        }
        WatchListStatus::Error => t_smol!(translate, "widget.watch_list.ticker_unavailable"),
        // A row is only ever ready with a ticker, which has labels of its own.
        WatchListStatus::Ready => SmolStr::new_static(MISSING_VALUE),
    }
}

/// The text drawn beside the listing when its stream has something to say beside it.
fn status_text(row: &WatchListRow, translate: &Translate) -> SmolStr {
    if status_visible(row) {
        pending_text(row.status(), translate)
    } else {
        SmolStr::default()
    }
}

/// Whether a row's status is drawn beside its name.
fn status_visible(row: &WatchListRow) -> bool {
    match row.status() {
        WatchListStatus::Error => true,
        WatchListStatus::Connecting | WatchListStatus::LoadingSnapshot => row.has_ticker(),
        WatchListStatus::Ready => false,
    }
}

/// The colour a row's status is drawn in.
fn status_role(row: Option<&Arc<WatchListRow>>) -> Role {
    match row.map(|row| row.status()) {
        Some(WatchListStatus::Error) => Role::StatusDangerBase,
        _ => Role::FgMuted,
    }
}

/// A market participant answers nothing while its resources are absent; the parser
/// only needs bytes for an image written without a size, and this sentence writes
/// one. The logo itself is not drawn by a table cell — the cell states the words.
struct TableMarkdownAssets;

impl MarkdownAssets for TableMarkdownAssets {
    fn load(&self, _path: &str) -> Option<Cow<'static, [u8]>> {
        None
    }
}

/// The sentence a row waiting for its market is drawn with, as plain text.
///
/// The catalogue sentence is rich text: it writes the market participant's logo as
/// an inline image and its name in bold. A kit table cell is a text cell rather than
/// a markdown component, so the sentence is read as the words it says — the markup is
/// removed by parsing the catalogue value and reading it back as plain text. The
/// logo and the emphasis are the two things a cell does not draw; the words, the
/// order and the request are the catalogue's.
fn market_not_loaded_text(
    translate: &Translate,
    venue: market_participants_model::identifier::VenueId,
) -> SmolStr {
    let source = t_smol!(
        translate,
        "widget.watch_list.market_metadata_not_loaded",
        logo = venue.asset_path(),
        market = venue.display_string(translate)
    );
    parse_document(&source, &TableMarkdownAssets).plain_text()
}

/// The columns the table declares, in header order.
///
/// Built when the table is built, because the kit table takes its headers as resolved
/// text; a language switch rebuilds the table.
pub(super) fn columns(market: WatchListMarket, translate: &Translate) -> Vec<Column> {
    let [change, change_percent, traded_quantity, traded_notional] = market.column_keys();
    let headers = [
        t_smol!(translate, "common.route"),
        t_smol!(translate, "common.instrument"),
        t_smol!(translate, "common.price"),
        t_smol!(translate, change),
        t_smol!(translate, change_percent),
        t_smol!(translate, traded_quantity),
        t_smol!(translate, traded_notional),
        // The last column is the row's own controls, which carry no label: nothing is
        // written over a column whose content is a button.
        SmolStr::default(),
    ];
    headers
        .into_iter()
        .zip(COLUMN_WEIGHTS)
        .zip(COLUMN_ALIGNS)
        .map(|((header, weight), align)| Column::flex(header, weight).align(align))
        .collect()
}

/// The options the watch-list tables need from the kit table: the control row height
/// and **no row selection**.
///
/// The old dataset drew rows to read, not rows to pick: it had no selection state and
/// no click handling. The kit table defaults to multi-select, so the pane turns
/// selection off explicitly here.
pub(super) fn table_options(row_height: f32) -> TableOptions {
    TableOptions::fixed(row_height).selection(SelectionMode::None)
}

/// The handles one table reads through, minted before the table itself exists.
///
/// The body builds its two frames before it builds the tables (the frames' visibility
/// closures read the rows signals), so the signals and the per-row slots are minted
/// first and handed to both halves.
#[derive(Clone)]
pub(super) struct TableHandle {
    pub rows: Signal<TableRows>,
    pub mirror: Rc<RefCell<TableRows>>,
    pub slots: ActionSlots,
}

impl TableHandle {
    /// Mints the handles for one table in the widget's scope.
    pub(super) fn new(cx: &mut WidgetCx<'_>) -> Self {
        Self {
            rows: cx.signal(TableRows::empty()),
            mirror: Rc::new(RefCell::new(TableRows::empty())),
            slots: Rc::new(RefCell::new(BTreeMap::new())),
        }
    }
}

/// One of the two tables, with the state the widget needs to keep it up to date.
pub(super) struct TablePane {
    pub table: Table,
    /// The node the table's outer box was built into.
    pub node: NodeId,
    /// The rows the pane last published to this table.
    pub rows: TableRows,
    /// The signal the cells read their rows from.
    row_signal: Signal<TableRows>,
    /// The mirror the row factory and the span callback read.
    pub row_mirror: Rc<RefCell<TableRows>>,
    /// The nodes the per-row controls are built into, by row index.
    pub slots: ActionSlots,
}

impl TablePane {
    /// Builds a table of one market into `parent` and mounts it.
    pub(super) fn build(
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        market: WatchListMarket,
        key_suffix: &str,
        signals: TableSignals,
        handle: TableHandle,
    ) -> Result<Self, EngineError> {
        let theme = cx.theme().clone();
        let font_size = theme.tokens().medium.font_size;
        let small_font_size = theme.tokens().small.font_size;
        let row_height = theme.tokens().medium.height;
        let key = cx.unique_key(key_suffix);
        let rows = handle.rows;
        let row_mirror = Rc::clone(&handle.mirror);
        let slots = Rc::clone(&handle.slots);
        let sink = CellSink {
            table_key: key.clone(),
            rows,
            row_mirror: Rc::clone(&row_mirror),
            catalog: Rc::clone(&signals.catalog),
            catalog_revision: signals.catalog_revision,
            language: signals.language,
            translate: signals.translate.clone(),
            theme: theme.clone(),
            font_size,
            small_font_size,
            slots: Rc::clone(&slots),
        };
        let cells = move |tree: &mut NodeTree, parent: NodeId, row: usize, column: usize| {
            sink.build(tree, parent, row, column);
        };
        let column_span = {
            let row_mirror = Rc::clone(&row_mirror);
            move |row: usize, column: usize| column_span(&row_mirror.borrow(), row, column)
        };
        let table = Table::new(
            cx.app_mut(),
            theme,
            key,
            columns(market, &signals.translate),
            table_options(row_height),
            cells,
            t_smol!(signals.translate, "common.no_data"),
        );
        table.set_column_span(column_span);
        table.set_initial_rows(cx.app_mut(), 0);
        let window = cx.window();
        let node = table.mount(cx.app_mut(), window, parent)?;
        Ok(Self {
            table,
            node,
            rows: TableRows::empty(),
            row_signal: rows,
            row_mirror,
            slots,
        })
    }

    /// Publishes a new row set: the mirror the structure is read from, the signal the
    /// cells read, and — when the row window itself has to be rebuilt — the mounted
    /// rows.
    pub(super) fn publish(
        &mut self,
        cx: &mut WidgetCx<'_>,
        rows: TableRows,
        rebuild: bool,
    ) -> Result<(), EngineError> {
        *self.row_mirror.borrow_mut() = rows.clone();
        cx.write(&self.row_signal, rows.clone());
        self.rows = rows;
        if rebuild {
            self.table.set_rows(cx.app_mut(), self.rows.len())?;
        }
        Ok(())
    }

    /// The rows the table currently draws.
    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }

    /// Releases the component (its handlers and signal scopes) and clears the slots.
    pub(super) fn release(&mut self, app: &mut App) -> usize {
        let released = self.table.release(app);
        self.slots.borrow_mut().clear();
        released
    }
}

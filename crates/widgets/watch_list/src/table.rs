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

pub(super) const COLUMN_COUNT: usize = 8;

const COLUMN_WEIGHTS: [f32; COLUMN_COUNT] = [0.09, 0.18, 0.12, 0.12, 0.12, 0.155, 0.155, 0.06];

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

pub(super) const MISSING_MARKET_COLUMN: usize = 1;

pub(super) const ACTIONS_COLUMN: usize = COLUMN_COUNT - 1;

pub(super) const MISSING_VALUE: &str = "—";

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

pub(super) type ActionSlots = Rc<RefCell<BTreeMap<usize, NodeId>>>;

#[derive(Clone)]
pub(super) struct TableSignals {
    pub catalog: Rc<RefCell<Arc<Catalog>>>,
    pub catalog_revision: Signal<u64>,
    pub language: Signal<LanguageCode>,
    pub translate: Translate,
}

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

#[derive(Clone, Copy)]
enum NumberColumn {
    Change,
    ChangePercent,
}

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

fn asset_display(display: &DisplaySnapshot, asset: &AssetId, translate: &Translate) -> SmolStr {
    display.resolver().asset_code(asset).map_or_else(
        |_| t_smol!(translate, "error.display.unknown_asset"),
        |code| SmolStr::new(code.as_str()),
    )
}

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

fn listing_label(catalog: &Catalog, row: &WatchListRow, translate: &Translate) -> SmolStr {
    ListingErrorPresentation::of(catalog.cached_listing_instrument(row.key()))
        .translated_message(translate)
}

fn pending_text(status: WatchListStatus, translate: &Translate) -> SmolStr {
    match status {
        WatchListStatus::Connecting => t_smol!(translate, "common.connecting"),
        WatchListStatus::LoadingSnapshot => {
            t_smol!(translate, "widget.watch_list.loading_snapshot")
        }
        WatchListStatus::Error => t_smol!(translate, "widget.watch_list.ticker_unavailable"),
        WatchListStatus::Ready => SmolStr::new_static(MISSING_VALUE),
    }
}

fn status_text(row: &WatchListRow, translate: &Translate) -> SmolStr {
    if status_visible(row) {
        pending_text(row.status(), translate)
    } else {
        SmolStr::default()
    }
}

fn status_visible(row: &WatchListRow) -> bool {
    match row.status() {
        WatchListStatus::Error => true,
        WatchListStatus::Connecting | WatchListStatus::LoadingSnapshot => row.has_ticker(),
        WatchListStatus::Ready => false,
    }
}

fn status_role(row: Option<&Arc<WatchListRow>>) -> Role {
    match row.map(|row| row.status()) {
        Some(WatchListStatus::Error) => Role::StatusDangerBase,
        _ => Role::FgMuted,
    }
}

struct TableMarkdownAssets;

impl MarkdownAssets for TableMarkdownAssets {
    fn load(&self, _path: &str) -> Option<Cow<'static, [u8]>> {
        None
    }
}

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
        SmolStr::default(),
    ];
    headers
        .into_iter()
        .zip(COLUMN_WEIGHTS)
        .zip(COLUMN_ALIGNS)
        .map(|((header, weight), align)| Column::flex(header, weight).align(align))
        .collect()
}

pub(super) fn table_options(row_height: f32) -> TableOptions {
    TableOptions::fixed(row_height).selection(SelectionMode::None)
}

#[derive(Clone)]
pub(super) struct TableHandle {
    pub rows: Signal<TableRows>,
    pub mirror: Rc<RefCell<TableRows>>,
    pub slots: ActionSlots,
}

impl TableHandle {
    pub(super) fn new(cx: &mut WidgetCx<'_>) -> Self {
        Self {
            rows: cx.signal(TableRows::empty()),
            mirror: Rc::new(RefCell::new(TableRows::empty())),
            slots: Rc::new(RefCell::new(BTreeMap::new())),
        }
    }
}

pub(super) struct TablePane {
    pub table: Table,
    pub node: NodeId,
    pub rows: TableRows,
    row_signal: Signal<TableRows>,
    pub row_mirror: Rc<RefCell<TableRows>>,
    pub slots: ActionSlots,
}

impl TablePane {
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

    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(super) fn release(&mut self, app: &mut App) -> usize {
        let released = self.table.release(app);
        self.slots.borrow_mut().clear();
        released
    }
}

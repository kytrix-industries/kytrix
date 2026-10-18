//! The watch-list widget.
//!
//! The widget follows a set of listings the user picked, one stream each, and
//! draws one row per listing. Everything the widget *does* around a frame —
//! connecting, loading, failing — is reported through the framework's status
//! handle; the body only ever draws the listings it follows.
//!
//! # The followed set is applied before the service confirms it
//!
//! Adding or removing a listing changes the widget's own list immediately, and
//! then asks the service for the new set. The user sees the row they asked for
//! without waiting for a round trip, and a request the service refuses is undone
//! by [`PendingOperation`], which remembers what the operation replaced. The
//! service only changes the streams it follows once the caller has the response,
//! so the two sides cannot settle on different sets without one of them failing —
//! and a failure is what the recovery re-synchronizes.
//!
//! # Rows are the widget's own, and the table is where they are drawn
//!
//! The rows live in a child entity — the table — because [`Table`] reads its rows
//! from an [`Entity`], and because that is the boundary the widget notifies when
//! what the table draws changes. The pane caches the body, so nothing else would
//! repaint it.
//!
//! # Every value is drawn with the unit it came with
//!
//! A listing publishes the traded quantity of its session in one unit and, when
//! it publishes one at all, a traded notional in an asset. Both are the listing's
//! own, so both are named where the row is drawn, and a value the listing does not
//! publish is drawn as missing rather than as zero or as the other unit.

use std::sync::Arc;

use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    button::Button,
    empty_state::EmptyState,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    markdown::view::{Markdown, MarkdownState},
    size::UnifiedSizable,
    style::theme::ThemeExt,
    table::{Table, TableDataSet},
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, ElementId, Empty, Entity, Hsla, InteractiveElement,
    IntoElement, Length, ParentElement, Render, RenderOnce, SharedString,
    StatefulInteractiveElement, Styled, WeakEntity, Window,
    base::{Align, Disableable},
    div,
    prelude::FluentBuilder,
    px, relative,
};
use i18n::t;
use instrument_selector_ui::instrument_selector::{
    InstrumentFilters, InstrumentKindFilter, InstrumentSelector, InstrumentStatusFilter,
    LoadedScope, offers_route,
};
use market_participants_connector::{
    catalog::{display::catalog::DisplaySnapshot, error::CatalogError},
    connector::MarketParticipantsConnector,
    venue::loaded::VenuesUpdated,
};
use market_participants_model::{
    asset::AssetId,
    identifier::VenueId,
    listing::route::VenueListingKey,
    market::{
        selection::InstrumentSelection,
        units::{MarketQuantity, QuantityUnit},
    },
    routing::CapabilityId,
};
use serde::{Deserialize, Deserializer, de};
use smol_str::format_smolstr;
use widget::{
    binding::{WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    error::{ListingErrorPresentation, LocalizedError, market_is_catalog_not_loaded},
    identity::{WidgetId, WidgetInstanceId},
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    venue::{VenueDisplay, route_display_string},
    view::{WidgetHeader, WidgetView},
};
use widget_tokio_runtime::global;

use super::{
    model::{
        MAX_WATCH_LIST_ENTRIES, PriceDirection, WatchListEntry, WatchListMarket, WatchListRow,
        WatchListRowDisplay, WatchListRowState, WatchListStatus, WatchListStreamError,
        WatchListStreamEvent, format_volume, sort_entries, split_rows,
    },
    service::{
        TickerSource, WatchListProtocol, WatchListRequest, WatchListResponse, WatchListService,
        WatchListServiceError, validate_replacement,
    },
};

const COLUMN_COUNT: usize = 8;
/// Column widths in header order, as a share of the pane.
///
/// The shared table draws every column at its declared width, so the columns are
/// sized by share of the pane rather than by a pixel width that would overflow a
/// narrow one. The shares add up to the whole pane, like every other table's: a row whose
/// columns asked for less than the pane would leave its last column short of the edge.
const COLUMN_WIDTHS: [Length; COLUMN_COUNT] = [
    Length::Definite(relative(0.09)),
    Length::Definite(relative(0.18)),
    Length::Definite(relative(0.12)),
    Length::Definite(relative(0.12)),
    Length::Definite(relative(0.12)),
    Length::Definite(relative(0.16)),
    Length::Definite(relative(0.16)),
    Length::Definite(relative(0.05)),
];
/// How each column is aligned, in column order.
///
/// A cell is aligned by the column it occupies and not by the column it is drawn in,
/// which is what makes a cell standing in for several columns read as the columns it
/// replaced.
const COLUMN_ALIGNS: [Align; COLUMN_COUNT] = [
    Align::Start,
    Align::Start,
    Align::End,
    Align::End,
    Align::End,
    Align::End,
    Align::End,
    Align::End,
];
/// How each column is aligned in a row whose market is not loaded.
///
/// The one cell such a row draws stands in for columns of two alignments — the name of the
/// listing at the start of the row and four numbers at its end — so it is centred in the room
/// they would have taken rather than pulled to either side. The route and the controls keep
/// their own alignment: a row is asked for its own alignments, and only the merged cell is
/// read out of them.
const AWAITING_ALIGNS: [Align; COLUMN_COUNT] = [
    Align::Start,
    Align::Center,
    Align::Center,
    Align::Center,
    Align::Center,
    Align::Center,
    Align::Center,
    Align::End,
];
/// The column a cell standing in for several columns is drawn in.
///
/// Everything between the route and the column of controls of a row whose market is not
/// loaded is one cell: the name of the listing, the price and the statistics of its table.
/// The route and the controls stay where they are, because the first is not a value of the
/// market behind the row — it names the scope the user picked — and the second is the user's
/// own: a listing whose market was never loaded is exactly the one they may want to take back
/// out, and a button is not a value of the market either.
const MISSING_MARKET_COLUMN: usize = 1;
/// The column a row's own controls are drawn in.
const ACTIONS_COLUMN: usize = COLUMN_COUNT - 1;
/// What a column says when the listing publishes no value for it.
const MISSING_VALUE: &str = "—";
/// The debug selector of each column of one row, in column order.
///
/// A cell of a row that stands in for several columns is drawn in the column it begins in
/// and covers the rest, so a test that reads the painted frame reads these: the column a
/// cell would have drawn is the column whose selector is absent.
const CELL_SELECTORS: [&str; COLUMN_COUNT] = [
    "WATCH_LIST_CELL_0",
    "WATCH_LIST_CELL_1",
    "WATCH_LIST_CELL_2",
    "WATCH_LIST_CELL_3",
    "WATCH_LIST_CELL_4",
    "WATCH_LIST_CELL_5",
    "WATCH_LIST_CELL_6",
    "WATCH_LIST_CELL_7",
];

/// The listings one watch list follows.
///
/// The entries are decoded through the same validation the service applies, so a
/// persisted list is either a list the service can follow or a configuration
/// error the framework reports.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct WatchListConfig {
    entries: Vec<VenueListingKey>,
}

#[derive(Deserialize)]
struct WatchListConfigWire {
    entries: Vec<VenueListingKey>,
}

impl<'de> Deserialize<'de> for WatchListConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WatchListConfigWire::deserialize(deserializer)?;
        validate_replacement(&wire.entries).map_err(de::Error::custom)?;

        Ok(Self {
            entries: wire.entries,
        })
    }
}

impl WidgetConfig for WatchListConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// The display code of one asset, as the captured generation published it.
///
/// Only the display catalog knows what an asset is called: the canonical identity
/// of a token is a digest, and the code the catalog publishes is the whole name of
/// the asset. An asset the captured generation does not
/// hold is the localized unknown — never an identity digest, and never a name
/// invented here, which would disagree with every other pane about the same asset.
fn asset_display(display: &DisplaySnapshot, asset: &AssetId, cx: &App) -> SharedString {
    display.resolver().asset_code(asset).map_or_else(
        |_| t!(cx, "error.display.unknown_asset"),
        |code| SharedString::new(code.as_str()),
    )
}

/// The label of one quantity unit, named the way that unit is measured.
///
/// The three traded units read differently — asset units are counted in the asset
/// they are denominated in, contracts are counted in whole contracts, and outcome
/// tokens in the shares of one outcome — so none of them may be labelled as
/// another.
fn unit_label(display: &DisplaySnapshot, unit: &QuantityUnit, cx: &App) -> SharedString {
    match unit {
        QuantityUnit::AssetUnits { asset } | QuantityUnit::Notional { asset } => {
            asset_display(display, asset, cx)
        }
        QuantityUnit::Contracts => t!(cx, "widget.watch_list.contracts"),
        QuantityUnit::OutcomeTokens { .. } => t!(cx, "widget.watch_list.outcome_tokens"),
    }
}

/// The name one listing is drawn and spoken about by.
///
/// The published name is the whole label: nothing is prefixed to it and nothing is
/// assembled beside it. The connector-aware boundary resolves the name and renders it,
/// and a listing the published generation does not name is the localized unknown, so
/// a row can never be labelled with another listing's symbol and never with an
/// identity digest. The route this row follows is stated in its own column.
fn listing_label(cx: &App, listing: &VenueListingKey) -> SharedString {
    ListingErrorPresentation::of(
        cx.global::<MarketParticipantsConnector>()
            .catalog()
            .cached_listing_instrument(listing),
    )
    .localized_message(cx)
}

/// A price and the asset it is quoted in.
fn price_text(
    display: &WatchListRowDisplay,
    generation: &DisplaySnapshot,
    cx: &App,
) -> SharedString {
    SharedString::new(
        format_smolstr!(
            "{} {}",
            display.price,
            asset_display(generation, &display.quote_asset, cx)
        )
        .as_str(),
    )
}

/// A traded value and the unit it is counted in.
///
/// A listing that publishes no traded quantity — or no traded notional — has
/// nothing for that column: it says the value is missing instead of falling back
/// to the other unit or to zero.
fn traded_text(
    quantity: Option<&MarketQuantity>,
    generation: &DisplaySnapshot,
    cx: &App,
) -> SharedString {
    quantity.map_or_else(
        || SharedString::new_static(MISSING_VALUE),
        |quantity| {
            SharedString::new(
                format_smolstr!(
                    "{} {}",
                    format_volume(quantity.value),
                    unit_label(generation, &quantity.unit, cx)
                )
                .as_str(),
            )
        },
    )
}

/// The rows one of the two tables draws.
///
/// A table is a separate entity because [`Table`] reads its rows from an [`Entity`]; it
/// holds one snapshot at a time, and a snapshot that changes nothing the table draws does
/// not reach it at all. The two tables are the same dataset over two sets of rows, which
/// is why the market they draw is a field rather than a second type.
struct WatchListTable {
    /// The market this table heads its columns for.
    market: WatchListMarket,
    rows: Arc<[Arc<WatchListRow>]>,
    /// The sentence a row waiting for its market is drawn with, one state per venue.
    ///
    /// It is an entity rather than text because the markdown component owns what it draws and
    /// parses a source once; the widget keeps one per venue so a row can say which market
    /// participant it waits for without making a state of its own on a frame. A venue's state
    /// is shared by every row of that venue, which is why it is a field of the table that draws
    /// the rows rather than of a row.
    sentences: HashMap<VenueId, Entity<MarkdownState>>,
    /// The widget the row controls act on, if it is still alive.
    widget: WeakEntity<WatchListWidget>,
    /// Whether the row controls accept input.
    disabled: bool,
}

impl WatchListTable {
    fn new(market: WatchListMarket, widget: WeakEntity<WatchListWidget>) -> Self {
        Self {
            market,
            rows: Arc::from(Vec::new()),
            sentences: HashMap::default(),
            widget,
            disabled: true,
        }
    }
}

impl TableDataSet<COLUMN_COUNT> for WatchListTable {
    fn rows_count(&self) -> usize {
        self.rows.len()
    }

    fn headers(&self, cx: &App) -> [SharedString; COLUMN_COUNT] {
        let [change, change_percent, traded_quantity, traded_notional] = self.market.column_keys();
        [
            t!(cx, "common.route"),
            t!(cx, "common.instrument"),
            t!(cx, "common.price"),
            t!(cx, change),
            t!(cx, change_percent),
            t!(cx, traded_quantity),
            t!(cx, traded_notional),
            // The last column is the row's own controls, which carry no label: nothing is
            // written over a column whose content is a button.
            SharedString::new_static(""),
        ]
    }

    fn column_aligns(&self) -> [Align; COLUMN_COUNT] {
        COLUMN_ALIGNS
    }

    /// One row's own alignments: a row waiting for its market states one thing across the
    /// columns its table keeps for values, so it is centred in them rather than aligned like
    /// the number it replaced.
    fn row_aligns(&self, row_index: usize) -> [Align; COLUMN_COUNT] {
        match self.rows.get(row_index).map(|row| row.state()) {
            Some(WatchListRowState::AwaitingMarket) => AWAITING_ALIGNS,
            _ => COLUMN_ALIGNS,
        }
    }

    fn column_widths(&self) -> [Length; COLUMN_COUNT] {
        COLUMN_WIDTHS
    }

    /// The columns of a row whose market is not loaded are one cell: everything between the
    /// route the user picked and the column of that row's own controls says which market
    /// participant has to be loaded before the rest of the row means anything.
    ///
    /// The last column is not covered: a listing whose market was never loaded is exactly the
    /// one a user may want to take back out of their list, and a control is not a value of the
    /// market behind the row.
    fn column_span(&self, row_index: usize, column_index: usize) -> usize {
        let awaiting_market = self
            .rows
            .get(row_index)
            .is_some_and(|row| matches!(row.state(), WatchListRowState::AwaitingMarket));
        if awaiting_market && column_index == MISSING_MARKET_COLUMN {
            COLUMN_COUNT - MISSING_MARKET_COLUMN - 1
        } else {
            1
        }
    }

    fn cell(&self, row_index: usize, column_index: usize, _cx: &App) -> impl IntoElement {
        let Some(row) = self.rows.get(row_index) else {
            return Empty.into_any_element();
        };

        let awaiting_market = matches!(row.state(), WatchListRowState::AwaitingMarket);
        let align = if awaiting_market {
            AWAITING_ALIGNS[column_index]
        } else {
            COLUMN_ALIGNS[column_index]
        };
        // The sentence a row waiting for its market is drawn with is the one the widget keeps
        // for the venue that row is on, which is a row of *that* market's table.
        let sentence = awaiting_market
            .then(|| self.sentences.get(&row.key().route.venue).cloned())
            .flatten();
        WatchListCell {
            row: row.clone(),
            column: column_index,
            selector: CELL_SELECTORS[column_index],
            align,
            sentence,
            widget: self.widget.clone(),
            disabled: self.disabled,
        }
        .into_any_element()
    }

    /// A confirmed list with nothing in it is an empty list, which is a fact rather
    /// than a state: the table says so instead of leaving the body under its headers
    /// blank.
    fn empty(&self, cx: &App) -> Option<AnyElement> {
        Some(EmptyState::new("WatchListEmpty", t!(cx, "common.no_data")).into_any_element())
    }
}

/// One cell of one row, drawn when the table asks for it.
///
/// The alignment the table gives this cell is the last covered column's, and it is applied to
/// the cell rather than by the cell: a cell that stands in for several columns is aligned like
/// the last of them, so a group that ends in a number ends where its numbers do. What this
/// carries is the same alignment for the *content* of a cell that is not merged — a value of a
/// column that ends at its end — which is what keeps a row of values aligned like its columns.
#[derive(IntoElement)]
struct WatchListCell {
    row: Arc<WatchListRow>,
    column: usize,
    /// The selector this cell's own frame is drawn under, which is the column it occupies.
    selector: &'static str,
    align: Align,
    /// The sentence this row says when its market is not loaded, for the one column that draws
    /// it: the venue's own state, kept by the table the row belongs to.
    sentence: Option<Entity<MarkdownState>>,
    /// The widget the row's own controls act on, if it is still alive.
    widget: WeakEntity<WatchListWidget>,
    /// Whether those controls accept input.
    disabled: bool,
}

impl RenderOnce for WatchListCell {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            row,
            column,
            selector,
            align,
            sentence,
            widget,
            disabled,
        } = self;

        let content = match column {
            // The watch list is the one surface that follows listings of several
            // routes, so this column states the whole user-visible route and not the
            // brand: two rows can carry the same name, and `Binance/SPOT` is what
            // tells them apart.
            0 => route_column(&row, cx).into_any_element(),
            // Everything between the route and the column of controls is one thing to a row
            // whose market is not loaded, and it says which market participant has to be
            // loaded before any of it can be read.
            MISSING_MARKET_COLUMN if matches!(row.state(), WatchListRowState::AwaitingMarket) => {
                match &sentence {
                    Some(sentence) => missing_market_cell(sentence),
                    None => pending_text(row.status(), cx).into_any_element(),
                }
            }
            1 => instrument_cell(&row, cx),
            2 => ticker_cell(&row, TickerLabel::Price, cx),
            3 => ticker_cell(&row, TickerLabel::Change, cx),
            4 => ticker_cell(&row, TickerLabel::ChangePercent, cx),
            5 => ticker_cell(&row, TickerLabel::TradedQuantity, cx),
            6 => ticker_cell(&row, TickerLabel::TradedNotional, cx),
            // The last column carries no value: what a row has to say about its own listing is
            // said by the columns that name it and the numbers under the other headings, and
            // the controls that change it are not a value of the market.
            _ => Empty.into_any_element(),
        };

        let merged = matches!(row.state(), WatchListRowState::AwaitingMarket)
            && column == MISSING_MARKET_COLUMN;

        // The row's own controls travel in the last column of every row, whatever that
        // row is doing: what the user follows is theirs to change, and a listing whose
        // market is not loaded is exactly the kind a user wants to take back out.
        let controls = if column == ACTIONS_COLUMN {
            actions_cell(&row, &widget, disabled, cx)
        } else {
            Empty.into_any_element()
        };

        div()
            .debug_selector(move || selector.into())
            // A cell that stands in for several columns holds the room the table gave it, and
            // the table centres it: where it sits is a row's own alignment, which the table
            // reads before it draws any of this. What is left to do here is the row's *content*
            // — a value aligned to the end of its column, or nothing at all.
            .when(merged, |cell| cell.w_full())
            .when(!merged && matches!(align, Align::End), |cell| {
                cell.justify_end()
            })
            .when(!merged && matches!(align, Align::Center), |cell| {
                cell.justify_center()
            })
            .child(content)
            .child(controls)
            .into_any_element()
    }
}

/// The one thing a row says while the market participant behind it is not loaded.
///
/// The market participant is named the way the rest of the interface names it: its logo from
/// the catalogue's mark of the venue and its own name, set bold, inside the catalogue's
/// sentence — the catalogue states where each belongs and how it is written, so neither is
/// assembled here. The logo is written inline rather than drawn beside the words, which is what
/// puts it where the sentence says it goes, between the verb and the name, instead of in front
/// of the whole sentence. The sentence is drawn by the markdown component, which is what makes
/// any of that read as the catalogue wrote it rather than as its source: the words, the
/// emphasis and the inline image are the component's business, and a cell that drew the source
/// would show a reader the punctuation.
///
/// The sentence states what has to be loaded and nothing about how: the control that loads
/// it is the one the placeholder names, and a table cell is not the place to point at it a
/// second time.
fn missing_market_cell(sentence: &Entity<MarkdownState>) -> AnyElement {
    div()
        .debug_selector(|| "WATCH_LIST_MISSING_MARKET_SENTENCE".into())
        .min_w_0()
        .truncate()
        .child(Markdown::new(sentence))
        .into_any_element()
}

/// The text of the first column: the whole user-visible route of the row.
///
/// A reader sees the route's own form — the venue's name in the language
/// the window shows, and the scope's canonical one — and the watch list states it in
/// full because it is the one surface that follows listings of several routes: the
/// venue's name alone would not tell two rows of one venue apart.
fn route_column(row: &WatchListRow, cx: &App) -> SharedString {
    route_display_string(&row.key().route, cx)
}

/// Which label of a row a cell draws.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TickerLabel {
    Price,
    Change,
    ChangePercent,
    TradedQuantity,
    TradedNotional,
}

/// What a row says while it has nothing to show yet.
fn pending_text(status: WatchListStatus, cx: &App) -> SharedString {
    match status {
        WatchListStatus::Connecting => t!(cx, "common.connecting"),
        WatchListStatus::LoadingSnapshot => t!(cx, "widget.watch_list.loading_snapshot"),
        WatchListStatus::Error => t!(cx, "widget.watch_list.ticker_unavailable"),
        // A row is only ever ready with a ticker, which has labels of its own.
        WatchListStatus::Ready => SharedString::new_static(MISSING_VALUE),
    }
}

/// The color a price and its changes are drawn in.
fn direction_color(direction: PriceDirection, cx: &App) -> Hsla {
    let theme = cx.theme();
    match direction {
        PriceDirection::Up => theme.roles.market.up.hsla,
        PriceDirection::Flat => theme.roles.market.flat.hsla,
        PriceDirection::Down => theme.roles.market.down.hsla,
    }
}

/// The listing, and what its stream is doing when it has something to say beside
/// it.
///
/// A row that is reconnecting keeps showing the prices it has, so the status is
/// only shown next to a ticker; a failed row says so whether or not it still has
/// one. A row whose market is not loaded never reaches here: what it draws stands in
/// for this column as well.
fn instrument_cell(row: &WatchListRow, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let status = match row.status() {
        WatchListStatus::Error => Some((cx, theme.roles.status.danger.base.hsla)),
        WatchListStatus::Connecting | WatchListStatus::LoadingSnapshot if row.has_ticker() => {
            Some((cx, theme.roles.fg.muted.hsla))
        }
        WatchListStatus::Connecting | WatchListStatus::LoadingSnapshot | WatchListStatus::Ready => {
            None
        }
    };

    div()
        .w_full()
        .min_w_0()
        .flex()
        .items_center()
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .child(listing_label(cx, row.key())),
        )
        .children(status.map(|(cx, color)| {
            div()
                .debug_selector(|| "WATCH_LIST_ROW_STATUS".into())
                .ml_2()
                .max_w(px(96.0))
                .flex_none()
                .truncate()
                .text_xs()
                .text_color(color)
                .child(pending_text(row.status(), cx))
        }))
        .into_any_element()
}

/// One number of a row, colored by the direction it moved in when it describes a
/// price.
fn ticker_cell(row: &WatchListRow, label: TickerLabel, cx: &App) -> AnyElement {
    let Some(display) = row.display() else {
        // Before the first ticker a row says what it is waiting for instead of
        // showing a number it does not have.
        return match label {
            TickerLabel::Price => div()
                .debug_selector(|| "WATCH_LIST_ROW_PRICE".into())
                .truncate()
                .text_color(cx.theme().roles.fg.muted.hsla)
                .child(pending_text(row.status(), cx))
                .into_any_element(),
            TickerLabel::Change
            | TickerLabel::ChangePercent
            | TickerLabel::TradedQuantity
            | TickerLabel::TradedNotional => div()
                .debug_selector(|| "WATCH_LIST_ROW_VALUE".into())
                .truncate()
                .child(SharedString::new_static(MISSING_VALUE))
                .into_any_element(),
        };
    };
    // One capture names every unit this cell draws, so the numbers inside one cell
    // never mix two publications. Each cell captures for itself while the table is
    // built, so the columns of one row may come from different generations: the
    // guarantee is per read, not per row and not per frame.
    let generation = cx
        .global::<MarketParticipantsConnector>()
        .catalog()
        .cached_display();
    let text = match label {
        TickerLabel::Price => price_text(display, &generation, cx),
        TickerLabel::Change => display.change.clone(),
        TickerLabel::ChangePercent => display.change_percent.clone(),
        TickerLabel::TradedQuantity => {
            traded_text(display.traded_quantity.as_ref(), &generation, cx)
        }
        TickerLabel::TradedNotional => {
            traded_text(display.traded_notional.as_ref(), &generation, cx)
        }
    };
    let cell = div().truncate().child(text);
    let cell = match label {
        TickerLabel::Price => cell.debug_selector(|| "WATCH_LIST_ROW_PRICE".into()),
        TickerLabel::Change
        | TickerLabel::ChangePercent
        | TickerLabel::TradedQuantity
        | TickerLabel::TradedNotional => cell,
    };

    match label {
        TickerLabel::Price | TickerLabel::Change | TickerLabel::ChangePercent => cell
            .text_color(direction_color(display.direction, cx))
            .into_any_element(),
        TickerLabel::TradedQuantity | TickerLabel::TradedNotional => cell.into_any_element(),
    }
}

/// The controls one row offers: retry for a failed stream, and removal.
///
/// A row whose market is not loaded offers removal alone. Retry reopens one listing's
/// stream, and there is no stream to reopen — what the row is waiting for is the market
/// participant, which the user loads from the 「市场」control and not from here — while
/// removal is about the user's own list rather than about the market behind it, so it is
/// offered whenever the pane is not busy with an operation of its own.
fn actions_cell(
    row: &WatchListRow,
    widget: &WeakEntity<WatchListWidget>,
    disabled: bool,
    cx: &App,
) -> AnyElement {
    let Some(widget) = widget.upgrade() else {
        // The row outlived the widget it belongs to, so there is nothing left to
        // act on.
        return Empty.into_any_element();
    };
    let color = cx.theme().roles.fg.default.hsla;
    let key = row.key().clone();
    // A row that failed before its first ticker is a row whose stream failed, and retrying
    // it is exactly what the control is for: what a retry needs is a stream, not a price.
    let retry = (row.state().is_read() && row.status() == WatchListStatus::Error).then(|| {
        instrument_button(
            InstrumentAction::Retry,
            &widget,
            key.clone(),
            disabled,
            t!(cx, "common.retry"),
            color,
        )
    });
    let remove = instrument_button(
        InstrumentAction::Remove,
        &widget,
        key.clone(),
        disabled,
        // The listing is named beside the verb rather than through the translation: the
        // catalogue's own remove message has no placeholder for it, and both languages read
        // verb-first, so the two are joined here — name after verb, the way a reader says it.
        SharedString::new(
            format_smolstr!(
                "{} {}",
                t!(cx, "widget.watch_list.remove"),
                listing_label(cx, &key)
            )
            .as_str(),
        ),
        color,
    );

    div()
        .flex()
        .items_center()
        .justify_center()
        .children(retry)
        .child(remove)
        .into_any_element()
}

/// The two controls a row offers.
#[derive(Clone, Copy)]
enum InstrumentAction {
    Retry,
    Remove,
}

impl InstrumentAction {
    const fn name(self) -> &'static str {
        match self {
            Self::Retry => "WatchListRetry",
            Self::Remove => "WatchListRemove",
        }
    }

    /// The debug selector this control is drawn under.
    ///
    /// The two are told apart by it, so a test that looks for one cannot be answered by
    /// the other: both are `Button`s inside the same cell, and a selector shared by the
    /// pair would make "the retry control is offered" true wherever removal is.
    const fn selector(self) -> &'static str {
        match self {
            Self::Retry => "WATCH_LIST_RETRY",
            Self::Remove => "WATCH_LIST_REMOVE",
        }
    }

    const fn icon(self) -> IconKind {
        match self {
            Self::Retry => IconKind::ArrowsClockwise,
            Self::Remove => IconKind::X,
        }
    }
}

/// A control that acts on one followed listing.
fn instrument_button(
    action: InstrumentAction,
    widget: &Entity<WatchListWidget>,
    key: Arc<VenueListingKey>,
    disabled: bool,
    label: SharedString,
    color: Hsla,
) -> Button {
    let widget = widget.clone();

    Button::new(ElementId::Name(SharedString::new_static(action.name())))
        .debug_selector(move || action.selector().into())
        .ghost()
        .small()
        .disabled(disabled)
        .aria_label(label)
        .on_click(move |_, _, cx| {
            let key = key.clone();
            widget.update(cx, move |this, cx| match action {
                InstrumentAction::Retry => this.retry(&key, cx),
                InstrumentAction::Remove => this.remove(&key, cx),
            });
        })
        .child(Icon::new(action.icon()).size_3().text_color(color))
}

pub struct WatchListWidget {
    config: WatchListConfig,
    binding: WidgetBinding<WatchListProtocol>,
    /// The table of the listings that trade around the clock.
    continuous_table: Entity<WatchListTable>,
    /// The table of the listings whose market keeps a calendar.
    session_table: Entity<WatchListTable>,
    status: Entity<WidgetStatus>,
    /// The recovery the framework offers for every failure this widget reports.
    recovery: WidgetAction,
    config_change: WidgetConfigChange,
    entries: HashMap<VenueListingKey, WatchListEntry>,
    /// Whether the service has confirmed the followed set at least once.
    ///
    /// It is what an empty list has to say: a list the user emptied is the empty table, while
    /// a list that has not been agreed to yet is a list still being asked about. What the
    /// framework is *told* while that answer is outstanding is not this field's business —
    /// rows are drawn for every followed listing from the moment there is one — but whether
    /// the empty table is drawn or the pane is still connecting for it is.
    confirmed: bool,
    /// The one operation this widget may have in flight.
    pending: Option<PendingOperation>,
    directory_generation: u64,
    /// Whether a market this list follows was loaded or refreshed again.
    ///
    /// Every stream resolves the increments its rows are written at when it is opened, so a
    /// commit that lands on one of the followed listings' markets replaces the generation behind
    /// that stream while the stream keeps what it resolved. The list therefore starts over, and
    /// the request it sends has to be the one that reopens those streams: [`Self::load`] reads
    /// this after its own gate — a list a shrinking commit sent back to waiting keeps it, so the
    /// load that brings the market back reopens them too — and [`Self::replace`] turns it into a
    /// `MarketReloaded`, because `Replace` keeps the streams of the listings that stay, which are
    /// exactly the ones that have to resolve again.
    market_reloaded: bool,
    /// The listings whose streams stopped being readable on their own.
    ///
    /// A route that loses its catalog is not asked to give the listing up: the stream it was
    /// opened for is parked on that error while the service still holds its key, so the next
    /// load that reads the listing has to open a stream for it rather than be handed the dead
    /// one. That is what this remembers — the load that re-creates the entry reads it and asks
    /// for the request that opens the streams again.
    dropped: HashSet<VenueListingKey>,
    /// Whether the list is waiting for its markets rather than reading them.
    ///
    /// A market the user has not loaded is one this pane can read nothing from, so its
    /// listings have no streams and no rows of their own: the widget keeps the followed
    /// set and draws one row per listing saying which market participant is missing. The
    /// streams it was reading are kept as well — the alternative is cancelling the
    /// binding, which is terminal, and this pane has to be able to read the markets again
    /// once the user loads them — so a ticker that was in flight when the routes went
    /// away can still arrive, and every message entry refuses while this is set: no entry
    /// exists to accept it into.
    awaiting_market: bool,
}

impl WidgetView for WatchListWidget {
    type Config = WatchListConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.market.watch_list");
    const DISPLAY_NAME: &'static str = "widget.names.watch_list";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;
    const DESCRIPTION: &'static str = "Live 24-hour statistics for followed listings";
    const ICON: Option<&'static str> = Some("widgets/watch_list.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: Entity<WidgetStatus>,
        _reload: impl Fn(&mut App) + 'static,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = cx.global::<MarketParticipantsConnector>().market();
        Self::new_with_source(config, source, status, config_change, recovery, cx)
    }

    /// The options this widget persists.
    ///
    /// The framework asks for these to write the pane back, so this answers with the
    /// field itself rather than computing anything.
    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    /// The list is the content, and it is ready as soon as the service has
    /// confirmed it — including an empty list. A list with nothing in it is what
    /// "no listings followed" looks like; it is not a state the widget reports.
    ///
    /// A pane whose market is not loaded has content whenever the user follows
    /// something: those listings are the user's list, and what the body draws for them
    /// is a table that says which market participant has to be loaded. A list that
    /// follows nothing has no such row to draw, so it is left to the framework's
    /// placeholder — the sentence that names every market participant this widget can
    /// read at all, and the control that loads one.
    ///
    /// Which is which half: what the user follows is what the body has to say, and what the pane
    /// was told is what an empty list has to say. A pane waiting for its markets is drawing its
    /// rows — one listing of a market that is in memory is enough for the table to stand, and the
    /// listings whose markets are not loaded are the rows that say so — so waiting gives up only
    /// the second half. A list that follows nothing and has no market to read is the one the
    /// placeholder stands in for, and [`Self::report_waiting`] asks this to choose between the
    /// two sentences the framework can be given for it.
    fn has_content(&self) -> bool {
        !self.config.entries.is_empty() || self.confirmed
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        // The markets this list follows have to be loaded before anything else happens
        // here, and the check is first on purpose: a route the user never loaded, or one a
        // shrinking commit withdrew, leaves the pane waiting for it instead of reaching the
        // branches below, which settle the listings the user stored.
        if !self.market_ready(cx) {
            self.wait_for_market(cx);
            return;
        }
        // Read past the gate: a commit on a market this list follows that took it away leaves
        // this set while the streams that resolved against the retired generation are still
        // alive, and the load that brings the market back has to reopen them.
        //
        // The streams of the listings this pane reads were resolved against the generation the
        // commit replaced, and the request that follows has to be the one that resolves them
        // again. `replace` reads that from `market_reloaded` itself, so a load that never sent
        // anything leaves it standing.
        let retry = self.status.is_failed(cx);
        if retry {
            // A recovery keeps the rows the user is looking at, so a load that was not allowed
            // to draft a request when the commit landed — every load a failed pane starts-up
            // starts is a recovery — has to reopen their streams itself. Nothing vouches for
            // the generations they hold: this pane could not read at all while they were being
            // replaced, and a commit it could not act on is still a commit that happened.
            self.market_reloaded = true;
        } else {
            // An explicit load starts over, because the widget cannot vouch for
            // what it was showing.
            self.entries.clear();
            self.confirmed = false;
        }
        let readable = self.readable(cx);
        for key in &self.config.entries {
            if !readable.contains(key) {
                // The market behind this listing is not in memory: it has no stream to
                // follow and no row of its own, and what the table draws for it says so.
                // The entry goes, so a message that was already in flight for it cannot
                // fill a row nothing backs any more.
                self.entries.remove(key);
                continue;
            }
            // A listing whose stream went dead on its own is opened again rather than handed
            // the stream it left behind: the service still holds the key of the parked one, and
            // only the request that opens the streams again gives it a new one.
            if self.dropped.contains(key) {
                self.market_reloaded = true;
            }
            self.entries
                .entry(key.clone())
                .or_insert_with(|| WatchListEntry::connecting(key));
        }
        // Every listing that was written down has been read again, whether it was readable or
        // not: one whose market is still gone is a row that waits, and the load after that one
        // reads it exactly like this.
        self.dropped.clear();
        self.pending = Some(PendingOperation::Load);
        self.status.report(WidgetState::Connecting, cx);
        self.sync_rows(cx);
        self.start(cx);
    }

    /// The only control this widget puts in its header: adding a listing.
    ///
    /// It refuses input while the pane waits for its markets, like every other control
    /// here: a listing can only be added from a market that is in memory, and the
    /// sentence the pane is waiting with is what tells the user to load one.
    fn header(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> WidgetHeader {
        WidgetHeader::empty().left(self.add_button(cx))
    }
}

impl WatchListWidget {
    /// The listings this pane can read right now: the followed ones whose route is in memory.
    ///
    /// What a listing is followed on is a scope, and a scope is readable exactly when the
    /// venue it belongs to is loaded — a venue appears in the loaded venues only once its
    /// whole directory has committed. A listing of a market nobody loaded is therefore a row
    /// with nothing behind it rather than a reason to read nothing at all, which is what lets
    /// the two kinds of row sit in one table.
    fn readable(&self, cx: &App) -> HashSet<VenueListingKey> {
        let venues = cx.global::<MarketParticipantsConnector>().venues();
        self.config
            .entries
            .iter()
            .filter(|key| venues.scope(&key.route).is_some())
            .cloned()
            .collect()
    }

    /// The set the service is asked for: the listings this pane reads, which is what the
    /// followed listings and the markets in memory come to.
    ///
    /// It is narrower than the configuration, and deliberately so: a listing whose market
    /// participant is not loaded has no stream, so there is nothing for the service to follow
    /// and nothing for it to be asked about. Such a listing keeps its row — the table draws one
    /// for every followed listing — and the request that follows its market being loaded is what
    /// opens its stream.
    ///
    /// Every request is built from this answer rather than from one of its two halves, so the
    /// service is never asked about a listing the pane is not reading. It is *not* the answer
    /// to "what does the service follow": a listing this pane gives up on its own — the one
    /// whose catalog went away, which is dropped row by row — stays in the service's set until
    /// the next request, because nothing was sent about it. That is harmless while the pane is
    /// not reading it (its messages fill no row), and it is what the request that follows would
    /// have to give up if a listing ever had to be dropped without one.
    fn served(&self, cx: &App) -> HashSet<VenueListingKey> {
        let readable = self.readable(cx);
        self.entries
            .keys()
            .filter(|key| readable.contains(key))
            .cloned()
            .collect()
    }

    /// Settles the followed set against what the venues still carry, and follows it.
    ///
    /// The set is written down to be followed, so a listing nobody publishes any more
    /// leaves it before anything is subscribed to: subscribing first would ask a venue
    /// for a listing it just said it does not have.
    ///
    /// This lookup is **advisory**: it only ever removes a listing, and only when the
    /// route answered that it does not publish it, or when the route is no longer
    /// registered for tickers at all. Everything it cannot answer — a venue that is
    /// quiet, a directory that could not be read — leaves the set alone and lets the
    /// request below speak for itself. That request is the authoritative one: the
    /// market participants connector resolves every listing — its rules, status and native
    /// identifiers — before anything is sent, so a directory that cannot be read is
    /// reported as the failure it is and never as a listing that is not there. A route
    /// that does not answer therefore stays silent here, and its listings stay and
    /// report their own errors, which the user can recover from.
    ///
    /// Whether the streams that follow have to be opened again is not an argument here but the
    /// pane's own [`Self::market_reloaded`], which the request that goes out reads and gives up —
    /// see [`Self::replace`], which states why it survives a load that never sent one.
    fn start(&mut self, cx: &mut Context<Self>) {
        // The pane reached here only past its own gate, so the markets it was waiting
        // for are in memory again and messages belong to them once more.
        self.awaiting_market = false;
        self.directory_generation = self.directory_generation.wrapping_add(1);
        let generation = self.directory_generation;
        let asked = self.config.entries.clone();
        // The request names the listings whose markets are in memory. The listings that are
        // only followed are left out of it: there is no stream to open for a listing whose
        // market participant is not loaded, and asking a venue for one would be asking it
        // about a scope it was never asked to read. The set is `readable` intersected with what
        // the pane holds, so a listing that is about to be settled away is not asked for either
        // — the settlement below is what decides that, and the request it is followed by names
        // the set it decided on.
        let served = self.served(cx);
        if asked.is_empty() {
            self.replace(&served, cx);
            return;
        }
        // Every lookup is started in one synchronous pass, and every answer is applied
        // in one settlement: each question was asked about the same set, so settling
        // them one by one would let a later answer judge a set an earlier one changed.
        // What the loaded venues answer is read once here for the same reason: every
        // question this settlement puts to a catalog comes out of one reading of them.
        // What an unasked listing then answers for itself is a different read, and
        // `listing_support` states why.
        let routes = LoadedScope::all(cx.global::<MarketParticipantsConnector>().venues());
        let catalog = cx.global::<MarketParticipantsConnector>().catalog();
        let answered = asked
            .iter()
            .map(|key| {
                // A route the reading does not carry is not asked: no question is put to a
                // catalog about a scope this pane holds no evidence for. That is *not* a
                // withdrawal — see `listing_support`: the venue may have loaded since the
                // reading, so the listing stays and the request below settles it.
                let loaded = offers_route(&routes, &key.route, CapabilityId::Tickers);
                let offered = loaded.then(|| catalog.contains(key));
                (key.clone(), offered)
            })
            .collect::<Vec<_>>();
        // The request is sent **after** the settlement and never before it: subscribing first
        // would ask a venue for a listing the settlement is about to say it no longer
        // publishes. Nothing is awaited here, so the request follows the answers by one turn
        // of the executor rather than by a round trip. The request is sent whatever the
        // settlement decided — what it settles is which listings are read, and a request built
        // from the set it read is the one the pane has to send, since the operation the load
        // started is waiting on it and a pane that sent nothing would wait for an answer that
        // is never coming. The two states that are not this pane's to send from are the failure
        // and the wait below: both are panes that read nothing, and neither has an operation of
        // its own in flight.
        cx.spawn(async move |this, cx| {
            let _ = this.update(cx, |this, cx| {
                // A set the user has changed since is theirs, and it asked for its own
                // replacement already.
                if this.directory_generation != generation {
                    return;
                }
                // A failure is terminal (§5.2): the settlement is about a read that is over,
                // and a pane the user is looking at a failure of is not one to start streams
                // for. What the settlement would have settled is not lost — the change is in
                // `config.entries`, which is what the recovery asks the venues for again.
                if this.status.is_failed(cx) {
                    return;
                }
                // A pane that went back to waiting while this was queued reads nothing, so it
                // asks for nothing: the streams it was reading are the ones waiting keeps
                // alive, and a request from here would cancel them.
                if this.awaiting_market {
                    return;
                }
                let answers = answered
                    .iter()
                    .map(|(key, offered)| ListingAnswer {
                        key: key.clone(),
                        support: listing_support(key, offered.as_ref(), cx),
                    })
                    .collect::<Vec<_>>();
                this.settle_listings(&asked, &served, &answers, cx);
                let served = this.served(cx);
                this.replace(&served, cx);
            });
        })
        .detach();
    }

    /// Drops the followed listings the routes no longer offer.
    ///
    /// `asked` is the followed set every answer was judged against, and `served` is the part of
    /// it the pane was reading when it asked. Both are compared before anything is applied: an
    /// answer is only worth acting on while the list it describes is still the list the user
    /// has, and while what the pane reads is still what it was reading — a venue commit that
    /// changed either of those asked for its own request, and this answer is about a set that
    /// no longer exists.
    ///
    /// Whether the answers were worth acting on is **not** what decides whether the request that
    /// follows is sent: that request is owed to the operation the load started, and a settlement
    /// that declined to act still leaves the pane waiting for it. This only decides what is
    /// withdrawn from the followed set.
    ///
    /// The answers are applied in one pass, because every listing was judged against the same
    /// set: the followed set is settled once, not once per listing.
    ///
    /// Only a proven "this route does not offer it" removes anything. A lookup that failed
    /// is a directory the widget could not read, which is a different thing from a listing a
    /// route does not carry, so the listing stays — the request that follows is what reports
    /// the directory's own failure.
    ///
    /// Answers that no longer describe the set the pane follows settle nothing: an answer is
    /// only worth acting on while the list it describes is still the list the user has, and
    /// while what the pane reads is still what it was reading when it asked. That decides what
    /// is withdrawn and nothing else — the request that follows the settlement is sent either
    /// way, because the operation the load started is waiting on it.
    fn settle_listings(
        &mut self,
        asked: &[VenueListingKey],
        served: &HashSet<VenueListingKey>,
        answers: &[ListingAnswer],
        cx: &mut Context<Self>,
    ) {
        if self.config.entries != asked {
            return;
        }
        if served.iter().any(|key| !self.entries.contains_key(key)) {
            return;
        }
        let mut withdrawn = HashSet::with_capacity(answers.len());
        withdrawn.extend(
            answers
                .iter()
                .filter(|answer| answer.support == ListingSupport::Withdrawn)
                .map(|answer| answer.key.clone()),
        );
        if withdrawn.is_empty() {
            return;
        }
        let before = self.config.entries.len();
        self.config.entries.retain(|key| !withdrawn.contains(key));
        log::warn!(
            "{} followed listing(s) are no longer offered; they left the list",
            before - self.config.entries.len()
        );
        for key in &withdrawn {
            self.entries.remove(key);
        }
        self.config_change.publish(cx);
        self.sync_rows(cx);
    }

    fn new_with_source<S>(
        config: WatchListConfig,
        source: S,
        status: Entity<WidgetStatus>,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: TickerSource,
    {
        let capacity = config.entries.len();
        let service = WatchListService::new(source);
        let binding = WidgetBinding::spawn_conditionally(
            global::handle(cx).handle(),
            service,
            cx,
            Self::apply_reliable,
            Self::apply_latest,
        );
        let widget = cx.weak_entity();
        let continuous_table =
            cx.new(|_| WatchListTable::new(WatchListMarket::Continuous, widget.clone()));
        let session_table = cx.new(|_| WatchListTable::new(WatchListMarket::Session, widget));
        // The pane recovers from the markets it waits for without polling: a venue commit is
        // the one event that can change what may be read, and a change that arrives while the
        // pane has nothing to show is a reason to load again.
        let events = cx.global::<MarketParticipantsConnector>().venues().events();
        cx.subscribe(&events, |this, _, event: &VenuesUpdated, cx| {
            this.on_venues_updated(event, cx);
        })
        .detach();

        Self {
            config,
            binding,
            continuous_table,
            session_table,
            status,
            recovery,
            config_change,
            entries: HashMap::with_capacity(capacity),
            confirmed: false,
            pending: None,
            directory_generation: 0,
            market_reloaded: false,
            dropped: HashSet::with_capacity(capacity),
            awaiting_market: false,
        }
    }

    /// Whether anything this list follows is in memory: at least one market that answers
    /// with tickers, and at least one of the routes the stored listings are on among them.
    ///
    /// A list that follows nothing still needs a market: the picker that adds a listing is
    /// drawn from the loaded venues, so with none of them loaded there is nothing to add —
    /// the pane waits for the market participant's metadata and says so, rather than drawing
    /// an empty table nothing can fill. Once one is loaded, a list that still follows nothing
    /// is the empty table's own business and this answers `true` for it.
    ///
    /// What is *not* asked is whether every followed route is in memory. A listing whose
    /// market is not loaded is a row with nothing behind it and is drawn as one; it is not a
    /// reason to read nothing at all, which would hide the listings whose markets the user
    /// did load. So the answer is "is there anything to read", and an empty followed set
    /// answers it with the markets themselves.
    fn market_ready(&self, cx: &App) -> bool {
        if !self.readable(cx).is_empty() {
            return true;
        }
        if !self.config.entries.is_empty() {
            return false;
        }
        // A list that follows nothing is read through the markets themselves: the picker that
        // adds a listing is drawn from the loaded venues, so with one of them loaded there is
        // something to add and the empty list is the empty table's own business.
        let venues = cx.global::<MarketParticipantsConnector>().venues();
        Self::listed_venues()
            .iter()
            .any(|venue| venues.loaded(venue).is_some())
    }

    /// Starts waiting for the markets this list follows, and draws what it follows.
    ///
    /// The routes behind the rows it was reading are gone, so nothing it holds is backed
    /// by a market any more: the entries go, which is also what stops messages from
    /// being accepted — the streams it was reading are still alive, and a ticker that was
    /// already in flight must not fill a row this pane can no longer vouch for. The
    /// stored listings are left alone: they are the user's, and loading the markets again
    /// is what brings them back. What the body draws for them is read off the
    /// configuration rather than off the entries, so every one of them is a row saying
    /// which market participant has to be loaded.
    ///
    /// A list that follows nothing has no such row to draw, which is the one case left to
    /// the framework: it reports the notice that names every market this widget could read
    /// and points at the control that loads one. Whenever there is a row to draw, the state
    /// is a working one: the words on screen are the row's, and a waiting notice beside
    /// them would be a second copy of the same sentence.
    fn wait_for_market(&mut self, cx: &mut Context<Self>) {
        self.awaiting_market = true;
        self.entries.clear();
        self.pending = None;
        self.sync_rows(cx);
        self.report_waiting(cx);
        cx.notify();
    }

    /// Reports the state a pane with nothing to read is in.
    ///
    /// Two panes are in it: the one that waits for the markets behind the listings it
    /// follows, whose rows say what has to be loaded, and the one that follows nothing at
    /// all, whose only way of saying it is the framework's placeholder. Which of the two this
    /// is depends on what the user follows rather than on why the pane stopped reading, so
    /// every path that leaves the pane with nothing to read reports it the same way — a list
    /// emptied by removing its last listing is the same list as one that never had a listing.
    ///
    /// A failure the pane was in is **replaced** by this: the user's recovery asks the gate
    /// again, and a gate that answers "nothing to read" is a wait — a state with no failure in
    /// it — so keeping the old sentence on screen would leave a failure that nothing clears,
    /// since a failed pane is not one a venue commit re-runs the load for.
    ///
    /// What it settles with the same answer is whether the pane counts as having content: a list
    /// with rows is content, and one with none is a list whose only way of saying anything is
    /// the framework's placeholder. That is [`Self::confirmed`]'s half of
    /// [`Self::has_content`], so it is given up here rather than by each of the paths that arrive
    /// here — a pane that stopped reading cannot vouch for a set it is not reading.
    ///
    /// Which of the two sentences it reports is the same question one step further on: a pane
    /// with nothing for a body to say names the markets it could load, and one with rows says
    /// what is missing in them — so this asks [`Self::has_content`], which is that question, and
    /// the framework is told the same thing the body is going to draw.
    fn report_waiting(&mut self, cx: &mut Context<Self>) {
        self.confirmed = false;
        if !self.has_content() {
            self.report_markets_not_loaded(cx);
        } else {
            self.status.report(WidgetState::Ready, cx);
        }
    }

    /// Reports the notice naming every market this widget could read at all.
    ///
    /// It is for the one pane that cannot say which market it is waiting for: the one that
    /// follows nothing, where the user has named none. The list is the capability's venue
    /// set off the definition table — the same answer whether or not anything is loaded and
    /// whatever the user has added — and the sentence is the framework's to resolve, in the
    /// language showing on the frame that draws it. A list that follows something names its
    /// markets in the rows themselves, which is why this is not how a pane with rows waits.
    fn report_markets_not_loaded(&self, cx: &mut Context<Self>) {
        let venues = Self::listed_venues();
        self.status.report(
            WidgetState::waiting(Notice::MarketNotLoaded {
                supported: venues.clone(),
                venues,
            }),
            cx,
        );
    }

    /// The venues the waiting notice lists: every venue whose loader can answer with tickers.
    ///
    /// What the list follows is the user's own set of listings, so the markets it waits for
    /// are only ever a subset of this. What it can read at all is not: that is this
    /// capability's venue set off the definition table, which is the same answer whether or
    /// not anything is loaded and whatever the user has added to the list.
    fn listed_venues() -> Vec<VenueId> {
        market_participants_connector::venue::definition::venues_supporting(CapabilityId::Tickers)
    }

    /// Runs the load a venue commit may have unblocked.
    ///
    /// A commit can matter to this list for either of two reasons. The first is that it changes
    /// whether the markets it needs are in memory, and the pane already holds that answer in the
    /// two things it maintains anyway: `awaiting_market` says it is not reading because a market
    /// it needs is missing, and `market_ready` says everything it needs is there now. A pane that
    /// is waiting for one and can now read starts over rather than re-checking; a pane that was
    /// reading one that is gone goes back to waiting.
    ///
    /// The second is that it landed on a market one of the followed listings is on, which is the
    /// user loading or refreshing that market: the catalog generation every one of those
    /// listings' streams resolved its increments against is replaced, so the list starts over —
    /// see `market_reloaded`, which is what makes the request the one that reopens their streams.
    /// That request carries the whole set it is to follow, so it does not matter whether a
    /// request of the list's own is still in flight or was replaced by it. A commit of a market
    /// none of the followed listings is on is neither, and leaves the pane and its streams
    /// exactly as they are. A failure is terminal: the event does not revive a failed pane, only
    /// the user's own recovery does.
    ///
    /// What a commit cannot mean is that the *set this list reads* changed on its own. That set
    /// is the followed listings intersected with the loaded scopes, and a scope enters it only
    /// through a commit on its own venue — the branch that starts the list over already. What is
    /// left for this handler is the gate, so a commit that changes neither the gate's answer nor
    /// one of these markets leaves the pane alone: starting it over would rebuild every entry as
    /// one waiting for a stream that the service, which never stopped following them, is not
    /// going to open again.
    fn on_venues_updated(&mut self, event: &VenuesUpdated, cx: &mut Context<Self>) {
        if self.status.is_failed(cx) {
            return;
        }
        // A commit on a market these listings are on is one of their own markets being loaded
        // again, so the list cannot keep reading the streams it holds.
        if self
            .config
            .entries
            .iter()
            .any(|key| key.route.venue == event.venue)
        {
            self.market_reloaded = true;
            self.load(cx);
            return;
        }
        if self.awaiting_market {
            if self.market_ready(cx) {
                self.load(cx);
            }
            return;
        }
        // A commit a list that is not waiting for anything has to act on is one that took away
        // the markets it was reading: the gate says there is nothing to read now, so it goes
        // back to waiting — and says so through the placeholder, since the rows it had are rows
        // the markets behind them no longer back.
        if !self.market_ready(cx) {
            self.wait_for_market(cx);
        }
    }

    /// Publishes the followed set to the two tables.
    ///
    /// A table is the only part of the body that draws rows, so it is notified at the
    /// exact boundaries where what it draws changes: an accepted row, a local change to
    /// the set, and the moment the controls stop accepting input. Notifying it is what
    /// busts the body's cache; nothing else repaints a cached body.
    ///
    /// The rows are read off the configuration rather than off the entries, because the
    /// two are not the same set: a listing whose market is not loaded has no entry and is
    /// still a listing the user follows, and a table that only drew entries would leave
    /// that listing off the screen without saying anything about it. Which table each row
    /// belongs to comes from the row itself, so this costs one pass over the followed
    /// listings — a hundred at the very most — and no lookup at all while the markets
    /// behind them are not loaded.
    fn sync_rows(&mut self, cx: &mut Context<Self>) {
        let (continuous, session) = split_rows(&self.config.entries, &self.entries);
        let disabled = self.controls_disabled(cx);
        for (table, rows) in [
            (&self.continuous_table, continuous),
            (&self.session_table, session),
        ] {
            table.update(cx, |table, cx| {
                table.rows = rows;
                table.disabled = disabled;
                cx.notify();
            });
        }
    }

    /// Sets the sentence every venue a waiting row is on is drawn with, in the language showing.
    ///
    /// It runs on every frame rather than where the rows are built, because the language a
    /// sentence is read in changes while the rows do not: a switch of language redraws the frame
    /// and rebuilds nothing, so a sentence read when the rows were built would stay in the
    /// language it was built in. A markdown state compares the source it is given and parses
    /// only what has changed, which is what makes this cheap enough to do every frame: the
    /// catalogue is read at most once per venue per frame, and nothing is parsed unless the
    /// words are new.
    fn sync_sentences(&mut self, cx: &mut Context<Self>) {
        let sentences = self.waiting_sentences(cx);
        for table in [&self.continuous_table, &self.session_table] {
            let sentences = sentences.clone();
            table.update(cx, |table, cx| {
                for (venue, source) in &sentences {
                    match table.sentences.get(venue) {
                        Some(state) => {
                            state.update(cx, |state, cx| state.set_source(source.clone(), cx));
                        }
                        None => {
                            table.sentences.insert(
                                *venue,
                                cx.new(|cx| MarkdownState::new(source.clone(), cx)),
                            );
                        }
                    }
                }
                // A venue no row is on any more keeps no state: the sentence it held is one
                // nothing draws, and the venues are a short list rather than a fixed one.
                table
                    .sentences
                    .retain(|venue, _| sentences.contains_key(venue));
            });
        }
    }

    /// The sentence every venue a waiting row is on is drawn with.
    ///
    /// It is the catalogue's own sentence, which writes the market participant's logo as an
    /// inline image of the path it is bound and sets the name it is bound in bold: what is
    /// bound here is the venue's logo asset and its name, and every mark of punctuation around
    /// them belongs to the sentence. The markdown component draws the result, so a row shows a
    /// reader the logo and the words rather than the source they are written in.
    fn waiting_sentences(&self, cx: &Context<Self>) -> HashMap<VenueId, SharedString> {
        let mut sentences = HashMap::default();
        for key in &self.config.entries {
            let venue = key.route.venue;
            sentences.entry(venue).or_insert_with(|| {
                t!(
                    cx,
                    "widget.watch_list.market_metadata_not_loaded",
                    logo = venue.asset_path(),
                    market = venue.display_string(cx)
                )
            });
        }
        sentences
    }

    /// Whether the row controls have to refuse input right now.
    ///
    /// One operation at a time keeps every message the widget sends unambiguous:
    /// a second request would cancel the first, and the service would then apply
    /// something the widget never learns about.
    fn controls_disabled(&self, cx: &Context<Self>) -> bool {
        self.pending.is_some() || self.status.is_failed(cx)
    }

    /// Asks the service to follow exactly `served`, the part of the followed set this pane is
    /// reading.
    ///
    /// The caller states the set rather than this reading it, because the caller is the one
    /// that knows *when* the set is being asked for: a settlement that changed what the pane
    /// reads has to ask for the set it has now decided on, and one that only removed a listing
    /// from the followed set has to ask for the set it asked for before.
    ///
    /// Whether the streams of those listings have to be resolved again is **this** widget's
    /// state rather than the caller's argument — see `market_reloaded`. It is given up when the
    /// request is **answered** and not when it is sent: only one request is in flight at a time
    /// ([`WidgetBinding::call`] drops the one before it), and a request that another one
    /// replaced was never handled by the service, so a flag spent on it would leave the streams
    /// it was about resolved at the generation it exists to replace.
    fn replace(&mut self, served: &HashSet<VenueListingKey>, cx: &mut Context<Self>) {
        let mut entries = served.iter().cloned().collect::<Vec<_>>();
        sort_entries(&mut entries);
        let request = if self.market_reloaded {
            WatchListRequest::MarketReloaded { entries }
        } else {
            WatchListRequest::Replace { entries }
        };
        self.binding.call(request, cx, Self::apply_rpc_result);
    }

    /// Starts following a listing the user picked.
    fn add(&mut self, selection: InstrumentSelection, cx: &mut Context<Self>) {
        // The control refuses input in both of these states, and a click that got through
        // anyway — the dialog was confirmed after the pane started waiting, say — must not
        // subscribe to a listing the pane has no market for.
        if self.awaiting_market || self.controls_disabled(cx) {
            return;
        }
        let key = selection.listing;
        if self.config.entries.contains(&key) {
            return;
        }
        if self.config.entries.len() == MAX_WATCH_LIST_ENTRIES {
            self.fail(WatchListFailure::Capacity, cx);
            return;
        }

        self.config.entries.push(key.clone());
        sort_entries(&mut self.config.entries);
        self.config_change.publish(cx);
        // What this pane reads is decided by the markets in memory and not by what the picker
        // offered: the dialog is built when the header is, and a venue that shrank in between
        // leaves the listing it named on a route nobody loaded. Such a listing is followed all
        // the same — the row says which market participant has to be loaded — but it has no
        // stream, exactly like a listing that was already waiting for its market.
        if self.readable(cx).contains(&key) {
            self.entries
                .insert(key.clone(), WatchListEntry::connecting(&key));
        }
        self.pending = Some(PendingOperation::Add(key));
        self.sync_rows(cx);
        let served = self.served(cx);
        self.replace(&served, cx);
    }

    /// Stops following one listing.
    ///
    /// The listing is dropped from the followed set whether or not there is an entry behind
    /// it: a row whose market is not loaded has none — nothing was ever subscribed to it —
    /// and that is exactly the row a user wants to take back out. What the removal replaces
    /// is therefore the entry when there is one, and nothing when there is not, which is
    /// what [`PendingOperation::Remove`] has to put back if the service refuses.
    ///
    /// A pane that is waiting for its markets asks for nothing, so it has no operation in
    /// flight either: the removal is a change to the user's own file, and it is the load that
    /// follows the market being loaded that carries it. That is also why the controls stay
    /// usable — they are disabled by an operation, and there is none.
    fn remove(&mut self, key: &VenueListingKey, cx: &mut Context<Self>) {
        if self.controls_disabled(cx) {
            return;
        }
        let Some(index) = self.config.entries.iter().position(|entry| entry == key) else {
            return;
        };
        let entry = self.entries.remove(key);
        let key = self.config.entries.remove(index);
        self.config_change.publish(cx);

        if self.awaiting_market {
            // Taking the last followed listing out of a list that is waiting for a market
            // leaves a list with no row to say anything with, so the pane goes back to the
            // placeholder it was waiting with — the same one a pane that never followed
            // anything shows, and the one that names the control that loads a market.
            self.pending = None;
            self.sync_rows(cx);
            self.report_waiting(cx);
            return;
        }
        self.pending = Some(PendingOperation::Remove(key, entry.map(Box::new)));
        self.sync_rows(cx);
        // The set the service is asked for is what this pane reads, which the removal may not
        // have changed at all: taking out a listing whose market was not loaded is a change to
        // the user's own file and not to any stream.
        let served = self.served(cx);
        self.replace(&served, cx);
    }

    /// Reopens the stream of one listing the user is retrying.
    fn retry(&mut self, key: &VenueListingKey, cx: &mut Context<Self>) {
        if self.controls_disabled(cx) {
            return;
        }
        let Some(entry) = self.entries.get(key) else {
            return;
        };
        if entry.status() != WatchListStatus::Error {
            return;
        }
        let Some(error) = entry.error().cloned() else {
            return;
        };
        if let Some(entry) = self.entries.get_mut(key) {
            entry.begin_retry();
        }

        self.pending = Some(PendingOperation::Retry {
            key: key.clone(),
            error,
        });
        self.sync_rows(cx);
        self.binding.call(
            WatchListRequest::Retry { key: key.clone() },
            cx,
            Self::apply_rpc_result,
        );
    }

    /// Reports a failure of the widget itself, with the framework's recovery.
    fn fail(&mut self, failure: WatchListFailure, cx: &mut Context<Self>) {
        let recovery = self.recovery.clone();
        self.pending = None;
        self.status
            .report(WidgetState::failed(Arc::new(failure), recovery), cx);
        // A failure also disables the row controls, which is part of what the
        // table draws.
        self.sync_rows(cx);
    }

    /// Applies the service's answer to the one operation that was in flight.
    ///
    /// Confirming an operation is what makes the list content, so an answer is only
    /// worth anything about a market the pane still has. The pane sends its requests
    /// while its routes are in memory, but a revocation can publish the notice before
    /// the answer arrives — the row-level "catalog is not loaded" the stream reports is
    /// exactly that — and confirming here would then report content and clear the
    /// notice the framework is drawing, leaving a body of rows nothing backs.
    ///
    /// Such an answer is dropped rather than remembered. It settles the operation it
    /// belonged to (which `wait_for_market` has already forgotten, since the request it
    /// describes became moot when the notice went up), and what the user asked for is
    /// not lost with it: the change is in `config.entries`, which is precisely the set
    /// the load that follows the next connection asks the venues for again.
    fn apply_rpc_result(
        &mut self,
        result: WidgetRpcResult<WatchListProtocol>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(WatchListResponse::Applied) => {
                // A failure is terminal (§5.2): the answer to a request that was already in
                // flight when the list failed is not a reason to report it as working again —
                // the failure the user is looking at, and the recovery it offers, would be
                // cleared by a status for a read that is over. What the answer confirms is not
                // lost: the change is in `config.entries`, which is what the recovery asks the
                // venues for again.
                if self.status.is_failed(cx) {
                    return;
                }
                // Answered either way, so nothing is in flight any more.
                self.pending = None;
                // The request that was answered is the one the reload flag was waiting for: the
                // streams it reopened were resolved at the retired generation, so the flag has
                // done its work and a later request is a `Replace` again.
                self.market_reloaded = false;
                if self.awaiting_market {
                    self.sync_rows(cx);
                    return;
                }
                self.confirmed = true;
                self.status.report(WidgetState::Ready, cx);
                self.sync_rows(cx);
            }
            Err(error) => {
                // The same terminal rule as the answer above, for the same reason: a refusal
                // that arrives after the list failed is about a request that is already over,
                // and the failure the user is looking at — with the recovery it offers — is
                // not replaced by a later one about the same dead request.
                if self.status.is_failed(cx) {
                    return;
                }
                if let Some(operation) = self.pending.take() {
                    operation.undo(&mut self.config, &mut self.entries);
                    // The followed set is the config, so putting a refused change back
                    // is a config change of its own.
                    self.config_change.publish(cx);
                }
                self.fail(WatchListFailure::Rpc(error), cx);
            }
        }
    }

    /// Applies one message from one listing's stream.
    ///
    /// A failure is terminal: only the recovery the user takes starts the widget
    /// again, so a message that arrives afterwards cannot revive a list that was
    /// reported broken.
    ///
    /// A read the directory answered "this route's catalog is not in memory" to is that
    /// listing's market going away underneath its stream rather than the listing failing: the
    /// row is a wait and not a failure to retry, and the pane goes back to the gate it asks
    /// before it subscribes to anything. Only that listing's row is affected — the listings of
    /// other markets have nothing to do with a route that went away, and a pane that dropped
    /// every row over one route would be hiding markets it can still read.
    fn apply_event(
        &mut self,
        key: &VenueListingKey,
        event: &WatchListStreamEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.status.is_failed(cx) || self.awaiting_market {
            return false;
        }
        if let WatchListStreamEvent::Error(WatchListStreamError::MarketParticipant(error)) = event
            && market_is_catalog_not_loaded(error)
        {
            // The listing stops being read: the route cannot answer for it, so it has no row
            // of its own values any more and the table draws the one that says which market
            // participant is missing. Whether that leaves the pane with nothing to read at all
            // is the gate's question, and it is asked here rather than assumed: a route can
            // lose its catalog while the venue that publishes it stays in memory, which is the
            // state this answer comes from.
            //
            // Nothing is asked of the service here. The key is written down instead, because
            // the stream it was opened for is parked on this error and the service still holds
            // it: the load that reads the listing again is what has to reopen a stream for it,
            // and a request sent from here would be one that cancels whatever operation the
            // user has in flight — a retry it would strand, since the request that replaces a
            // retry cannot revive the stream the retry was for.
            self.entries.remove(key);
            self.dropped.insert(key.clone());
            if self.market_ready(cx) {
                self.sync_rows(cx);
            } else {
                self.wait_for_market(cx);
            }
            return true;
        }
        let changed = self
            .entries
            .get_mut(key)
            .is_some_and(|entry| entry.apply_event(event));
        if changed {
            self.sync_rows(cx);
        }

        changed
    }

    fn apply_reliable(
        &mut self,
        key: VenueListingKey,
        message: Arc<WatchListStreamEvent>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.apply_event(&key, message.as_ref(), cx)
    }

    fn apply_latest(
        &mut self,
        key: VenueListingKey,
        message: Arc<WatchListStreamEvent>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.apply_event(&key, message.as_ref(), cx)
    }

    /// The filters the picker this control opens is built with.
    ///
    /// The loaded venues are read here, where the control is built, rather than kept: the
    /// header is rebuilt every frame, so a scope loaded after this list started reading is a
    /// scope the user may add a listing from, and one a shrinking commit withdrew is not. A
    /// venue commit re-runs [`Self::load`] only while the pane is waiting for a market, so a
    /// reading kept from the last load would stop offering every venue the user loads later.
    /// This is the whole of that rule, and it is what the freshness test asserts.
    fn instrument_filters(&self, cx: &App) -> InstrumentFilters {
        InstrumentFilters {
            kind_filter: InstrumentKindFilter::All,
            status_filter: InstrumentStatusFilter::Any,
            required_capability: CapabilityId::Tickers,
            entry_predicate: None,
            routes: LoadedScope::all(cx.global::<MarketParticipantsConnector>().venues()),
        }
    }

    /// The control that opens the listing selector.
    ///
    /// It opens on nothing in particular: the selector decides which route to
    /// open on when the caller follows none, so the widget never has to make a
    /// route up, and nothing is preselected on the user's behalf.
    ///
    /// A control that refuses input carries no click at all: a disabled button in this
    /// framework is drawn as disabled but a handler would still run, and the dialog this one
    /// opens is an action of its own. [`Self::add`] refuses on its own as well, because the
    /// dialog can be confirmed after the pane has started waiting for its market.
    fn add_button(&self, cx: &Context<Self>) -> AnyElement {
        let disabled = self.awaiting_market || self.controls_disabled(cx);
        let filters = self.instrument_filters(cx);
        let widget = cx.entity().downgrade();

        div()
            .debug_selector(|| "WATCH_LIST_ADD".into())
            .occlude()
            .child(
                Button::new(ElementId::Name(SharedString::new_static("WatchListAdd")))
                    .filled()
                    .small()
                    .disabled(disabled)
                    .aria_label(t!(cx, "widget.watch_list.add"))
                    .when(!disabled, |button| {
                        button.on_click(move |_, window, app| {
                            let widget = widget.clone();
                            InstrumentSelector::show_with_filters(
                                None,
                                filters.clone(),
                                std::rc::Rc::new(move |selection, _, app| {
                                    // The selector does not keep the pane alive: a pane gone by the
                                    // time a listing is confirmed has nothing left to add to.
                                    let _ = widget.update(app, |this, cx| this.add(selection, cx));
                                }),
                                window,
                                app,
                            );
                        })
                    })
                    .child(Icon::new(IconKind::Plus).size_3()),
            )
            .into_any_element()
    }
}

impl Render for WatchListWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The body is only embedded while the widget has content, so this is the
        // ready state and nothing else. Everything the widget is *doing* is drawn
        // by the pane around this.
        //
        // What a waiting row says is read here rather than where the rows are built, because
        // the language a sentence is read in can change while the rows do not: a switch of
        // language rebuilds the frame and nothing else, so a sentence kept from the last time
        // the rows were built would stay in the language it was built in. Reading it again on
        // every frame is what a markdown state is for — it compares the source and parses only
        // when it has changed.
        self.sync_sentences(cx);
        // Which table is drawn is decided by the rows it holds, which is content
        // rather than a state: a table with no rows of its own is not drawn, and the
        // one case where both are empty draws the continuous table so that the empty
        // list says it is empty instead of leaving the body blank. Each table takes
        // half the height when both are drawn, so each one gets a bounded box: the
        // table itself is `size_full`, and two of those side by side in a column would
        // each ask for the whole body.
        let continuous = self.continuous_table.read(cx).rows_count();
        let session = self.session_table.read(cx).rows_count();
        let draw_continuous = continuous > 0 || session == 0;

        div()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .when(draw_continuous, |this| {
                this.child(
                    div()
                        // A stable static id: `aria_label` is only available on an element
                        // that has one, and the id is this table's own scope rather than
                        // anything about the instance the pane is.
                        .id(ElementId::Name(SharedString::new_static(
                            "WatchListTableContinuousFrame",
                        )))
                        .debug_selector(|| "WATCH_LIST_CONTINUOUS_TABLE".into())
                        .aria_label(t!(cx, "widget.watch_list.table"))
                        .flex_1()
                        .min_h_0()
                        .child(Table::new(
                            // The table keeps its own scroll position in state keyed by
                            // this id, so the two tables must not share one and each has
                            // to keep its own across frames.
                            ElementId::Name(SharedString::new_static("WatchListTableContinuous")),
                            &self.continuous_table,
                        )),
                )
            })
            .when(session > 0, |this| {
                this.child(
                    div()
                        .id(ElementId::Name(SharedString::new_static(
                            "WatchListTableSessionFrame",
                        )))
                        .debug_selector(|| "WATCH_LIST_SESSION_TABLE".into())
                        .aria_label(t!(cx, "widget.watch_list.table"))
                        .flex_1()
                        .min_h_0()
                        .child(Table::new(
                            ElementId::Name(SharedString::new_static("WatchListTableSession")),
                            &self.session_table,
                        )),
                )
            })
    }
}

/// A failure of the widget itself, rather than of one followed listing.
///
/// A row reports its own stream failure where the row is. Only what the widget as
/// a whole cannot do is reported through its status, because that is what the
/// framework turns into a banner or a placeholder.
#[derive(Debug)]
enum WatchListFailure {
    /// The service refused an operation.
    Rpc(RpcCallError<WatchListServiceError>),
    /// The list already follows as many listings as it can.
    Capacity,
}

impl LocalizedError for WatchListFailure {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Rpc(error) => error.localized_message(cx),
            Self::Capacity => t!(
                cx,
                "error.watch_list.capacity",
                maximum = MAX_WATCH_LIST_ENTRIES
            ),
        }
    }
}

/// How the stored listing stands against what its route still publishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListingSupport {
    /// The route publishes the listing, and it still accepts followers.
    Offered,
    /// The listing is provably not followable: it left the catalog, its venue settled or
    /// delisted it, or the directory answered that its route is no longer registered.
    Withdrawn,
    /// The route could not be asked, which proves nothing about the listing — including
    /// the case of a reading that did not hold the route, because a load may have
    /// committed between that reading and this question.
    Unknown,
}

/// What the directory answered about one followed listing.
struct ListingAnswer {
    key: VenueListingKey,
    support: ListingSupport,
}

/// What the published catalog answered about one listing.
///
/// Only two answers withdraw a followed listing: the route answered that it does not
/// publish it — including by reporting that it is not a registered route any more — or
/// the entry it publishes is in a terminal lifecycle state, which is a listing no venue
/// serves any more.
///
/// A question nobody asked proves nothing and leaves the stored value alone. The route was
/// not asked when the reading this settlement started with did not carry it, but that reading
/// is not the loaded venues: a load commits on the runtime's own thread, so it may have landed
/// between the reading and this answer. Reading "not in that reading" as a withdrawal
/// would delete a listing from the user's persisted entries over that race.
///
/// What decides it is therefore the live answer, not that reading: a route that is loaded
/// *now* and still serves no tickers is a scope nothing can be followed on, and its
/// listings leave the list — while a route that is not loaded now is a question nobody
/// asked, and its listings stay for the request that follows to settle.
fn listing_support(
    listing: &VenueListingKey,
    offered: Option<&Result<bool, CatalogError>>,
    cx: &App,
) -> ListingSupport {
    match offered {
        // The reading did not hold the route, so there was nothing to ask. Whether that is
        // a withdrawal is a question for the loaded venues, which are read here rather than
        // remembered: a load may have committed since that reading was taken.
        None => {
            let loaded = cx
                .try_global::<MarketParticipantsConnector>()
                .and_then(|connector| connector.venues().scope(&listing.route));
            let serves_tickers = loaded.as_ref().is_some_and(|scope| {
                scope
                    .capabilities
                    .iter()
                    .any(|support| support.capability == CapabilityId::Tickers)
            });
            match loaded {
                // The scope is there and answers no tickers: nothing can be followed on it,
                // so the listing leaves the list rather than being asked for again.
                Some(_) if !serves_tickers => ListingSupport::Withdrawn,
                // The scope is not in memory, so nobody can say what it carries. The listing
                // is the user's and stays: the row it gets says which market participant has
                // to be loaded, and the load that brings that market back settles it.
                _ => ListingSupport::Unknown,
            }
        }
        Some(Ok(true)) => match cx
            .global::<MarketParticipantsConnector>()
            .catalog()
            .cached_listing(listing)
        {
            Some(entry) if entry.snapshot.status.is_terminal() => ListingSupport::Withdrawn,
            _ => ListingSupport::Offered,
        },
        Some(Ok(false)) => ListingSupport::Withdrawn,
        // A route the registry has withdrawn since is a scope nothing can be followed
        // on, and the directory says so itself: the registry snapshot a widget holds was
        // taken when it was built, so relying on it alone would leave the listing in the
        // list until its stream failed.
        Some(Err(CatalogError::RouteNotRegistered { .. })) => ListingSupport::Withdrawn,
        Some(Err(error)) => {
            log::warn!(
                "asking whether `{}` is still published failed; it stays in the list and its \
                 stream reports what went wrong: {error}",
                listing.listing_id
            );
            ListingSupport::Unknown
        }
    }
}

/// The one operation the widget may have in flight.
///
/// The widget applies a change locally before the service confirms it, so this is
/// also the record of what has to be put back when the service refuses it.
enum PendingOperation {
    /// The followed set is being replaced by an explicit or recovery load — or by the
    /// market behind it being loaded again, which asks for the same streams to be
    /// resolved once more — and none of those changes anything locally.
    Load,
    /// A listing was added before the service confirmed the new set.
    Add(VenueListingKey),
    /// A listing was removed before the service confirmed the new set; the entry it had is
    /// kept until the removal is, and is `None` for a listing whose market was not loaded —
    /// there was nothing to keep.
    Remove(VenueListingKey, Option<Box<WatchListEntry>>),
    /// A listing's stream is being reopened; its failure is kept until the retry is
    /// confirmed.
    Retry {
        key: VenueListingKey,
        error: WatchListStreamError,
    },
}

impl PendingOperation {
    /// Puts back what this operation changed, now that the service refused it.
    fn undo(
        self,
        config: &mut WatchListConfig,
        entries: &mut HashMap<VenueListingKey, WatchListEntry>,
    ) {
        match self {
            Self::Load => {}
            Self::Add(key) => {
                config.entries.retain(|entry| entry != &key);
                entries.remove(&key);
            }
            Self::Remove(key, entry) => {
                config.entries.push(key.clone());
                sort_entries(&mut config.entries);
                if let Some(entry) = entry {
                    entries.insert(key, *entry);
                }
            }
            Self::Retry { key, error } => {
                if let Some(entry) = entries.get_mut(&key) {
                    entry.apply_event(&WatchListStreamEvent::Error(error));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        future::pending,
        rc::Rc,
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use app_config::widget::VersionedWidgetConfig;
    /// The layer the application mounts its root and its dialogs under, which a header test needs
    /// for the picker a control opens to be drawn anywhere a test can look.
    use base_ui::layer::Layer;
    use gpui_kit::{
        App, Entity, Modifiers, MouseButton, TestAppContext, VisualTestContext, WeakEntity, Window,
        div, px, size,
    };
    use i18n::language::codes::LanguageCode;
    use market_participants_connector::{
        catalog::{
            batch::CatalogEntry,
            error::{CatalogError, SourceError},
            store::Catalog,
            test_support::{self, Fixture},
        },
        market::error::MarketError,
        venue::definition::{CapabilitySupport, ScopeEntry},
    };
    use market_participants_model::{
        decimal::{Price, Quantity},
        listing::{
            listing_impl::ListingStatus, route::VenueRoute, rules::ListingPrecision,
            venue::PriceBasis,
        },
        market::{
            price::MarketPrice,
            selection::ListingTicker,
            units::{MarketQuantity, QuantityUnit},
        },
    };
    use smol_str::SmolStr;
    use widget::{protocol::RequestId, venue::VenueDisplay};

    use super::*;
    use crate::service::TickerEventSubscription;
    /// A source whose streams are open but never report anything.
    #[derive(Clone, Copy)]
    struct PendingSource;

    struct PendingSubscription;

    impl TickerSource for PendingSource {
        type Subscription = PendingSubscription;

        async fn fetch_ticker(
            &self,
            _listing: VenueListingKey,
        ) -> Result<ListingTicker, MarketError> {
            pending().await
        }

        async fn subscribe_ticker(
            &self,
            _listing: VenueListingKey,
        ) -> Result<Self::Subscription, MarketError> {
            Ok(PendingSubscription)
        }
    }

    impl TickerEventSubscription for PendingSubscription {
        fn precision(&self) -> ListingPrecision {
            precision()
        }

        async fn recv(&mut self) -> Option<Result<Arc<ListingTicker>, MarketError>> {
            pending().await
        }
    }

    /// A source that resolves a subscription through the catalogue the fixture publishes, at the
    /// moment it is asked for, and records the increments that listing was published at then.
    ///
    /// A stream's resolution is what a venue reload replaces, so a test about a reload has to see
    /// both that the list asked for a new stream and which generation's increments that stream was
    /// opened at: this is the one seam where the two are a single record. Each subscription also
    /// carries a flag of its own that is set when it is dropped, because "the source was asked
    /// again" is not the same claim as "the stream that was running is gone".
    #[derive(Clone)]
    struct ResolvingSource {
        catalog: Arc<Catalog>,
        resolutions: Arc<Mutex<Vec<ListingPrecision>>>,
        live: Arc<Mutex<Vec<Arc<AtomicBool>>>>,
    }

    impl ResolvingSource {
        /// The drop flag of every stream it was asked for, in order.
        fn streams(&self) -> Vec<Arc<AtomicBool>> {
            self.live
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    /// One live stream of a resolving source, writing its rows at the increments it resolved.
    struct ResolvedSubscription {
        precision: ListingPrecision,
        dropped: Arc<AtomicBool>,
    }

    impl Drop for ResolvedSubscription {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    impl TickerEventSubscription for ResolvedSubscription {
        fn precision(&self) -> ListingPrecision {
            self.precision
        }

        async fn recv(&mut self) -> Option<Result<Arc<ListingTicker>, MarketError>> {
            pending().await
        }
    }

    impl TickerSource for ResolvingSource {
        type Subscription = ResolvedSubscription;

        async fn fetch_ticker(
            &self,
            _listing: VenueListingKey,
        ) -> Result<ListingTicker, MarketError> {
            Ok(ticker("BTCUSDT", 100, 100))
        }

        async fn subscribe_ticker(
            &self,
            listing: VenueListingKey,
        ) -> Result<Self::Subscription, MarketError> {
            let published = self
                .catalog
                .cached_listing(&listing)
                .expect("the fixture publishes the listing the list follows");
            let precision = ListingPrecision::of(&published.snapshot.rules);
            let dropped = Arc::new(AtomicBool::new(false));
            self.live
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(Arc::clone(&dropped));
            self.resolutions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(precision);
            Ok(ResolvedSubscription { precision, dropped })
        }
    }
    /// The increments every subscription one resolving source was asked for was published at.
    fn resolutions_of(resolutions: &Arc<Mutex<Vec<ListingPrecision>>>) -> Vec<ListingPrecision> {
        resolutions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// A route that answers, and a record of what the widget followed.
    #[derive(Clone, Default)]
    struct AnsweringSource {
        /// The listings the widget subscribed to, in order.
        subscribed: Arc<Mutex<Vec<VenueListingKey>>>,
    }

    impl AnsweringSource {
        fn subscribed(&self) -> Vec<VenueListingKey> {
            self.subscribed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        /// Forgets what it was asked for, so a test can assert about what follows rather than
        /// about what the pane it was built over had already asked for.
        fn forget(&self) {
            self.subscribed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
        }
    }

    impl TickerSource for AnsweringSource {
        type Subscription = PendingSubscription;

        async fn fetch_ticker(
            &self,
            _listing: VenueListingKey,
        ) -> Result<ListingTicker, MarketError> {
            pending().await
        }

        async fn subscribe_ticker(
            &self,
            listing: VenueListingKey,
        ) -> Result<Self::Subscription, MarketError> {
            // Recorded before the stream is handed over, so a test can tell which
            // listings the widget went and followed without a stream ever reporting.
            self.subscribed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(listing);
            Ok(PendingSubscription)
        }
    }

    /// Lets a widget and its service run until what a test is waiting for has happened.
    ///
    /// Two executors are involved: the widget sends its commands from GPUI's own, and the
    /// service answers on the application runtime. So both are given a turn — the widget
    /// first, because a command that has not been sent is a service with nothing to answer.
    fn settle_until(cx: &mut TestAppContext, what: &str, ready: impl Fn(&App) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let handle = cx.update(runtime_handle);
            handle.block_on(async { tokio::task::yield_now().await });
            if cx.update(|cx| ready(cx)) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{what} should happen");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The installed application runtime's handle.
    fn runtime_handle(cx: &mut App) -> tokio::runtime::Handle {
        global::handle(cx).handle().clone()
    }

    /// Counts the venue commits it is told about.
    ///
    /// A test that asserts a pane did *not* act on a commit still has to know the commit reached
    /// it: this probe subscribes the same way and after the pane did, so the delivery it counts is
    /// one the pane's own handler has already seen.
    struct CommitProbe {
        deliveries: Arc<AtomicUsize>,
    }

    impl CommitProbe {
        fn new(
            cx: &mut Context<Self>,
            events: &Entity<market_participants_connector::venue::loaded::VenueEventSource>,
            deliveries: Arc<AtomicUsize>,
        ) -> Self {
            cx.subscribe(events, |probe, _, _: &VenuesUpdated, _| {
                probe.deliveries.fetch_add(1, Ordering::AcqRel);
            })
            .detach();
            Self { deliveries }
        }
    }

    /// Draws one more frame with every cache missed, so what a debug selector reports is what this
    /// frame painted rather than what the frame before it left behind. A paint action is what fills
    /// a selector, and a frame that hits the body cache reuses the last paint range without drawing
    /// (AGENTS.md §10); `window.refresh()` is the one frame that cannot hit it. It matters
    /// for an absence most of all: a reused paint range would keep saying a cell is gone.
    fn repaint(window: &mut VisualTestContext) {
        window.update(|window, _| window.refresh());
        window.run_until_parked();
    }

    struct BodyHarness(Entity<WatchListWidget>);

    impl Render for BodyHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.0.clone())
        }
    }

    struct HeaderHarness(Entity<WatchListWidget>);

    impl Render for HeaderHarness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().children(
                self.0
                    .update(cx, |widget, cx| widget.header(window, cx).left),
            )
        }
    }

    fn route() -> VenueRoute {
        test_support::spot_route()
    }

    /// The scope another venue's load commits, for the commits this list does not follow.
    fn okx_spot_route() -> VenueRoute {
        test_support::route("okx", "spot")
    }

    fn key(symbol: &str) -> VenueListingKey {
        test_support::listing_key(&route(), symbol)
    }

    /// The venue of the second market a mixed-market case loads, and the route the build
    /// quotes a session market on.
    const SECOND_VENUE: &str = "longbridge";
    const SECOND_SCOPE: &str = "us";

    /// The same, on the second venue the mixed-market cases load, which is the one whose
    /// scope declares a session market.
    ///
    /// The key names the venue the scope belongs to as well as the scope: the loaded venues
    /// answer by route, so a session scope read on another venue is a route nobody loaded.
    fn stock(symbol: &str) -> VenueListingKey {
        test_support::listing_key(&test_support::route(SECOND_VENUE, SECOND_SCOPE), symbol)
    }

    fn indexed_key(index: usize) -> VenueListingKey {
        key(&format!("A{index}"))
    }

    fn selection(symbol: &str) -> InstrumentSelection {
        InstrumentSelection {
            listing: key(symbol),
        }
    }

    fn quote() -> AssetId {
        test_support::token("binance", "USDT")
    }

    fn base() -> AssetId {
        test_support::token("binance", "BTC")
    }

    fn price(value: u64) -> MarketPrice {
        MarketPrice {
            value: Price::from(value),
            quote_asset: quote(),
            basis: PriceBasis::PerAssetUnit,
        }
    }

    fn ticker(symbol: &str, open: u64, close: u64) -> ListingTicker {
        ListingTicker {
            listing: key(symbol),
            monotonic_id: Some(1),
            open: price(open),
            high: Some(price(close)),
            low: Some(price(close)),
            close: price(close),
            traded_quantity: Some(MarketQuantity {
                value: Quantity::from(10_u64),
                unit: QuantityUnit::AssetUnits { asset: base() },
            }),
            traded_notional: Some(MarketQuantity {
                value: Quantity::from(1_000_u64),
                unit: QuantityUnit::Notional { asset: quote() },
            }),
        }
    }

    /// The increments the fixture subscription reports for its listing.
    fn precision() -> ListingPrecision {
        ListingPrecision {
            price_tick: "0.01".parse().expect("a price tick should parse"),
            quantity_tick: "0.001".parse().expect("a quantity step should parse"),
        }
    }

    fn config(entries: Vec<VenueListingKey>) -> WatchListConfig {
        WatchListConfig { entries }
    }

    /// An error the service can answer an operation with.
    fn refusal() -> RpcCallError<WatchListServiceError> {
        RpcCallError::Service {
            request_id: RequestId::new(1),
            source: WatchListServiceError::EntryNotActive,
        }
    }

    /// A lookup that failed, which is what a route that did not answer reports.
    fn no_answer() -> CatalogError {
        CatalogError::Source(Box::new(SourceError::Network {
            reason: SmolStr::new_static("the test venue did not answer"),
        }))
    }

    /// Creates a widget the way the framework does, without loading it.
    ///
    /// The market is not published, so a pane built this way waits for it: the tests
    /// that need rows and statuses to drive build theirs over a directory that has
    /// published the listings they follow.
    fn constructed(cx: &mut TestAppContext, config: WatchListConfig) -> Entity<WatchListWidget> {
        built(cx, config, PendingSource, Fixture::unanswering())
    }

    /// The same, over a source a test arranges and a directory it installs.
    fn built<S>(
        cx: &mut TestAppContext,
        config: WatchListConfig,
        source: S,
        directory: Fixture,
    ) -> Entity<WatchListWidget>
    where
        S: TickerSource + Clone,
    {
        built_with_change(cx, config, source, directory, WidgetConfigChange::default())
    }

    /// The same, over a config change a test can count.
    fn built_with_change<S>(
        cx: &mut TestAppContext,
        config: WatchListConfig,
        source: S,
        directory: Fixture,
        change: WidgetConfigChange,
    ) -> Entity<WatchListWidget>
    where
        S: TickerSource + Clone,
    {
        built_market(cx, config, source, directory, change, true)
    }

    /// The same, with the fixture venue loaded or left out of the loaded venues.
    fn built_market<S>(
        cx: &mut TestAppContext,
        config: WatchListConfig,
        source: S,
        directory: Fixture,
        change: WidgetConfigChange,
        loaded: bool,
    ) -> Entity<WatchListWidget>
    where
        S: TickerSource + Clone,
    {
        built_over(cx, config, source, directory, change, |cx, _| {
            install_connector(cx, loaded);
        })
    }

    /// The same, over a registration a test states itself.
    fn built_over_descriptor<S>(
        cx: &mut TestAppContext,
        config: WatchListConfig,
        source: S,
        directory: Fixture,
        change: WidgetConfigChange,
        capabilities: &[CapabilityId],
    ) -> Entity<WatchListWidget>
    where
        S: TickerSource + Clone,
    {
        built_over(
            cx,
            config,
            source,
            directory,
            change,
            move |cx, directory| {
                install_connector_with_capabilities(cx, directory, capabilities);
            },
        )
    }

    /// The same, with the market participants root's own registration left to `install`.
    ///
    /// The directory is built first and the root is wired to the very catalogue it
    /// holds, so the pane's gate and its reads look at one directory rather than two.
    fn built_over<S>(
        cx: &mut TestAppContext,
        config: WatchListConfig,
        source: S,
        directory: Fixture,
        change: WidgetConfigChange,
        install: impl FnOnce(&mut App, &Fixture),
    ) -> Entity<WatchListWidget>
    where
        S: TickerSource + Clone,
    {
        cx.executor().allow_parking();
        let entries = directory.entries().to_vec();
        let route = directory.route().clone();
        // The market is installed and its commit flushed before the list is built, which is the
        // order production has: the user loads the market, and a list opened afterwards reads the
        // generation that load published rather than a commit about a market it was not reading.
        cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            global::install(cx).expect("test Tokio runtime should build");
            let catalog = Arc::clone(directory.catalog_arc());
            market_participants_connector::test_support::install_root(cx, Arc::clone(&catalog));
            install(cx, &directory);
            // The root the harness reads through must be the one that owns the
            // fixture's catalogue: a load that installed a second, empty root would
            // leave the pane reading one directory and the fixture publishing into
            // another, and the pane's own gate cannot tell the two apart.
            assert!(
                Arc::ptr_eq(
                    &cx.global::<MarketParticipantsConnector>().catalog_arc(),
                    &catalog
                ),
                "the installed root must own the fixture's own catalogue"
            );
            // The fixture route is published as the market's catalog: a route the
            // venue answers nothing for is a market whose catalog is not in memory,
            // and that is the state a pane waits in.
            if !entries.is_empty() && catalog.is_registered(&route) {
                test_support::publish_fixture_route(&catalog, &route, entries)
                    .expect("the fixture route should publish its catalog");
            }
        });
        cx.run_until_parked();
        cx.update(|cx| {
            cx.new(|cx| {
                let status = cx.new(|_| WidgetStatus::new());
                let weak: WeakEntity<WatchListWidget> = cx.weak_entity();
                let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
                    let _ = weak.update(cx, |widget, cx| widget.load(cx));
                });

                WatchListWidget::new_with_source(config, source, status, change, recovery, cx)
            })
        })
    }

    /// Installs the market participants root the pane reads its routing from.
    ///
    /// The registry is what says which routes a market has, so a test that drives a
    /// pane connects the fixture venue the way the connection action does: the pane
    /// reads the loaded venues again on every load, and a venue nobody loaded is one it
    /// waits for.
    fn install_connector(cx: &mut App, loaded: bool) {
        // The root the harness installed owns the directory's own catalogue rather
        // than a second, empty one: the pane's gate asks the directory and the
        // service resolves listings against whatever the root owns, so two different
        // catalogues would let a gate/read mismatch pass unnoticed.
        let connector = cx.global::<MarketParticipantsConnector>();
        if loaded {
            connector
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[
                        market_participants_model::identifier::VenueId::from_name(
                            test_support::VENUE,
                        )
                        .expect("the fixture venue is canonical"),
                    ]),
                    &[],
                    &[],
                )
                .expect("the fixture venue's routes should load");
        }
    }

    /// Loads the fixture spot route for `capabilities` on the root the harness installed.
    ///
    /// The load is what the pane reads: a scope that does not carry tickers is a scope
    /// the list can follow nothing on, even though the venue is loaded and its catalog
    /// is in memory — so a test of that state states the capability set itself rather
    /// than taking the one this build speaks. The root the harness installed owns the
    /// fixture's own catalogue, so the pane's gate and its reads share one directory.
    fn install_connector_with_capabilities(
        cx: &mut App,
        directory: &Fixture,
        capabilities: &[CapabilityId],
    ) {
        let venue = market_participants_model::identifier::VenueId::from_name(test_support::VENUE)
            .expect("the fixture venue is canonical");
        let mut carried = Vec::with_capacity(capabilities.len() + 1);
        carried.push(CapabilitySupport {
            capability: CapabilityId::MarketCatalog,
            needs_auth: false,
        });
        carried.extend(capabilities.iter().map(|capability| CapabilitySupport {
            capability: *capability,
            needs_auth: false,
        }));
        let scope = ScopeEntry {
            scope: directory.route().scope.clone(),
            catalog_source: Some(
                market_participants_connector::catalog::source::CatalogSourceId::new(
                    "fixture-catalog",
                )
                .expect("the fixture source id is canonical"),
            ),
            normalization: test_support::case_insensitive_symbols(),
            capabilities: carried,
        };
        let connector = cx.global::<MarketParticipantsConnector>();
        connector
            .load_fixture(&[], &[(venue, vec![scope])], &[])
            .expect("the fixture route should load for the stated capabilities");
    }

    /// A list that follows two markets, only one of which is loaded, reads the one it can:
    /// the rows of the market in memory are the listings the pane subscribed to, and the rows
    /// of the market nobody loaded say which market participant has to be loaded. Both kinds
    /// of row are drawn at once, and each goes to the table of its own route.
    #[gpui_kit::test]
    fn a_list_should_read_the_markets_it_has_and_say_what_the_others_need(cx: &mut TestAppContext) {
        let source = AnsweringSource::default();
        // Only the fixture venue is loaded. The second listing is not one of a route nobody
        // registered — the venue carries that scope — it is one whose market participant the
        // user has not loaded, which is the state the pane has to draw a row for.
        let widget = built_over(
            cx,
            config(vec![key("BTCUSDT"), stock("AAPL")]),
            source.clone(),
            directory(&[entry("BTCUSDT", "BTC")]),
            WidgetConfigChange::default(),
            |cx, _| install_connector(cx, true),
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        cx.update(|cx| {
            let widget = widget.read(cx);
            assert!(
                !widget.awaiting_market,
                "there is a market to read, so the pane is not waiting for one"
            );
            assert_eq!(
                widget.entries.keys().collect::<Vec<_>>(),
                vec![&key("BTCUSDT")],
                "the listing whose scope is in memory is the one the pane follows"
            );
            assert_eq!(
                widget.continuous_table.read(cx).rows_count(),
                1,
                "and it is drawn in the table of its own market"
            );
            assert_eq!(
                widget.session_table.read(cx).rows_count(),
                1,
                "beside the listing that waits for the market nobody loaded"
            );
        });
        cx.update(|cx| {
            assert!(
                row_of(&widget, 0, cx).state().is_read(),
                "the row of the market in memory is a stream rather than a message"
            );
            assert_eq!(
                row_of(&widget, 1, cx).state(),
                &WatchListRowState::AwaitingMarket,
                "while the other row says which market participant has to be loaded"
            );
        });
        // What the service is asked to follow is what the pane can read: the listing whose
        // market is not loaded is not in the request, because there is no stream for it to
        // open — a request that named it would be one the venue has nothing to answer.
        settle_until(cx, "the list to subscribe", |_| {
            !source.subscribed().is_empty()
        });
        let mut subscribed = source.subscribed();
        sort_entries(&mut subscribed);
        assert_eq!(
            subscribed,
            vec![key("BTCUSDT")],
            "and the listing waiting for its market is not asked for"
        );
    }

    /// A widget loaded over a source a test arranges, with nothing answered by hand.
    fn widget_over<S>(
        cx: &mut TestAppContext,
        config: WatchListConfig,
        source: S,
        directory: Fixture,
    ) -> Entity<WatchListWidget>
    where
        S: TickerSource + Clone,
    {
        let widget = built(cx, config, source, directory);
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        widget
    }

    /// Creates a widget in the state an accepted load leaves it in.
    ///
    /// The load the framework starts is answered over the runtime, and when that
    /// answer lands is not something a test can decide. The test answers it here
    /// instead; `PendingSource` accepts every subscription, so the request being
    /// answered for real later can only answer the same thing.
    fn widget(cx: &mut TestAppContext, config: WatchListConfig) -> Entity<WatchListWidget> {
        let widget = constructed(cx, config);
        cx.update(|cx| {
            widget.update(cx, |widget, cx| {
                widget.load(cx);
                widget.apply_rpc_result(Ok(WatchListResponse::Applied), cx);
            });
        });
        widget
    }

    /// A widget that already follows one listing.
    fn scoped_widget(cx: &mut TestAppContext) -> Entity<WatchListWidget> {
        // The listing it follows is one the fixture venue publishes, so the pane has
        // the row every test below drives.
        let widget = built(
            cx,
            config(vec![key("BTCUSDT")]),
            PendingSource,
            directory(&[entry("BTCUSDT", "BTC")]),
        );
        cx.update(|cx| {
            widget.update(cx, |widget, cx| {
                widget.load(cx);
                widget.apply_rpc_result(Ok(WatchListResponse::Applied), cx);
            });
        });
        widget
    }

    /// The same, over a directory that has published the given entries.
    ///
    /// The display codes of a generation are decided from the listings that
    /// generation publishes, so a test that asserts an asset *name* has to publish
    /// a listing denominated in that asset first: a directory that has published
    /// nothing names nothing.
    fn scoped_widget_over(
        cx: &mut TestAppContext,
        entries: &[CatalogEntry],
    ) -> Entity<WatchListWidget> {
        cx.update(|cx| global::install(cx).expect("test Tokio runtime should build"));
        let directory = directory(entries);
        cx.update(|cx| {
            global::handle(cx)
                .handle()
                .block_on(directory.ensure())
                .expect("the fixture spot route should publish its catalog");
        });
        let widget = built(cx, config(vec![key("BTCUSDT")]), PendingSource, directory);
        cx.update(|cx| {
            widget.update(cx, |widget, cx| {
                widget.load(cx);
                widget.apply_rpc_result(Ok(WatchListResponse::Applied), cx);
            });
        });
        widget
    }

    fn status(widget: &Entity<WatchListWidget>, cx: &App) -> WidgetState {
        let status = widget.read(cx).status.clone();
        status.state(cx)
    }

    fn failure(widget: &Entity<WatchListWidget>, cx: &App) -> Option<SharedString> {
        status(widget, cx).failure().map(|failure| failure.text(cx))
    }

    /// The rows both tables draw, in the order the two are drawn in.
    ///
    /// The fixture listings are continuous ones, so every existing assertion about the
    /// rows of a widget reads the same rows it read before the split; the sum is what
    /// keeps that true for a test that follows a listing of the other market as well.
    fn rows(widget: &Entity<WatchListWidget>, cx: &App) -> usize {
        let widget = widget.read(cx);
        widget.continuous_table.read(cx).rows_count() + widget.session_table.read(cx).rows_count()
    }

    /// One row, numbered across both tables: the continuous table's rows first, then the
    /// session table's, which is the order they are drawn in.
    fn row_of(widget: &Entity<WatchListWidget>, index: usize, cx: &App) -> Arc<WatchListRow> {
        let widget = widget.read(cx);
        let continuous = &widget.continuous_table.read(cx).rows;
        if index < continuous.len() {
            return continuous[index].clone();
        }
        widget.session_table.read(cx).rows[index - continuous.len()].clone()
    }

    /// Delivers a reliable message the way the binding does.
    fn publish_reliable(
        widget: &Entity<WatchListWidget>,
        key: &VenueListingKey,
        event: WatchListStreamEvent,
        cx: &mut App,
    ) {
        widget.update(cx, |widget, cx| {
            let _ = widget.apply_reliable(key.clone(), Arc::new(event), cx);
        });
    }

    /// Delivers a latest-value message the way the binding does.
    fn publish_latest(
        widget: &Entity<WatchListWidget>,
        key: &VenueListingKey,
        event: WatchListStreamEvent,
        cx: &mut App,
    ) {
        widget.update(cx, |widget, cx| {
            let _ = widget.apply_latest(key.clone(), Arc::new(event), cx);
        });
    }

    /// Commits one ticker to one row, the way its stream does.
    fn accept_ticker(
        widget: &Entity<WatchListWidget>,
        symbol: &str,
        open: u64,
        close: u64,
        cx: &mut App,
    ) {
        let key = key(symbol);
        publish_latest(
            widget,
            &key,
            WatchListStreamEvent::Snapshot {
                ticker: Arc::new(ticker(symbol, open, close)),
                precision: precision(),
            },
            cx,
        );
        publish_reliable(
            widget,
            &key,
            WatchListStreamEvent::Status(WatchListStatus::Ready),
            cx,
        );
    }

    /// Reports a failed stream for one row.
    fn fail_stream(widget: &Entity<WatchListWidget>, symbol: &str, cx: &mut App) {
        publish_reliable(
            widget,
            &key(symbol),
            WatchListStreamEvent::Error(WatchListStreamError::MarketParticipant(
                MarketError::ServiceClosed,
            )),
            cx,
        );
    }

    fn price_of(row: &WatchListRow) -> Option<String> {
        row.display().map(|display| display.price.to_string())
    }

    /// The row of a listing the user follows is content from the moment the load starts:
    /// what the user asked for is the list, not the service's answer about it, and a row
    /// that is connecting says so in its own instrument column. What the body draws while
    /// the answer is in flight is therefore the table, under the placeholder's own
    /// subject — and the framework draws no placeholder over content.
    #[gpui_kit::test]
    fn a_widget_should_report_connecting_with_content_from_the_load_it_started(
        cx: &mut TestAppContext,
    ) {
        let widget = widget_over(
            cx,
            config(vec![key("BTCUSDT")]),
            PendingSource,
            directory(&[entry("BTCUSDT", "BTC")]),
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));

        cx.update(|cx| {
            assert!(
                widget.read(cx).has_content(),
                "the listing the user follows is a row, whatever its stream is doing"
            );
            assert_eq!(rows(&widget, cx), 1);
            assert_eq!(status(&widget, cx), WidgetState::Connecting);
            assert!(
                status(&widget, cx).failure().is_none(),
                "a load in flight is work, not a failure"
            );
            assert!(
                status(&widget, cx).notice().is_none(),
                "a pane that draws its rows is not waiting for a placeholder"
            );
        });
    }

    #[gpui_kit::test]
    fn an_accepted_replacement_should_become_content_and_clear_the_status(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);

        cx.update(|cx| {
            assert!(widget.read(cx).has_content());
            assert_eq!(status(&widget, cx), WidgetState::Ready);
            assert!(status(&widget, cx).failure().is_none());
            assert_eq!(rows(&widget, cx), 1);
        });
    }

    /// Connecting successfully is enough to show the table, and a list with
    /// nothing in it is content — an empty table — not a state to report: the
    /// table is what says it holds nothing.
    #[gpui_kit::test]
    fn an_empty_list_should_be_content_and_report_nothing(cx: &mut TestAppContext) {
        let widget = widget(cx, WatchListConfig::default());

        cx.update(|cx| {
            assert!(widget.read(cx).has_content());
            assert_eq!(rows(&widget, cx), 0);
            // An empty list draws one table — the continuous one — so that is the dataset
            // whose emptiness has to be sayable; the session table is not in the element
            // tree at all while both are empty.
            let table = widget.read(cx).continuous_table.clone();
            assert!(
                TableDataSet::<COLUMN_COUNT>::empty(table.read(cx), cx).is_some(),
                "an empty list has to say it is empty rather than leave its body blank"
            );
            let status = status(&widget, cx);
            assert_eq!(status, WidgetState::Ready);
            assert!(status.failure().is_none());
            assert!(
                status.notice().is_none(),
                "the empty state belongs to the table, not to the framework's placeholder"
            );
        });
    }

    /// Emptying a list the service has already confirmed leaves the empty table, not a
    /// placeholder: the user asked for the last listing to go, so "nothing is followed"
    /// is their own answer and the table is what says it. This is the one state that
    /// `has_content` decides from the confirmation rather than from the followed set.
    #[gpui_kit::test]
    fn removing_the_last_listing_of_a_confirmed_list_should_leave_the_empty_table(
        cx: &mut TestAppContext,
    ) {
        let widget = scoped_widget(cx);
        cx.update(|cx| widget.update(cx, |widget, cx| widget.remove(&key("BTCUSDT"), cx)));

        cx.update(|cx| {
            assert!(
                widget.read(cx).config.entries.is_empty(),
                "the user took the listing out"
            );
            assert!(
                widget.read(cx).confirmed,
                "and the service had already agreed to the set it left"
            );
            assert!(
                widget.read(cx).has_content(),
                "an empty list is the empty table rather than a placeholder"
            );
            assert_eq!(rows(&widget, cx), 0);
            assert!(
                !status(&widget, cx).is_failed(),
                "removing a listing is not a failure of the list"
            );
        });
    }

    /// Taking a listing out of a list whose market is not loaded works: the row is drawn,
    /// the user is entitled to drop it, and there is no stream behind it to tell the service
    /// about — the removal is a change to the followed set, which is the user's own file.
    #[gpui_kit::test]
    fn a_listing_should_be_removable_while_its_market_is_not_loaded(cx: &mut TestAppContext) {
        let published = Rc::new(Cell::new(0));
        let widget = built_market(
            cx,
            config(vec![key("BTCUSDT")]),
            PendingSource,
            Fixture::unanswering(),
            WidgetConfigChange::new({
                let published = published.clone();
                move |_| published.set(published.get() + 1)
            }),
            false,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        cx.update(|cx| {
            assert!(
                widget.read(cx).awaiting_market,
                "the market behind the listing is not loaded"
            );
            assert!(
                !widget.read(cx).entries.contains_key(&key("BTCUSDT")),
                "and nothing was ever subscribed to it"
            );

            widget.update(cx, |widget, cx| widget.remove(&key("BTCUSDT"), cx));

            assert!(
                widget.read(cx).config.entries.is_empty(),
                "the listing the user dropped left the followed set"
            );
            assert_eq!(
                rows(&widget, cx),
                0,
                "and its row left both tables rather than staying behind"
            );
            assert!(
                !status(&widget, cx).is_failed(),
                "removing what the user follows is not a failure"
            );
            assert!(
                widget.read(cx).pending.is_none(),
                "and nothing was asked of the service, so there is no operation in flight to \
                 wait on: the change is in the user's own file"
            );
            assert!(
                !widget.read(cx).has_content(),
                "a list that follows nothing and was never read has no listing to draw, so \
                 what stands in for its body is the framework's placeholder"
            );
            assert!(
                status(&widget, cx)
                    .notice()
                    .is_some_and(|notice| notice.resolve(cx).contains("Binance")),
                "and that placeholder is the sentence that names the markets it could load, \
                 not the spinner of a pane whose read is still on its way: {}",
                status(&widget, cx).notice().map_or_else(
                    || "no notice at all".to_owned(),
                    |notice| notice.resolve(cx).to_string()
                )
            );
        });
        assert_eq!(
            published.get(),
            1,
            "the removal is a configuration change and is written back"
        );
    }

    /// A refusal is a failure of the list as a whole, and the row the user asked for is
    /// still the row they follow: `has_content` is about what the list holds rather than
    /// about what the service answered, so the failure is drawn as the banner over the
    /// row rather than as a placeholder that hides it.
    #[gpui_kit::test]
    fn a_refused_replacement_should_report_a_failure_over_the_row_it_kept(cx: &mut TestAppContext) {
        let widget = constructed(cx, config(vec![key("BTCUSDT")]));
        cx.update(|cx| {
            widget.update(cx, |widget, cx| {
                widget.load(cx);
                widget.apply_rpc_result(Err(refusal()), cx);
            });
        });

        cx.update(|cx| {
            assert!(widget.read(cx).has_content());
            assert_eq!(rows(&widget, cx), 1);
            assert_eq!(
                failure(&widget, cx).map(|text| text.to_string()),
                Some("The instrument is no longer in the watch list".to_owned())
            );
            let status = status(&widget, cx);
            assert!(
                status
                    .failure()
                    .map(|failure| failure.action())
                    .is_some_and(WidgetAction::is_enabled),
                "every failure offers the way out of it"
            );
        });
    }

    #[gpui_kit::test]
    fn a_committed_ticker_should_fill_the_row(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);

        cx.update(|cx| {
            assert_eq!(price_of(&row_of(&widget, 0, cx)), None);
            accept_ticker(&widget, "BTCUSDT", 100, 120, cx);

            let row = row_of(&widget, 0, cx);
            assert_eq!(row.status(), WatchListStatus::Ready);
            assert_eq!(price_of(&row).as_deref(), Some("120.00"));
            assert_eq!(
                row.display()
                    .map(|display| display.change_percent.to_string()),
                Some("20.00%".to_owned())
            );
        });
    }

    /// A failing listing is the row's own business: the widget keeps everything
    /// else and reports nothing about the list as a whole.
    #[gpui_kit::test]
    fn a_failed_row_should_keep_its_ticker_and_leave_the_widget_alone(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);

        cx.update(|cx| {
            accept_ticker(&widget, "BTCUSDT", 100, 120, cx);
            fail_stream(&widget, "BTCUSDT", cx);

            let row = row_of(&widget, 0, cx);
            assert_eq!(row.status(), WatchListStatus::Error);
            assert_eq!(price_of(&row).as_deref(), Some("120.00"));
            assert!(widget.read(cx).has_content());
            assert!(!status(&widget, cx).is_failed());
        });
    }

    /// A row whose stream answers "this route's catalog is not in memory" is not a failed
    /// row: the market went away underneath the read, so that listing stops being read and
    /// its row says which market participant has to be loaded. No retry control is offered for
    /// a listing that is not what went wrong. The stored set is left alone: nothing about the
    /// market says anything about what the route publishes.
    ///
    /// The row is all that is lost. The venue is still in memory — what it lost is its
    /// directory — so the pane is not waiting for a market and the listings of the routes that
    /// did answer keep being read.
    #[gpui_kit::test]
    fn a_row_whose_market_has_no_catalog_should_stop_being_read(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);

        cx.update(|cx| {
            accept_ticker(&widget, "BTCUSDT", 100, 120, cx);
            assert!(widget.read(cx).has_content(), "the row is what arrived");

            publish_reliable(
                &widget,
                &key("BTCUSDT"),
                WatchListStreamEvent::Error(WatchListStreamError::MarketParticipant(
                    MarketError::Catalog(Box::new(CatalogError::CatalogNotLoaded {
                        route: route(),
                    })),
                )),
                cx,
            );

            let status = status(&widget, cx);
            assert!(
                status.failure().is_none(),
                "a catalog that has not loaded is a wait, not a failure"
            );
            assert!(
                status.notice().is_none(),
                "the row itself is what says which market participant is missing"
            );
            assert!(
                !widget.read(cx).entries.contains_key(&key("BTCUSDT")),
                "the listing whose catalog is not in memory has nothing behind it"
            );
            assert!(
                !widget.read(cx).awaiting_market,
                "and the venue it is on is still in memory, so the pane is not waiting for one"
            );
            assert!(
                widget.read(cx).has_content(),
                "the listing the user follows is still drawn"
            );
            let row = row_of(&widget, 0, cx);
            assert_eq!(
                row.state(),
                &WatchListRowState::AwaitingMarket,
                "and the row says what it is waiting for rather than showing what it had"
            );
            assert_eq!(
                widget.read(cx).config.entries,
                vec![key("BTCUSDT")],
                "waiting for a market proves nothing about what the route publishes"
            );
        });
    }

    /// An answer that was already in flight when the market went away is an answer about
    /// a market this pane no longer has.
    ///
    /// The service did follow the set it was sent, but nothing it followed can be read any
    /// more, so treating the answer as content would report a body of values no market
    /// backs. The rows stay, because the listings are the user's and the row that says so
    /// is what each listing is worth while the market is gone.
    ///
    /// Waiting for a market is not a failure either, so the pane is not failed.
    #[gpui_kit::test]
    fn a_confirmation_that_lands_while_the_market_is_gone_should_not_report_values(
        cx: &mut TestAppContext,
    ) {
        let widget = scoped_widget(cx);

        cx.update(|cx| {
            accept_ticker(&widget, "BTCUSDT", 100, 120, cx);
            assert!(widget.read(cx).has_content(), "the row is what arrived");
            publish_reliable(
                &widget,
                &key("BTCUSDT"),
                WatchListStreamEvent::Error(WatchListStreamError::MarketParticipant(
                    MarketError::Catalog(Box::new(CatalogError::CatalogNotLoaded {
                        route: route(),
                    })),
                )),
                cx,
            );
            assert!(
                !widget.read(cx).entries.contains_key(&key("BTCUSDT")),
                "the market went away underneath the row"
            );

            // The replacement the pane asked for while it still had the market is
            // answered only now.
            widget.update(cx, |widget, cx| {
                widget.apply_rpc_result(Ok(WatchListResponse::Applied), cx);
            });

            assert!(
                !widget.read(cx).entries.contains_key(&key("BTCUSDT")),
                "an answer about a market that is gone does not make the pane read again"
            );
            assert_eq!(
                price_of(&row_of(&widget, 0, cx)),
                None,
                "the row it drew from that market carries no values any more"
            );
            assert!(
                status(&widget, cx).notice().is_none(),
                "and the pane is not waiting for a placeholder over its own rows"
            );
            assert!(
                !status(&widget, cx).is_failed(),
                "waiting for a market is a wait, not a failure"
            );
            assert!(
                widget.read(cx).pending.is_none(),
                "the answer settles the operation it belonged to"
            );
        });
    }

    /// A listing whose catalog is gone answers while this pane is still settling the load that
    /// started before it: the answer removes that one listing's entry, and the request the load
    /// owes is still sent — for the listings that are left. A pane that skipped its request
    /// because a set it decided on had changed under it would wait for an answer that never
    /// comes, with every control disabled by the operation it is waiting on.
    #[gpui_kit::test]
    fn a_catalog_answer_that_lands_before_the_settlement_should_not_stop_the_request(
        cx: &mut TestAppContext,
    ) {
        let source = AnsweringSource::default();
        let widget = built(
            cx,
            config(vec![key("BTCUSDT"), key("ETHUSDT")]),
            source.clone(),
            directory(&[entry("BTCUSDT", "BTC"), entry("ETHUSDT", "ETH")]),
        );

        cx.update(|cx| {
            // Both answers arrive in the same turn: the settlement of the load that was just
            // started has not run yet, and the error takes one listing's entry away first.
            widget.update(cx, |widget, cx| {
                widget.load(cx);
                widget.apply_event(
                    &key("BTCUSDT"),
                    &WatchListStreamEvent::Error(WatchListStreamError::MarketParticipant(
                        MarketError::Catalog(Box::new(CatalogError::CatalogNotLoaded {
                            route: route(),
                        })),
                    )),
                    cx,
                );
            });
        });
        // Driven until the request the load owes has been sent and answered: what this pins is
        // that both happen at all, so the pane's own bookkeeping is what is waited for.
        settle_until(cx, "the pane to be answered", |cx| {
            widget.read(cx).pending.is_none()
        });
        settle_until(cx, "the healthy listing to be subscribed", |_| {
            source.subscribed().contains(&key("ETHUSDT"))
        });

        cx.update(|cx| {
            let widget = widget.read(cx);
            assert!(
                widget.pending.is_none(),
                "the load's own request was sent and answered, so the pane is not waiting"
            );
            assert!(
                !widget.entries.contains_key(&key("BTCUSDT")),
                "the listing whose catalog is gone is not read"
            );
            assert!(
                widget.entries.contains_key(&key("ETHUSDT")),
                "and the other one is"
            );
            assert!(
                !widget.continuous_table.read(cx).disabled
                    && !widget.session_table.read(cx).disabled,
                "so its controls work again rather than staying disabled forever"
            );
        });
        cx.update(|cx| {
            assert_ne!(
                status(&widget, cx),
                WidgetState::Connecting,
                "the pane is not left connecting for an answer nothing is going to give"
            );
        });
    }

    /// Taking a listing out of a pane that is waiting for its market reports what the list now
    /// is: a list with rows left waits with them, and one left with nothing says so through the
    /// framework's placeholder. A status that was a failure before the removal is replaced by
    /// that answer — the rows on screen are not a failure, and a failed pane is not one a venue
    /// commit starts over.
    ///
    /// Nothing is asked of the service on the way, so nothing is in flight and the controls stay
    /// usable: a user who opens a list whose market is not loaded can take every listing back
    /// out of it, one after another, without ever having loaded that market.
    #[gpui_kit::test]
    fn removing_a_waiting_row_should_report_what_the_list_now_is(cx: &mut TestAppContext) {
        let widget = built_market(
            cx,
            config(vec![key("BTCUSDT"), key("ETHUSDT")]),
            PendingSource,
            Fixture::unanswering(),
            WidgetConfigChange::default(),
            false,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));

        cx.update(|cx| {
            assert!(widget.read(cx).awaiting_market, "no market is loaded");
            assert_eq!(
                rows(&widget, cx),
                2,
                "both listings are drawn as waiting rows"
            );

            widget.update(cx, |widget, cx| widget.remove(&key("BTCUSDT"), cx));

            assert_eq!(rows(&widget, cx), 1, "the row the user dropped is gone");
            assert!(
                widget.read(cx).pending.is_none(),
                "and nothing was asked of the service, so nothing is in flight"
            );
            assert!(
                !widget.read(cx).continuous_table.read(cx).disabled
                    && !widget.read(cx).session_table.read(cx).disabled,
                "which leaves the controls on the rows that are left usable"
            );
            assert_eq!(
                status(&widget, cx),
                WidgetState::Ready,
                "and a pane with a row left says what is missing in that row"
            );
            assert!(
                status(&widget, cx).notice().is_none(),
                "rather than putting a placeholder over rows it has"
            );

            // The second removal of a waiting list happens without anything having answered
            // the first: there was nothing to answer.
            widget.update(cx, |widget, cx| widget.remove(&key("ETHUSDT"), cx));

            assert!(
                !widget.read(cx).has_content(),
                "the list follows nothing now, so it has nothing to draw"
            );
            assert!(
                status(&widget, cx).notice().is_some(),
                "so what stands in for its body is the sentence that names the markets it \
                 could load"
            );
        });
    }

    /// A recovery the user takes while no market is loaded does not leave the failure it
    /// replaced on screen: the recovery asks the gate again, and the answer is the waiting
    /// state — rows that say which market participant has to be loaded, and a state with no
    /// failure in it. A failure left behind would be terminal, and a failed pane is not one a
    /// venue commit starts over, so the user would have to take the recovery twice.
    #[gpui_kit::test]
    fn a_recovery_with_no_market_loaded_should_wait_rather_than_stay_failed(
        cx: &mut TestAppContext,
    ) {
        let widget = built_market(
            cx,
            config(vec![key("BTCUSDT")]),
            PendingSource,
            Fixture::unanswering(),
            WidgetConfigChange::default(),
            false,
        );
        // The service refuses the set, which is the failure the user then recovers from.
        let failure = cx.update(|cx| {
            widget.update(cx, |widget, cx| {
                widget.load(cx);
                widget.apply_rpc_result(Err(refusal()), cx);
            });
            status(&widget, cx)
        });
        assert!(failure.is_failed(), "the list is in its terminal failure");

        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));

        cx.update(|cx| {
            assert!(
                !status(&widget, cx).is_failed(),
                "the failure the recovery replaced is not left on screen: {:?}",
                status(&widget, cx)
            );
            assert!(
                widget.read(cx).awaiting_market,
                "the pane waits for the market it cannot read"
            );
            assert_eq!(
                rows(&widget, cx),
                1,
                "and what it draws is the row that says what is missing"
            );
            assert_eq!(
                status(&widget, cx),
                WidgetState::Ready,
                "which the row says in words of its own rather than standing behind a \
                 placeholder"
            );
        });
    }

    /// A pane that has failed is not one to start streams for: the settlement its load started
    /// is skipped, and so is the request that load owes — a request from a failed pane would
    /// subscribe the service to listings whose every message the pane then drops.
    #[gpui_kit::test]
    fn a_failed_pane_should_not_ask_the_service_for_anything(cx: &mut TestAppContext) {
        let source = AnsweringSource::default();
        let widget = built(
            cx,
            config(vec![key("BTCUSDT")]),
            source.clone(),
            directory(&[entry("BTCUSDT", "BTC")]),
        );
        // The load the pane was built over has already been answered for; what this test is
        // about is what the failed pane does with the load it is given next.
        settle_source(cx);
        source.forget();

        cx.update(|cx| {
            widget.update(cx, |widget, cx| {
                widget.load(cx);
                // The refusal lands in the same turn, before the settlement the load spawned
                // has had a chance to run.
                widget.apply_rpc_result(Err(refusal()), cx);
            });
        });
        // Both executors are given their turns for longer than a request takes to arrive, so a
        // request that was sent would have reached the source by the time this returns. What is
        // waited for is a deadline and not a state: every state this pane passes through on the
        // way is reached without the request.
        settle_source(cx);

        cx.update(|cx| {
            assert!(
                status(&widget, cx).is_failed(),
                "the list is in its terminal failure"
            );
        });
        assert!(
            source.subscribed().is_empty(),
            "a failed pane asked the service to follow {:?}",
            source.subscribed()
        );
    }

    /// A pane that went back to waiting while its load was settling does not send the request
    /// that load owed either: it reads nothing, so it asks for nothing — and the request it
    /// would send is the empty one, which would cancel the streams waiting deliberately keeps.
    #[gpui_kit::test]
    fn a_waiting_pane_should_not_ask_the_service_for_anything(cx: &mut TestAppContext) {
        let source = AnsweringSource::default();
        let widget = built(
            cx,
            config(vec![key("BTCUSDT")]),
            source.clone(),
            directory(&[entry("BTCUSDT", "BTC")]),
        );
        cx.update(|cx| {
            widget.update(cx, |widget, cx| {
                widget.load(cx);
                // The market goes away under the load, in the same turn: the pane is waiting
                // before the settlement its load spawned runs.
                let venue =
                    market_participants_model::identifier::VenueId::from_name(test_support::VENUE)
                        .expect("the fixture venue is canonical");
                cx.global::<MarketParticipantsConnector>()
                    .load_fixture(&[], &[(venue, Vec::new())], &[])
                    .expect("the shrinking commit lands");
                widget.wait_for_market(cx);
            });
        });
        // Both executors are given their turns for longer than a request takes to arrive, so a
        // request that was sent would have reached the source by the time this returns. What is
        // waited for is a deadline and not a state: every state this pane passes through on the
        // way is reached without the request.
        settle_source(cx);

        cx.update(|cx| {
            assert!(widget.read(cx).awaiting_market, "the pane waits");
        });
        assert!(
            source.subscribed().is_empty(),
            "a waiting pane asked the service to follow {:?}",
            source.subscribed()
        );
    }

    /// Runs both executors for longer than a request takes to arrive, and answers nothing.
    ///
    /// A test that asserts a pane did *not* ask the service for anything cannot wait for a
    /// state: every state the pane passes through on the way — failed, waiting — is reached
    /// without a request. So it waits for a deadline instead, long enough that a request which
    /// was going to be sent would have been delivered, and then asserts on what the source
    /// holds.
    fn settle_source(cx: &mut TestAppContext) {
        let deadline = std::time::Instant::now() + Duration::from_millis(250);
        while std::time::Instant::now() < deadline {
            cx.run_until_parked();
            let handle = cx.update(runtime_handle);
            handle.block_on(async { tokio::task::yield_now().await });
        }
    }

    /// A catalog error in one row is not a reason to make a request: another row's row may have
    /// an operation of the user's in flight, and a request sent from here would be the one that
    /// replaces it. What the branch writes down instead is the listing, so the next load reads it
    /// through a stream the service opens rather than the parked one it left behind.
    ///
    /// What this pins is that the pane is still waiting on the user's own operation afterwards,
    /// and that the listing whose catalog went away is still a row that waits.
    #[gpui_kit::test]
    fn a_catalog_error_should_not_take_a_retry_with_it(cx: &mut TestAppContext) {
        let widget = scoped_widget_over(cx, &[entry("BTCUSDT", "BTC"), entry("ETHUSDT", "ETH")]);
        cx.update(|cx| {
            widget.update(cx, |widget, cx| {
                widget.add(selection("ETHUSDT"), cx);
                widget.apply_rpc_result(Ok(WatchListResponse::Applied), cx);
            });
        });

        cx.update(|cx| {
            accept_ticker(&widget, "BTCUSDT", 100, 120, cx);
            accept_ticker(&widget, "ETHUSDT", 10, 12, cx);
            fail_stream(&widget, "ETHUSDT", cx);
            widget.update(cx, |widget, cx| widget.retry(&key("ETHUSDT"), cx));
            assert!(
                matches!(
                    widget.read(cx).pending,
                    Some(PendingOperation::Retry { .. })
                ),
                "the retry is the operation in flight"
            );

            // The other row's catalog goes away while that retry is still unanswered.
            publish_reliable(
                &widget,
                &key("BTCUSDT"),
                WatchListStreamEvent::Error(WatchListStreamError::MarketParticipant(
                    MarketError::Catalog(Box::new(CatalogError::CatalogNotLoaded {
                        route: route(),
                    })),
                )),
                cx,
            );

            assert!(
                !widget.read(cx).entries.contains_key(&key("BTCUSDT")),
                "the row that says what is missing stopped being read"
            );
            assert!(
                matches!(
                    widget.read(cx).pending,
                    Some(PendingOperation::Retry { .. })
                ),
                "and it did not cancel the retry the user asked for: {}",
                widget.read(cx).pending.is_some()
            );

            // The retry is answered as the user's own answer, which is what tells the two
            // requests apart: an answer to a request of the pane's own would have gone through
            // `apply_rpc_result` before this ran and cleared the operation.
            widget.update(cx, |widget, cx| {
                widget.apply_rpc_result(Ok(WatchListResponse::Applied), cx);
            });
            assert!(widget.read(cx).pending.is_none());
            assert!(
                !widget.read(cx).awaiting_market,
                "and the pane is still reading the market both rows are on"
            );
            assert!(
                !widget.read(cx).entries.contains_key(&key("BTCUSDT")),
                "while the listing whose catalog went away stays a row that waits"
            );
        });
    }

    /// The other half: a listing whose stream went dead on its own is read again by a stream the
    /// service opens, rather than being handed back the one it left behind. The key stays in the
    /// service's set until something asks for the streams again, and that something is the load
    /// that reads the listing — nothing else can, because the row that says what is missing is
    /// not a failure and offers no control of its own.
    #[gpui_kit::test]
    fn a_listing_whose_catalog_went_away_should_be_read_by_a_new_stream(cx: &mut TestAppContext) {
        let resolutions = Arc::new(Mutex::new(Vec::new()));
        let directory = directory(&[entry("BTCUSDT", "BTC")]);
        let source = ResolvingSource {
            catalog: Arc::clone(directory.catalog_arc()),
            resolutions: Arc::clone(&resolutions),
            live: Arc::new(Mutex::new(Vec::new())),
        };
        let widget = widget_over(cx, config(vec![key("BTCUSDT")]), source, directory);
        settle_until(cx, "the list's stream to be opened", |_| {
            resolutions_of(&resolutions).len() == 1
        });

        cx.update(|cx| {
            accept_ticker(&widget, "BTCUSDT", 100, 120, cx);
            publish_reliable(
                &widget,
                &key("BTCUSDT"),
                WatchListStreamEvent::Error(WatchListStreamError::MarketParticipant(
                    MarketError::Catalog(Box::new(CatalogError::CatalogNotLoaded {
                        route: route(),
                    })),
                )),
                cx,
            );
            assert!(
                !widget.read(cx).entries.contains_key(&key("BTCUSDT")),
                "the listing stopped being read"
            );
        });

        // The market is refreshed — with a catalogue that restates the listing — which is what
        // brings the listing back into reading.
        cx.update(|cx| {
            test_support::publish_fixture_catalogs(cx, &[(route(), vec![restated_entry("0.01")])]);
            widget.update(cx, |widget, cx| widget.load(cx));
        });
        settle_until(cx, "the listing to be read by a new stream", |_| {
            resolutions_of(&resolutions).len() == 2
        });

        cx.update(|cx| {
            assert!(
                widget.read(cx).entries.contains_key(&key("BTCUSDT")),
                "the listing is read again"
            );
            assert_eq!(
                resolutions_of(&resolutions)[1].price_tick.to_string(),
                "0.01",
                "and the stream it is read by is one the service opened now rather than the \
                 parked one it left behind"
            );
        });
    }

    /// A row whose market has no catalog costs exactly one row: the other markets this pane
    /// reads are still read, and a ticker for one of them still fills its row. This is the
    /// per-listing answer to a route that went away, and it is deliberately not the pane's
    /// own: a list that dropped every row over one route would be hiding markets the user can
    /// still read.
    #[gpui_kit::test]
    fn a_listing_whose_catalog_is_gone_should_not_stop_the_other_rows(cx: &mut TestAppContext) {
        let widget = scoped_widget_over(cx, &[entry("BTCUSDT", "BTC"), entry("ETHUSDT", "ETH")]);
        cx.update(|cx| {
            widget.update(cx, |widget, cx| {
                widget.add(selection("ETHUSDT"), cx);
                widget.apply_rpc_result(Ok(WatchListResponse::Applied), cx);
            });
        });

        cx.update(|cx| {
            accept_ticker(&widget, "BTCUSDT", 100, 120, cx);
            accept_ticker(&widget, "ETHUSDT", 10, 12, cx);
            assert_eq!(rows(&widget, cx), 2, "both listings are read");
            assert!(widget.read(cx).entries.contains_key(&key("ETHUSDT")));

            publish_reliable(
                &widget,
                &key("BTCUSDT"),
                WatchListStreamEvent::Error(WatchListStreamError::MarketParticipant(
                    MarketError::Catalog(Box::new(CatalogError::CatalogNotLoaded {
                        route: route(),
                    })),
                )),
                cx,
            );

            assert!(
                !widget.read(cx).entries.contains_key(&key("BTCUSDT")),
                "the listing whose catalog is gone stopped being read"
            );
            assert!(
                widget.read(cx).entries.contains_key(&key("ETHUSDT")),
                "and the other one did not: its market is a different one"
            );
            assert!(
                !widget.read(cx).awaiting_market,
                "so the pane is not waiting for a market it can read"
            );
            assert_eq!(
                rows(&widget, cx),
                2,
                "both listings are still drawn, one with its values and one without"
            );
            assert_eq!(
                price_of(&row_of(&widget, 1, cx)).as_deref(),
                Some("12.00"),
                "the row that is still read keeps what it has"
            );

            // A ticker for the healthy row is still accepted, which is the half a pane that
            // had given up on every row could not do.
            accept_ticker(&widget, "ETHUSDT", 10, 14, cx);
            assert_eq!(
                price_of(&row_of(&widget, 1, cx)).as_deref(),
                Some("14.00"),
                "and it keeps being read after the other route went away"
            );
        });
    }

    /// A listing the user adds while a row of the same list is waiting for its catalog is
    /// added normally: what is waiting is one listing's read, not the pane. A ticker for a
    /// listing the pane does not follow still fills nothing.
    #[gpui_kit::test]
    fn a_listing_should_be_addable_while_another_row_waits_for_its_catalog(
        cx: &mut TestAppContext,
    ) {
        let widget = scoped_widget_over(cx, &[entry("BTCUSDT", "BTC"), entry("ETHUSDT", "ETH")]);
        cx.update(|cx| {
            accept_ticker(&widget, "BTCUSDT", 100, 120, cx);
            publish_reliable(
                &widget,
                &key("BTCUSDT"),
                WatchListStreamEvent::Error(WatchListStreamError::MarketParticipant(
                    MarketError::Catalog(Box::new(CatalogError::CatalogNotLoaded {
                        route: route(),
                    })),
                )),
                cx,
            );
            assert!(
                !widget.read(cx).entries.contains_key(&key("BTCUSDT")),
                "the listing whose catalog is not in memory stopped being read"
            );
            assert!(
                !widget.read(cx).awaiting_market,
                "and the pane is still reading, so it is not waiting for a market"
            );

            widget.update(cx, |widget, cx| widget.add(selection("ETHUSDT"), cx));

            assert_eq!(
                widget.read(cx).config.entries,
                vec![key("BTCUSDT"), key("ETHUSDT")],
                "the listing the user picked joined the followed set"
            );
            assert!(
                widget.read(cx).entries.contains_key(&key("ETHUSDT")),
                "and it is read, because the venue its route is on is in memory"
            );

            accept_ticker(&widget, "BTCUSDT", 100, 120, cx);

            assert!(
                !widget.read(cx).entries.contains_key(&key("BTCUSDT")),
                "a ticker for a listing the pane stopped reading fills nothing"
            );
            assert_eq!(
                rows(&widget, cx),
                2,
                "and both listings are still drawn: the one being read and the one waiting"
            );
        });
    }

    #[gpui_kit::test]
    fn retrying_a_failed_row_should_reopen_it_without_forgetting_the_price(
        cx: &mut TestAppContext,
    ) {
        let widget = scoped_widget(cx);
        cx.update(|cx| {
            accept_ticker(&widget, "BTCUSDT", 100, 120, cx);
            fail_stream(&widget, "BTCUSDT", cx);
            widget.update(cx, |widget, cx| widget.retry(&key("BTCUSDT"), cx));

            assert!(matches!(
                widget.read(cx).pending,
                Some(PendingOperation::Retry { .. })
            ));
            let row = row_of(&widget, 0, cx);
            assert_eq!(row.status(), WatchListStatus::Connecting);
            assert_eq!(price_of(&row).as_deref(), Some("120.00"));
        });
    }

    #[gpui_kit::test]
    fn a_refused_retry_should_restore_the_failure_it_replaced(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        cx.update(|cx| {
            accept_ticker(&widget, "BTCUSDT", 100, 120, cx);
            fail_stream(&widget, "BTCUSDT", cx);
            widget.update(cx, |widget, cx| widget.retry(&key("BTCUSDT"), cx));
            widget.update(cx, |widget, cx| widget.apply_rpc_result(Err(refusal()), cx));

            let row = row_of(&widget, 0, cx);
            assert_eq!(row.status(), WatchListStatus::Error);
            assert_eq!(price_of(&row).as_deref(), Some("120.00"));
            assert!(widget.read(cx).pending.is_none());
            assert!(status(&widget, cx).is_failed());
        });
    }

    #[gpui_kit::test]
    fn adding_a_listing_should_show_it_before_the_service_confirms_it(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        cx.update(|cx| {
            widget.update(cx, |widget, cx| widget.add(selection("ETHUSDT"), cx));

            assert_eq!(
                widget.read(cx).config.entries,
                vec![key("BTCUSDT"), key("ETHUSDT")]
            );
            assert_eq!(rows(&widget, cx), 2);
            assert!(matches!(
                widget.read(cx).pending,
                Some(PendingOperation::Add(_))
            ));
            assert!(
                widget.read(cx).continuous_table.read(cx).disabled
                    && widget.read(cx).session_table.read(cx).disabled,
                "the controls wait for the operation they started"
            );
        });
    }

    /// A listing the list already follows is not an addition: it is the same
    /// scope, so nothing is changed and nothing is asked of the service.
    #[gpui_kit::test]
    fn adding_a_listing_the_list_already_follows_should_change_nothing(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        cx.update(|cx| {
            widget.update(cx, |widget, cx| {
                widget.apply_rpc_result(Ok(WatchListResponse::Applied), cx);
                widget.add(selection("BTCUSDT"), cx);
            });

            assert_eq!(widget.read(cx).config.entries, vec![key("BTCUSDT")]);
            assert_eq!(rows(&widget, cx), 1);
            assert!(
                widget.read(cx).pending.is_none(),
                "a listing that is already followed is not a new operation"
            );
            assert!(!status(&widget, cx).is_failed());
        });
    }

    #[gpui_kit::test]
    fn a_refused_add_should_take_the_row_back(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        cx.update(|cx| {
            widget.update(cx, |widget, cx| widget.add(selection("ETHUSDT"), cx));
            widget.update(cx, |widget, cx| widget.apply_rpc_result(Err(refusal()), cx));

            assert_eq!(widget.read(cx).config.entries, vec![key("BTCUSDT")]);
            assert_eq!(rows(&widget, cx), 1);
            assert!(!widget.read(cx).entries.contains_key(&key("ETHUSDT")));
            assert!(widget.read(cx).pending.is_none());
            assert!(status(&widget, cx).is_failed());
        });
    }

    #[gpui_kit::test]
    fn removing_a_listing_should_hide_it_before_the_service_confirms_it(cx: &mut TestAppContext) {
        let widget = widget_over(
            cx,
            config(vec![key("BTCUSDT"), key("ETHUSDT")]),
            PendingSource,
            directory(&[entry("BTCUSDT", "BTC"), entry("ETHUSDT", "ETH")]),
        );
        cx.update(|cx| {
            widget.update(cx, |widget, cx| {
                widget.apply_rpc_result(Ok(WatchListResponse::Applied), cx);
            });
        });
        cx.update(|cx| {
            widget.update(cx, |widget, cx| widget.remove(&key("BTCUSDT"), cx));

            assert_eq!(widget.read(cx).config.entries, vec![key("ETHUSDT")]);
            assert_eq!(rows(&widget, cx), 1);
            assert!(matches!(
                widget.read(cx).pending,
                Some(PendingOperation::Remove(..))
            ));
        });
    }

    #[gpui_kit::test]
    fn a_refused_remove_should_put_the_row_back_with_what_it_showed(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        cx.update(|cx| {
            accept_ticker(&widget, "BTCUSDT", 100, 120, cx);
            widget.update(cx, |widget, cx| widget.remove(&key("BTCUSDT"), cx));
            assert_eq!(rows(&widget, cx), 0);

            widget.update(cx, |widget, cx| widget.apply_rpc_result(Err(refusal()), cx));

            assert_eq!(widget.read(cx).config.entries, vec![key("BTCUSDT")]);
            let row = row_of(&widget, 0, cx);
            assert_eq!(row.status(), WatchListStatus::Ready);
            assert_eq!(price_of(&row).as_deref(), Some("120.00"));
        });
    }

    /// Two listings of one instrument are two rows of their own, and the config
    /// keeps both: neither replaces the other, and neither is translated into it.
    #[gpui_kit::test]
    fn two_listings_of_one_instrument_should_stay_independent(cx: &mut TestAppContext) {
        let instrument = test_support::spot_token(test_support::VENUE, "BTC", "USDT");
        let first = test_support::entry(&route(), instrument.clone(), "BTCUSDT");
        let second = test_support::entry(&route(), instrument, "BTCUSDT-PERP");
        assert_eq!(
            first.instrument.id(),
            second.instrument.id(),
            "the fixture is one instrument listed twice"
        );
        let widget = widget_over(
            cx,
            config(vec![first.listing.key.clone(), second.listing.key.clone()]),
            PendingSource,
            directory(&[first.clone(), second.clone()]),
        );
        cx.update(|cx| {
            widget.update(cx, |widget, cx| {
                widget.apply_rpc_result(Ok(WatchListResponse::Applied), cx);
            });
        });

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).config.entries,
                vec![first.listing.key.clone(), second.listing.key.clone()],
                "the list follows the exact listings the user added"
            );
            assert_eq!(rows(&widget, cx), 2);
            assert_eq!(row_of(&widget, 0, cx).key().as_ref(), &first.listing.key);
            assert_eq!(row_of(&widget, 1, cx).key().as_ref(), &second.listing.key);
        });

        cx.update(|cx| accept_ticker(&widget, "BTCUSDT", 100, 120, cx));
        cx.update(|cx| {
            assert_eq!(
                price_of(&row_of(&widget, 0, cx)).as_deref(),
                Some("120.00"),
                "the ticker belongs to the listing it was published for"
            );
            assert_eq!(
                price_of(&row_of(&widget, 1, cx)),
                None,
                "the other listing of the same instrument keeps its own row"
            );
        });
    }

    #[gpui_kit::test]
    fn a_full_list_should_refuse_another_listing(cx: &mut TestAppContext) {
        let widget = widget(
            cx,
            config((0..MAX_WATCH_LIST_ENTRIES).map(indexed_key).collect()),
        );
        cx.update(|cx| {
            widget.update(cx, |widget, cx| widget.add(selection("OVERFLOW"), cx));

            assert_eq!(widget.read(cx).config.entries.len(), MAX_WATCH_LIST_ENTRIES);
            assert!(
                failure(&widget, cx).is_some_and(|text| text.contains("100")),
                "the user is told how many listings the list holds"
            );
        });
    }

    /// A failure is terminal, so a message that arrives after it cannot revive a
    /// list the user was told is broken.
    #[gpui_kit::test]
    fn a_message_that_arrives_after_a_failure_should_be_ignored(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        cx.update(|cx| {
            widget.update(cx, |widget, cx| {
                widget.load(cx);
                widget.apply_rpc_result(Err(refusal()), cx);
            });
            accept_ticker(&widget, "BTCUSDT", 100, 120, cx);

            assert!(status(&widget, cx).is_failed());
            assert_eq!(row_of(&widget, 0, cx).status(), WatchListStatus::Connecting);
            assert_eq!(price_of(&row_of(&widget, 0, cx)), None);
        });
    }

    /// A failure is terminal, so the answer to a request that was already in flight when the
    /// list failed cannot report it as working again: the recovery the framework puts on screen
    /// is the only way out of a failure, and a status for a read that is over would take it away
    /// again.
    #[gpui_kit::test]
    fn an_answer_that_arrives_after_a_failure_should_be_ignored(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        cx.update(|cx| {
            widget.update(cx, |widget, cx| widget.apply_rpc_result(Err(refusal()), cx));
            assert!(
                status(&widget, cx).is_failed(),
                "the list is in its terminal failure"
            );

            widget.update(cx, |widget, cx| {
                widget.apply_rpc_result(Ok(WatchListResponse::Applied), cx)
            });

            let status = status(&widget, cx);
            assert!(
                status.is_failed(),
                "a late acknowledgement must not report the failed list as working"
            );
            assert!(
                status
                    .failure()
                    .is_some_and(|failure| failure.action().is_enabled()),
                "and the way out of the failure must still be there"
            );
        });
    }

    #[gpui_kit::test]
    fn the_recovery_should_resynchronize_the_followed_set(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        cx.update(|cx| {
            accept_ticker(&widget, "BTCUSDT", 100, 120, cx);
            widget.update(cx, |widget, cx| widget.apply_rpc_result(Err(refusal()), cx));

            // A failure is reported beside the rows the user is still reading
            // rather than in place of them.
            assert!(status(&widget, cx).is_failed());
            assert!(widget.read(cx).has_content());
            assert_eq!(price_of(&row_of(&widget, 0, cx)).as_deref(), Some("120.00"));

            let recovery = status(&widget, cx)
                .failure()
                .map(|failure| failure.action().clone())
                .expect("a failure should offer a way out");
            recovery.run(cx);

            assert!(!status(&widget, cx).is_failed());
            assert!(matches!(
                widget.read(cx).pending,
                Some(PendingOperation::Load)
            ));
            assert!(
                widget.read(cx).has_content(),
                "the recovery keeps the rows the user is still reading"
            );
            assert_eq!(price_of(&row_of(&widget, 0, cx)).as_deref(), Some("120.00"));

            widget.update(cx, |widget, cx| {
                widget.apply_rpc_result(Ok(WatchListResponse::Applied), cx)
            });
            assert_eq!(status(&widget, cx), WidgetState::Ready);
            assert!(widget.read(cx).pending.is_none());
        });
    }

    /// A reload the user asked for starts over: what the pane was showing is not
    /// something it can vouch for any more, so every row goes back to connecting and
    /// keeps nothing. The rows themselves stay, because they are the listings the user
    /// follows rather than the values they were showing.
    #[gpui_kit::test]
    fn an_explicit_reload_should_start_over_and_keep_the_rows(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        cx.update(|cx| {
            accept_ticker(&widget, "BTCUSDT", 100, 120, cx);
            widget.update(cx, |widget, cx| widget.load(cx));

            assert!(widget.read(cx).has_content());
            assert_eq!(rows(&widget, cx), 1);
            assert_eq!(status(&widget, cx), WidgetState::Connecting);
            assert_eq!(row_of(&widget, 0, cx).status(), WatchListStatus::Connecting);
            assert_eq!(price_of(&row_of(&widget, 0, cx)), None);
        });
    }

    /// The table renders one control per row, and the retry control only exists where there
    /// is a stream to reopen. A row that received no ticker still has one — its stream failed,
    /// which is exactly what a retry is for — while a row whose market is not loaded has none,
    /// and no control may offer to reopen what was never opened.
    #[gpui_kit::test]
    fn the_row_controls_should_follow_the_row_state(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        let (_, cx) = cx.add_window_view({
            let widget = widget.clone();
            move |_, _| BodyHarness(widget)
        });
        cx.simulate_resize(size(px(900.0), px(240.0)));
        cx.run_until_parked();
        // The row is drawn inside the body, which is the one cached layer, so both the presence and
        // the absence below are read off a frame that could not have hit the cache.
        repaint(cx);

        assert!(
            cx.debug_bounds("WATCH_LIST_ROW_PRICE").is_some(),
            "a row without a ticker still shows what it is waiting for"
        );
        assert!(cx.debug_bounds("WATCH_LIST_ROW_STATUS").is_none());
        assert!(cx.debug_bounds("WATCH_LIST_RETRY").is_none());

        // A stream that fails before its first ticker is a failed row like any other: what a
        // retry reaches is the stream, which this row has.
        cx.update(|_, cx| fail_stream(&widget, "BTCUSDT", cx));
        cx.run_until_parked();
        repaint(cx);

        assert!(
            cx.debug_bounds("WATCH_LIST_ROW_STATUS").is_some(),
            "the row says what its stream is doing"
        );
        assert!(
            cx.debug_bounds("WATCH_LIST_RETRY").is_some(),
            "and offers the retry that reopens it, whether or not a ticker arrived first"
        );

        // The market goes away: the row has no stream behind it any more, so there is
        // nothing to reopen and the control goes with it.
        cx.update(|_, cx| {
            accept_ticker(&widget, "BTCUSDT", 100, 120, cx);
            publish_reliable(
                &widget,
                &key("BTCUSDT"),
                WatchListStreamEvent::Error(WatchListStreamError::MarketParticipant(
                    MarketError::Catalog(Box::new(CatalogError::CatalogNotLoaded {
                        route: route(),
                    })),
                )),
                cx,
            );
        });
        cx.run_until_parked();
        repaint(cx);

        assert!(
            cx.debug_bounds("WATCH_LIST_RETRY").is_none(),
            "a row waiting for its market has no stream a retry could reopen"
        );
    }

    /// The two frames the body draws, one per table. Which of them is drawn is a
    /// visibility rule rather than a number in a dataset, so a test reads it off the
    /// freshly painted frame.
    const CONTINUOUS_TABLE: &str = "WATCH_LIST_CONTINUOUS_TABLE";
    const SESSION_TABLE: &str = "WATCH_LIST_SESSION_TABLE";
    /// The sentence a region with nothing in it draws, as `base_ui` marks it.
    const EMPTY_STATE: &str = "EMPTY_STATE";
    /// One number of a row, which the merged cell stands in for.
    const ROW_PRICE: &str = "WATCH_LIST_ROW_PRICE";
    /// The control a row offers whatever it is doing.
    const ROW_REMOVE: &str = "WATCH_LIST_REMOVE";

    /// Draws one widget in a window of the size the other body cases use, on a frame that
    /// could not have hit a cache.
    fn body_window<'a>(
        cx: &'a mut TestAppContext,
        widget: &Entity<WatchListWidget>,
    ) -> &'a mut VisualTestContext {
        let widget = widget.clone();
        let (_, window) = cx.add_window_view(move |_, _| BodyHarness(widget));
        window.simulate_resize(size(px(900.0), px(240.0)));
        window.run_until_parked();
        repaint(window);
        window
    }

    /// A row whose market is not loaded draws the route it follows, one cell where the
    /// name of its listing and the values of its table would be, and its own controls —
    /// and that cell really does stand in for them: it is as wide as the columns between
    /// the route and the controls put together, which is the width the table would have
    /// given to those columns.
    ///
    /// The table lays its cells out in a row of declared widths, so "one cell across
    /// several columns" is a width and an absence rather than a span: this reads the
    /// painted frame, because that is where both are facts.
    #[gpui_kit::test]
    fn a_row_waiting_for_its_market_should_draw_one_cell_where_its_values_would_be(
        cx: &mut TestAppContext,
    ) {
        // The venue is loaded and registered but nothing of it is published, which is the
        // state of a pane opened before the market participants it follows have committed.
        let widget = built_market(
            cx,
            config(vec![key("BTCUSDT")]),
            PendingSource,
            directory(&[entry("BTCUSDT", "BTC")]),
            WidgetConfigChange::default(),
            false,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        let window = body_window(cx, &widget);

        window.cx.update(|cx| {
            assert!(widget.read(cx).awaiting_market, "no market is readable");
            assert_eq!(
                row_of(&widget, 0, cx).state(),
                &WatchListRowState::AwaitingMarket
            );
            assert_eq!(widget.read(cx).continuous_table.read(cx).rows_count(), 1);
        });

        let row = window
            .debug_bounds(CONTINUOUS_TABLE)
            .expect("the table of the row is drawn");
        let cells = CELL_SELECTORS.map(|selector| window.debug_bounds(selector));
        let price = window.debug_bounds(ROW_PRICE);
        let remove = window.debug_bounds(ROW_REMOVE);
        let sentence = window
            .debug_bounds("WATCH_LIST_MISSING_MARKET_SENTENCE")
            .expect("the cell says which market participant has to be loaded");
        let merged = cells[MISSING_MARKET_COLUMN]
            .expect("the row draws the one cell that stands in for its values");

        assert!(
            price.is_none(),
            "a row waiting for its market draws no price of its own: {price:?}"
        );
        for (column, drawn) in cells.iter().enumerate() {
            let expected = column <= MISSING_MARKET_COLUMN || column == COLUMN_COUNT - 1;
            assert_eq!(
                drawn.is_some(),
                expected,
                "column {column} is {} of a row whose market is not loaded",
                if expected { "a cell" } else { "not part" }
            );
        }
        assert!(
            merged.size.width
                > cells[0]
                    .expect("the row draws the route it follows")
                    .size
                    .width
                    * 2.0,
            "the cell stands in for every column but the route and the last one, so it is \
             drawn wider than any one of them: {merged:?} in {row:?}"
        );
        assert!(
            remove.is_some_and(|remove| {
                remove.left() > merged.left() && remove.right() <= row.right()
            }),
            "the controls of the row are still drawn, inside the row, after the cell that \
             replaced the values: {remove:?} after {merged:?} in {row:?}"
        );
        // The column of controls is the last one and holds nothing else: the value that used to
        // share it is a column of its own again, and this row draws none of the values anyway.
        // What is checked here is that the control is drawn in a column after the cell that
        // replaced the values, which is what tells the two apart.
        assert!(
            window.debug_bounds("WATCH_LIST_ROW_VALUE").is_none(),
            "a row waiting for its market draws no number of its own"
        );
        // What the merged cell drew is the sentence, and it is the only thing in that room: the
        // row draws no cell for the columns it stands in for. Where that sentence sits in the
        // room is the claim that the merge is real and that the row centred it, and it is read
        // off the gaps around it below.
        assert!(
            sentence.left() >= merged.left() && sentence.right() <= merged.right(),
            "the sentence is drawn inside the cell that stands in for the values: \
             {sentence:?} in {merged:?}"
        );
        assert!(
            merged.right() < row.right() && merged.left() > row.left(),
            "and the cell is drawn between the route and the controls rather than over \
             them: {merged:?} in {row:?}"
        );
        // Where the merged cell sits across the columns it covers is what its own bounds do not
        // state, so it is stated by the two things that say it: the sentence asks the component
        // that draws it to centre it — the same `<p align="center">` pair the framework's own
        // placeholder sentence is written with, because a markdown block is as wide as its
        // column and what a reader sees is the line inside it — and the cell it is drawn in
        // spans the room between the route and the controls rather than sitting in one column.
        let route = cells[0].expect("the row draws the route it follows");
        let controls = cells[COLUMN_COUNT - 1].expect("the row draws its own controls");
        assert!(
            sentence.left() >= route.right() && sentence.right() <= controls.left(),
            "the cell that stands in for the values is drawn between the route and the \
             controls rather than over them: {sentence:?} between {route:?} and {controls:?}"
        );
        assert!(
            window
                .cx
                .update(|cx| t!(cx, "widget.watch_list.market_metadata_not_loaded")
                    .contains("align=\"center\"")),
            "and the sentence asks to be centred in that room rather than pulled to a side \
             of it"
        );
    }

    /// Which table a listing is drawn in is a fact about its own route rather than about the
    /// directory: the scope it is quoted on is in its key, and the calendar the published
    /// rules carry is not read at all. A listing is therefore drawn under the headings of the
    /// market it belongs to — the one whose own columns state what that market's numbers
    /// mean — and never under the other one's, whether or not the market behind it is loaded.
    #[gpui_kit::test]
    fn a_listing_should_be_drawn_in_the_table_of_the_market_it_is_quoted_on(
        cx: &mut TestAppContext,
    ) {
        // Only the continuous market is loaded, and neither route publishes anything: the
        // placement is read off the two routes, which is the state no generation is asked
        // about at all.
        let widget = built_over(
            cx,
            config(vec![key("BTCUSDT"), stock("AAPL")]),
            PendingSource,
            directory(&[entry("BTCUSDT", "BTC")]),
            WidgetConfigChange::default(),
            |cx, _| install_connector(cx, true),
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        let window = body_window(cx, &widget);

        window.cx.update(|cx| {
            let widget = widget.read(cx);
            assert_eq!(
                widget.session_table.read(cx).rows_count(),
                1,
                "the listing of a session market is drawn in the session table"
            );
            assert_eq!(
                widget.continuous_table.read(cx).rows_count(),
                1,
                "and the one that trades around the clock in the other"
            );
            assert_eq!(
                widget.session_table.read(cx).rows[0].key().as_ref(),
                &stock("AAPL"),
                "the session table holds the listing quoted on the scope that declares it"
            );
            let headers = widget
                .session_table
                .read(cx)
                .headers(cx)
                .map(|header| header.to_string());
            assert_eq!(
                headers[3..COLUMN_COUNT - 1],
                [
                    t!(cx, "widget.watch_list.us.change").to_string(),
                    t!(cx, "widget.watch_list.us.change_percent").to_string(),
                    t!(cx, "widget.watch_list.us.traded_quantity").to_string(),
                    t!(cx, "widget.watch_list.us.traded_notional").to_string(),
                ],
                "and it is headed by that market's own four columns"
            );
            assert_eq!(
                headers[ACTIONS_COLUMN], "",
                "under a column of controls, which carries no heading"
            );
        });
        assert!(
            window.debug_bounds(SESSION_TABLE).is_some(),
            "the table the row belongs to is drawn"
        );
    }

    /// A listing that trades around the clock is drawn in the continuous table, and the
    /// session table is not in the frame at all — neither its header nor a row of it.
    #[gpui_kit::test]
    fn a_continuous_listing_should_be_drawn_in_the_continuous_table_alone(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        let window = body_window(cx, &widget);

        window.cx.update(|cx| {
            assert_eq!(widget.read(cx).session_table.read(cx).rows_count(), 0);
            assert_eq!(widget.read(cx).continuous_table.read(cx).rows_count(), 1);
        });
        assert!(window.debug_bounds(CONTINUOUS_TABLE).is_some());
        assert!(
            window.debug_bounds(SESSION_TABLE).is_none(),
            "the session table has nothing to draw, so its header is gone with it"
        );
    }

    /// An empty list still draws a table, and that table says it is empty: the one case
    /// where both markets have no rows draws the continuous table, so the body is never
    /// left blank and never carries two empty states.
    #[gpui_kit::test]
    fn an_empty_list_should_draw_one_table_and_its_empty_state(cx: &mut TestAppContext) {
        let widget = widget(cx, WatchListConfig::default());
        let window = body_window(cx, &widget);

        window.cx.update(|cx| {
            let widget = widget.read(cx);
            assert!(widget.has_content(), "the confirmed empty set is content");
            assert_eq!(widget.continuous_table.read(cx).rows_count(), 0);
            assert_eq!(widget.session_table.read(cx).rows_count(), 0);
        });
        assert!(
            window.debug_bounds(CONTINUOUS_TABLE).is_some(),
            "an empty body still draws the table that says so"
        );
        assert!(
            window.debug_bounds(SESSION_TABLE).is_none(),
            "and not a second one beside it: two empty states is not what an empty list is"
        );
        assert!(
            window.debug_bounds(EMPTY_STATE).is_some(),
            "the drawn table is the one that carries the empty state"
        );
    }

    #[gpui_kit::test]
    fn the_header_should_offer_adding_a_listing(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        let (_, cx) = cx.add_window_view(move |_, _| HeaderHarness(widget));
        cx.simulate_resize(size(px(900.0), px(240.0)));
        cx.run_until_parked();

        assert!(cx.debug_bounds("WATCH_LIST_ADD").is_some());
    }

    /// The selector panel, as the dialog draws it. The selector's own tests read the same selector
    /// by name; this module states it because it is the dialog this pane's control opens rather
    /// than the control this module draws.
    const SELECTOR_PANEL: &str = "INSTRUMENT_SELECTOR";

    /// The add control offers its picker while the list is reading a market: the header control is
    /// what a listing is added through, and what it opens is the selector the caller described.
    #[gpui_kit::test]
    fn the_add_control_should_offer_its_picker_while_the_list_is_reading(cx: &mut TestAppContext) {
        let widget = scoped_widget_over(cx, &[entry("BTCUSDT", "BTC")]);
        // The control opens a dialog, and a dialog is drawn by the layer the application mounts
        // one under: without it a click that did open the picker would leave nothing to see.
        let root = cx.update(move |cx| cx.new(|_| HeaderHarness(widget)));
        let (_, window) = cx.add_window_view(|_, _| Layer::new(root));
        window.simulate_resize(size(px(900.0), px(240.0)));
        window.run_until_parked();

        press(window, "WATCH_LIST_ADD");
        assert!(
            window.debug_bounds(SELECTOR_PANEL).is_some(),
            "a list that is reading a market offers the picker its listings come from"
        );
    }

    /// The add control refuses input while the list waits for its markets: a listing can only be
    /// added from a market that is in memory, which is what the sentence the pane waits with says
    /// as well.
    #[gpui_kit::test]
    fn the_add_control_should_refuse_input_while_the_market_is_gone(cx: &mut TestAppContext) {
        let widget = scoped_widget_over(cx, &[entry("BTCUSDT", "BTC")]);
        let root = cx.update({
            let widget = widget.clone();
            move |cx| cx.new(|_| HeaderHarness(widget))
        });
        let (_, window) = cx.add_window_view(|_, _| Layer::new(root));
        window.simulate_resize(size(px(900.0), px(240.0)));
        window.run_until_parked();

        window.cx.update(|cx| {
            // The market the list reads leaves the loaded venues, which is what a shrinking
            // commit does: the pane has nothing to read and says so.
            let venue =
                market_participants_model::identifier::VenueId::from_name(test_support::VENUE)
                    .expect("the fixture venue is canonical");
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(&[], &[(venue, Vec::new())], &[])
                .expect("the shrinking commit lands");
            widget.update(cx, |widget, cx| widget.load(cx));
            assert!(
                widget.read(cx).awaiting_market,
                "the list waits for the market it lost"
            );
        });
        window.run_until_parked();

        assert!(
            window.debug_bounds("WATCH_LIST_ADD").is_some(),
            "the control is still what the header draws"
        );
        press(window, "WATCH_LIST_ADD");
        assert!(
            window.debug_bounds(SELECTOR_PANEL).is_none(),
            "a control that cannot add anything must not offer a picker"
        );
    }

    /// Presses what `selector` names, the way a window delivers a press: the button goes down, the
    /// frame that follows is drawn, and only then does it come up.
    fn press(window: &mut VisualTestContext, selector: &'static str) {
        let target = window
            .debug_bounds(selector)
            .expect("the harness draws what it is asked to press")
            .center();
        window.simulate_mouse_down(target, MouseButton::Left, Modifiers::none());
        window.run_until_parked();
        window.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
        window.run_until_parked();
        window.update(|window, _| window.refresh());
        window.run_until_parked();
    }

    /// The column set is the widget's own decision, and nothing else notices when
    /// one of them goes missing. The fixture listings declare the around-the-clock
    /// calendar, so the columns asserted here are the continuous table's.
    ///
    /// The last column is the row's own controls, and it carries no label: what a button says
    /// is written on the button, and a heading over it would name a value the column does not
    /// hold.
    #[gpui_kit::test]
    fn the_table_should_offer_the_followed_columns(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        let (headers, quantity, notional) = cx.update(|cx| {
            (
                widget
                    .read(cx)
                    .continuous_table
                    .read(cx)
                    .headers(cx)
                    .map(|header| header.to_string()),
                t!(cx, "widget.watch_list.traded_quantity_24h").to_string(),
                t!(cx, "widget.watch_list.traded_notional_24h").to_string(),
            )
        });

        assert_eq!(
            headers,
            [
                "Route".to_owned(),
                "Instrument".to_owned(),
                "Price".to_owned(),
                "24h Change".to_owned(),
                "24h Change %".to_owned(),
                quantity,
                notional,
                String::new(),
            ]
        );
    }

    /// The two tables head their columns with the two sets of labels, and the difference
    /// is exactly the four columns the two statements are about: a session's statistics
    /// are not a day's, so one set may never be drawn under the other's heading.
    #[gpui_kit::test]
    fn the_two_tables_should_offer_their_own_column_sets(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        let (continuous, session, continuous_keys, session_keys) = cx.update(|cx| {
            let widget = widget.read(cx);
            let headers = |table: &Entity<WatchListTable>| {
                table.read(cx).headers(cx).map(|header| header.to_string())
            };
            (
                headers(&widget.continuous_table),
                headers(&widget.session_table),
                [
                    t!(cx, "widget.watch_list.change_24h").to_string(),
                    t!(cx, "widget.watch_list.change_percent_24h").to_string(),
                    t!(cx, "widget.watch_list.traded_quantity_24h").to_string(),
                    t!(cx, "widget.watch_list.traded_notional_24h").to_string(),
                ],
                [
                    t!(cx, "widget.watch_list.us.change").to_string(),
                    t!(cx, "widget.watch_list.us.change_percent").to_string(),
                    t!(cx, "widget.watch_list.us.traded_quantity").to_string(),
                    t!(cx, "widget.watch_list.us.traded_notional").to_string(),
                ],
            )
        });

        assert_eq!(
            continuous[..3],
            session[..3],
            "the route, the instrument and the price read the same in both tables"
        );
        assert_eq!(continuous[3..7], continuous_keys);
        assert_eq!(
            session[3..7],
            session_keys,
            "the session table heads the same four columns with its own statement"
        );
        assert_ne!(
            continuous[3..7],
            session[3..7],
            "the two markets state their statistics differently, so the labels differ"
        );
    }

    /// The first column states the whole user-visible route, not the venue brand: two
    /// rows of different routes can carry the same name, and the route is what tells
    /// them apart.
    #[gpui_kit::test]
    fn the_first_column_should_state_the_route_the_row_follows(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        cx.update(|cx| {
            let row = row_of(&widget, 0, cx);

            assert_eq!(
                route_column(&row, cx).as_ref(),
                "Binance/SPOT",
                "the column draws the route the way a reader sees it"
            );
            assert_ne!(
                route_column(&row, cx).as_ref(),
                row.key().route.venue.as_str(),
                "the canonical identity is not what the column draws"
            );
        });
    }

    /// Each traded column is drawn in the unit the ticker published it in, and a unit
    /// the listing does not publish is drawn as missing rather than as the other unit
    /// or as zero.
    #[gpui_kit::test]
    fn the_traded_columns_should_show_the_unit_their_listing_publishes(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        cx.update(|cx| {
            let mut published = ticker("BTCUSDT", 100, 120);
            published.traded_quantity = Some(MarketQuantity {
                value: Quantity::from(1_500_u64),
                unit: QuantityUnit::Contracts,
            });
            published.traded_notional = None;
            publish_latest(
                &widget,
                &key("BTCUSDT"),
                WatchListStreamEvent::Snapshot {
                    ticker: Arc::new(published),
                    precision: precision(),
                },
                cx,
            );
            publish_reliable(
                &widget,
                &key("BTCUSDT"),
                WatchListStreamEvent::Status(WatchListStatus::Ready),
                cx,
            );

            let row = row_of(&widget, 0, cx);
            let display = row.display().expect("the row has labels");
            let generation = cx
                .global::<MarketParticipantsConnector>()
                .catalog()
                .cached_display();
            let contracts = t!(cx, "widget.watch_list.contracts");

            assert_eq!(
                traded_text(display.traded_quantity.as_ref(), &generation, cx).to_string(),
                format!("1.50K {contracts}"),
                "a quantity counted in contracts is labelled in contracts"
            );
            assert_eq!(
                traded_text(display.traded_notional.as_ref(), &generation, cx).to_string(),
                MISSING_VALUE,
                "a notional the listing does not publish is missing, not zero"
            );
        });
    }

    /// The units are the listing's own: the same two columns read differently for a
    /// listing that trades an asset and for one that trades an amount.
    #[gpui_kit::test]
    fn the_traded_columns_should_follow_the_published_unit(cx: &mut TestAppContext) {
        let widget = scoped_widget_over(cx, &[entry("BTCUSDT", "BTC")]);
        cx.update(|cx| {
            accept_ticker(&widget, "BTCUSDT", 100, 120, cx);

            let row = row_of(&widget, 0, cx);
            let display = row.display().expect("the row has labels");
            let generation = cx
                .global::<MarketParticipantsConnector>()
                .catalog()
                .cached_display();
            let quantity = display
                .traded_quantity
                .as_ref()
                .expect("the fixture publishes a traded quantity");

            assert_eq!(
                quantity.unit,
                QuantityUnit::AssetUnits { asset: base() },
                "the row keeps the unit it was given"
            );
            assert_eq!(
                traded_text(display.traded_quantity.as_ref(), &generation, cx).to_string(),
                "10.00 BTC",
                "an asset quantity is counted in that asset"
            );

            let mut amount_only = ticker("BTCUSDT", 100, 120);
            amount_only.traded_quantity = None;
            publish_latest(
                &widget,
                &key("BTCUSDT"),
                WatchListStreamEvent::Snapshot {
                    ticker: Arc::new(amount_only),
                    precision: precision(),
                },
                cx,
            );
            publish_reliable(
                &widget,
                &key("BTCUSDT"),
                WatchListStreamEvent::Status(WatchListStatus::Ready),
                cx,
            );

            let row = row_of(&widget, 0, cx);
            let display = row.display().expect("the row has labels");
            let generation = cx
                .global::<MarketParticipantsConnector>()
                .catalog()
                .cached_display();

            assert_eq!(
                traded_text(display.traded_quantity.as_ref(), &generation, cx).to_string(),
                MISSING_VALUE,
                "a traded quantity the listing does not publish is missing"
            );
            assert_eq!(
                traded_text(display.traded_notional.as_ref(), &generation, cx).to_string(),
                "1.00K USDT",
                "the notional is an amount in the asset the venue states it in"
            );
        });
    }

    /// The followed set is the one thing this widget persists, so it has to
    /// survive the config envelope the framework stores it in.
    #[test]
    fn the_followed_set_should_survive_a_config_round_trip() {
        let config = config(vec![key("BTCUSDT"), key("ETHUSDT")]);
        let stored = VersionedWidgetConfig::encode(&config).expect("config should encode");
        let decoded = stored.decode::<WatchListConfig>();

        assert_eq!(decoded.entries, config.entries);
        // A list created without a config follows nothing rather than inventing
        // listings to follow.
        assert!(WatchListConfig::default().entries.is_empty());
    }

    /// The payload holds listing keys and nothing else.
    ///
    /// A display name is derived from the catalog generation that is published when
    /// the row is drawn, so storing one would freeze a name the next generation may
    /// render differently — and would have to be migrated when it does. The display
    /// contract's own rule: the payload keeps the key, the name is read back.
    #[test]
    fn the_payload_should_hold_listing_keys_and_no_display_string() {
        let config = config(vec![key("BTCUSDT")]);
        let stored = VersionedWidgetConfig::encode(&config).expect("config should encode");
        let envelope = serde_json::to_value(&stored).expect("the envelope should encode");
        let payload: serde_json::Value =
            serde_json::from_str(envelope["payload"].as_str().expect("the payload is text"))
                .expect("the payload should be json");
        let entry = &payload["entries"][0];

        let mut keys = entry
            .as_object()
            .expect("an entry is an object")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        assert_eq!(keys, ["listing_id", "route"]);
        assert!(
            entry["listing_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("listing-id/v1/")),
            "the entry states the canonical listing identity: {entry}"
        );
        assert!(
            entry["route"]["venue"] == "binance" && entry["route"]["scope"] == "spot",
            "the entry states the route of the listing: {entry}"
        );
        assert!(
            payload.get("entries").is_some() && payload.as_object().is_some_and(|o| o.len() == 1),
            "the payload is the followed set and nothing else: {payload}"
        );
    }

    /// A stored list that breaks one of the type's invariants is a config this
    /// build cannot read, so it is replaced by the default one rather than by a
    /// list the service would refuse.
    #[test]
    fn a_config_beyond_the_capacity_should_be_reset() {
        let config = config(
            (0..=MAX_WATCH_LIST_ENTRIES)
                .map(indexed_key)
                .collect::<Vec<_>>(),
        );
        let stored = VersionedWidgetConfig::encode(&config).expect("config should encode");

        assert!(
            stored.decode::<WatchListConfig>().entries.is_empty(),
            "a persisted list the service would refuse must not come back"
        );
    }

    #[test]
    fn a_config_that_follows_one_listing_twice_should_be_reset() {
        let config = config(vec![key("BTCUSDT"), key("BTCUSDT")]);
        let stored = VersionedWidgetConfig::encode(&config).expect("config should encode");

        assert!(stored.decode::<WatchListConfig>().entries.is_empty());
    }

    /// The previous build persisted a route and an instrument per entry; the listing
    /// key is what this build reads, so that payload is not a config it can decode. It
    /// is replaced by the default one, which the pane then writes back.
    #[test]
    fn an_old_payload_that_names_routes_and_instruments_should_be_reset() {
        let stored: VersionedWidgetConfig = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "payload": concat!(
                r#"{"entries":[{"route":{"venue":"binance","scope":"spot"},"#,
                r#""instrument":{"symbol":"BTCUSDT"}}]}"#,
            ),
        }))
        .expect("the stored envelope should decode");

        let decoded = stored.decode::<WatchListConfig>();

        assert!(
            decoded.entries.is_empty(),
            "an entry without a listing key is not a listing this build can follow"
        );
    }

    /// A followed listing the route no longer offers leaves the list, and the pane is
    /// told so it can be written back with the set it is actually following.
    #[gpui_kit::test]
    fn a_followed_listing_the_route_dropped_should_leave_the_list(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        let asked = cx.update(|cx| widget.read(cx).config.entries.clone());
        let readable = cx.update(|cx| widget.read(cx).readable(cx));
        let answers = [ListingAnswer {
            key: key("BTCUSDT"),
            support: ListingSupport::Withdrawn,
        }];
        cx.update(|cx| {
            widget.update(cx, |widget, cx| {
                widget.settle_listings(&asked, &readable, &answers, cx);
            });
        });

        cx.update(|cx| {
            assert!(
                widget.read(cx).config.entries.is_empty(),
                "BTCUSDT is not offered by the route any more"
            );
            assert!(
                widget.read(cx).entries.is_empty(),
                "and its row goes with it rather than being drawn for nothing"
            );
        });
    }

    /// A route that did not answer proves nothing, so the list is left alone and each
    /// stream reports its own error.
    #[gpui_kit::test]
    fn a_route_that_did_not_answer_should_leave_the_list_alone(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        let asked = cx.update(|cx| widget.read(cx).config.entries.clone());
        let readable = cx.update(|cx| widget.read(cx).readable(cx));
        cx.update(|cx| {
            let support = listing_support(&key("BTCUSDT"), Some(&Err(no_answer())), cx);
            assert_eq!(support, ListingSupport::Unknown);
            let answers = [ListingAnswer {
                key: key("BTCUSDT"),
                support,
            }];
            widget.update(cx, |widget, cx| {
                widget.settle_listings(&asked, &readable, &answers, cx);
            });
        });

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).config.entries,
                vec![key("BTCUSDT")],
                "the venue not answering is not the venue saying no"
            );
        });
    }

    /// A venue that was unloaded while the lookup was in flight — a shrinking commit took
    /// every scope of it away — is a question nobody can answer any more. That proves nothing
    /// about the listing either: it stays in the persisted entries, and the request that
    /// follows reports the refusal instead of the pane silently dropping what the user
    /// follows.
    #[gpui_kit::test]
    fn a_venue_unloaded_while_the_lookup_was_in_flight_should_leave_the_list_alone(
        cx: &mut TestAppContext,
    ) {
        let widget = scoped_widget(cx);
        let asked = cx.update(|cx| widget.read(cx).config.entries.clone());
        let readable = cx.update(|cx| widget.read(cx).readable(cx));
        cx.update(|cx| {
            // The venue loaded again and published no scope at all, which is what a shrinking
            // commit does: nothing of it is loaded any more.
            let venue =
                market_participants_model::identifier::VenueId::from_name(test_support::VENUE)
                    .expect("the fixture venue is canonical");
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(&[], &[(venue, Vec::new())], &[])
                .expect("the shrinking commit lands");
            assert!(
                cx.global::<MarketParticipantsConnector>()
                    .venues()
                    .scope(&route())
                    .is_none(),
                "the fixture venue really is unloaded, so nothing can be asked about it"
            );

            let support = listing_support(&key("BTCUSDT"), None, cx);
            assert_eq!(
                support,
                ListingSupport::Unknown,
                "a venue unloaded under the lookup proves nothing about the listing"
            );
            let answers = [ListingAnswer {
                key: key("BTCUSDT"),
                support,
            }];
            widget.update(cx, |widget, cx| {
                widget.settle_listings(&asked, &readable, &answers, cx);
            });
        });

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).config.entries,
                vec![key("BTCUSDT")],
                "unloading the venue must not delete the user's entry"
            );
            assert!(
                !widget.read(cx).entries.is_empty(),
                "and its row stays with it rather than being dropped for nothing"
            );
        });
    }

    /// A route the reading the settlement started with did not hold was never asked, and a
    /// question nobody asked proves nothing: a load commits on the runtime's own thread, so
    /// the venue may have been loaded between the reading and this answer. Reading "not in
    /// that reading" as a withdrawal would delete a listing from the user's persisted entries
    /// over that race, so the answer is `Unknown` and the request that follows settles it.
    #[gpui_kit::test]
    fn a_route_the_reading_missed_should_leave_the_list_alone(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        let asked = cx.update(|cx| widget.read(cx).config.entries.clone());
        let readable = cx.update(|cx| widget.read(cx).readable(cx));
        cx.update(|cx| {
            let support = listing_support(&key("BTCUSDT"), None, cx);
            assert_eq!(
                support,
                ListingSupport::Unknown,
                "a route that was not in the reading was never asked"
            );
            let answers = [ListingAnswer {
                key: key("BTCUSDT"),
                support,
            }];
            widget.update(cx, |widget, cx| {
                widget.settle_listings(&asked, &readable, &answers, cx);
            });
        });

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).config.entries,
                vec![key("BTCUSDT")],
                "a load landing between the reading and the answer must not delete the \
                 user's entry"
            );
            assert!(
                !widget.read(cx).entries.is_empty(),
                "and its row stays with it rather than being dropped for nothing"
            );
        });
    }

    /// A list the user has changed since is theirs, not the answer's to overwrite.
    #[gpui_kit::test]
    fn a_list_changed_while_the_lookup_was_in_flight_should_survive_it(cx: &mut TestAppContext) {
        let widget = scoped_widget(cx);
        let asked = cx.update(|cx| widget.read(cx).config.entries.clone());
        let readable = cx.update(|cx| widget.read(cx).readable(cx));
        let answers = [ListingAnswer {
            key: key("BTCUSDT"),
            support: ListingSupport::Withdrawn,
        }];
        cx.update(|cx| {
            widget.update(cx, |widget, cx| {
                widget.config.entries = vec![key("SOLUSDT")];
                // The answer is about the set that held BTCUSDT, which the user has
                // moved away from.
                widget.settle_listings(&asked, &readable, &answers, cx);
            });
        });

        cx.update(|cx| {
            assert_eq!(widget.read(cx).config.entries, vec![key("SOLUSDT")]);
        });
    }

    /// A market whose catalog is not in memory is waited for rather than read: the pane's
    /// own gate refuses before anything is subscribed to, nothing the user stored is
    /// touched — waiting proves nothing about what a route publishes — and the pane draws
    /// the row the user follows, saying which market participant has to be loaded, instead
    /// of failing over a market that is merely not there yet.
    #[gpui_kit::test]
    fn a_market_whose_catalog_is_not_in_memory_should_not_be_subscribed_to(
        cx: &mut TestAppContext,
    ) {
        let source = AnsweringSource::default();
        // The directory answers nothing and the venue is loaded nowhere: under V3 those
        // are one state — a venue appears in the loaded venues exactly when its whole
        // catalog committed — so the fixture states the failing directory with no load.
        let widget = built_market(
            cx,
            config(vec![key("BTCUSDT")]),
            source.clone(),
            Fixture::failing(),
            WidgetConfigChange::default(),
            false,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));

        cx.update(|cx| {
            assert!(
                source.subscribed().is_empty(),
                "a market the user has not loaded is not asked for tickers"
            );
            assert_eq!(
                widget.read(cx).config.entries,
                vec![key("BTCUSDT")],
                "waiting for a market proves nothing about what the route publishes"
            );
            let status = status(&widget, cx);
            assert!(
                !status.is_failed(),
                "a market that is not in memory is not a failure of the watch list"
            );
            assert!(
                widget.read(cx).awaiting_market,
                "the pane waits for the market it cannot read"
            );
            let row = row_of(&widget, 0, cx);
            assert_eq!(row.state(), &WatchListRowState::AwaitingMarket);
            assert_eq!(
                row.key().route.venue,
                route().venue,
                "and the row names the market participant it waits for"
            );
        });
    }

    /// A route the registry registers but does not declare tickers on is a scope nothing
    /// can be followed on, so the listings of it leave the list and the pane is told — or
    /// the next start would only drop them again.
    ///
    /// This is the reachable half of the pair below: the venue *is* loaded and its
    /// catalog *is* in memory, so the pane's own gate passes and it is the capability
    /// check that settles the set. Nothing but the loaded venues say a loaded venue
    /// cannot answer tickers, which is why the fixture states the descriptor itself.
    #[gpui_kit::test]
    fn a_listing_of_a_route_without_tickers_should_leave_the_list(cx: &mut TestAppContext) {
        let published = Rc::new(Cell::new(0));
        let entries = vec![entry("BTCUSDT", "BTC")];
        let directory = directory(&entries);
        let widget = built_over_descriptor(
            cx,
            config(vec![key("BTCUSDT")]),
            PendingSource,
            directory,
            WidgetConfigChange::new({
                let published = published.clone();
                move |_| published.set(published.get() + 1)
            }),
            // The catalog is what makes the pane read the route at all; tickers are the
            // one capability it needs and the one this route does not declare.
            &[],
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        settle_until(
            cx,
            "the listing no route serves tickers for to leave",
            |cx| widget.read(cx).config.entries.is_empty(),
        );

        cx.update(|cx| {
            assert!(
                widget.read(cx).config.entries.is_empty(),
                "a listing of a route that declares no tickers cannot be followed"
            );
            assert!(
                !status(&widget, cx).is_failed(),
                "a route that serves no tickers is not a failure: the list follows nothing"
            );
        });
        assert_eq!(
            published.get(),
            1,
            "the removal is a configuration change and is written back"
        );
    }

    /// A market that is not loaded at all is a different state from a market that is
    /// loaded and serves no tickers: the pane waits for the user to load it instead
    /// of dropping what they follow, and nothing about the stored list is touched. What
    /// the user sees while it waits is the row they follow, saying so.
    #[gpui_kit::test]
    fn a_listing_of_an_unloaded_market_should_leave_the_list_waiting(cx: &mut TestAppContext) {
        let published = Rc::new(Cell::new(0));
        let widget = built_market(
            cx,
            config(vec![key("BTCUSDT")]),
            PendingSource,
            Fixture::unanswering(),
            WidgetConfigChange::new({
                let published = published.clone();
                move |_| published.set(published.get() + 1)
            }),
            false,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).config.entries,
                vec![key("BTCUSDT")],
                "a market that is not loaded does not drop what the user follows"
            );
            let status = status(&widget, cx);
            assert!(
                !status.is_failed(),
                "an unloaded market is not a failure: the list follows nothing"
            );
            assert!(
                widget.read(cx).awaiting_market && widget.read(cx).has_content(),
                "the pane waits by drawing the listing it cannot read: the row that says \
                 which market participant has to be loaded is what its body has to say"
            );
            assert_eq!(
                row_of(&widget, 0, cx).state(),
                &WatchListRowState::AwaitingMarket
            );
        });
        assert_eq!(published.get(), 0, "waiting changes no configuration");
    }

    /// A market revoked after the pane was built is a market that is not in memory any
    /// more, so the pane waits for the user to load it again instead of dropping
    /// what they follow.
    #[gpui_kit::test]
    fn a_market_revoked_after_construction_should_leave_the_list_waiting(cx: &mut TestAppContext) {
        let published = Rc::new(Cell::new(0));
        let entries = vec![entry("BTCUSDT", "BTC")];
        let directory = directory(&entries);
        let widget = built_with_change(
            cx,
            config(vec![key("BTCUSDT")]),
            PendingSource,
            directory,
            WidgetConfigChange::new({
                let published = published.clone();
                move |_| published.set(published.get() + 1)
            }),
        );
        // The pane starts over a market that is really there, which is what makes the
        // revocation below a change of state rather than the state it was built in.
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).config.entries,
                vec![key("BTCUSDT")],
                "the load left the followed listing in the list"
            );
        });

        cx.update(|cx| {
            let venue =
                market_participants_model::identifier::VenueId::from_name(test_support::VENUE)
                    .expect("the fixture venue is canonical");
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(&[], &[(venue, Vec::new())], &[])
                .expect("an empty scope set unloads the venue's slot");
            widget.update(cx, |widget, cx| widget.load(cx));
        });

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).config.entries,
                vec![key("BTCUSDT")],
                "a market that is gone does not unmake the user's list"
            );
            assert!(
                !status(&widget, cx).is_failed(),
                "a market that is no longer registered is not a failure"
            );
            assert!(
                widget.read(cx).awaiting_market,
                "the pane waits for the market it can no longer read"
            );
            assert_eq!(
                row_of(&widget, 0, cx).state(),
                &WatchListRowState::AwaitingMarket,
                "and the row it draws names that market participant"
            );
        });
        assert_eq!(published.get(), 0, "waiting changes no configuration");
    }

    /// The other half of the same story: a pane built while the market was never there
    /// waits for it from the first frame, which is the state a fresh workspace opens in
    /// before the user has loaded anything.
    #[gpui_kit::test]
    fn a_market_never_loaded_should_leave_the_list_waiting(cx: &mut TestAppContext) {
        let published = Rc::new(Cell::new(0));
        let widget = built_market(
            cx,
            config(vec![key("BTCUSDT")]),
            PendingSource,
            Fixture::unanswering(),
            WidgetConfigChange::new({
                let published = published.clone();
                move |_| published.set(published.get() + 1)
            }),
            false,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).config.entries,
                vec![key("BTCUSDT")],
                "a market nobody loaded does not unmake the user's list"
            );
            assert!(
                !status(&widget, cx).is_failed(),
                "a market nobody loaded is not a failure"
            );
            assert!(
                widget.read(cx).awaiting_market,
                "the pane waits for its market"
            );
            assert_eq!(
                row_of(&widget, 0, cx).state(),
                &WatchListRowState::AwaitingMarket
            );
        });
        assert_eq!(published.get(), 0, "waiting changes no configuration");
    }

    /// What a row says while its market is not loaded is a message of the catalogue and not
    /// a sentence stored with the row: the row holds the venue it names, and the frame that
    /// draws it reads those words in the language showing then. Three things follow, and this
    /// pins them: every locale this build ships defines the key — a key no locale defines
    /// renders as the empty string, so the cell would be blank; the sentence names the market
    /// the row waits for, which is the difference between the sentence with the name bound and
    /// the one with a hole in it; and the frame draws the cell that carries it.
    #[gpui_kit::test]
    fn a_row_should_name_the_market_to_load_in_the_language_showing(cx: &mut TestAppContext) {
        const KEY: &str = "widget.watch_list.market_metadata_not_loaded";

        let widget = built_market(
            cx,
            config(vec![key("BTCUSDT")]),
            PendingSource,
            Fixture::unanswering(),
            WidgetConfigChange::default(),
            false,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));

        let i18n = i18n::global::embedded::load().expect("the embedded locales should load");
        for language in i18n.languages() {
            let keys = i18n
                .keys(language)
                .expect("an embedded language exposes its keys")
                .collect::<Vec<_>>();
            assert!(
                keys.contains(&KEY),
                "{language} does not define {KEY}, so the row it draws is blank"
            );
            assert!(
                !i18n.translate(language, KEY).is_empty(),
                "{language} defines {KEY} and renders it empty"
            );
        }

        // The row itself holds no words: what it carries is the market participant to
        // name, and the sentence is resolved where the row is drawn.
        cx.update(|cx| {
            assert_eq!(
                row_of(&widget, 0, cx).state(),
                &WatchListRowState::AwaitingMarket
            );
            assert_eq!(row_of(&widget, 0, cx).key().route.venue, route().venue);
        });

        // The sentence the catalogue asks for is the words with the logo and the name in them,
        // and the catalogue writes both: the logo as an inline image of the path it is bound,
        // the name in bold. A call site that bound neither asks for a different sentence — the
        // same words with either hole left in — and the frame draws the one above.
        let window = body_window(cx, &widget);
        let (without_the_market, named) = window.cx.update(|cx| {
            let named = t!(
                cx,
                KEY,
                logo = route().venue.asset_path(),
                market = route().venue.display_string(cx)
            )
            .to_string();
            let unbound = t!(cx, KEY).to_string();

            assert_ne!(
                named, unbound,
                "the catalogue asks for a market participant, so naming one reads differently"
            );
            assert!(
                named.contains(route().venue.asset_path()),
                "the sentence states the market participant's own logo: {named}"
            );
            assert!(
                named.contains(&format!("**{}**", route().venue.display_string(cx))),
                "and sets its name in bold rather than as plain words: {named}"
            );
            (unbound.len(), named.len())
        });

        assert!(
            window
                .debug_bounds("WATCH_LIST_MISSING_MARKET_SENTENCE")
                .is_some_and(|sentence| sentence.size.width > px(0.0)),
            "the row draws the sentence that names the market it waits for"
        );
        assert!(
            named > without_the_market,
            "and the sentence the catalogue resolves for that call is the one with the market \
             in it rather than the one with a hole in it"
        );

        // What the cell draws is the sentence the widget holds for the venue, and what the row
        // shows is what the markdown component read out of it: the catalogue's punctuation is
        // what makes the name bold and the logo an image, so a source that carried the name
        // plainly would draw it plainly, and a cell that drew the source would show a reader
        // the punctuation.
        window.cx.update(|cx| {
            let table = widget.read(cx).continuous_table.clone();
            let sentence = table
                .read(cx)
                .sentences
                .get(&route().venue)
                .expect("the table keeps the sentence of every venue its rows are on")
                .clone();
            let sentence = sentence.read(cx);
            let source = sentence.source().to_string();
            let drawn = sentence.plain_text().to_string();
            let name = route().venue.display_string(cx).to_string();

            assert!(
                source.contains(&format!("**{name}**")),
                "the sentence the row draws sets the market participant's name in bold: {source}"
            );
            assert!(
                source.contains(&format!("file://{}", route().venue.asset_path())),
                "and draws its logo from the market participant's own asset: {source}"
            );
            assert!(
                !drawn.contains("**") && !drawn.contains("![]("),
                "while what a reader is shown carries none of that markdown: {drawn}"
            );
            assert!(
                drawn.contains(&name),
                "it is the catalogue's words with the market participant in them: {drawn}"
            );
        });
    }

    /// The sentence a waiting row is drawn with follows the language the interface is showing,
    /// and nothing tells the widget that the language changed: what a switch does is rebuild the
    /// frame, so a sentence read while the rows were built would stay in the language it was
    /// built in. Reading it again on every frame is what this pins — the state is handed the
    /// words of the language showing now, and the markdown component parses them again because
    /// they are not the ones it holds.
    #[gpui_kit::test]
    fn a_waiting_row_should_say_what_it_has_to_say_in_the_language_showing(
        cx: &mut TestAppContext,
    ) {
        let widget = built_market(
            cx,
            config(vec![key("BTCUSDT")]),
            PendingSource,
            Fixture::unanswering(),
            WidgetConfigChange::default(),
            false,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        let window = body_window(cx, &widget);
        let venue = route().venue;

        let drawn = |cx: &mut App| {
            let table = widget.read(cx).continuous_table.clone();
            let sentence = table
                .read(cx)
                .sentences
                .get(&venue)
                .expect("the table keeps the sentence of every venue its rows are on")
                .clone();
            let sentence = sentence.read(cx);
            (
                sentence.source().to_string(),
                sentence.plain_text().to_string(),
            )
        };

        // The language is switched the way the application switches it — the global is set and
        // the windows are refreshed — and the frame after it is the frame a pane draws. The
        // refresh is what the window does with that; the frame itself is drawn here, because a
        // test has no loop to draw one.
        window.cx.update(|cx| {
            cx.set_global(LanguageCode::En);
            cx.refresh_windows();
        });
        repaint(&mut window.clone());
        let (english_source, english_drawn) = window.cx.update(drawn);

        window.cx.update(|cx| {
            cx.set_global(LanguageCode::ZhCn);
            cx.refresh_windows();
        });
        repaint(&mut window.clone());
        let (chinese_source, chinese_drawn) = window.cx.update(drawn);

        assert_ne!(
            english_source, chinese_source,
            "the sentence is read in the language the interface is showing, and it is \
             {english_source}"
        );
        assert!(
            english_drawn.contains("Load the metadata"),
            "and the language the row started in is the one it was drawn in: {english_drawn}"
        );
        assert!(
            chinese_drawn.contains("请先加载"),
            "while the language it was switched to is the one it is drawn in afterwards: \
             {chinese_drawn}"
        );
        for drawn in [&english_drawn, &chinese_drawn] {
            assert!(
                !drawn.contains("**") && !drawn.contains("![]("),
                "and neither of them shows a reader the markdown they are written in: {drawn}"
            );
        }
    }

    /// A list that follows nothing still waits for its first market through the
    /// framework's placeholder: the picker that adds a listing is drawn from the loaded
    /// venues, so with none of them loaded there is nothing to add and nothing to fill
    /// the table with. No single market can be named for it — the user has named none —
    /// so the placeholder lists the venues that could be one instead, and the list itself
    /// draws nothing: it follows nothing, so it has no row to say anything with.
    #[gpui_kit::test]
    fn a_list_that_follows_nothing_should_wait_for_its_first_market(cx: &mut TestAppContext) {
        let widget = built_market(
            cx,
            WatchListConfig::default(),
            PendingSource,
            Fixture::unanswering(),
            WidgetConfigChange::default(),
            false,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));

        cx.update(|cx| {
            let status = status(&widget, cx);
            assert!(
                status.failure().is_none(),
                "an unloaded market is a wait rather than a failure"
            );
            let notice = status
                .notice()
                .expect("a list with nothing in it still has no market to add from");
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("Binance") && sentence.contains("OKX"),
                "the placeholder names every market this list could read: {sentence}"
            );
            assert!(
                !widget.read(cx).has_content(),
                "a list that follows nothing has no row to say anything with"
            );

            // The words are the frame's to resolve, so a language the list did not report in
            // is still the language it waits in.
            cx.set_global(LanguageCode::ZhCn);
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("币安") && sentence.contains("欧易"),
                "and says the same thing in Chinese now: {sentence}"
            );
        });
    }

    /// The other side of that gate: a market is loaded, so an empty list is the empty table
    /// it says it is — "nothing followed yet" is not a state to wait in, and the user can add
    /// to it from what is there.
    #[gpui_kit::test]
    fn a_list_that_follows_nothing_over_a_loaded_market_should_report_no_notice(
        cx: &mut TestAppContext,
    ) {
        let widget = constructed(cx, WatchListConfig::default());
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));

        cx.update(|cx| {
            assert!(
                status(&widget, cx).notice().is_none(),
                "a market is loaded, so there is no market to wait for"
            );
        });
    }

    /// Loading the venue is what a waiting pane recovers from, by itself and
    /// without being reopened (⑯).
    #[gpui_kit::test]
    fn loading_the_venue_should_let_the_waiting_list_subscribe(cx: &mut TestAppContext) {
        let source = AnsweringSource::default();
        let entries = vec![entry("BTCUSDT", "BTC")];
        let widget = built_market(
            cx,
            config(vec![key("BTCUSDT")]),
            source.clone(),
            directory(&entries),
            WidgetConfigChange::default(),
            false,
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        cx.update(|cx| {
            assert!(
                widget.read(cx).awaiting_market
                    && row_of(&widget, 0, cx).state() == &WatchListRowState::AwaitingMarket,
                "the pane waits for a market nobody loaded, with the row it follows on screen"
            );

            let venue =
                market_participants_model::identifier::VenueId::from_name(test_support::VENUE)
                    .expect("the fixture venue is canonical");
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[venue]),
                    &[],
                    &[],
                )
                .expect("the fixture venue's routes should load");
            // A publication is what emits the directory's change signal; a load alone
            // does not.
            market_participants_connector::catalog::test_support::publish_fixture_catalogs(
                cx,
                &[(route(), entries)],
            );
        });
        // The directory signals its publications through its own task, so the event
        // that re-runs the load arrives on the executor.
        cx.run_until_parked();
        settle_until(cx, "the list to subscribe", |_| {
            !source.subscribed().is_empty()
        });

        cx.update(|cx| {
            let status = status(&widget, cx);
            assert!(
                status.notice().is_none(),
                "the pane left the notice without being reopened"
            );
            assert!(!status.is_failed(), "recovery is not a failure");
        });
    }

    /// A list opened following nothing reads nothing, and the listing the user then adds is what
    /// moves what it reads — not a venue commit. A commit of another venue therefore cannot move
    /// the list's routes, so the pane leaves the rows it is reading exactly as they are rather than
    /// starting over what the user is watching.
    ///
    /// The commit is proved to have reached the pane by a probe subscribed after it did, and what
    /// proves the pane did not start over is its own directory generation beside the confirmed
    /// rows it was drawing.
    #[gpui_kit::test]
    fn adding_a_listing_should_leave_the_list_alone_on_another_venues_commit(
        cx: &mut TestAppContext,
    ) {
        let widget = built(
            cx,
            config(Vec::new()),
            PendingSource,
            directory(&[entry("BTCUSDT", "BTC")]),
        );
        cx.update(|cx| {
            widget.update(cx, |widget, cx| {
                widget.load(cx);
                // The load of a list that follows nothing is answered before the user adds
                // anything: the add control refuses input while an operation is in flight.
                widget.apply_rpc_result(Ok(WatchListResponse::Applied), cx);
            });
        });
        // The commits the fixture itself made — the venue loading, and the catalog publication —
        // are drained before anything is added: what this test is about is the commit below, and
        // one still in flight would be handled as a commit of a market the list had only just
        // started following.
        cx.run_until_parked();
        let deliveries = Arc::new(AtomicUsize::new(0));
        let _probe = cx.update(|cx| {
            let events = cx.global::<MarketParticipantsConnector>().venues().events();
            cx.new(|cx| CommitProbe::new(cx, &events, Arc::clone(&deliveries)))
        });
        let delivered = deliveries.load(Ordering::Acquire);

        cx.update(|cx| {
            assert!(
                widget.read(cx).config.entries.is_empty(),
                "the pane is opened following nothing"
            );
            widget.update(cx, |widget, cx| widget.add(selection("BTCUSDT"), cx));

            assert_eq!(
                widget.read(cx).config.entries,
                vec![key("BTCUSDT")],
                "the listing the user added is what the list follows"
            );
        });
        let generation = cx.read(|cx| widget.read(cx).directory_generation);

        // Another venue commits: nothing about the list's routes moved, so the rows it is
        // following are left exactly as they are.
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[
                        market_participants_model::identifier::VenueId::Okx,
                    ]),
                    &[],
                    &[],
                )
                .expect("another venue loads");
        });
        settle_until(
            cx,
            "the other venue's commit to reach its subscribers",
            |_| deliveries.load(Ordering::Acquire) > delivered,
        );

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).directory_generation,
                generation,
                "a commit of a venue this list does not follow must not start it over"
            );
            assert!(
                widget.read(cx).confirmed,
                "the rows the user is reading must not be dropped"
            );
            assert_eq!(
                rows(&widget, cx),
                1,
                "and the listing the user added must still be the row on screen"
            );
            assert_eq!(
                status(&widget, cx),
                WidgetState::Ready,
                "and the pane must not go back to connecting"
            );
        });
    }

    /// A refresh of a market this list follows replaces the catalogue generation every one of its
    /// streams resolved their rows' increments against, while those streams keep what they resolved
    /// when they were opened. So the list starts over — the rows go, because nothing on screen can
    /// be vouched for — and the request it sends is the one that reopens every one of those
    /// streams, at the generation the load published.
    ///
    /// What proves both halves is the source's own record: the subscription it was asked for at the
    /// moment the service opened it, and the increments that listing was published at then.
    #[gpui_kit::test]
    fn a_refresh_of_a_market_the_list_follows_should_reopen_its_streams(cx: &mut TestAppContext) {
        let resolutions = Arc::new(Mutex::new(Vec::new()));
        let directory = directory(&[entry("BTCUSDT", "BTC")]);
        let source = ResolvingSource {
            catalog: Arc::clone(directory.catalog_arc()),
            resolutions: Arc::clone(&resolutions),
            live: Arc::new(Mutex::new(Vec::new())),
        };
        let widget = widget_over(cx, config(vec![key("BTCUSDT")]), source.clone(), directory);
        // Both halves of start-up are waited for: the stream opening and the list confirming the
        // set it is reading are two answers, and they do not have to arrive in one turn.
        settle_until(cx, "the list's stream to be opened", |_| {
            resolutions_of(&resolutions).len() == 1
        });
        settle_until(cx, "the list to confirm its set", |cx| {
            widget.read(cx).confirmed
        });
        let first = source.streams()[0].clone();
        let generation = cx.read(|cx| widget.read(cx).directory_generation);

        let venue = market_participants_model::identifier::VenueId::from_name(test_support::VENUE)
            .expect("the fixture venue is canonical");
        cx.update(|cx| {
            // The same venue loads again and the catalogue it republishes restates the listing's
            // price tick, which is the shape a production load has: it re-fetches the catalogue.
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(
                        std::slice::from_ref(&venue),
                    ),
                    &[],
                    &[],
                )
                .expect("the list's venue loads again");
            test_support::publish_fixture_catalogs(cx, &[(route(), vec![restated_entry("0.01")])]);
        });
        // Both halves are waited for: the source being asked again is one claim, and the stream
        // that was running being gone is another — the service opens the new one and retires the
        // old one on either side of a task boundary, so waiting for the first alone would read
        // the second while it is still up.
        settle_until(cx, "the list's first stream to be retired", |_| {
            first.load(Ordering::Acquire)
        });
        settle_until(cx, "the list to open a new stream", |_| {
            resolutions_of(&resolutions).len() == 2
        });

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).directory_generation,
                generation + 1,
                "a refresh of a market the list follows starts it over"
            );
            assert!(
                !status(&widget, cx).is_failed(),
                "and the request it sends is one the service accepts: {status:?}",
                status = status(&widget, cx)
            );
            assert_eq!(
                resolutions_of(&resolutions)[1].price_tick.to_string(),
                "0.01",
                "the stream the service opened resolved at the generation this load published"
            );
            assert!(
                first.load(Ordering::Acquire),
                "and it is the stream the list was reading that was retired"
            );
            assert!(
                source
                    .streams()
                    .last()
                    .is_some_and(|last| !last.load(Ordering::Acquire)),
                "which leaves the stream it was replaced by running"
            );
        });
    }

    /// A market that goes away and comes back reopens the streams the list was reading before
    /// it went: those streams resolved the increments of the retired generation and are still
    /// alive, so the request that brings the market back has to be the one that re-opens them
    /// — the wait in between is not a reason to keep what they resolved.
    #[gpui_kit::test]
    fn a_market_that_comes_back_should_reopen_the_streams_the_wait_kept(cx: &mut TestAppContext) {
        let resolutions = Arc::new(Mutex::new(Vec::new()));
        let directory = directory(&[entry("BTCUSDT", "BTC")]);
        let source = ResolvingSource {
            catalog: Arc::clone(directory.catalog_arc()),
            resolutions: Arc::clone(&resolutions),
            live: Arc::new(Mutex::new(Vec::new())),
        };
        let widget = widget_over(cx, config(vec![key("BTCUSDT")]), source, directory);
        settle_until(cx, "the list's stream to be opened", |_| {
            resolutions_of(&resolutions).len() == 1
        });

        let venue = market_participants_model::identifier::VenueId::from_name(test_support::VENUE)
            .expect("the fixture venue is canonical");
        cx.update(|cx| {
            // The venue loads again and publishes no scope: the listing's market is not in
            // memory, so the pane waits for it — and the flag that says its streams have to be
            // resolved again is not spent by a load that sent nothing.
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(&[], &[(venue, Vec::new())], &[])
                .expect("the shrinking commit lands");
        });
        settle_until(cx, "the pane to wait for its market", |cx| {
            widget.read(cx).awaiting_market
        });

        cx.update(|cx| {
            // The user loads the market again, with a catalogue that restates the listing's
            // price tick so a stream that resolved the old one can be told apart from a new one.
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(
                        std::slice::from_ref(&venue),
                    ),
                    &[],
                    &[],
                )
                .expect("the market loads again");
            test_support::publish_fixture_catalogs(cx, &[(route(), vec![restated_entry("0.01")])]);
        });
        settle_until(cx, "the list to open a new stream", |_| {
            resolutions_of(&resolutions).len() == 2
        });

        cx.update(|cx| {
            assert!(
                !widget.read(cx).awaiting_market,
                "the listing is read again"
            );
            assert_eq!(
                resolutions_of(&resolutions)[1].price_tick.to_string(),
                "0.01",
                "and the stream the service opened resolved at the generation this load \
                 published rather than keeping the one it resolved before the wait"
            );
        });
    }

    /// A market that a failed pane could not act on is still a market that was replaced: the
    /// recovery keeps the rows the user was looking at, so it is the request that has to reopen
    /// their streams. Nothing else can — a failed pane does not act on commits, and the streams
    /// that survived the failure resolved the increments of the generation those commits
    /// replaced.
    #[gpui_kit::test]
    fn a_recovery_should_reopen_the_streams_a_commit_replaced_while_it_was_failed(
        cx: &mut TestAppContext,
    ) {
        let resolutions = Arc::new(Mutex::new(Vec::new()));
        let directory = directory(&[entry("BTCUSDT", "BTC")]);
        let source = ResolvingSource {
            catalog: Arc::clone(directory.catalog_arc()),
            resolutions: Arc::clone(&resolutions),
            live: Arc::new(Mutex::new(Vec::new())),
        };
        let widget = widget_over(cx, config(vec![key("BTCUSDT")]), source, directory);
        settle_until(cx, "the list's stream to be opened", |_| {
            resolutions_of(&resolutions).len() == 1
        });

        cx.update(|cx| {
            widget.update(cx, |widget, cx| {
                widget.apply_rpc_result(Err(refusal()), cx);
            });
            assert!(status(&widget, cx).is_failed(), "the list has failed");
        });

        let venue = market_participants_model::identifier::VenueId::from_name(test_support::VENUE)
            .expect("the fixture venue is canonical");
        cx.update(|cx| {
            // The market the failed list was reading is refreshed underneath it: the commit is
            // one this pane is not allowed to act on, and it republishes the listing at a new
            // price tick so a stream that resolved the old one can be told apart from a new one.
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(
                        std::slice::from_ref(&venue),
                    ),
                    &[],
                    &[],
                )
                .expect("the market loads again");
            test_support::publish_fixture_catalogs(cx, &[(route(), vec![restated_entry("0.01")])]);
        });
        settle_until(cx, "the failed pane to stay as it is", |cx| {
            status(&widget, cx).is_failed()
        });

        // The user takes the recovery, which is the only thing that starts the list again.
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        settle_until(cx, "the recovery to open a new stream", |_| {
            resolutions_of(&resolutions).len() == 2
        });

        assert_eq!(
            resolutions_of(&resolutions)[1].price_tick.to_string(),
            "0.01",
            "the stream the recovery opened resolved at the generation the refresh published \
             rather than keeping the one it resolved before the failure"
        );
    }

    /// The other half of that rule: a scope loaded *after* the list started reading is a scope
    /// the list's picker offers. The control is built from the loaded venues on the frame that
    /// builds it, so a commit the pane does not act on still widens what the user may add from —
    /// a reading kept from the last load would leave the venue the user just loaded out of the
    /// picker altogether, neither its column nor its catalog.
    ///
    /// The commit is proved to have reached the pane by a probe subscribed after it did, and what
    /// proves the freshness does not ride on starting over is the pane's own directory generation
    /// beside the confirmed rows it was drawing.
    #[gpui_kit::test]
    fn a_venue_loaded_after_the_list_started_reading_should_be_offered_by_its_picker(
        cx: &mut TestAppContext,
    ) {
        let widget = scoped_widget(cx);
        // The commits the fixture itself made — the venue loading, and the catalog publication —
        // are drained before the probe is subscribed: what this test is about is the commit below.
        cx.run_until_parked();
        let deliveries = Arc::new(AtomicUsize::new(0));
        let _probe = cx.update(|cx| {
            let events = cx.global::<MarketParticipantsConnector>().venues().events();
            cx.new(|cx| CommitProbe::new(cx, &events, Arc::clone(&deliveries)))
        });
        let delivered = deliveries.load(Ordering::Acquire);
        let generation = cx.read(|cx| widget.read(cx).directory_generation);
        // The premise the assertion below turns on: the venue is not one the picker offers yet,
        // so what widens it is this commit and not a route the fixture had already loaded.
        cx.update(|cx| {
            assert!(
                !offers_route(
                    &widget.read(cx).instrument_filters(cx).routes,
                    &okx_spot_route(),
                    CapabilityId::Tickers
                ),
                "the venue is not loaded before the commit below"
            );
        });

        // Another venue commits: nothing about the list's routes moved, so the rows it is
        // following are left exactly as they are.
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[
                        market_participants_model::identifier::VenueId::Okx,
                    ]),
                    &[],
                    &[],
                )
                .expect("another venue loads");
        });
        settle_until(
            cx,
            "the other venue's commit to reach its subscribers",
            |_| deliveries.load(Ordering::Acquire) > delivered,
        );

        cx.update(|cx| {
            let filters = widget.read(cx).instrument_filters(cx);
            assert!(
                offers_route(&filters.routes, &okx_spot_route(), CapabilityId::Tickers),
                "a scope loaded after the list started reading is one its picker offers"
            );
            assert_eq!(
                widget.read(cx).directory_generation,
                generation,
                "and the freshness does not ride on starting the list over"
            );
            assert!(
                widget.read(cx).confirmed,
                "the rows the user is reading are still confirmed"
            );
            assert_eq!(
                rows(&widget, cx),
                1,
                "and the listing the user follows is still the row on screen"
            );
        });
    }

    /// A commit on a market none of the followed listings is on leaves the pane exactly as it
    /// is — including a row that a catalog of its own took out of the reading one row at a
    /// time. Starting the list over for it would rebuild every entry as one waiting for a
    /// stream the service never closed, so the rows the user is reading would stop carrying
    /// values without anything having gone wrong with them.
    #[gpui_kit::test]
    fn a_commit_on_another_market_should_not_start_the_list_over(cx: &mut TestAppContext) {
        let widget = scoped_widget_over(cx, &[entry("BTCUSDT", "BTC"), entry("ETHUSDT", "ETH")]);
        cx.update(|cx| {
            widget.update(cx, |widget, cx| {
                widget.add(selection("ETHUSDT"), cx);
                widget.apply_rpc_result(Ok(WatchListResponse::Applied), cx);
            });
        });
        // The load the pane was built over is allowed to settle before anything is measured:
        // what this test is about is the commit below, not the start-up.
        settle_until(cx, "the pane's own load to settle", |cx| {
            widget.read(cx).pending.is_none()
        });
        let generation = cx.update(|cx| widget.read(cx).directory_generation);

        cx.update(|cx| {
            accept_ticker(&widget, "BTCUSDT", 100, 120, cx);
            accept_ticker(&widget, "ETHUSDT", 10, 12, cx);
            // One listing's catalog goes away underneath it: that row says what is missing,
            // and the other one keeps being read.
            publish_reliable(
                &widget,
                &key("BTCUSDT"),
                WatchListStreamEvent::Error(WatchListStreamError::MarketParticipant(
                    MarketError::Catalog(Box::new(CatalogError::CatalogNotLoaded {
                        route: route(),
                    })),
                )),
                cx,
            );
            assert!(!widget.read(cx).entries.contains_key(&key("BTCUSDT")));
        });

        // Another market is loaded — one no followed listing is on.
        cx.update(|cx| {
            let other = market_participants_model::identifier::VenueId::from_name("okx")
                .expect("canonical");
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(&[], &[(other, Vec::new())], &[])
                .expect("the commit lands");
        });
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).directory_generation,
                generation,
                "a commit about another market did not start this list over"
            );
            // The rows are the followed listings in their own order, so row 0 is BTCUSDT —
            // the one whose catalog went away.
            assert_eq!(
                row_of(&widget, 0, cx).state(),
                &WatchListRowState::AwaitingMarket,
                "the listing whose catalog is gone still says so"
            );
            assert!(
                widget.read(cx).entries.contains_key(&key("ETHUSDT")),
                "and the listing that was being read still is"
            );
        });

        // A ticker for the healthy row is still accepted, which is what a pane whose entries
        // had been rebuilt as connecting could not do.
        cx.update(|cx| {
            accept_ticker(&widget, "ETHUSDT", 10, 14, cx);
            assert_eq!(
                price_of(&row_of(&widget, 1, cx)).as_deref(),
                Some("14.00"),
                "the row the list was reading kept its stream"
            );
        });
    }

    /// A pane that was reading an empty list loses its markets: the gate is asked again, so
    /// the pane says which markets it could load rather than drawing an empty table whose
    /// picker has nothing in it.
    #[gpui_kit::test]
    fn a_commit_that_takes_the_last_market_away_should_send_an_empty_list_back_to_waiting(
        cx: &mut TestAppContext,
    ) {
        let widget = built(
            cx,
            config(Vec::new()),
            PendingSource,
            directory(&[entry("BTCUSDT", "BTC")]),
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        cx.update(|cx| {
            assert!(
                !widget.read(cx).awaiting_market,
                "a list that follows nothing is read through the markets themselves"
            );
            assert!(
                status(&widget, cx).notice().is_none(),
                "and one of them is loaded, so the pane draws its empty table"
            );
        });

        cx.update(|cx| {
            let venue =
                market_participants_model::identifier::VenueId::from_name(test_support::VENUE)
                    .expect("the fixture venue is canonical");
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(&[], &[(venue, Vec::new())], &[])
                .expect("the shrinking commit lands");
        });
        // The pane is given its turn to be told, the way a window's own event loop does.
        settle_until(cx, "the commit to reach the pane", |cx| {
            widget.read(cx).awaiting_market
        });

        cx.update(|cx| {
            assert!(
                widget.read(cx).awaiting_market,
                "with no market loaded there is nothing to read and nothing to add"
            );
            assert!(
                status(&widget, cx).notice().is_some(),
                "so the pane says which markets it could load: {:?}",
                status(&widget, cx)
            );
        });
    }

    /// The other direction of the same rule: a commit that takes the list's own markets away is a
    /// market it can no longer read, so the pane goes back to waiting for it — without being
    /// reopened, and without losing the listings the user follows.
    #[gpui_kit::test]
    fn a_commit_that_takes_the_lists_own_market_away_should_send_it_back_to_waiting(
        cx: &mut TestAppContext,
    ) {
        let widget = scoped_widget(cx);
        cx.update(|cx| {
            assert!(widget.read(cx).has_content(), "the list is reading rows");
        });

        // The venue loads again and publishes no scope at all, which is what a shrinking commit
        // does: the market behind the row is not in memory any more.
        cx.update(|cx| {
            let venue =
                market_participants_model::identifier::VenueId::from_name(test_support::VENUE)
                    .expect("the fixture venue is canonical");
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(&[], &[(venue, Vec::new())], &[])
                .expect("the shrinking commit lands");
        });
        cx.run_until_parked();

        cx.update(|cx| {
            assert!(
                widget.read(cx).awaiting_market,
                "the list waits for the market it lost"
            );
            assert!(
                widget.read(cx).has_content(),
                "what it follows is still the user's list, and the row that says which \
                 market participant it is waiting for is what it draws"
            );
            assert_eq!(
                widget.read(cx).config.entries,
                vec![key("BTCUSDT")],
                "a market that went away does not unmake the user's list"
            );
            assert_eq!(
                rows(&widget, cx),
                1,
                "the listing the user follows still has its row"
            );
            assert_eq!(
                status(&widget, cx),
                WidgetState::Ready,
                "a pane whose body says what is missing is not waiting for a placeholder"
            );
        });
    }

    /// A failure is terminal: a venue commit does not revive a failed list, even when the commit
    /// takes the markets its rows came from away and puts them back.
    ///
    /// The first commit is the one a list without the terminal guard would act on: it takes the
    /// list's own markets away, so an unguarded handler would replace the failure with the neutral
    /// notice. The second puts the market back, which a handler that judged "did what I read
    /// change" without the guard would read as a scope that appeared and would start over for.
    #[gpui_kit::test]
    fn a_venue_commit_should_not_revive_a_failed_pane(cx: &mut TestAppContext) {
        let widget = built(
            cx,
            config(vec![key("BTCUSDT")]),
            PendingSource,
            directory(&[entry("BTCUSDT", "BTC")]),
        );
        cx.update(|cx| widget.update(cx, |widget, cx| widget.load(cx)));
        // The answer to the load this list started is drained before the failure is reported:
        // what this test is about is the commits below, and an acknowledgement still in flight
        // would be an answer about the list rather than about them.
        settle_until(cx, "the load's own answer to land", |cx| {
            widget.read(cx).confirmed
        });
        let failure = cx.update(|cx| {
            widget.update(cx, |widget, cx| widget.apply_rpc_result(Err(refusal()), cx));

            let state = status(&widget, cx);
            assert!(state.is_failed(), "the list is in its terminal failure");
            state
        });
        let venue = market_participants_model::identifier::VenueId::from_name(test_support::VENUE)
            .expect("the fixture venue is canonical");

        let deliveries = Arc::new(AtomicUsize::new(0));
        let _probe = cx.update(|cx| {
            let events = cx.global::<MarketParticipantsConnector>().venues().events();
            cx.new(|cx| CommitProbe::new(cx, &events, Arc::clone(&deliveries)))
        });
        // The market the list follows leaves the loaded venues...
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(&[], &[(venue, Vec::new())], &[])
                .expect("the shrinking commit lands");
        });
        settle_until(cx, "the shrinking commit to reach its subscribers", |_| {
            deliveries.load(Ordering::Acquire) >= 1
        });
        // ...and comes back.
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[venue]),
                    &[],
                    &[],
                )
                .expect("the venue loads again");
        });
        settle_until(
            cx,
            "the commit that puts the market back to reach its subscribers",
            |_| deliveries.load(Ordering::Acquire) >= 2,
        );

        cx.update(|cx| {
            assert_eq!(
                status(&widget, cx),
                failure,
                "a venue commit must not revive a failed list"
            );
            assert!(
                !widget.read(cx).awaiting_market,
                "a list in its terminal failure is not a list waiting for a market"
            );
        });
    }

    /// What each route still carries is settled before anything is followed: a listing
    /// nobody publishes never gets subscribed to, and the ones that survive are the set
    /// the service is asked for.
    #[gpui_kit::test]
    fn the_set_that_is_followed_should_be_the_set_the_routes_still_carry(cx: &mut TestAppContext) {
        let source = AnsweringSource::default();
        let widget = widget_over(
            cx,
            config(vec![key("BTCUSDT"), key("ETHUSDT")]),
            source.clone(),
            directory(&[entry("BTCUSDT", "BTC")]),
        );

        settle_until(cx, "the widget to subscribe", |_| {
            !source.subscribed().is_empty()
        });
        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).config.entries,
                vec![key("BTCUSDT")],
                "ETHUSDT is not offered by the route any more"
            );
            assert!(!widget.read(cx).entries.contains_key(&key("ETHUSDT")));
        });
        // Compared as a set: the one request names every listing it follows, and the order the
        // service opens their streams in is its own business rather than something a pane asks
        // for.
        let mut subscribed = source.subscribed();
        sort_entries(&mut subscribed);
        assert_eq!(
            subscribed,
            vec![key("BTCUSDT")],
            "the service was asked to follow a listing the route no longer publishes"
        );
    }

    /// A set the routes carry none of is asked for as the empty set, rather than every
    /// listing in it being followed and then dropped.
    #[gpui_kit::test]
    fn a_set_the_routes_dropped_entirely_should_follow_nothing(cx: &mut TestAppContext) {
        let source = AnsweringSource::default();
        let widget = widget_over(
            cx,
            config(vec![key("BTCUSDT")]),
            source.clone(),
            directory(&[entry("ETHUSDT", "ETH")]),
        );

        settle_until(cx, "the widget to settle its set", |cx| {
            widget.read(cx).config.entries.is_empty()
        });
        cx.update(|cx| {
            assert!(widget.read(cx).entries.is_empty());
            assert!(
                !status(&widget, cx).is_failed(),
                "a route without the listing is not a failure: the list follows nothing"
            );
        });
        assert!(
            source.subscribed().is_empty(),
            "the widget followed a listing the route no longer publishes"
        );
    }

    /// A listing its venue has settled or delisted is one no stream can serve, so it is
    /// dropped like a listing the route stopped publishing.
    #[gpui_kit::test]
    fn a_delisted_listing_should_leave_the_list(cx: &mut TestAppContext) {
        let source = AnsweringSource::default();
        let widget = widget_over(
            cx,
            config(vec![key("BTCUSDT"), key("ETHUSDT")]),
            source.clone(),
            directory(&[
                test_support::with_status(entry("BTCUSDT", "BTC"), ListingStatus::Delisted),
                entry("ETHUSDT", "ETH"),
            ]),
        );

        settle_until(cx, "the widget to settle its set", |cx| {
            widget.read(cx).config.entries == vec![key("ETHUSDT")]
                && !source.subscribed().is_empty()
        });
        cx.update(|cx| {
            assert!(
                !status(&widget, cx).is_failed(),
                "a delisted listing is not a failure of the watch list"
            );
        });
        // Compared as a set, for the same reason the other subscribed assertion is.
        let mut subscribed = source.subscribed();
        sort_entries(&mut subscribed);
        assert_eq!(subscribed, vec![key("ETHUSDT")]);
    }

    /// The main text of a row's instrument column is the name its generation
    /// published, and nothing else: the fixture listing's native symbol never appears
    /// in it, and neither does its canonical identity.
    ///
    /// The row's first column is user-visible text, and `instrument_cell` renders
    /// exactly this read of the row's own key, so a fallback to the venue's symbol or
    /// to `listing-id/v1/…` is what this catches.
    #[gpui_kit::test]
    fn a_row_should_be_named_by_the_published_display(cx: &mut TestAppContext) {
        let widget = scoped_widget_over(cx, &[entry("BTCUSDT", "BTC")]);

        cx.update(|cx| {
            let row = row_of(&widget, 0, cx);
            let name = listing_label(cx, row.key());

            assert_eq!(name.as_ref(), "BTC/USDT");
            assert!(
                !name.contains("BTCUSDT"),
                "the venue's own symbol is not a name: {name}"
            );
            assert!(
                !name.contains("listing-id/v1/"),
                "a canonical identity is not a name: {name}"
            );
        });
    }

    /// A listing no published generation names is the localized unknown rather than
    /// its identity, so a route that never answered cannot put a digest on a row.
    #[gpui_kit::test]
    fn a_listing_no_generation_names_should_be_the_localized_unknown(cx: &mut TestAppContext) {
        let widget = constructed(cx, config(vec![key("BTCUSDT")]));

        cx.update(|cx| {
            let followed = widget
                .read(cx)
                .config
                .entries
                .first()
                .cloned()
                .expect("the fixture config follows a listing");
            let name = listing_label(cx, &followed);

            assert_eq!(name, t!(cx, "error.display.unknown_listing"));
            assert!(
                !name.contains("listing-id/v1/"),
                "an identity is not a name: {name}"
            );
        });
    }

    /// One catalog entry of the fixture listing.
    ///
    /// The instrument is part of the fixture rather than fixed, because two listings
    /// of one route can carry two different names and the row a reader follows is
    /// told apart by its key.
    fn entry(symbol: &str, base: &str) -> CatalogEntry {
        test_support::entry(
            &route(),
            test_support::spot_token(test_support::VENUE, base, "USDT"),
            symbol,
        )
    }

    /// A directory that publishes the given entries.
    fn directory(entries: &[CatalogEntry]) -> Fixture {
        Fixture::new(entries.to_vec())
    }

    /// The fixture listing again, published at another price tick.
    ///
    /// It is the shape a production load commits: the load re-fetches the catalogue, and what it
    /// publishes is the listing restated at the venue's own increments.
    fn restated_entry(price_tick: &str) -> CatalogEntry {
        let mut restated = entry("BTCUSDT", "BTC");
        restated.snapshot.rules.common.price.tick = price_tick
            .parse()
            .expect("the restated price tick should parse");
        restated
    }
}

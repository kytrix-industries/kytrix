//! The order-book widget.
//!
//! One widget follows one venue and one instrument, and draws the depth that
//! venue publishes for it: the ask ladder above the mid price, the bid ladder
//! below it, and the imbalance between the two sides. A book that has not been
//! pointed at an instrument yet asks for one and opens no stream; everything it
//! *does* around a frame — connecting, synchronizing the opening book, failing —
//! is reported through the framework's status handle, and the body only ever
//! draws the book it has.
//!
//! # The ladder: labels are nodes, depth bars are a canvas
//!
//! Each side is a [`List`] of row labels — three text nodes per row, all of them
//! `bind_text` closures over the book signal this pane publishes — with a depth
//! bar canvas drawn behind the list. Splitting the two is what makes an easing
//! frame cheap: the bars move on every frame while the labels only change when
//! the book does (the model reuses each formatted label for as long as its value
//! is the same). The canvas carries quads only, so no glyph lease or registered
//! geometry has to be handed back; what the pane keeps is the prepared frame key
//! in [`paint`](crate::paint), and a steady `sync` writes no canvas at all (I3).
//!
//! The rows are ordinary nodes rather than one big canvas for a second reason:
//! a ladder is text, and text belongs to the engine. It is laid out, shaped,
//! clipped and exposed to assistive technology by the same code as every other
//! label in the product, and a language or appearance switch needs no canvas
//! rebuild.
//!
//! # Three pieces of local state
//!
//! Three things make the book feel like a book rather than a picture, and each of
//! them is deliberately kept out of the status:
//!
//! - **the grouped ladder**, which is rebuilt from the snapshot the widget is
//!   already holding whenever the grouping, the row limit or the pane's height
//!   changes. None of those is worth reopening a stream for;
//! - **the depth animation**, which eases every published book towards the ratios
//!   it asks for, and stops as soon as the bars have arrived. It is driven by one
//!   [E10](ui_core::app::App::after_scoped) timer chain at the engine's frame
//!   tick: a steady book schedules no timer, and a settled movement stops the
//!   chain (I3, `reduce_motion` lands the bars where they belong);
//! - **the previous mid price**, which is only there to say which way the mid
//!   price has moved since the frame before.
//!
//! # The header: one picker and two drawing choices
//!
//! The left group holds the instrument picker — the same
//! [`InstrumentButton`](instrument_selector_ui::instrument_button::InstrumentButton)
//! every pane that follows a listing draws — and the right group the price
//! grouping and the row limit, both [`Select`]s. The picker opens the selector
//! through [`SelectorHost`], the host the selector crate froze with the widget
//! contract; the host takes the composition root's shared services handle
//! (`Rc::clone(cx.services_rc())`), never a second aggregate assembled here.
//!
//! A control's label and its entries are chosen when it is built, so a language
//! switch, a new selection or a directory generation that may have widened what
//! the picker offers rebuilds it in place; a header row the pane replaced first
//! is a stale parent, and the component is released without touching the nodes
//! (AGENTS §15.1). Releasing the picker's [`InstrumentButtonHandle`] is what
//! takes its click handler and signal scope down — a header rebuilt on every
//! language switch would otherwise leak one component scope per rebuild (W2a
//! review F2).
//!
//! # Confirmation is a signal, not a re-entry
//!
//! The selector's confirm handler and the two pickers' change handlers run with
//! an application context and no widget context, so they cannot call into the
//! pane: they write the request into a signal and ask for a frame, and the next
//! `sync` applies it. That is the same split the news and market-trade panes use
//! for their header controls (AGENTS §4.2: a control asks for the next `sync`
//! instead of re-entering the widget).
//!
//! # The directory
//!
//! The pane never loads the directory and never polls the catalogue: it re-reads
//! the answers its [`MarketDirectory`] gives on every load, and re-runs that load
//! when the injected [`catalog_generation`](widget::services::WidgetServices::catalog_generation)
//! signal moves (AGENTS §8.3). A listing the directory provably dropped — it left
//! the catalogue, or its route left the loaded venues — is reset to "nothing
//! chosen"; a lookup that failed proves nothing and leaves the stored value
//! alone.

use std::{
    cell::{Cell, RefCell},
    convert::Infallible,
    mem,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    kit::{
        empty_state::EmptyState,
        icon::{IconKind, IconSpec},
        list::{List, ListOptions},
        motion::{DEPTH_ANIMATION_SETTLE_EPSILON, depth_animation_progress_after},
        select::{Select, SelectItem, SelectOptions},
        separator::Separator,
        size::UnifiedSize,
    },
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use instrument_selector_ui::{
    host::SelectorHost,
    instrument_button::{InstrumentButton, InstrumentButtonHandle},
    instrument_selector::{
        InstrumentFilters, InstrumentKindFilter, InstrumentStatusFilter, LoadedScope, offers_route,
    },
};
use market_participants_connector::catalog::display::catalog::DisplaySnapshot;
use market_participants_model::{
    asset::AssetId,
    decimal::{Price, Tick},
    listing::route::VenueListingKey,
    market::{book::MarketSequence, selection::InstrumentSelection, units::QuantityUnit},
    routing::CapabilityId,
};
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::A11yPatch,
    app::{App, TimerId},
    error::EngineError,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::style::StylePatch,
    text::{TextAlign, TextStyle},
    window::WindowId,
};
use widget::{
    binding::{Inbound, Pending, WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    error::{TranslatedError, market_is_catalog_not_loaded},
    identity::{WidgetId, WidgetInstanceId},
    protocol::RequestId,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

use super::{
    directory::{
        ConnectorDirectory, ListingSupport, MarketDirectory, listing_support, market_venues,
    },
    model::{
        GroupedBook, GroupedLevel, OrderBookMetadata, OrderBookSnapshot, PRICE_MULTIPLIERS,
        WINDOW_SIZES, default_window_size, interpolated_ratio, is_after, price_step_label,
        price_step_labels, window_sizes,
    },
    paint::{
        ASK_BARS_SELECTOR, BID_BARS_SELECTOR, Imbalance, SideCanvas, drawable_rows, row_stride,
    },
    service::{
        OrderBookProtocol, OrderBookRequest, OrderBookResponse, OrderBookService, OrderBookStatus,
        OrderBookStreamError, OrderBookStreamKey, OrderBookStreamMessage,
    },
};

/// The depth the service asks a venue for. A venue that publishes shallower than
/// this answers with what it has.
const SNAPSHOT_DEPTH: u16 = 5_000;

/// What one depth-animation tick advances the bars by.
///
/// The chain is an E10 one-shot timer that re-arms itself, and each callback moves
/// the easing by exactly this step: the movement is **not** measured from elapsed
/// engine time (the engine exposes no clock read surface), so a dropped frame
/// lengthens the animation by the ticks it lost instead of being skipped over.
/// The settle predicate reads the same step, which is what stops the chain.
const DEPTH_ANIMATION_TICK: Duration = Duration::from_millis(16);

/// The widget-local names of the header controls' kit keys.
///
/// A kit key is **application-level** — the button derives its click action from
/// it (`"<key>.click"`) and a select its trigger action (`"<key>.trigger"`) — so
/// the pane does not spell the keys out: [`WidgetCx::unique_key`] stamps each
/// suffix with the pane's instance scope. The same suffix in the same instance
/// always mints the same key, which is what lets a control rebuilt on a later
/// frame reclaim the identity it registered.
const INSTRUMENT_KEY_SUFFIX: &str = "order_book.header.instrument";
const PRICE_STEP_KEY_SUFFIX: &str = "order_book.header.price_step";
const WINDOW_SIZE_KEY_SUFFIX: &str = "order_book.header.window_size";
const ASK_LIST_KEY_SUFFIX: &str = "order_book.ask";
const BID_LIST_KEY_SUFFIX: &str = "order_book.bid";

/// Which venue and instrument a book follows, and how finely it groups them.
///
/// The scope is optional because a book can exist before the user has chosen
/// anything: it then asks for an instrument instead of drawing a default one
/// nobody asked for. The two drawing choices are stored with the scope because
/// they are what the user picked, not what the venue said: a book of the same
/// instrument grouped to a finer step is a different chart, and reopening the
/// pane should show the one that was there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderBookConfig {
    selection: Option<InstrumentSelection>,
    price_multiplier: u32,
    window_size: usize,
}

impl Default for OrderBookConfig {
    fn default() -> Self {
        Self {
            selection: None,
            price_multiplier: PRICE_MULTIPLIERS[0],
            window_size: WINDOW_SIZES[0],
        }
    }
}

impl WidgetConfig for OrderBookConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

impl OrderBookConfig {
    /// Settles the two drawing choices against the ones this book offers.
    ///
    /// A persisted config can name a grouping or a row limit this build no longer
    /// offers — or a row limit the route's book is too shallow to fill. Such a
    /// choice goes back to the default one: a neighbouring choice is something the
    /// user did not pick, and drawing the book with it would look like a decision
    /// they made. The row limit is judged only when the route's depth is known: a
    /// book that follows nothing has no route to judge it against, and its stored
    /// choice is left alone until one is chosen. Returns whether anything was
    /// replaced, which is what the pane reports so the replacement is written back.
    fn settle(&mut self, depth: Option<u16>) -> bool {
        let defaults = Self::default();
        let mut settled = false;
        if !PRICE_MULTIPLIERS.contains(&self.price_multiplier) {
            self.price_multiplier = defaults.price_multiplier;
            settled = true;
        }
        if let Some(depth) = depth
            && !window_sizes(Some(depth)).contains(&self.window_size)
        {
            self.window_size = default_window_size(Some(depth));
            settled = true;
        }

        settled
    }
}

/// A failure the book can report that is not already a stream error.
#[derive(Clone, Debug, PartialEq, Eq)]
enum OrderBookFailure {
    Stream(OrderBookStreamError),
    Rpc(widget::rpc::RpcCallError<Infallible>),
}

impl TranslatedError for OrderBookFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Stream(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
}

/// One command this pane sent and has not seen the answer to.
///
/// A book reads through one binding and issues one kind of command, so the tag
/// names the read: it is the identity the binding returned that decides whose
/// answer arrived. A reply whose identity is not tracked answers a call the pane
/// already replaced (a new selection, a retry, or signing out of a route) and is
/// dropped and counted rather than failing the scope that replaced it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    /// The read: `Load` or `Retry`.
    Start,
}

/// The direction the mid price moved in since the frame before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MidDirection {
    Up,
    Down,
    Flat,
}

/// One choice made in the header's right group, waiting for the next `sync`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderRequest {
    /// The price-grouping multiplier the user picked.
    PriceStep(u32),
    /// The row limit the user picked.
    WindowSize(usize),
}

/// The labels the ladder's columns are written with.
///
/// They come from the listing's own rules — the asset its prices are quoted in and
/// what its quantities count — never from a field name, and the name of an asset is
/// the display code the published generation decided on.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OrderBookUnitLabels {
    price: SmolStr,
    quantity: SmolStr,
}

impl OrderBookUnitLabels {
    fn of(metadata: &OrderBookMetadata, display: &DisplaySnapshot, translate: &Translate) -> Self {
        Self {
            price: asset_display(display, &metadata.quote_asset, translate),
            quantity: quantity_label(&metadata.quantity_unit, display, translate),
        }
    }
}

/// The display code of one asset, as the captured generation published it.
///
/// Only the display catalog knows what an asset is called: the canonical identity
/// of a token is a digest, and the code it publishes is the whole name of the
/// asset. An asset the captured generation does not name is the localized unknown
/// rather than an identity digest.
fn asset_display(display: &DisplaySnapshot, asset: &AssetId, translate: &Translate) -> SmolStr {
    display.resolver().asset_code(asset).map_or_else(
        |_| t_smol!(translate, "error.display.unknown_asset"),
        |code| SmolStr::new(code.as_str()),
    )
}

/// The label of what one quantity counts.
fn quantity_label(
    unit: &QuantityUnit,
    display: &DisplaySnapshot,
    translate: &Translate,
) -> SmolStr {
    match unit {
        QuantityUnit::AssetUnits { asset } | QuantityUnit::Notional { asset } => {
            asset_display(display, asset, translate)
        }
        QuantityUnit::Contracts => t_smol!(translate, "widget.order_book.contracts_unit"),
        QuantityUnit::OutcomeTokens { .. } => {
            t_smol!(translate, "widget.order_book.outcome_tokens_unit")
        }
    }
}

/// Which side of the book one list draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowSide {
    Ask,
    Bid,
}

impl RowSide {
    /// The themed colour of this side's prices.
    fn price_role(self) -> Role {
        match self {
            Self::Ask => Role::MarketDown,
            Self::Bid => Role::MarketUp,
        }
    }

    /// The themed colour of this side's depth bars.
    fn depth_role(self) -> Role {
        match self {
            Self::Ask => Role::MarketDownWeak,
            Self::Bid => Role::MarketUpWeak,
        }
    }
}

/// The arrow and the direction the mid price moved in.
fn mid_direction_of(current: Option<Price>, previous: Option<Price>) -> MidDirection {
    match (current, previous) {
        (Some(current), Some(previous)) if current > previous => MidDirection::Up,
        (Some(current), Some(previous)) if current < previous => MidDirection::Down,
        _ => MidDirection::Flat,
    }
}

/// Whether a movement still has enough left to be worth a frame.
fn depth_is_active(progress: f32, span: f32) -> bool {
    span * (1.0 - progress) > DEPTH_ANIMATION_SETTLE_EPSILON
}

/// The state the depth-timer chain reads and writes.
///
/// It is shared with the timer callbacks (which own no widget context), so every
/// value it touches is a signal or a cell the callbacks may reach. `alive` is
/// what keeps a released pane's chain from reading signals whose scope has gone.
struct DepthTickerState {
    progress: Signal<f32>,
    span: Signal<f32>,
    ticking: Cell<bool>,
    alive: Cell<bool>,
    window: WindowId,
    scope: NodeId,
    timer: RefCell<Option<TimerId>>,
}

impl DepthTickerState {
    /// Schedules the next tick, remembering the timer so `release` can cancel it.
    fn schedule(self: &Rc<Self>, app: &mut App) {
        let state = Rc::clone(self);
        match app.after_scoped(self.window, self.scope, DEPTH_ANIMATION_TICK, move |app| {
            tick_depth(app, state);
        }) {
            Ok(timer) => *self.timer.borrow_mut() = Some(timer),
            Err(error) => {
                log::warn!("the order book's depth animation could not schedule a tick: {error}");
                self.ticking.set(false);
            }
        }
    }
}

/// One tick of the depth animation: advance the progress signal and ask for the
/// frame that draws the bars at it.
fn tick_depth(app: &mut App, state: Rc<DepthTickerState>) {
    if !state.alive.get() {
        return;
    }
    let progress = *app.read(&state.progress);
    let span = *app.read(&state.span);
    let next = depth_animation_progress_after(progress, span, DEPTH_ANIMATION_TICK);
    app.write(&state.progress, next);
    let _ = app.request_repaint(state.window);
    if !depth_is_active(next, span) {
        state.ticking.set(false);
        return;
    }
    state.schedule(app);
}

/// A grouped ladder as the body's bindings read it.
///
/// The signal carries the shared allocation rather than the drawing itself, so two
/// publishes of the same grouped book compare equal by pointer and a steady `sync`
/// writes nothing: the widget only ever replaces a grouped book wholesale (the
/// model reuses labels inside a rebuild), and an unchanged book is never replaced.
#[derive(Clone, Default)]
struct PublishedBook {
    book: Option<Arc<GroupedBook>>,
}

impl PartialEq for PublishedBook {
    fn eq(&self, other: &Self) -> bool {
        match (&self.book, &other.book) {
            (None, None) => true,
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }
}

impl PublishedBook {
    /// The newest trade's mid price, when the book has one.
    fn mid_price_text(&self) -> Option<&SmolStr> {
        self.book.as_ref()?.mid_price_text.as_ref()
    }

    /// The spread between the best bid and the best ask, when the book has both.
    fn spread_text(&self) -> Option<&SmolStr> {
        self.book.as_ref()?.spread_text.as_ref()
    }

    /// One side's share of the visible quantity.
    fn imbalance_text(&self, side: RowSide) -> Option<&SmolStr> {
        let book = self.book.as_ref()?;
        Some(match side {
            RowSide::Ask => &book.ask_imbalance_text,
            RowSide::Bid => &book.bid_imbalance_text,
        })
    }

    /// One visible level of `side`, best first.
    fn level(&self, side: RowSide, row: usize) -> Option<&GroupedLevel> {
        let book = self.book.as_ref()?;
        match side {
            RowSide::Ask => book.asks.get(row),
            RowSide::Bid => book.bids.get(row),
        }
    }

    /// Whether the book has no level on either side.
    fn is_empty(&self) -> bool {
        self.book
            .as_ref()
            .is_some_and(|book| book.asks.is_empty() && book.bids.is_empty())
    }

    /// Which way the mid price moved against `previous`.
    fn direction_against(&self, previous: Option<Price>) -> MidDirection {
        mid_direction_of(self.book.as_ref().and_then(|book| book.mid_price), previous)
    }
}

/// The header's left group: the instrument picker and the copy it was built with.
struct HeaderLeft {
    control: InstrumentButtonHandle,
    parent: NodeId,
    built: HeaderState,
}

/// What the header's left control states, as one comparable value.
///
/// The generation is part of it because the filters snapshot the control opens its
/// selector with is read from the loaded venues when the control is built: a venue
/// loaded later is one the user must be able to pick on, and a control built
/// before that commit would keep opening a picker that does not offer it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HeaderState {
    selection: Option<InstrumentSelection>,
    language: LanguageCode,
    generation: u64,
}

/// The header's right group: the two drawing choices and the copy they were built
/// with.
struct HeaderRight {
    /// The pane's group container the controls are rebuilt into.
    slot: NodeId,
    /// The widget's own row inside that container, which a rebuild replaces.
    root: NodeId,
    price_step: Select,
    window_size: Select,
    built: HeaderRightState,
}

/// What the two drawing pickers state, as one comparable value.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HeaderRightState {
    language: LanguageCode,
    depth: Option<u16>,
    price_tick: Option<Tick>,
    price_multiplier: u32,
    window_size: usize,
    disabled: bool,
}

pub struct OrderBookWidget {
    config: OrderBookConfig,
    /// The executor bridge, when this host has a directory to drive a service from.
    ///
    /// A headless host without a connector has nothing to start: the pane waits
    /// for the directory instead of spawning a service it could never ask a
    /// question.
    binding: Option<WidgetBinding<OrderBookProtocol>>,
    /// The command in flight, keyed by the identity the binding returned.
    pending: Pending<PendingCommand>,
    status: WidgetStatus,
    /// The recovery the framework offers for every failure this widget reports.
    recovery: WidgetAction,
    config_change: WidgetConfigChange,
    /// The loaded directory, when the host has one.
    directory: Option<Rc<dyn MarketDirectory>>,
    /// The body root and the two ladder areas, once the body has been built.
    body_root: Option<NodeId>,
    ask_area: Option<NodeId>,
    bid_area: Option<NodeId>,
    /// The two row lists and the canvases behind them.
    ask_list: Option<List>,
    bid_list: Option<List>,
    ask_bars: SideCanvas,
    bid_bars: SideCanvas,
    imbalance: Imbalance,
    /// The grouped ladder the body draws, published to every `bind_text` that
    /// reads a row, the mid price or the imbalance.
    book: Signal<PublishedBook>,
    /// The labels of the price and quantity columns, published on metadata change.
    unit_labels: Signal<OrderBookUnitLabels>,
    /// Which way the mid price moved since the frame before.
    mid_direction: Signal<MidDirection>,
    /// The depth easing: how far the bars have come and how far they have to go.
    depth_progress: Signal<f32>,
    depth_span: Signal<f32>,
    /// The empty-state sentence, in the language the body was last synchronized in.
    empty_text: Signal<SmolStr>,
    /// Selections confirmed in the header control, waiting for the next `sync`.
    selection_requests: Signal<Option<InstrumentSelection>>,
    /// Drawing choices made in the header pickers, waiting for the next `sync`.
    header_requests: Signal<Option<HeaderRequest>>,
    language: Signal<LanguageCode>,
    /// The directory generation the composition root publishes.
    generation: Signal<u64>,
    translate: Translate,
    theme: ThemeHandle,
    /// The book the stream last published, which is what regrouping reads from.
    snapshot: Option<OrderBookSnapshot>,
    /// The grouped ladder a frame draws.
    grouped: Option<Arc<GroupedBook>>,
    /// The sequence a book has to reach before the widget is done working.
    ready: Option<MarketSequence>,
    metadata: Option<OrderBookMetadata>,
    /// The labels of the price-grouping choices, formatted once per price tick.
    price_step_labels: [SmolStr; PRICE_MULTIPLIERS.len()],
    /// How many rows one side has room for, as the last layout of the ladder said.
    drawable_rows: usize,
    previous_mid_price: Option<Price>,
    /// Whether the snapshot metadata moved since it was last published.
    book_dirty: bool,
    /// Whether the ladder is waiting for its market rather than reading it.
    ///
    /// The pane keeps its stream while it waits — the alternative is cancelling the
    /// binding, which is terminal, and this pane has to be able to read the market
    /// again once the user loads it — so a book that was in flight when the route
    /// went away can still arrive. Accepting it would put a snapshot back and make
    /// the pane report content over the notice the framework is drawing, so every
    /// message entry refuses while this is set. The stream key cannot do this job:
    /// it says which scope a book belongs to, and this one belongs to the scope the
    /// notice is about.
    awaiting_market: bool,
    /// Whether the venue this book reads was loaded or refreshed again.
    ///
    /// Such a commit replaces the catalog generation the live stream resolved its
    /// rules, units and price steps against, while the stream itself keeps what it
    /// resolved when it was opened. So the book has to start over, and the request
    /// it sends has to be the one that opens a new stream.
    market_reloaded: bool,
    /// The generation this pane has already reacted to.
    seen_generation: u64,
    /// The language the body's own static copy was last written in.
    built_language: Option<LanguageCode>,
    /// The depth-animation chain, while one is running.
    depth_ticker: Option<Rc<DepthTickerState>>,
    header_left: Option<HeaderLeft>,
    header_right: Option<HeaderRight>,
}

impl WidgetView for OrderBookWidget {
    type Config = OrderBookConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.order_book");
    const DISPLAY_NAME: &'static str = "widget.names.order_book";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.order_book");
    const ICON: Option<&'static str> = Some("widgets/order_book.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: WidgetStatus,
        _reload: ReloadHandle,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError> {
        // A host without a directory has nothing to ask and nothing to start; the
        // pane reports the neutral notice and waits. Every application supplies one.
        let Some(connector) = cx.services().market_participants.clone() else {
            return Ok(Self::assemble(
                config,
                None,
                None,
                status,
                config_change,
                recovery,
                cx,
            ));
        };
        let service = OrderBookService::new(connector.market(), SNAPSHOT_DEPTH);
        let binding = WidgetBinding::spawn(cx, service);
        let directory: Rc<dyn MarketDirectory> = Rc::new(ConnectorDirectory::new(connector));
        Ok(Self::assemble(
            config,
            Some(binding),
            Some(directory),
            status,
            config_change,
            recovery,
            cx,
        ))
    }

    /// The options this widget persists.
    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    /// A book is content as soon as one has been accepted — including an empty
    /// one. An empty book is what "this instrument has published nothing" looks
    /// like: the ladder is still there, and there is nothing to report about it.
    fn has_content(&self) -> bool {
        self.snapshot.is_some()
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        // The market this book follows has to be loaded before anything else
        // happens here, and the check is first on purpose: a route the user never
        // loaded, or one a shrinking commit has since taken away, leaves the pane
        // waiting for it instead of reaching the branches below, which settle the
        // selection the user stored.
        let Some(directory) = self.directory.clone() else {
            self.wait_for_market(cx);
            return;
        };
        if !self.market_ready(directory.as_ref()) {
            self.wait_for_market(cx);
            return;
        }
        // The pane reached here only past its own gate, so the markets it was
        // waiting for are in memory again and messages belong to them once more.
        self.awaiting_market = false;
        // Read past the gate: a commit on the book's own venue that took its
        // market away leaves this set while the stream that resolved against the
        // retired generation is still alive, and the load that brings the market
        // back has to reopen that stream.
        let market_reloaded = mem::take(&mut self.market_reloaded);
        let retry = self.status.is_failed(cx.app());
        if !retry && !market_reloaded {
            // An explicit load starts over, because the widget cannot vouch for
            // what it was showing. A retry keeps the book the user is reading, and
            // so does a directory-generation reload: that one re-resolves the rules
            // and units behind a book the user is still looking at (AGENTS §8.3),
            // so it only replaces the subscription and keeps the ladder up until
            // the new generation publishes the book it holds.
            self.clear_book(cx);
        }
        self.start(retry, market_reloaded, cx);
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let theme = self.theme.clone();
        let tokens = theme.tokens();
        let font_size = tokens.small.font_size;
        let label_height = tokens.small.height;
        let mid_height = tokens.medium.height + tokens.large.font_size;
        let imbalance_height = tokens.small.height;
        let stride = row_stride(font_size);

        let translate = self.translate.clone();
        let language = self.language;
        let root = NodeBuilder::boxed()
            .id("TEST_ORDER_BOOK_LADDER")
            .style(
                StyleBuilder::new()
                    .relative()
                    .flex_1()
                    .flex_col()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .bind_a11y(move |read| {
                let _language = *read.read(&language);
                A11yPatch {
                    label: Some(t_smol!(translate, "widget.order_book.book")),
                    ..A11yPatch::default()
                }
            })
            .build(cx.tree_mut(), parent)?;
        self.body_root = Some(root);

        self.build_column_labels(cx, root, font_size, label_height)?;
        Separator::horizontal(theme.clone()).build(cx.tree_mut(), root)?;
        let ask_area = self.build_ladder_area(cx, root, RowSide::Ask, font_size, stride)?;
        self.ask_area = Some(ask_area);
        Separator::horizontal(theme.clone()).build(cx.tree_mut(), root)?;
        self.build_mid_price(cx, root, mid_height)?;
        Separator::horizontal(theme.clone()).build(cx.tree_mut(), root)?;
        let bid_area = self.build_ladder_area(cx, root, RowSide::Bid, font_size, stride)?;
        self.bid_area = Some(bid_area);
        self.build_imbalance(cx, root, imbalance_height, font_size)?;
        self.build_empty_state(cx, root)?;

        self.publish_book(cx)?;
        self.publish_unit_labels(cx);
        self.publish_empty_text(cx);
        // A movement started before the body existed picks its timer chain back up
        // here, now that there is a node to scope it to.
        self.ensure_depth_ticker(cx);
        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        // The directory generation is the one signal the framework's surface root
        // also reads, so a write schedules the frame that runs this hook — including
        // for a pane that is only waiting and has no body. Nothing else may poll the
        // directory (AGENTS §8.3).
        let generation = *cx.read(&self.generation);
        if generation != self.seen_generation {
            self.seen_generation = generation;
            if !self.status.is_failed(cx.app()) {
                // A new generation may have replaced the one the live stream
                // resolved its listing against, so the load that follows asks for a
                // new stream.
                self.market_reloaded = true;
                self.load(cx);
            }
        }

        // The binding delivers messages instead of calling back into the widget;
        // this is the one place they are applied, where the widget has a context
        // again.
        if let Some(binding) = self.binding.as_ref() {
            for inbound in binding.take() {
                self.apply(inbound, cx);
            }
        }

        // A confirmation is applied here rather than in the control's handler,
        // which owns no widget context (see the module documentation).
        let requested = (*cx.read(&self.selection_requests)).clone();
        if let Some(selection) = requested {
            cx.write(&self.selection_requests, None);
            self.select_selection(selection, cx);
        }
        let requested = *cx.read(&self.header_requests);
        if let Some(request) = requested {
            cx.write(&self.header_requests, None);
            match request {
                HeaderRequest::PriceStep(multiplier) => {
                    self.select_price_multiplier(multiplier, cx)
                }
                HeaderRequest::WindowSize(window_size) => {
                    self.select_window_size(window_size, cx);
                }
            }
        }

        // The rows one side has room for follows the laid-out height of the ladder:
        // a pane squeezed under the row limit groups fewer levels, and the rows it
        // does group are the ones it can draw.
        let ladder_bounds = self.ask_area.and_then(|node| {
            cx.tree()
                .get(node)
                .ok()
                .map(|target| target.layout().bounds)
        });
        if let Some(bounds) = ladder_bounds
            && bounds.size.height > 0.0
        {
            let drawable = drawable_rows(bounds, self.theme.tokens().small.font_size);
            if drawable != self.drawable_rows {
                self.drawable_rows = drawable;
                self.regroup(cx);
            }
        }

        if self.book_dirty {
            self.book_dirty = false;
            self.publish_book(cx)?;
        }

        self.sync_bars(cx)?;
        self.publish_empty_text(cx);
        self.sync_lists(cx)?;
        self.sync_header(cx)?;

        Ok(())
    }

    fn header(
        &mut self,
        group: HeaderGroup,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        match group {
            HeaderGroup::Left => {
                // The pane is building its row; the control this widget built into
                // the previous one went with it. Only the component is the widget's
                // to take down.
                self.release_header_left(cx, true);
                self.build_header_left(parent, cx)
            }
            HeaderGroup::Right => {
                self.release_header_right(cx, true);
                self.build_header_right(parent, cx)
            }
        }
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        // The component scopes are this widget's in every case, which is why the
        // handles the controls hand back are released here, while their nodes are
        // still alive (the framework removes the surface subtree after this hook).
        self.release_header_left(cx, false);
        self.release_header_right(cx, false);
        if let Some(list) = self.ask_list.take() {
            list.release(cx.app_mut());
        }
        if let Some(list) = self.bid_list.take() {
            list.release(cx.app_mut());
        }
        self.stop_depth_animation(cx.app_mut());
        // Dropping the bridge aborts its forwarding tasks and cancels the service.
        if let Some(mut binding) = self.binding.take() {
            binding.cancel();
        }
        self.body_root = None;
        self.ask_area = None;
        self.bid_area = None;
    }
}

impl OrderBookWidget {
    /// Builds a widget over the collaborators a caller hands it.
    fn assemble(
        config: OrderBookConfig,
        binding: Option<WidgetBinding<OrderBookProtocol>>,
        directory: Option<Rc<dyn MarketDirectory>>,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let book = cx.signal(PublishedBook::default());
        let unit_labels = cx.signal(OrderBookUnitLabels {
            price: SmolStr::new_static("--"),
            quantity: SmolStr::new_static("--"),
        });
        let mid_direction = cx.signal(MidDirection::Flat);
        let depth_progress = cx.signal(1.0_f32);
        let depth_span = cx.signal(0.0_f32);
        let empty_text = cx.signal(SmolStr::new_static(""));
        let selection_requests = cx.signal(None::<InstrumentSelection>);
        let header_requests = cx.signal(None::<HeaderRequest>);
        let language = cx.services().language;
        let generation = cx.services().catalog_generation;
        let seen_generation = *cx.read(&generation);
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();

        Self {
            config,
            binding,
            pending: Pending::new(),
            status,
            recovery,
            config_change,
            directory,
            body_root: None,
            ask_area: None,
            bid_area: None,
            ask_list: None,
            bid_list: None,
            ask_bars: SideCanvas::new(),
            bid_bars: SideCanvas::new(),
            imbalance: Imbalance::new(),
            book,
            unit_labels,
            mid_direction,
            depth_progress,
            depth_span,
            empty_text,
            selection_requests,
            header_requests,
            language,
            generation,
            translate,
            theme,
            snapshot: None,
            grouped: None,
            ready: None,
            metadata: None,
            price_step_labels: price_step_labels(None),
            drawable_rows: WINDOW_SIZES[WINDOW_SIZES.len() - 1],
            previous_mid_price: None,
            book_dirty: false,
            awaiting_market: false,
            market_reloaded: false,
            seen_generation,
            built_language: None,
            depth_ticker: None,
            header_left: None,
            header_right: None,
        }
    }

    /// Builds a widget over a test's own source and directory.
    #[cfg(test)]
    fn with_source<S: super::service::OrderBookSource>(
        config: OrderBookConfig,
        source: S,
        directory: Rc<dyn MarketDirectory>,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let service = OrderBookService::new(source, SNAPSHOT_DEPTH);
        let binding = WidgetBinding::spawn(cx, service);
        Self::assemble(
            config,
            Some(binding),
            Some(directory),
            status,
            config_change,
            recovery,
            cx,
        )
    }

    // ---------------------------------------------------------------- body

    /// Builds the three column labels over the ladders.
    fn build_column_labels(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        font_size: f32,
        height: f32,
    ) -> Result<(), EngineError> {
        let row = NodeBuilder::boxed()
            .id("TEST_ORDER_BOOK_COLUMNS")
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .flex_none()
                    .height(height)
                    .padding_x(8.0)
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        let translate = self.translate.clone();
        let language = self.language;
        let labels = self.unit_labels;
        let theme = self.theme.clone();
        for (key, id) in [
            (
                "widget.order_book.price_with_unit",
                "TEST_ORDER_BOOK_COLUMN_PRICE",
            ),
            (
                "widget.order_book.amount_with_unit",
                "TEST_ORDER_BOOK_COLUMN_AMOUNT",
            ),
            (
                "widget.order_book.total_with_unit",
                "TEST_ORDER_BOOK_COLUMN_TOTAL",
            ),
        ] {
            let translate = translate.clone();
            let theme = theme.clone();
            NodeBuilder::text(SmolStr::default(), aligned_text(font_size))
                .id(id)
                .style(
                    StyleBuilder::new()
                        .flex_1()
                        .min_width_0()
                        .height_full()
                        .build(),
                )
                .bind_text(move |read| {
                    let _language = *read.read(&language);
                    let labels = read.read(&labels);
                    let unit = match key {
                        "widget.order_book.price_with_unit" => labels.price.clone(),
                        "widget.order_book.amount_with_unit" => labels.quantity.clone(),
                        _ => labels.quantity.clone(),
                    };
                    t_smol!(translate, key, unit = unit)
                })
                .bind_style(move |read| StylePatch {
                    foreground: Some(theme.role(read, Role::FgMuted)),
                    ..StylePatch::default()
                })
                .build(cx.tree_mut(), row)?;
        }
        Ok(())
    }

    /// Builds one side of the ladder: the depth-bar canvas and the row list over it.
    fn build_ladder_area(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        side: RowSide,
        font_size: f32,
        stride: f32,
    ) -> Result<NodeId, EngineError> {
        let area = NodeBuilder::boxed()
            .id(match side {
                RowSide::Ask => "TEST_ORDER_BOOK_ASKS",
                RowSide::Bid => "TEST_ORDER_BOOK_BIDS",
            })
            .style(
                StyleBuilder::new()
                    .relative()
                    .flex_1()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;

        let theme = self.theme.clone();
        let selector = match side {
            RowSide::Ask => ASK_BARS_SELECTOR,
            RowSide::Bid => BID_BARS_SELECTOR,
        };
        match side {
            RowSide::Ask => self.ask_bars.build(cx, area, selector, theme.clone())?,
            RowSide::Bid => self.bid_bars.build(cx, area, selector, theme.clone())?,
        }

        let book = self.book;
        let row_theme = self.theme.clone();
        let key = cx.unique_key(match side {
            RowSide::Ask => ASK_LIST_KEY_SUFFIX,
            RowSide::Bid => BID_LIST_KEY_SUFFIX,
        });
        let list = List::new(
            cx.app_mut(),
            theme,
            key,
            ListOptions::fixed(stride),
            move |tree, parent, row| {
                build_row(tree, parent, row, side, book, row_theme.clone(), font_size)
            },
        );
        let count = match side {
            RowSide::Ask => self.grouped.as_ref().map_or(0, |book| book.asks.len()),
            RowSide::Bid => self.grouped.as_ref().map_or(0, |book| book.bids.len()),
        };
        list.set_initial_count(cx.app_mut(), count);
        let window = cx.window();
        list.mount(cx.app_mut(), window, area)?;
        match side {
            RowSide::Ask => self.ask_list = Some(list),
            RowSide::Bid => self.bid_list = Some(list),
        }
        Ok(area)
    }

    /// Builds the mid-price row: the direction arrow, the price and the spread.
    fn build_mid_price(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        height: f32,
    ) -> Result<(), EngineError> {
        let tokens = self.theme.tokens();
        let font_size = tokens.large.font_size;
        let small_size = tokens.small.font_size;
        let row = NodeBuilder::boxed()
            .id("TEST_ORDER_BOOK_MID")
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .flex_none()
                    .height(height)
                    .gap(8.0)
                    .padding_x(8.0)
                    .min_width_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;

        let direction = self.mid_direction;
        let window = cx.window();
        for (kind, role, shown) in [
            (IconKind::ArrowUp, Role::MarketUp, MidDirection::Up),
            (IconKind::ArrowDown, Role::MarketDown, MidDirection::Down),
        ] {
            let slot = NodeBuilder::boxed()
                .style(StyleBuilder::new().flex_none().build())
                .bind_style(move |read| StylePatch {
                    visible: Some(*read.read(&direction) == shown),
                    ..StylePatch::default()
                })
                .build(cx.tree_mut(), row)?;
            let icon = IconSpec {
                kind,
                size: tokens.small.icon_size,
                ..IconSpec::new(kind)
            };
            if let Err(error) = icon.build_themed(cx.app_mut(), window, slot, &self.theme, role) {
                log::warn!("the order book's mid-price arrow could not be built: {error}");
            }
        }

        let book = self.book;
        NodeBuilder::text(SmolStr::default(), bold_text(font_size))
            .id("TEST_ORDER_BOOK_MID_PRICE")
            .style(StyleBuilder::new().flex_none().build())
            .bind_text(move |read| {
                let book = read.read(&book);
                book.mid_price_text()
                    .cloned()
                    .unwrap_or_else(|| SmolStr::new_static("--"))
            })
            .bind_style({
                let theme = self.theme.clone();
                let direction = self.mid_direction;
                move |read| {
                    let role = match *read.read(&direction) {
                        MidDirection::Up => Role::MarketUp,
                        MidDirection::Down => Role::MarketDown,
                        MidDirection::Flat => Role::MarketFlat,
                    };
                    StylePatch {
                        foreground: Some(theme.role(read, role)),
                        ..StylePatch::default()
                    }
                }
            })
            .build(cx.tree_mut(), row)?;

        let translate = self.translate.clone();
        let language = self.language;
        NodeBuilder::text(SmolStr::default(), TextStyle::new(small_size))
            .id("TEST_ORDER_BOOK_SPREAD")
            .style(StyleBuilder::new().flex_none().build())
            .bind_text(move |read| {
                let _language = *read.read(&language);
                let book = read.read(&book);
                book.spread_text().map_or_else(
                    || SmolStr::new_static("--"),
                    |spread| t_smol!(translate, "widget.order_book.spread", value = spread),
                )
            })
            .bind_style({
                let theme = self.theme.clone();
                move |read| StylePatch {
                    foreground: Some(theme.role(read, Role::FgMuted)),
                    ..StylePatch::default()
                }
            })
            .build(cx.tree_mut(), row)?;
        Ok(())
    }

    /// Builds the imbalance bar: the split canvas with the two shares over it.
    fn build_imbalance(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        height: f32,
        font_size: f32,
    ) -> Result<(), EngineError> {
        let row = NodeBuilder::boxed()
            .id("TEST_ORDER_BOOK_IMBALANCE_ROW")
            .style(
                StyleBuilder::new()
                    .relative()
                    .flex_none()
                    .height(height)
                    .min_width_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        let theme = self.theme.clone();
        self.imbalance.build(cx, row, theme.clone())?;
        let overlay = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .relative()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .size_full()
                    .padding_x(8.0)
                    .build(),
            )
            .build(cx.tree_mut(), row)?;

        let book = self.book;
        for (side, role, id) in [
            (
                RowSide::Ask,
                Role::MarketDown,
                "TEST_ORDER_BOOK_ASK_IMBALANCE",
            ),
            (
                RowSide::Bid,
                Role::MarketUp,
                "TEST_ORDER_BOOK_BID_IMBALANCE",
            ),
        ] {
            NodeBuilder::text(SmolStr::default(), bold_text(font_size))
                .id(id)
                .style(StyleBuilder::new().flex_none().build())
                .bind_text(move |read| {
                    let book = read.read(&book);
                    book.imbalance_text(side)
                        .cloned()
                        .unwrap_or_else(|| SmolStr::new_static("--"))
                })
                .bind_style({
                    let theme = theme.clone();
                    move |read| StylePatch {
                        foreground: Some(theme.role(read, role)),
                        ..StylePatch::default()
                    }
                })
                .build(cx.tree_mut(), overlay)?;
        }
        Ok(())
    }

    /// Builds the "nothing published" state over the empty ladder.
    ///
    /// A book with no level on either side draws an empty ladder between its labels,
    /// which leaves it and a book that failed to load looking the same. The book is
    /// still the pane's content — the venue published it — so the pane says what it
    /// holds rather than letting the framework's placeholder speak for it.
    fn build_empty_state(&self, cx: &mut WidgetCx<'_>, parent: NodeId) -> Result<(), EngineError> {
        let book = self.book;
        let overlay = NodeBuilder::boxed()
            .id("TEST_ORDER_BOOK_EMPTY_OVERLAY")
            .style(
                StyleBuilder::new()
                    .absolute()
                    .left(0.0)
                    .top(0.0)
                    .size_full()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .build(),
            )
            .bind_style(move |read| StylePatch {
                visible: Some(read.read(&book).is_empty()),
                ..StylePatch::default()
            })
            .build(cx.tree_mut(), parent)?;
        EmptyState::new(self.theme.clone(), "TEST_ORDER_BOOK_EMPTY")
            .bind_text(self.empty_text)
            .build(cx.tree_mut(), overlay)?;
        Ok(())
    }

    // ------------------------------------------------------------ publishing

    /// Publishes the grouped ladder and the two lists' row counts.
    fn publish_book(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let published = PublishedBook {
            book: self.grouped.clone(),
        };
        let (ask_count, bid_count) = self
            .grouped
            .as_ref()
            .map_or((0, 0), |book| (book.asks.len(), book.bids.len()));
        let direction = published.direction_against(self.previous_mid_price);
        let changed = cx.write(&self.book, published);
        cx.write(&self.mid_direction, direction);
        if let Some(list) = self.ask_list.as_ref() {
            list.set_count(cx.app_mut(), ask_count)?;
        }
        if let Some(list) = self.bid_list.as_ref() {
            list.set_count(cx.app_mut(), bid_count)?;
        }
        if changed {
            cx.request_frame()?;
        }
        Ok(())
    }

    /// Publishes the column labels when the metadata (and so the units) moved.
    fn publish_unit_labels(&mut self, cx: &mut WidgetCx<'_>) {
        let labels = self.unit_labels_now();
        cx.write(&self.unit_labels, labels);
    }

    /// Publishes the empty-state sentence in the language showing now.
    fn publish_empty_text(&mut self, cx: &mut WidgetCx<'_>) {
        let language = *cx.read(&self.language);
        if self.built_language == Some(language) {
            return;
        }
        self.built_language = Some(language);
        cx.write(&self.empty_text, t_smol!(self.translate, "common.no_data"));
    }

    /// The column labels the current metadata resolves.
    fn unit_labels_now(&self) -> OrderBookUnitLabels {
        let Some(metadata) = self.metadata.as_ref() else {
            return OrderBookUnitLabels {
                price: SmolStr::new_static("--"),
                quantity: SmolStr::new_static("--"),
            };
        };
        let Some(directory) = self.directory.as_ref() else {
            return OrderBookUnitLabels {
                price: SmolStr::new_static("--"),
                quantity: SmolStr::new_static("--"),
            };
        };
        OrderBookUnitLabels::of(metadata, &directory.display(), &self.translate)
    }

    /// Draws the two bar canvases and the imbalance bar at the progress showing now.
    fn sync_bars(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(book) = self.grouped.clone() else {
            return Ok(());
        };
        let progress = *cx.read(&self.depth_progress);
        let font_size = self.theme.tokens().small.font_size;
        let appearance = self.theme.current(cx.app());
        let resolved = self.theme.theme_for(appearance);
        let ask_color = resolved.role(RowSide::Ask.depth_role());
        let bid_color = resolved.role(RowSide::Bid.depth_role());
        let ask_levels = book.asks.clone();
        let ask_offset = self
            .ask_list
            .as_ref()
            .and_then(|list| list.offset(cx.app()))
            .unwrap_or(0.0);
        self.ask_bars.sync(
            cx,
            &ask_levels,
            progress,
            font_size,
            ask_offset,
            ask_color,
            true,
        )?;
        let bid_levels = book.bids.clone();
        let bid_offset = self
            .bid_list
            .as_ref()
            .and_then(|list| list.offset(cx.app()))
            .unwrap_or(0.0);
        self.bid_bars.sync(
            cx,
            &bid_levels,
            progress,
            font_size,
            bid_offset,
            bid_color,
            false,
        )?;
        let ask_ratio =
            interpolated_ratio(book.previous_imbalance, book.current_imbalance, progress);
        self.imbalance.sync(cx, ask_ratio, ask_color, bid_color)?;
        Ok(())
    }

    /// Lets both lists settle what the last layout did (row mounts, scrollbar).
    fn sync_lists(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if let Some(list) = self.ask_list.as_ref() {
            list.sync(cx.app_mut())?;
        }
        if let Some(list) = self.bid_list.as_ref() {
            list.sync(cx.app_mut())?;
        }
        Ok(())
    }

    // --------------------------------------------------------------- loading

    /// Whether what this book needs is in memory: at least one market that answers
    /// with depth, and the route of the stored selection among them.
    fn market_ready(&self, directory: &dyn MarketDirectory) -> bool {
        if !directory.any_market_loaded() {
            return false;
        }

        match self.config.selection.as_ref() {
            Some(selection) => directory.is_loaded(&selection.listing.route),
            None => true,
        }
    }

    /// Reports the neutral notice for the market this book follows, and drops the
    /// ladder it was drawing.
    fn wait_for_market(&mut self, cx: &mut WidgetCx<'_>) {
        self.awaiting_market = true;
        self.clear_book(cx);
        self.report_markets_not_loaded(cx);
    }

    /// Reports the notice naming the market this book follows.
    fn report_markets_not_loaded(&self, cx: &mut WidgetCx<'_>) {
        let supported = market_venues();
        let venues = self
            .config
            .selection
            .as_ref()
            .map(|selection| vec![selection.listing.route.venue])
            .unwrap_or_else(|| supported.clone());

        self.status.report(
            cx.app_mut(),
            WidgetState::waiting(Notice::MarketNotLoaded { venues, supported }),
        );
    }

    /// Reports the prompt asking for an instrument.
    fn report_nothing_chosen(&self, cx: &mut WidgetCx<'_>) {
        self.status.report(
            cx.app_mut(),
            WidgetState::waiting(Notice::message(
                "widget.order_book.select_instrument_prompt",
            )),
        );
    }

    /// Reports the neutral notice for a read the directory answered "not loaded" to.
    fn report_waiting_for_directory(
        &mut self,
        error: &OrderBookStreamError,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        let OrderBookStreamError::MarketParticipant(error) = error else {
            return false;
        };
        if !market_is_catalog_not_loaded(error) && !error.is_venue_not_loaded() {
            return false;
        }
        self.wait_for_market(cx);
        true
    }

    /// The stream scope this book would accept messages from.
    fn current_key(&self) -> Option<OrderBookStreamKey> {
        self.config.selection.as_ref().map(|selection| {
            OrderBookStreamKey::new(
                selection.listing.clone(),
                self.config.price_multiplier,
                self.config.window_size,
            )
        })
    }

    /// Starts the stream, or restarts it for a scope the user is retrying.
    fn start(&mut self, retry: bool, market_reloaded: bool, cx: &mut WidgetCx<'_>) {
        // The pane reached here only past its own gate, so the market it was
        // waiting for is in memory again and messages belong to it once more.
        self.awaiting_market = false;
        self.ready = None;
        let Some(directory) = self.directory.clone() else {
            self.nothing_chosen(cx);
            return;
        };
        let Some(selection) = self.config.selection.clone() else {
            self.clear_book(cx);
            self.report_nothing_chosen(cx);
            return;
        };
        // The row limits the book offers follow from how deep its venue publishes,
        // and that bound is known before the stream is opened: a stored choice the
        // route cannot fill goes back to the default here, so the request and the
        // header agree on one choice from the first frame.
        let depth = directory.order_book_depth(&selection.listing.route);
        if self.config.settle(depth) {
            log::warn!(
                "the book's stored grouping or row limit is not offered here; it was reset to \
                 the default"
            );
            self.config_change.publish(cx.app_mut());
        }
        // What the program still serves is asked before the book reads, but only as
        // advice: a listing that is gone goes back to "nothing chosen" instead of
        // being drawn and failing. The question is not the authority on whether the
        // book can be followed — the request below is, because the connector
        // resolves the listing's own rules and units inside it. So a lookup that
        // failed is only logged and the request goes out anyway.
        let routes = directory.loaded_scopes();
        if !offers_route(&routes, &selection.listing.route, CapabilityId::OrderBook) {
            self.withdraw(&selection.listing, "is no longer a loaded route", cx);
            return;
        }
        let offered = directory.contains(&selection.listing);
        let support = listing_support(&selection.listing, offered, &routes, directory.as_ref());
        if support == ListingSupport::Withdrawn {
            self.withdraw(&selection.listing, "is no longer offered by its route", cx);
            return;
        }
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        // A new scope drops what the previous one drew; a retry keeps the book the
        // user is still reading while the generation behind it is replaced.
        if !retry && !market_reloaded {
            self.clear_book(cx);
        }
        let key = OrderBookStreamKey::new(
            selection.listing.clone(),
            self.config.price_multiplier,
            self.config.window_size,
        );
        let request = if retry || market_reloaded {
            OrderBookRequest::Retry(key)
        } else {
            OrderBookRequest::Load(key)
        };
        if let Some(binding) = self.binding.as_mut() {
            let request_id = binding.call(cx, request);
            self.pending.track(request_id, PendingCommand::Start);
        }
    }

    /// Says what a book with nothing to follow is doing: nothing, and it is not a
    /// failure.
    fn nothing_chosen(&mut self, cx: &mut WidgetCx<'_>) {
        self.clear_book(cx);
        self.report_nothing_chosen(cx);
    }

    /// Drops a selection the directory no longer serves.
    fn withdraw(&mut self, listing: &VenueListingKey, cause: &str, cx: &mut WidgetCx<'_>) {
        log::warn!(
            "`{}` {cause}; the book went back to having nothing chosen",
            listing.listing_id
        );
        self.config.selection = None;
        self.config_change.publish(cx.app_mut());
        self.nothing_chosen(cx);
    }

    /// Drops everything that was derived from the previous scope.
    ///
    /// The metadata goes with it because a new listing is rounded and labelled in
    /// its own units, so the header would otherwise name the previous listing's
    /// price step until the new stream says otherwise.
    fn clear_book(&mut self, cx: &mut WidgetCx<'_>) {
        self.snapshot = None;
        self.grouped = None;
        self.metadata = None;
        self.price_step_labels = price_step_labels(None);
        self.previous_mid_price = None;
        self.ready = None;
        self.stop_depth_animation(cx.app_mut());
        cx.write(&self.depth_progress, 1.0);
        cx.write(&self.depth_span, 0.0);
        cx.write(
            &self.unit_labels,
            OrderBookUnitLabels {
                price: SmolStr::new_static("--"),
                quantity: SmolStr::new_static("--"),
            },
        );
        self.book_dirty = true;
    }

    // -------------------------------------------------------------- messages

    fn apply(&mut self, inbound: Inbound<OrderBookProtocol>, cx: &mut WidgetCx<'_>) {
        match inbound {
            Inbound::Reliable { key, message } => self.apply_reliable(key, &message, cx),
            Inbound::Latest { key, message } => self.apply_latest(key, &message, cx),
            Inbound::Rpc { request_id, result } => self.apply_rpc(request_id, result, cx),
        }
    }

    /// Applies one status the stream reported.
    fn apply_reliable(
        &mut self,
        key: OrderBookStreamKey,
        message: &OrderBookStreamMessage,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx.app())
            || self.awaiting_market
        {
            return;
        }
        let OrderBookStreamMessage::Status(status) = message else {
            return;
        };
        match status {
            OrderBookStatus::Connecting => {
                if self.snapshot.is_none() {
                    self.status.report(cx.app_mut(), WidgetState::Connecting);
                }
            }
            OrderBookStatus::LoadingSnapshot(metadata) => {
                if self.set_metadata(metadata.clone(), cx) {
                    self.book_dirty = true;
                }
                self.status.report(cx.app_mut(), WidgetState::Loading);
            }
            OrderBookStatus::Ready { revision } => {
                self.ready = Some(revision.clone());
                // The reliable channel and the latest one have no order between
                // them, so this can arrive before the book it describes: claiming
                // readiness now would be claiming a book the widget does not have.
                if self
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| !is_after(revision, &snapshot.sequence))
                {
                    self.status.report(cx.app_mut(), WidgetState::Ready);
                } else {
                    self.status.report(cx.app_mut(), WidgetState::Loading);
                }
            }
            OrderBookStatus::Error(error) => {
                if self.report_waiting_for_directory(error, cx) {
                    return;
                }
                let recovery = self.recovery.clone();
                self.status.report(
                    cx.app_mut(),
                    WidgetState::failed(
                        Arc::new(OrderBookFailure::Stream(error.clone())),
                        recovery,
                    ),
                );
            }
        }
    }

    /// Applies a book the stream published.
    fn apply_latest(
        &mut self,
        key: OrderBookStreamKey,
        message: &OrderBookStreamMessage,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx.app())
            || self.awaiting_market
        {
            return;
        }
        if let OrderBookStreamMessage::Snapshot(snapshot) = message {
            self.accept_snapshot(snapshot.clone(), cx);
        }
    }

    /// Accepts a published book as the whole of what the widget draws.
    fn accept_snapshot(&mut self, snapshot: OrderBookSnapshot, cx: &mut WidgetCx<'_>) {
        // A book the stream has already overtaken would only make the ladder jump
        // backwards. Within one generation the venue's sequence only moves
        // forwards, so a book that is strictly behind the one on screen is one that
        // was overtaken by a status, which is not something to draw.
        if self
            .snapshot
            .as_ref()
            .is_some_and(|current| is_after(&current.sequence, &snapshot.sequence))
        {
            return;
        }
        let metadata_changed = self.set_metadata(snapshot.metadata.clone(), cx);
        let sequence = snapshot.sequence.clone();
        self.snapshot = Some(snapshot);
        if metadata_changed {
            self.book_dirty = true;
        }
        let regrouped = self.regroup(cx);
        let _ = regrouped;
        // Readiness is reported at the protocol's own boundary: a complete book is
        // not the same thing as the book the status said was complete, and this is
        // where the two meet.
        if self
            .ready
            .as_ref()
            .is_some_and(|ready| !is_after(ready, &sequence))
        {
            self.status.report(cx.app_mut(), WidgetState::Ready);
        }
    }

    fn apply_rpc_result(
        &mut self,
        result: WidgetRpcResult<OrderBookProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        // An answer that arrives after the pane failed, or while it waits for its
        // market, is about a call that no longer belongs to what the pane is
        // showing: the failure is terminal until the user recovers (AGENTS §4.4).
        if self.status.is_failed(cx.app()) || self.awaiting_market {
            return;
        }
        match result {
            Ok(OrderBookResponse::StreamStarted) => {}
            Err(error) => {
                let recovery = self.recovery.clone();
                self.status.report(
                    cx.app_mut(),
                    WidgetState::failed(Arc::new(OrderBookFailure::Rpc(error)), recovery),
                );
            }
        }
    }

    /// Applies one RPC reply, once its identity says it answers the command in
    /// flight.
    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<OrderBookProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        if let Some(PendingCommand::Start) = self.pending.take(request_id) {
            self.apply_rpc_result(result, cx);
        }
    }

    // ------------------------------------------------------------- regrouping

    /// Rebuilds the drawn ladder from the snapshot the widget holds.
    ///
    /// Returns whether the drawn book changed, which is also whether the body's
    /// rows and bars have to be published again.
    fn regroup(&mut self, cx: &mut WidgetCx<'_>) -> bool {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return false;
        };
        let previous_mid_price = self.grouped.as_ref().and_then(|book| book.mid_price);
        let progress = *cx.read(&self.depth_progress);
        let grouped = GroupedBook::build(
            snapshot,
            self.grouped.as_deref(),
            progress,
            self.config.price_multiplier,
            self.effective_window_size(),
        );
        let changed = self
            .grouped
            .as_ref()
            .is_none_or(|current| !current.has_same_current_state(&grouped));
        if !changed {
            return false;
        }
        self.previous_mid_price = previous_mid_price;
        self.grouped = Some(Arc::new(grouped));
        self.book_dirty = true;
        self.restart_depth_animation(cx);
        true
    }

    /// How many rows one side may keep: the user's limit, narrowed to what the
    /// pane has room to draw.
    fn effective_window_size(&self) -> usize {
        self.config.window_size.min(self.drawable_rows.max(1))
    }

    /// The depth the book being drawn was published at.
    fn book_depth(&self) -> Option<u16> {
        self.metadata.as_ref().map(|metadata| metadata.depth)
    }

    /// The increment the grouping labels are written in: the book's own tick once
    /// one has been published, and the venue's catalog before that.
    fn display_price_tick(&self) -> Option<Tick> {
        self.grouped
            .as_ref()
            .map(|book| book.price_tick)
            .or_else(|| {
                self.metadata
                    .as_ref()
                    .map(|metadata| metadata.precision.price_tick)
            })
    }

    /// The label of one grouping choice, reusing the one formatted for the current
    /// price tick.
    fn price_step_label(&self, multiplier: u32) -> SmolStr {
        PRICE_MULTIPLIERS
            .iter()
            .position(|candidate| *candidate == multiplier)
            .map_or_else(
                || price_step_label(self.display_price_tick(), multiplier),
                |index| self.price_step_labels[index].clone(),
            )
    }

    fn set_metadata(&mut self, metadata: OrderBookMetadata, cx: &mut WidgetCx<'_>) -> bool {
        if self.metadata.as_ref() == Some(&metadata) {
            return false;
        }
        self.price_step_labels = price_step_labels(Some(metadata.precision.price_tick));
        self.metadata = Some(metadata);
        self.publish_unit_labels(cx);
        true
    }

    // -------------------------------------------------------------- animation

    /// Sends the bars after the ratios the ladder now asks for.
    ///
    /// Every restart is measured from where the previous frame had painted, so an
    /// update that arrives mid-flight retargets the movement instead of resetting
    /// it. Decorative movement is what the platform's reduce-motion setting asks
    /// not to have, so with it set the bars simply arrive.
    fn restart_depth_animation(&mut self, cx: &mut WidgetCx<'_>) {
        let reduce_motion = *cx.read(&cx.services().reduce_motion);
        let span = self
            .grouped
            .as_ref()
            .map_or(0.0, |book| book.animation_span());
        if reduce_motion || !depth_is_active(0.0, span) {
            cx.write(&self.depth_progress, 1.0);
            cx.write(&self.depth_span, 0.0);
            self.stop_depth_animation(cx.app_mut());
            return;
        }
        cx.write(&self.depth_span, span);
        cx.write(&self.depth_progress, 0.0);
        self.ensure_depth_ticker(cx);
    }

    /// Starts the depth-timer chain if the movement showing now still needs frames.
    ///
    /// A movement can be started before the body exists (the stream published the
    /// book on a frame whose `has_content` the framework had not sampled yet), and a
    /// timer chain needs a node to scope itself to: the body's own root is where a
    /// later `body` call picks the movement back up.
    fn ensure_depth_ticker(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(scope) = self.body_root else {
            return;
        };
        let progress = *cx.read(&self.depth_progress);
        let span = *cx.read(&self.depth_span);
        if !depth_is_active(progress, span) {
            return;
        }
        if *cx.read(&cx.services().reduce_motion) {
            return;
        }
        if self
            .depth_ticker
            .as_ref()
            .is_some_and(|state| state.alive.get() && state.ticking.get())
        {
            return;
        }
        let state = Rc::new(DepthTickerState {
            progress: self.depth_progress,
            span: self.depth_span,
            ticking: Cell::new(true),
            alive: Cell::new(true),
            window: cx.window(),
            scope,
            timer: RefCell::new(None),
        });
        state.schedule(cx.app_mut());
        self.depth_ticker = Some(state);
    }

    /// Stops the animation chain and gives its pending timer back.
    fn stop_depth_animation(&mut self, app: &mut App) {
        let Some(state) = self.depth_ticker.take() else {
            return;
        };
        state.alive.set(false);
        state.ticking.set(false);
        if let Some(timer) = state.timer.borrow_mut().take() {
            let _ = app.cancel_timer(&timer);
        }
    }

    // ---------------------------------------------------------------- header

    /// The filters the picker this control opens is built with.
    ///
    /// The loaded venues are read here, where the control is built, rather than
    /// kept: a scope loaded after this book started reading is a scope the user may
    /// select, and one a shrinking commit withdrew is not.
    fn instrument_filters(&self) -> InstrumentFilters {
        InstrumentFilters {
            kind_filter: InstrumentKindFilter::All,
            status_filter: InstrumentStatusFilter::Any,
            required_capability: CapabilityId::OrderBook,
            entry_predicate: None,
            routes: self.directory.as_ref().map_or_else(
                || Arc::from(Vec::<LoadedScope>::new()),
                |directory| directory.loaded_scopes(),
            ),
        }
    }

    /// The selector host the header control opens its dialog through.
    ///
    /// The host is the one thing in this widget that owns the services rather than
    /// borrowing them, so it takes the shared handle the composition root handed
    /// over ([`WidgetCx::services_rc`]) — the same aggregate the widget works over,
    /// not a second one assembled from its fields.
    fn selector_host(&self, cx: &WidgetCx<'_>) -> SelectorHost {
        let host = SelectorHost::new(Rc::clone(cx.services_rc()));
        match &self.directory {
            Some(directory) => host.with_catalog(Some(directory.catalog())),
            None => host,
        }
    }

    fn header_state(&self, cx: &WidgetCx<'_>) -> HeaderState {
        HeaderState {
            selection: self.config.selection.clone(),
            language: *cx.read(&self.language),
            generation: self.seen_generation,
        }
    }

    fn header_right_state(&self, cx: &WidgetCx<'_>) -> HeaderRightState {
        HeaderRightState {
            language: *cx.read(&self.language),
            depth: self.book_depth(),
            price_tick: self.display_price_tick(),
            price_multiplier: self.config.price_multiplier,
            window_size: self.config.window_size,
            disabled: self.awaiting_market || self.snapshot.is_none(),
        }
    }

    /// Builds the instrument control into the header's left group.
    fn build_header_left(
        &mut self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let host = self.selector_host(cx);
        let filters = self.instrument_filters();
        let selection = self.config.selection.clone();
        let requests = self.selection_requests;
        let button = InstrumentButton::new(
            cx.unique_key(INSTRUMENT_KEY_SUFFIX),
            selection,
            filters,
            "widget.order_book.select_instrument",
            move |selection: InstrumentSelection, app: &mut App, window: WindowId| {
                hand_over_selection(requests, selection, app, window);
            },
        );
        let window = cx.window();
        let control = button.build(&host, cx.app_mut(), window, parent)?;
        self.header_left = Some(HeaderLeft {
            control,
            parent,
            built: self.header_state(cx),
        });
        Ok(())
    }

    /// Builds the two drawing pickers into the header's right group.
    fn build_header_right(
        &mut self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let row = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .gap(4.0)
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        let theme = self.theme.clone();
        let translate = self.translate.clone();
        let window = cx.window();
        let disabled = self.awaiting_market || self.snapshot.is_none();
        let requests = self.header_requests;

        let price_items = PRICE_MULTIPLIERS
            .iter()
            .map(|multiplier| {
                SelectItem::new(
                    format_smolstr!("{multiplier}"),
                    self.price_step_label(*multiplier),
                )
            })
            .collect::<Vec<_>>();
        let price_step = self.build_select(
            cx,
            row,
            SelectSpec {
                key_suffix: PRICE_STEP_KEY_SUFFIX,
                aria_key: "widget.order_book.price_grouping",
                items: price_items,
                chosen: format_smolstr!("{}", self.config.price_multiplier),
                placeholder: self.price_step_label(self.config.price_multiplier),
                kind: HeaderSelect::PriceStep,
            },
            requests,
            disabled,
            theme.clone(),
            window,
        )?;

        let window_items = window_sizes(self.book_depth())
            .iter()
            .map(|rows| {
                SelectItem::new(
                    format_smolstr!("{rows}"),
                    t_smol!(translate, "widget.order_book.rows", count = rows),
                )
            })
            .collect::<Vec<_>>();
        let window_size = self.build_select(
            cx,
            row,
            SelectSpec {
                key_suffix: WINDOW_SIZE_KEY_SUFFIX,
                aria_key: "widget.order_book.visible_rows",
                items: window_items,
                chosen: format_smolstr!("{}", self.config.window_size),
                placeholder: t_smol!(
                    translate,
                    "widget.order_book.rows",
                    count = self.config.window_size
                ),
                kind: HeaderSelect::WindowSize,
            },
            requests,
            disabled,
            theme,
            window,
        )?;

        self.header_right = Some(HeaderRight {
            slot: parent,
            root: row,
            price_step,
            window_size,
            built: self.header_right_state(cx),
        });
        Ok(())
    }

    /// Builds one drawing picker over the entries `spec` names into `parent`.
    fn build_select(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        spec: SelectSpec,
        requests: Signal<Option<HeaderRequest>>,
        disabled: bool,
        theme: ThemeHandle,
        window: WindowId,
    ) -> Result<Select, EngineError> {
        let options = SelectOptions::default()
            .size(UnifiedSize::Small)
            .label(t_smol!(self.translate, spec.aria_key))
            .placeholder(spec.placeholder);
        let key = cx.unique_key(spec.key_suffix);
        let layer = base_ui::kit::layer::Layer::clone(cx.layer());
        let router = base_ui::kit::overlay::AnimationRouter::clone(cx.animation());
        let kind = spec.kind;
        let select = Select::new(cx.app_mut(), theme, layer, router, key, options).on_change(
            move |app, chosen| {
                let Some(id) = chosen.first() else {
                    return;
                };
                let request = match kind {
                    HeaderSelect::PriceStep => id.parse::<u32>().ok().map(HeaderRequest::PriceStep),
                    HeaderSelect::WindowSize => {
                        id.parse::<usize>().ok().map(HeaderRequest::WindowSize)
                    }
                };
                if let Some(request) = request {
                    hand_over_header_request(requests, request, app, window);
                }
            },
        );
        select
            .set_items(cx.app_mut(), spec.items)
            .unwrap_or_else(|error| {
                log::warn!(
                    "the order book's picker `{}` could not set its items: {error}",
                    spec.key_suffix
                );
            });
        select.set_chosen(cx.app_mut(), [spec.chosen.as_str()], false);
        select.build(cx.app_mut(), window, parent)?;
        // Disable after the build: `Interactive::build` initialises its state from the
        // builder flags, so a `set_disabled` written before it would be overwritten.
        let _ = select.set_disabled(cx.app_mut(), disabled);
        Ok(select)
    }

    /// Keeps the header controls in step with the copy they were built with.
    fn sync_header(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if let Some(header) = self.header_left.as_ref() {
            let moved = header.built.language != *cx.read(&self.language)
                || header.built.generation != self.seen_generation
                || header.built.selection.as_ref() != self.config.selection.as_ref();
            if moved {
                let parent = header.parent;
                if cx.tree().get(parent).is_err() {
                    // The pane took its row down; the control went with it, and only
                    // the component (its click handler and signal scope) is left to
                    // release.
                    self.release_header_left(cx, false);
                } else {
                    self.release_header_left(cx, true);
                    self.build_header_left(parent, cx)?;
                }
            }
        }
        if let Some(header) = self.header_right.as_ref() {
            let moved = header.built != self.header_right_state(cx);
            if moved {
                let slot = header.slot;
                if cx.tree().get(slot).is_err() {
                    // The pane took its group down; the widget's row went with it, and
                    // only the components are left to release.
                    self.release_header_right(cx, false);
                } else {
                    self.release_header_right(cx, true);
                    self.build_header_right(slot, cx)?;
                }
            }
        }
        Ok(())
    }

    /// Takes down the instrument control's component.
    ///
    /// `remove_node` is true when the pane's row is still there and the widget is
    /// replacing its own control in it; a row the pane already took down needs no
    /// node removal (`StaleNode`). The node itself belongs to the pane's header
    /// row, but the **component** — its click handler and its signal scope — is
    /// this widget's in every case, which is why the handle the selector's control
    /// hands back is released here.
    fn release_header_left(&mut self, cx: &mut WidgetCx<'_>, remove_node: bool) {
        let Some(header) = self.header_left.take() else {
            return;
        };
        let node = header.control.node();
        if remove_node && cx.tree().get(node).is_ok() {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, node);
        }
        let _ = header.control.release(cx.app_mut());
    }

    /// Takes down the two drawing pickers and their row.
    ///
    /// `remove_node` is true when the pane's group is still there and the widget is
    /// replacing its own row in it; a group the pane already took down needs no node
    /// removal. The row is this widget's own, so it is the node that is removed.
    fn release_header_right(&mut self, cx: &mut WidgetCx<'_>, remove_node: bool) {
        let Some(header) = self.header_right.take() else {
            return;
        };
        if remove_node && cx.tree().get(header.root).is_ok() {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, header.root);
        }
        header.price_step.release(cx.app_mut());
        header.window_size.release(cx.app_mut());
    }

    // ------------------------------------------------------------ selections

    /// Applies an instrument picked in the header as one scope transition.
    fn select_selection(&mut self, selection: InstrumentSelection, cx: &mut WidgetCx<'_>) -> bool {
        // Picking the scope that is already shown is a request to open it again
        // when it is not working, and nothing at all otherwise.
        if self.config.selection.as_ref() == Some(&selection) && !self.status.is_failed(cx.app()) {
            return false;
        }
        self.config.selection = Some(selection);
        self.config_change.publish(cx.app_mut());
        // The ladder on screen belongs to the scope the user just left.
        self.clear_book(cx);
        // A pane that is waiting for its market is a pane whose gate `start` would
        // not run: `start` reads the loaded scopes of a directory that has none and
        // withdraws the choice the user just made. `load` is the path that runs the
        // gate, and it keeps the choice while it waits (AGENTS §4.2).
        if self.awaiting_market {
            self.load(cx);
        } else {
            self.start(false, false, cx);
        }
        true
    }

    fn select_price_multiplier(&mut self, multiplier: u32, cx: &mut WidgetCx<'_>) {
        if self.config.price_multiplier == multiplier || !PRICE_MULTIPLIERS.contains(&multiplier) {
            return;
        }
        self.config.price_multiplier = multiplier;
        self.config_change.publish(cx.app_mut());
        self.restream(cx);
        self.regroup(cx);
    }

    fn select_window_size(&mut self, window_size: usize, cx: &mut WidgetCx<'_>) {
        if self.config.window_size == window_size
            || !window_sizes(self.book_depth()).contains(&window_size)
        {
            return;
        }
        self.config.window_size = window_size;
        self.config_change.publish(cx.app_mut());
        self.restream(cx);
        self.regroup(cx);
    }

    /// Starts the generation the drawing choices now describe.
    ///
    /// Both choices are part of the stream's own scope, so the generation that was
    /// publishing for the previous one is replaced. Nothing on screen is dropped:
    /// the ladder the user is reading stays up until the new generation publishes
    /// the book it holds.
    fn restream(&mut self, cx: &mut WidgetCx<'_>) {
        // A failed pane is waiting for the user, not for a reload, so the stream it
        // would replace is left alone.
        if self.status.is_failed(cx.app()) {
            return;
        }
        let Some(key) = self.current_key() else {
            return;
        };
        if let Some(binding) = self.binding.as_mut() {
            let request_id = binding.call(cx, OrderBookRequest::Load(key));
            self.pending.track(request_id, PendingCommand::Start);
        }
    }
}

/// Which drawing choice one select in the header's right group states.
#[derive(Clone, Copy)]
enum HeaderSelect {
    PriceStep,
    WindowSize,
}

/// Everything one drawing picker is built from.
struct SelectSpec {
    key_suffix: &'static str,
    aria_key: &'static str,
    items: Vec<SelectItem>,
    chosen: SmolStr,
    placeholder: SmolStr,
    kind: HeaderSelect,
}

/// Hands a confirmed selection over to the next `sync`.
///
/// The control's confirm handler runs with an application context and no widget
/// context, so it cannot call into the pane: it writes the request signal and asks
/// for a frame, and [`WidgetView::sync`] applies the choice and publishes the
/// configuration change. A confirmation that wrote nothing would leave the pane
/// following the instrument it was already on, however far the user browsed.
fn hand_over_selection(
    requests: Signal<Option<InstrumentSelection>>,
    selection: InstrumentSelection,
    app: &mut App,
    window: WindowId,
) {
    app.write(&requests, Some(selection));
    let _ = app.request_repaint(window);
}

/// Hands a drawing choice over to the next `sync`, the same way a confirmed
/// selection is handed over.
fn hand_over_header_request(
    requests: Signal<Option<HeaderRequest>>,
    request: HeaderRequest,
    app: &mut App,
    window: WindowId,
) {
    app.write(&requests, Some(request));
    let _ = app.request_repaint(window);
}

/// A text style right-aligned in its column, which is what every ladder number is.
fn aligned_text(size: f32) -> TextStyle {
    let mut style = TextStyle::new(size);
    style.alignment = TextAlign::Right;
    style
}

/// A bold text style for the mid price and the imbalance shares.
fn bold_text(size: f32) -> TextStyle {
    let mut style = TextStyle::new(size);
    style.weight = 700;
    style
}

/// Builds one row of a ladder: the three labels, each bound to the book signal.
///
/// The row factory runs **without an application context** (AGENTS §9), so every
/// dynamic value here is a `bind_text` / `bind_style` closure over a signal the
/// pane owns rather than something read while the row is built.
fn build_row(
    tree: &mut ui_core::node::tree::NodeTree,
    parent: NodeId,
    row: usize,
    side: RowSide,
    book: Signal<PublishedBook>,
    theme: ThemeHandle,
    font_size: f32,
) -> Result<(), EngineError> {
    let container = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .size_full()
                .min_width_0()
                .build(),
        )
        .build(tree, parent)?;
    for column in 0..3usize {
        let mut builder = NodeBuilder::text(SmolStr::default(), aligned_text(font_size))
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .min_width_0()
                    .height_full()
                    .build(),
            )
            .bind_text(move |read| {
                let book = read.read(&book);
                book.level(side, row)
                    .map(|level| match column {
                        0 => level.price_text.clone(),
                        1 => level.quantity_text.clone(),
                        _ => level.cumulative_quantity_text.clone(),
                    })
                    .unwrap_or_default()
            });
        builder = if column == 0 {
            let theme = theme.clone();
            let role = side.price_role();
            builder.bind_style(move |read| StylePatch {
                foreground: Some(theme.role(read, role)),
                ..StylePatch::default()
            })
        } else {
            let theme = theme.clone();
            builder.bind_style(move |read| StylePatch {
                foreground: Some(theme.role(read, Role::FgDefault)),
                ..StylePatch::default()
            })
        };
        builder.build(tree, container)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;

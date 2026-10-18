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
//! Three pieces of local state make the book feel like a book rather than a
//! picture, and each of them is deliberately kept out of the status:
//!
//! - **the grouped ladder**, which is rebuilt from the snapshot the widget is
//!   already holding whenever the grouping, the row limit or the pane's height
//!   changes. None of those is worth reopening a stream for;
//! - **the depth animation**, which eases every published book towards the ratios
//!   it asks for, and stops as soon as the bars have arrived;
//! - **the previous mid price**, which is only there to say which way the mid
//!   price has moved since the frame before.

use std::{convert::Infallible, rc::Rc, sync::Arc, time::Instant};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    empty_state::EmptyState,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    menu::{MenuItem, MenuItemAction},
    select::Select,
    size::UnifiedSizable,
    style::theme::{Theme, ThemeExt},
};
use gpui_kit::{
    AnyElement, App, Context, ElementId, Entity, FontWeight, Hsla, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window,
    base::Disableable, div, relative,
};
use i18n::t;
use instrument_selector_ui::{
    instrument_button::InstrumentButton,
    instrument_selector::{
        InstrumentFilters, InstrumentKindFilter, InstrumentStatusFilter, LoadedScope, offers_route,
    },
};
use market_participants_connector::{
    catalog::{display::catalog::DisplaySnapshot, error::CatalogError, store::Catalog},
    connector::MarketParticipantsConnector,
    venue::loaded::VenuesUpdated,
};
use market_participants_model::{
    asset::AssetId,
    decimal::{Price, Tick},
    identifier::VenueId,
    listing::route::VenueListingKey,
    market::{book::MarketSequence, selection::InstrumentSelection, units::QuantityUnit},
    routing::CapabilityId,
};
use serde::{Deserialize, Serialize};
use smol_str::format_smolstr;
use widget::{
    binding::{WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    error::{LocalizedError, market_is_catalog_not_loaded},
    identity::{WidgetId, WidgetInstanceId},
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    view::{WidgetHeader, WidgetView},
};
use widget_tokio_runtime::global;

use super::{
    model::{
        GroupedBook, OrderBookMetadata, OrderBookSnapshot, PRICE_MULTIPLIERS, WINDOW_SIZES,
        default_window_size, interpolated_ratio, is_after, price_step_label, price_step_labels,
        window_sizes,
    },
    paint::{
        DEPTH_ANIMATION_SETTLE_EPSILON, HEADER_HEIGHT, IMBALANCE_HEIGHT, MID_PRICE_HEIGHT,
        OrderBookPaintCaches, SideCanvas, depth_animation_progress_after,
    },
    service::{
        OrderBookProtocol, OrderBookRequest, OrderBookResponse, OrderBookService, OrderBookSource,
        OrderBookStatus, OrderBookStreamError, OrderBookStreamKey, OrderBookStreamMessage,
    },
};

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
    Rpc(RpcCallError<Infallible>),
}

impl LocalizedError for OrderBookFailure {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Stream(error) => error.localized_message(cx),
            Self::Rpc(error) => error.localized_message(cx),
        }
    }
}

/// What the program knows about a listing this pane is configured to follow.
///
/// The three are kept apart because only one of them is an answer: a question
/// that could not be put proves nothing about the listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListingSupport {
    /// The published catalog holds the listing, and the route still serves depth.
    Supported,
    /// The program answered that it does not serve this listing: the route left
    /// the registry, the route publishes no depth, or the catalog does not hold
    /// the listing.
    Unsupported,
    /// The question could not be answered.
    Unknown,
}

/// Asks the program whether it still serves `listing`.
///
/// The loaded scopes answer first because they are the only source of what a route
/// publishes, and the catalog answers for the listing itself. A catalog failure
/// is reported as the unknown answer it is: a venue that did not reply is not a
/// venue that delisted the instrument.
fn listing_support(
    catalog: &Catalog,
    routes: &[LoadedScope],
    listing: &VenueListingKey,
) -> ListingSupport {
    if !offers_route(routes, &listing.route, CapabilityId::OrderBook) {
        return ListingSupport::Unsupported;
    }
    match catalog.resolve_listing(listing) {
        Ok(resolved) if resolved.status.is_terminal() => ListingSupport::Unsupported,
        Ok(_) => ListingSupport::Supported,
        Err(CatalogError::ListingNotFound { .. } | CatalogError::RouteNotRegistered { .. }) => {
            ListingSupport::Unsupported
        }
        Err(error) => {
            log::warn!(
                "asking the catalog whether it still lists `{}` failed; the book asks for the \
                 stream anyway: {error}",
                listing.listing_id.as_str()
            );
            ListingSupport::Unknown
        }
    }
}

/// The labels the ladder's columns are written with.
///
/// They come from the listing's own rules — the asset its prices are quoted in and
/// what its quantities count — never from a field name, and the name of an asset is
/// the display code one published generation decided on.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OrderBookUnitLabels {
    price: SharedString,
    quantity: SharedString,
}

impl OrderBookUnitLabels {
    fn of(metadata: &OrderBookMetadata, cx: &App) -> Self {
        // One capture names both columns, so the ladder never mixes two
        // publications even if the catalog is republished while it is labelled.
        let generation = cx
            .global::<MarketParticipantsConnector>()
            .catalog()
            .cached_display();
        Self {
            price: asset_display(&generation, &metadata.quote_asset, cx),
            quantity: quantity_label(&metadata.quantity_unit, &generation, cx),
        }
    }
}

/// The display code of one asset, as the captured generation published it.
///
/// Only the display catalog knows what an asset is called: the canonical identity
/// of a token is a digest, and the code it publishes is the whole name of the
/// asset. An asset the captured generation does not name is
/// the localized unknown rather than an identity digest.
fn asset_display(generation: &DisplaySnapshot, asset: &AssetId, cx: &App) -> SharedString {
    generation.resolver().asset_code(asset).map_or_else(
        |_| t!(cx, "error.display.unknown_asset"),
        |code| SharedString::new(code.as_str()),
    )
}

/// The label of what one quantity counts.
fn quantity_label(unit: &QuantityUnit, generation: &DisplaySnapshot, cx: &App) -> SharedString {
    match unit {
        QuantityUnit::AssetUnits { asset } | QuantityUnit::Notional { asset } => {
            asset_display(generation, asset, cx)
        }
        QuantityUnit::Contracts => t!(cx, "widget.order_book.contracts_unit"),
        QuantityUnit::OutcomeTokens { .. } => t!(cx, "widget.order_book.outcome_tokens_unit"),
    }
}

/// Where the depth bars are in their journey to the ratios the last book asked
/// for.
///
/// The progress is measured from the elapsed time rather than from a frame count,
/// so a movement looks the same however the frames happen to be spaced, and an
/// interrupted movement keeps its speed instead of restarting.
#[derive(Clone, Copy, Debug)]
struct DepthAnimationState {
    progress: f32,
    transition_span: f32,
    last_frame: Option<Instant>,
}

impl DepthAnimationState {
    const fn settled() -> Self {
        Self {
            progress: 1.0,
            transition_span: 0.0,
            last_frame: None,
        }
    }

    /// Whether the bars still have a visible distance to cover, which is also
    /// whether a frame is worth scheduling for.
    fn is_active(self) -> bool {
        self.transition_span * (1.0 - self.progress) > DEPTH_ANIMATION_SETTLE_EPSILON
    }

    fn restart(&mut self, transition_span: f32) {
        self.transition_span = if transition_span.is_finite() {
            transition_span.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.progress = if self.transition_span > DEPTH_ANIMATION_SETTLE_EPSILON {
            0.0
        } else {
            1.0
        };
        self.last_frame = None;
    }

    /// Advances to `now` and reports how far the bars have come.
    fn advance(&mut self, now: Instant) -> f32 {
        if !self.is_active() {
            self.last_frame = None;
            return 1.0;
        }

        if let Some(previous_frame) = self.last_frame.replace(now) {
            self.progress = depth_animation_progress_after(
                self.progress,
                self.transition_span,
                now.saturating_duration_since(previous_frame),
            );
            if !self.is_active() {
                self.last_frame = None;
            }
        }
        self.progress
    }
}

pub struct OrderBookWidget {
    config: OrderBookConfig,
    binding: WidgetBinding<OrderBookProtocol>,
    status: Entity<WidgetStatus>,
    /// The recovery the framework offers for every failure this widget reports.
    recovery: WidgetAction,
    config_change: WidgetConfigChange,
    directory_generation: u64,
    /// The book the stream last published, which is what regrouping reads from:
    /// changing the grouping or the row limit redraws this book rather than
    /// asking the venue for another one.
    snapshot: Option<OrderBookSnapshot>,
    /// The grouped ladder a frame draws.
    grouped: Option<Arc<GroupedBook>>,
    /// The sequence a book has to reach before the widget is done working, as the
    /// stream's own status named it.
    ready: Option<MarketSequence>,
    metadata: Option<OrderBookMetadata>,
    /// The labels of the price-grouping choices, formatted once per price tick.
    price_step_labels: [SharedString; PRICE_MULTIPLIERS.len()],
    /// The labels of the price and quantity columns, formatted once per scope.
    unit_labels: OrderBookUnitLabels,
    depth_animation: DepthAnimationState,
    previous_mid_price: Option<Price>,
    /// The prepared frames and the row count the pane currently has room for. The
    /// canvases fill both in, and the widget reads the row count back to decide
    /// how much of the ladder is worth grouping.
    paint_caches: Rc<OrderBookPaintCaches>,
    /// Whether the ladder is waiting for its market rather than reading it.
    ///
    /// The pane keeps its stream while it waits — the alternative is cancelling the
    /// binding, which is terminal, and this pane has to be able to read the market
    /// again once the user loads it — so a book that was in flight when the route
    /// went away can still arrive. Accepting it would put a snapshot back and make the
    /// pane report content over the notice the framework is drawing, so every message
    /// entry refuses while this is set. The stream key cannot do this job: it says
    /// which scope a book belongs to, and this one belongs to the scope the notice is
    /// about.
    awaiting_market: bool,
    /// Whether the venue this book reads was loaded or refreshed again.
    ///
    /// Such a commit replaces the catalog generation the live stream resolved its rules, units
    /// and price steps against, while the stream itself keeps what it resolved when it was
    /// opened. So the book has to start over, and the request it sends has to be the one that
    /// opens a new stream: [`Self::load`] reads this after its own gate — a book a shrinking
    /// commit sent back to waiting keeps it, so the load that brings the market back reopens the
    /// stream too — and it is what makes the request a `Retry` rather than the idempotent `Load`,
    /// which the service answers without touching a stream for that scope that is already live.
    market_reloaded: bool,
}

impl WidgetView for OrderBookWidget {
    type Config = OrderBookConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.market.order_book");
    const DISPLAY_NAME: &'static str = "widget.names.order_book";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;
    const DESCRIPTION: &'static str = "Live grouped market depth";
    const ICON: Option<&'static str> = Some("widgets/order_book.png");

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

    /// A book is content as soon as one has been accepted — including an empty
    /// one. An empty book is what "this instrument has published nothing" looks
    /// like: the ladder is still there, and there is nothing to report about it.
    fn has_content(&self) -> bool {
        self.snapshot.is_some()
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        // The market this book follows has to be loaded before anything else
        // happens here, and the check is first on purpose: a route the user never
        // loaded, or one a shrinking commit has since taken away, leaves the
        // pane waiting for it instead of reaching the branches below, which settle
        // the selection the user stored.
        if !self.market_ready(cx) {
            self.wait_for_market(cx);
            return;
        }
        // Read past the gate: a commit on the book's own venue that took its market away leaves
        // this set while the stream that resolved against the retired generation is still alive,
        // and the load that brings the market back has to reopen that stream.
        let market_reloaded = std::mem::take(&mut self.market_reloaded);
        let retry = self.status.is_failed(cx);
        self.start(retry, market_reloaded, cx);
    }

    /// The scope on the left and the two drawing choices on the right.
    ///
    /// Neither group is usable before the market participant's metadata is: the drawing choices refuse input
    /// while the book waits for its market, and the instrument picker disables itself whenever the
    /// loaded venues offer it nothing to select from.
    fn header(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> WidgetHeader {
        WidgetHeader::empty()
            .left(self.instrument_button(cx))
            .right(self.drawing_controls(cx))
    }
}

impl OrderBookWidget {
    fn new_with_source<S>(
        config: OrderBookConfig,
        source: S,
        status: Entity<WidgetStatus>,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: OrderBookSource,
    {
        let service = OrderBookService::new(source, SNAPSHOT_DEPTH);
        let binding = WidgetBinding::spawn_conditionally(
            global::handle(cx).handle(),
            service,
            cx,
            Self::apply_reliable,
            Self::apply_latest,
        );
        // The pane recovers from the market it waits for without polling: a venue commit is
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
            status,
            recovery,
            config_change,
            directory_generation: 0,
            snapshot: None,
            grouped: None,
            ready: None,
            metadata: None,
            price_step_labels: price_step_labels(None),
            unit_labels: OrderBookUnitLabels {
                price: SharedString::new_static("--"),
                quantity: SharedString::new_static("--"),
            },
            depth_animation: DepthAnimationState::settled(),
            previous_mid_price: None,
            paint_caches: Rc::new(OrderBookPaintCaches::new()),
            awaiting_market: false,
            market_reloaded: false,
        }
    }

    /// The stream scope this book would accept messages from.
    ///
    /// Absent until an instrument is chosen: nothing is streaming, so nothing is
    /// in scope.
    fn current_key(&self) -> Option<OrderBookStreamKey> {
        self.config.selection.as_ref().map(|selection| {
            OrderBookStreamKey::new(
                selection.listing.clone(),
                self.config.price_multiplier,
                self.config.window_size,
            )
        })
    }

    /// Whether what this book needs is in memory: at least one market that answers with depth,
    /// and the route of the stored selection among them.
    ///
    /// A book that follows nothing still needs a market: the picker is drawn from the loaded
    /// venues, so with none of them loaded there is nothing to choose from — the pane waits for
    /// the market participant's metadata and says so, rather than asking for a choice that is not there.
    /// Once one is loaded, a book that still follows nothing is the "select an instrument"
    /// notice's business and this answers `true` for it.
    ///
    /// Reading a market is only possible once the user has loaded it, and "not
    /// loaded" is a wait rather than a failure. One question answers it: a scope in
    /// the loaded venues is a scope whose catalog committed with it.
    fn market_ready(&self, cx: &App) -> bool {
        if !Self::any_market_loaded(cx) {
            return false;
        }

        let Some(route) = self
            .config
            .selection
            .as_ref()
            .map(|selection| &selection.listing.route)
        else {
            return true;
        };

        cx.global::<MarketParticipantsConnector>()
            .venues()
            .scope(route)
            .is_some()
    }

    /// Reports the notice the pane is standing in with: the market it waits for, or the
    /// instrument it has nothing chosen for.
    ///
    /// The list the sentence ends with is every venue that answers with depth — the
    /// capability this pane is defined by, read off the definition table rather than off
    /// what happens to be loaded — while the market it waits for is the one behind the
    /// selection, or that whole list while nothing is chosen and no single venue can be named.
    ///
    /// Which of the two references applies is what the wait itself says: the paths that drop the
    /// ladder for a market set the flag, and the one that starts reading again clears it, so a
    /// book that reached [`Self::start`] past its own gate is asking for an instrument rather than
    /// waiting for a venue.
    ///
    /// This is the one place that reference is produced — [`Self::wait_for_market`] and
    /// [`Self::start`] are the two that stand in one of them — and the sentence is the
    /// framework's to resolve, in the language showing on the frame that draws it.
    fn report_notice(&self, cx: &mut Context<Self>) {
        let notice = if self.awaiting_market {
            let supported = Self::listed_venues();
            let venues = self
                .config
                .selection
                .as_ref()
                .map(|selection| vec![selection.listing.route.venue])
                .unwrap_or_else(|| supported.clone());
            Notice::MarketNotLoaded { venues, supported }
        } else {
            Notice::message("widget.order_book.select_instrument_prompt")
        };
        self.status.report(WidgetState::waiting(notice), cx);
    }

    /// The venues the waiting notice lists: every venue whose loader can answer with depth.
    ///
    /// A book reads one venue — the one behind the instrument the user followed — so the
    /// market it waits for follows the selection. What it can read at all does not: that is
    /// this capability's venue set off the definition table, which is the same answer whether
    /// or not anything is loaded and whichever instrument is followed.
    fn listed_venues() -> Vec<VenueId> {
        market_participants_connector::venue::definition::venues_supporting(CapabilityId::OrderBook)
    }

    /// Whether any market this book could read is loaded, whatever the user selected.
    ///
    /// This is the first half of the pane's precondition, and it is asked before the selection is
    /// looked at on purpose: the picker a book with nothing followed draws is built from the
    /// loaded venues, so with none of them loaded there is nothing to choose from, and "nothing is
    /// chosen" and "the market is not loaded" are one and the same thing to the user.
    fn any_market_loaded(cx: &App) -> bool {
        let loaded = cx.global::<MarketParticipantsConnector>().venues();
        Self::listed_venues()
            .iter()
            .any(|venue| loaded.loaded(venue).is_some())
    }

    /// Reports the neutral notice for the market this book follows, and drops the
    /// ladder it was drawing.
    ///
    /// The route behind that ladder is gone, so nothing on screen is backed by a
    /// market any more, and the framework only draws the notice while the pane
    /// reports no content — which is why the pane also stops accepting messages: the
    /// stream it was reading is still alive, and a book that was already in flight
    /// would otherwise put content back over the notice. The stored selection is left
    /// alone: it is the user's, and loading the market again is what brings it back.
    fn wait_for_market(&mut self, cx: &mut Context<Self>) {
        self.awaiting_market = true;
        self.clear_book();
        self.report_notice(cx);
        cx.notify();
    }

    /// Reports the neutral notice for a read the loaded venues answered "not loaded"
    /// to, and drops the ladder it was drawing.
    ///
    /// Two answers reach this, and they are the same wait: a directory whose catalog is
    /// not in memory and a scope the loaded venues no longer hold — the market the
    /// user has not loaded, or one a shrinking commit took away between the pane's gate
    /// and the request that was already in flight. Neither is a failure: the
    /// notice is what the next load brings back, and a failed pane is terminal, so no
    /// later commit may revive it. Returns whether it reported, so a caller keeps its
    /// own failure path for everything else.
    fn report_waiting_for_directory(
        &mut self,
        error: &OrderBookStreamError,
        cx: &mut Context<Self>,
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

    /// Runs the load a venue commit may have unblocked.
    ///
    /// A commit can matter to this book for either of two reasons. The first is that it changes
    /// whether the market it needs is in memory, and the pane already holds that answer in the two
    /// things it maintains anyway: `awaiting_market` says it is not reading because a market it
    /// needs is missing, and `market_ready` says everything it needs is there now. A pane that is
    /// waiting for one and can now read starts over rather than re-checking; a pane that was
    /// reading one that is gone goes back to waiting.
    ///
    /// The second is that it landed on the venue behind the stored selection, which is the user
    /// loading or refreshing the market this book reads: the catalog generation the live stream
    /// resolved its rules, units and price steps against is replaced, so the book starts over — see
    /// `market_reloaded`, which is what makes the request the one that opens a new stream. A commit
    /// of any other venue is neither, and leaves the pane and its stream exactly as they are. A
    /// failure is terminal: the event does not revive a failed pane, only the user's own recovery
    /// does.
    fn on_venues_updated(&mut self, event: &VenuesUpdated, cx: &mut Context<Self>) {
        if self.status.is_failed(cx) {
            return;
        }
        // A commit on the venue the stored selection is on is this book's own market being
        // loaded again, so the book cannot keep reading the stream it holds.
        if self
            .config
            .selection
            .as_ref()
            .is_some_and(|selection| selection.listing.route.venue == event.venue)
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
        if !self.market_ready(cx) {
            self.wait_for_market(cx);
        }
    }

    /// Drops everything that was derived from the previous scope.
    ///
    /// The metadata goes with it because a new listing is rounded and labelled in
    /// its own units, so the header would otherwise name the previous listing's
    /// price step until the new stream says otherwise.
    fn clear_book(&mut self) {
        self.snapshot = None;
        self.grouped = None;
        self.metadata = None;
        self.price_step_labels = price_step_labels(None);
        self.unit_labels = OrderBookUnitLabels {
            price: SharedString::new_static("--"),
            quantity: SharedString::new_static("--"),
        };
        self.previous_mid_price = None;
        self.depth_animation = DepthAnimationState::settled();
        self.paint_caches.clear();
    }

    /// Starts the stream, or restarts it for a scope the user is retrying.
    ///
    /// The two booleans are two separate decisions. `retry` keeps the ladder the user is still
    /// reading — blanking it to reconnect would flash a screen of data away — while an explicit
    /// load starts over, because the widget cannot vouch for what it was showing.
    /// `market_reloaded` is a commit on the venue this book reads: the user loaded that market
    /// again, so the ladder still goes — it was grouped and labelled at the generation the commit
    /// replaced — while the request is the replacing one.
    fn start(&mut self, retry: bool, market_reloaded: bool, cx: &mut Context<Self>) {
        // The pane reached here only past its own gate, so the market it was waiting
        // for is in memory again and messages belong to it once more.
        self.awaiting_market = false;
        self.directory_generation = self.directory_generation.wrapping_add(1);
        let generation = self.directory_generation;
        // The row limits the book offers follow from how deep its venue publishes,
        // and that bound is known before the stream is opened: a stored choice the
        // route cannot fill goes back to the default here, so the request and the
        // header agree on one choice from the first frame.
        let depth = self.config.selection.as_ref().and_then(|selection| {
            cx.global::<MarketParticipantsConnector>()
                .market()
                .order_book_depth(&selection.listing.route)
                .ok()
        });
        if self.config.settle(depth) {
            log::warn!(
                "the book's stored grouping or row limit is not offered here; \
                 it was reset to the default"
            );
            self.config_change.publish(cx);
        }
        self.ready = None;
        let Some(selection) = self.config.selection.clone() else {
            // There is nothing to draw yet, so there is no market to read. The
            // widget has no content of its own, so it says what it is waiting for
            // instead of reporting work it is not doing.
            self.clear_book();
            self.report_notice(cx);
            cx.notify();
            return;
        };
        if !retry {
            self.clear_book();
        }
        self.status.report(WidgetState::Connecting, cx);
        // What the program still serves is asked before the book reads, but only
        // as advice: a listing that is gone goes back to "nothing chosen" instead of
        // being drawn and failing. The question is not the authority on whether the
        // book can be followed — the request below is, because the connector resolves
        // the listing's own rules and units inside it. So a lookup that failed is
        // only logged and the request goes out anyway: a catalog or network failure
        // comes back as the failure it is. See `Self::settle_selection`.
        let catalog = cx.global::<MarketParticipantsConnector>().catalog();
        let routes = LoadedScope::all(cx.global::<MarketParticipantsConnector>().venues());
        let support = listing_support(catalog, &routes, &selection.listing);
        cx.spawn(async move |this, cx| {
            let _ = this.update(cx, |this, cx| {
                if this.directory_generation != generation
                    || !this.settle_selection(&selection, support, cx)
                {
                    return;
                }
                let key = OrderBookStreamKey::new(
                    selection.listing.clone(),
                    this.config.price_multiplier,
                    this.config.window_size,
                );
                let request = if retry || market_reloaded {
                    OrderBookRequest::Retry(key)
                } else {
                    OrderBookRequest::Load(key)
                };
                this.binding.call(request, cx, Self::apply_rpc_result);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Drops a selection the program no longer serves.
    ///
    /// Returns whether this answer still describes the scope the book follows, which
    /// is what decides whether the request for it is made. The question is
    /// advisory: only an answer that *names* the listing as unsupported resets it.
    /// A question that failed or never answered proves nothing — the venue may
    /// simply be quiet — so the selection is kept and the request that follows
    /// reports its own error, which the user can recover from. A choice the user
    /// has made since is theirs, and it asked for its own request.
    fn settle_selection(
        &mut self,
        asked: &InstrumentSelection,
        support: ListingSupport,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.config.selection.as_ref() != Some(asked) {
            return false;
        }
        if support != ListingSupport::Unsupported {
            return true;
        }
        log::warn!(
            "`{}` is no longer published by {}; the book went back to having nothing chosen",
            asked.listing.listing_id.as_str(),
            asked.listing.route
        );
        self.config.selection = None;
        self.config_change.publish(cx);
        self.start(false, false, cx);
        false
    }

    fn apply_rpc_result(
        &mut self,
        result: WidgetRpcResult<OrderBookProtocol>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(OrderBookResponse::StreamStarted) => {}
            Err(error) => {
                let recovery = self.recovery.clone();
                self.status.report(
                    WidgetState::failed(Arc::new(OrderBookFailure::Rpc(error)), recovery),
                    cx,
                );
            }
        }
    }

    /// Applies a status the stream reported.
    ///
    /// Returns whether anything a frame draws changed; a status is drawn by the
    /// pane around the body, so publishing it is enough.
    fn apply_reliable(
        &mut self,
        key: OrderBookStreamKey,
        message: Arc<OrderBookStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx)
            || self.awaiting_market
        {
            return false;
        }
        let OrderBookStreamMessage::Status(status) = message.as_ref() else {
            return false;
        };
        match status {
            OrderBookStatus::Connecting => self.status.report(WidgetState::Connecting, cx),
            OrderBookStatus::LoadingSnapshot(metadata) => {
                if self.set_metadata(metadata.clone(), cx) {
                    cx.notify();
                }
                self.status.report(WidgetState::Loading, cx);
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
                    self.status.report(WidgetState::Ready, cx);
                } else {
                    self.status.report(WidgetState::Loading, cx);
                }
            }
            OrderBookStatus::Error(error) => {
                if self.report_waiting_for_directory(error, cx) {
                    return true;
                }
                let recovery = self.recovery.clone();
                self.status.report(
                    WidgetState::failed(
                        Arc::new(OrderBookFailure::Stream(error.clone())),
                        recovery,
                    ),
                    cx,
                );
            }
        }
        false
    }

    /// Applies a book the stream published.
    ///
    /// Returns whether the ladder changed, which is what the framework turns into
    /// a repaint: the body is cached, and a book that draws the same pixels is not
    /// worth a frame.
    fn apply_latest(
        &mut self,
        key: OrderBookStreamKey,
        message: Arc<OrderBookStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx)
            || self.awaiting_market
        {
            return false;
        }
        let OrderBookStreamMessage::Snapshot(snapshot) = message.as_ref() else {
            return false;
        };
        self.accept_snapshot(snapshot.clone(), cx)
    }

    /// Accepts a published book as the whole of what the widget draws.
    fn accept_snapshot(&mut self, snapshot: OrderBookSnapshot, cx: &mut Context<Self>) -> bool {
        // A book the stream has already overtaken would only make the ladder jump
        // backwards. Within one generation the venue's sequence only moves
        // forwards, so a book that is strictly behind the one on screen is one
        // that was overtaken by a status, which is not something to draw.
        if self
            .snapshot
            .as_ref()
            .is_some_and(|current| is_after(&current.sequence, &snapshot.sequence))
        {
            return false;
        }
        let metadata_changed = self.set_metadata(snapshot.metadata.clone(), cx);
        let sequence = snapshot.sequence.clone();
        self.snapshot = Some(snapshot);
        let regrouped = self.regroup(cx);
        // Readiness is reported at the protocol's own boundary: a complete book is
        // not the same thing as the book the status said was complete, and this is
        // where the two meet.
        if self
            .ready
            .as_ref()
            .is_some_and(|ready| !is_after(ready, &sequence))
        {
            self.status.report(WidgetState::Ready, cx);
        }
        regrouped || metadata_changed
    }

    /// Rebuilds the drawn ladder from the snapshot the widget holds.
    ///
    /// Returns whether anything a frame draws changed, which is also whether the
    /// body's cache has to be busted.
    fn regroup(&mut self, cx: &Context<Self>) -> bool {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return false;
        };
        let previous_mid_price = self.grouped.as_ref().and_then(|book| book.mid_price);
        let grouped = GroupedBook::build(
            snapshot,
            self.grouped.as_deref(),
            self.depth_animation.progress,
            self.config.price_multiplier,
            self.effective_window_size(),
        );
        if self
            .grouped
            .as_ref()
            .is_some_and(|current| current.has_same_current_state(&grouped))
        {
            return false;
        }
        self.previous_mid_price = previous_mid_price;
        self.grouped = Some(Arc::new(grouped));
        self.restart_animation(cx);
        true
    }

    /// Sends the bars after the ratios the ladder now asks for.
    ///
    /// Every restart is measured from where the previous frame had painted, so an
    /// update that arrives mid-flight retargets the movement instead of resetting
    /// it. Decorative movement is what the platform's reduce-motion setting asks
    /// not to have, so with it set the bars simply arrive.
    fn restart_animation(&mut self, cx: &Context<Self>) {
        if cx.reduce_motion() {
            self.depth_animation = DepthAnimationState::settled();
            return;
        }
        let span = self
            .grouped
            .as_ref()
            .map_or(0.0, |book| book.animation_span());
        self.depth_animation.restart(span);
    }

    /// How many rows the ladder may keep: the user's limit, narrowed to what the
    /// pane has room to draw.
    fn effective_window_size(&self) -> usize {
        self.config
            .window_size
            .min(self.paint_caches.drawable_rows().max(1))
    }

    fn set_metadata(&mut self, metadata: OrderBookMetadata, cx: &App) -> bool {
        if self.metadata.as_ref() == Some(&metadata) {
            return false;
        }
        self.price_step_labels = price_step_labels(Some(metadata.precision.price_tick));
        self.unit_labels = OrderBookUnitLabels::of(&metadata, cx);
        self.metadata = Some(metadata);
        true
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
    fn price_step_label(&self, multiplier: u32) -> SharedString {
        PRICE_MULTIPLIERS
            .iter()
            .position(|candidate| *candidate == multiplier)
            .map_or_else(
                || price_step_label(self.display_price_tick(), multiplier),
                |index| self.price_step_labels[index].clone(),
            )
    }

    /// Applies an instrument picked in the header as one scope transition.
    ///
    /// Returns whether the selection was accepted. A selection names the exact
    /// listing, so there is nothing to check about which market it belongs to: the
    /// key is the listing's own.
    fn select_selection(&mut self, selection: InstrumentSelection, cx: &mut Context<Self>) -> bool {
        // Picking the scope that is already shown is a request to open it again
        // when it is not working, and nothing at all otherwise.
        if self.config.selection.as_ref() == Some(&selection) && !self.status.is_failed(cx) {
            return false;
        }
        self.config.selection = Some(selection);
        self.config_change.publish(cx);
        self.start(false, false, cx);
        true
    }

    fn select_price_multiplier(&mut self, multiplier: u32, cx: &mut Context<Self>) {
        if self.config.price_multiplier == multiplier || !PRICE_MULTIPLIERS.contains(&multiplier) {
            return;
        }
        self.config.price_multiplier = multiplier;
        self.config_change.publish(cx);
        self.restream(cx);
        if self.regroup(cx) {
            cx.notify();
        }
    }

    fn select_window_size(&mut self, window_size: usize, cx: &mut Context<Self>) {
        if self.config.window_size == window_size
            || !window_sizes(self.book_depth()).contains(&window_size)
        {
            return;
        }
        self.config.window_size = window_size;
        self.config_change.publish(cx);
        self.restream(cx);
        if self.regroup(cx) {
            cx.notify();
        }
    }

    /// Starts the generation the drawing choices now describe.
    ///
    /// Both choices are part of the stream's own scope, so the generation that was
    /// publishing for the previous one is replaced. Nothing on screen is dropped:
    /// the ladder the user is reading stays up until the new generation publishes
    /// the book it holds.
    fn restream(&mut self, cx: &mut Context<Self>) {
        // A failed pane is waiting for the user, not for a reload, so the stream it
        // would replace is left alone: the recovery control is what starts the next
        // generation.
        if self.status.is_failed(cx) {
            return;
        }
        let Some(key) = self.current_key() else {
            return;
        };
        self.binding
            .call(OrderBookRequest::Load(key), cx, Self::apply_rpc_result);
    }

    /// The control that opens the instrument picker.
    ///
    /// Before anything is chosen it offers the choice rather than reporting a
    /// scope, because there is no scope yet. What it states once a listing is
    /// followed — the published name and the route it is on — is the control's own
    /// text, so the ladder and the dialog cannot disagree about which listing is
    /// followed.
    fn instrument_button(&self, cx: &Context<Self>) -> AnyElement {
        let widget = cx.entity().downgrade();

        InstrumentButton::new(
            ElementId::Name(SharedString::new_static("OrderBookInstrument")),
            self.config.selection.clone(),
            InstrumentFilters {
                kind_filter: InstrumentKindFilter::All,
                status_filter: InstrumentStatusFilter::Any,
                required_capability: CapabilityId::OrderBook,
                entry_predicate: None,
                routes: LoadedScope::all(cx.global::<MarketParticipantsConnector>().venues()),
            },
            "widget.order_book.select_instrument",
            move |selection, _, app| {
                // The selector does not keep the pane alive: a pane gone by the
                // time an instrument is confirmed has nothing left to select into.
                let _ = widget.update(app, |this, cx| {
                    this.select_selection(selection, cx);
                });
            },
        )
        .into_any_element()
    }

    /// The two drawing choices, in the header's right group.
    ///
    /// Both are disabled until a book has been published, because neither can
    /// change anything before there is a ladder to regroup — but they keep showing
    /// what the user chose, so a book that is still connecting does not silently
    /// read as the first option of each list. A pane waiting for its market has no book to
    /// regroup either, and that is said here rather than left to the dropped ladder alone.
    fn drawing_controls(&self, cx: &Context<Self>) -> AnyElement {
        let disabled = self.awaiting_market || self.snapshot.is_none();

        div()
            .occlude()
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .child(self.price_step_select(cx, disabled))
            .child(self.window_size_select(cx, disabled))
            .into_any_element()
    }

    /// The price-grouping picker.
    ///
    /// A choice is identified by the multiplier it applies, written the way the
    /// book writes prices, so the trigger label and the menu entries are the same
    /// text the ladder uses.
    fn price_step_select(&self, cx: &Context<Self>, disabled: bool) -> Select {
        let selected =
            SharedString::new(format_smolstr!("{}", self.config.price_multiplier).as_str());
        let select = PRICE_MULTIPLIERS.iter().copied().fold(
            Select::new(ElementId::Name(SharedString::new_static(
                "OrderBookPriceStep",
            )))
            .placeholder(self.price_step_label(self.config.price_multiplier))
            .ghost()
            .small()
            .selected_item(Some(selected))
            .disabled(disabled)
            .aria_label(t!(cx, "widget.order_book.price_grouping")),
            |select, multiplier| {
                select.menu_item(MenuItem::Action(MenuItemAction::new(
                    SharedString::new(format_smolstr!("{multiplier}").as_str()),
                    self.price_step_label(multiplier),
                )))
            },
        );

        select.on_change({
            let widget = cx.entity();
            move |multiplier, _, app| {
                let Some(multiplier) = multiplier
                    .as_deref()
                    .and_then(|value| value.parse::<u32>().ok())
                else {
                    return;
                };
                widget.update(app, |this, cx| {
                    this.select_price_multiplier(multiplier, cx);
                });
            }
        })
    }

    /// The depth the book being drawn was published at.
    ///
    /// The metadata arrives before the first snapshot, so a pane with a book knows
    /// how deep its venue publishes; a pane that follows nothing, or whose stream
    /// has not answered yet, has no bound to offer its row limits from.
    fn book_depth(&self) -> Option<u16> {
        self.metadata.as_ref().map(|metadata| metadata.depth)
    }

    /// The row-limit picker.
    fn window_size_select(&self, cx: &Context<Self>, disabled: bool) -> Select {
        let selected = SharedString::new(format_smolstr!("{}", self.config.window_size).as_str());
        let select = window_sizes(self.book_depth()).iter().copied().fold(
            Select::new(ElementId::Name(SharedString::new_static(
                "OrderBookWindowSize",
            )))
            .placeholder(window_size_label(self.config.window_size, cx))
            .ghost()
            .small()
            .selected_item(Some(selected))
            .disabled(disabled)
            .aria_label(t!(cx, "widget.order_book.visible_rows")),
            |select, rows| {
                select.menu_item(MenuItem::Action(MenuItemAction::new(
                    SharedString::new(format_smolstr!("{rows}").as_str()),
                    window_size_label(rows, cx),
                )))
            },
        );

        select.on_change({
            let widget = cx.entity();
            move |rows, _, app| {
                let Some(rows) = rows
                    .as_deref()
                    .and_then(|value| value.parse::<usize>().ok())
                else {
                    return;
                };
                widget.update(app, |this, cx| {
                    this.select_window_size(rows, cx);
                });
            }
        })
    }

    /// The reaction to a pane whose height changed the number of rows it fits.
    ///
    /// A canvas learns its box during prepaint, which runs while this widget may
    /// still be on the stack, so the regroup waits for the effect cycle to end
    /// rather than updating an entity that is already being read.
    fn on_layout_change(&self, cx: &Context<Self>) -> impl FnOnce(&mut App) + 'static {
        let widget = cx.entity().downgrade();
        move |cx: &mut App| {
            cx.defer(move |cx| {
                let _ = widget.update(cx, |widget, cx| {
                    if widget.regroup(cx) {
                        cx.notify();
                    }
                });
            });
        }
    }
}

/// How much of a venue's book a pane is worth copying. A book deeper than this is
/// never drawn: the ladder shows a few dozen rows of it.
const SNAPSHOT_DEPTH: u16 = 5_000;

/// The label of one row limit.
fn window_size_label(rows: usize, cx: &App) -> SharedString {
    t!(cx, "widget.order_book.rows", count = rows)
}

impl Render for OrderBookWidget {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The body is only embedded while the widget has content, so this is the
        // ready state and nothing else. Everything the book is *doing* is drawn by
        // the pane around this.
        let animation_progress = self.depth_animation.advance(Instant::now());
        if self.depth_animation.is_active() {
            // The body is cached, and a cached subtree is only repainted when its
            // own view says so. An easing bar is exactly that case, so it asks for
            // the next frame here — this notifies the view it is called from.
            window.request_animation_frame();
        }

        // A ladder is only ever drawn from a published book, and a book only ever
        // comes from a scope the user picked, so both are present here — but the
        // frame is not worth building for either one on its own.
        let (Some(book), Some(_)) = (self.grouped.clone(), self.config.selection.as_ref()) else {
            return div().size_full().min_h_0().into_any_element();
        };
        let theme = cx.theme();
        let price_unit = self.unit_labels.price.clone();
        let quantity_unit = self.unit_labels.quantity.clone();
        let quote = book
            .mid_price_text
            .clone()
            .unwrap_or_else(|| SharedString::new_static("--"));
        let spread = book.spread_text.as_ref().map_or_else(
            || SharedString::new_static("--"),
            |spread| t!(cx, "widget.order_book.spread", value = spread),
        );
        let (mid_price_color, mid_price_direction) =
            mid_price_direction(book.mid_price, self.previous_mid_price, theme);
        let ask_ratio = interpolated_ratio(
            book.previous_imbalance,
            book.current_imbalance,
            animation_progress,
        );

        let column_labels = div()
            .h(HEADER_HEIGHT)
            .flex_none()
            .flex()
            .items_center()
            .px_2()
            .border_b_1()
            .border_color(theme.roles.line.divider.hsla)
            .text_xs()
            .text_color(theme.roles.fg.muted.hsla)
            .child(column_label(t!(
                cx,
                "widget.order_book.price_with_unit",
                unit = price_unit
            )))
            .child(column_label(t!(
                cx,
                "widget.order_book.amount_with_unit",
                unit = quantity_unit.clone()
            )))
            .child(column_label(t!(
                cx,
                "widget.order_book.total_with_unit",
                unit = quantity_unit
            )));
        let asks = SideCanvas {
            levels: book.asks.clone(),
            price_color: theme.roles.market.down.hsla,
            quantity_color: theme.roles.fg.default.hsla,
            depth_color: theme.roles.market.down_weak.hsla,
            animation_progress,
            asks: true,
        }
        .render(self.paint_caches.clone(), self.on_layout_change(cx));
        let bids = SideCanvas {
            levels: book.bids.clone(),
            price_color: theme.roles.market.up.hsla,
            quantity_color: theme.roles.fg.default.hsla,
            depth_color: theme.roles.market.up_weak.hsla,
            animation_progress,
            asks: false,
        }
        .render(self.paint_caches.clone(), self.on_layout_change(cx));
        let mid_price = div()
            .h(MID_PRICE_HEIGHT)
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            .overflow_hidden()
            .px_2()
            .border_y_1()
            .border_color(theme.roles.line.divider.hsla)
            .children(
                mid_price_direction
                    .map(|kind| Icon::new(kind).size_4().text_color(mid_price_color)),
            )
            .child(
                div()
                    .text_2xl()
                    .font_weight(FontWeight::BOLD)
                    .text_color(mid_price_color)
                    .child(quote),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.roles.fg.muted.hsla)
                    .child(spread),
            );
        let imbalance = div()
            .relative()
            .h(IMBALANCE_HEIGHT)
            .flex_none()
            .overflow_hidden()
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(relative(ask_ratio))
                    .bg(theme.roles.market.down_weak.hsla),
            )
            .child(
                div()
                    .absolute()
                    .right_0()
                    .top_0()
                    .bottom_0()
                    .w(relative(1.0 - ask_ratio))
                    .bg(theme.roles.market.up_weak.hsla),
            )
            .child(
                div()
                    .relative()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .text_sm()
                    .font_weight(FontWeight::BOLD)
                    .child(
                        div()
                            .text_color(theme.roles.market.down.hsla)
                            .child(book.ask_imbalance_text.clone()),
                    )
                    .child(
                        div()
                            .text_color(theme.roles.market.up.hsla)
                            .child(book.bid_imbalance_text.clone()),
                    ),
            );

        // A book with no level on either side draws an empty ladder between its
        // labels, which leaves it and a book that failed to load looking the same.
        // The book is still the pane's content — the venue published it — so the
        // pane says what it holds rather than letting the framework's placeholder
        // speak for it.
        let empty = (book.asks.is_empty() && book.bids.is_empty()).then(|| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .child(EmptyState::new("OrderBookEmpty", t!(cx, "common.no_data")))
        });

        div()
            .id(ElementId::Name(SharedString::new_static("OrderBookLadder")))
            .debug_selector(|| "ORDER_BOOK_LADDER".into())
            .aria_label(t!(cx, "widget.order_book.book"))
            .relative()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .child(column_labels)
            .child(div().min_h_0().flex_1().overflow_hidden().child(asks))
            .child(mid_price)
            .child(div().min_h_0().flex_1().overflow_hidden().child(bids))
            .child(imbalance)
            .children(empty)
            .into_any_element()
    }
}

/// One column label of the ladder, right-aligned like the numbers under it.
fn column_label(label: SharedString) -> impl IntoElement {
    div()
        .min_w_0()
        .flex_1()
        .truncate()
        .text_right()
        .child(label)
}

/// The arrow and the colour of the mid price, which follow the direction it moved
/// in since the frame before.
fn mid_price_direction(
    current: Option<Price>,
    previous: Option<Price>,
    theme: &Theme,
) -> (Hsla, Option<IconKind>) {
    match (current, previous) {
        (Some(current), Some(previous)) if current > previous => {
            (theme.roles.market.up.hsla, Some(IconKind::ArrowUp))
        }
        (Some(current), Some(previous)) if current < previous => {
            (theme.roles.market.down.hsla, Some(IconKind::ArrowDown))
        }
        _ => (theme.roles.market.flat.hsla, None),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        str::FromStr,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use app_config::widget::VersionedWidgetConfig;
    use base_ui::{animation::ANIMATION_TICK, layer::Layer};
    use gpui_kit::{
        AppContext, Entity, Modifiers, Render, TestAppContext, VisualTestContext, WeakEntity,
        Window, div, point, px, size,
    };
    use i18n::language::codes::LanguageCode;
    use market_participants_connector::{
        catalog::batch::CatalogEntry,
        connector::{MarketParticipantsConnector, MarketParticipantsConnectorInput},
        error::MarketParticipantError,
        market::{
            domain::DisconnectReason,
            error::{MarketError, MarketNetworkError},
        },
        runtime::MarketParticipantNetworkSnapshot,
    };
    use market_participants_model::{
        decimal::{Quantity, Tick},
        listing::{
            listing_impl::ListingStatus, route::VenueListingKey, rules::ListingPrecision,
            venue::PriceBasis,
        },
        market::{
            book::{ListingOrderBook, ListingOrderBookLevel},
            price::MarketPrice,
            units::{MarketQuantity, QuantityUnit},
        },
    };
    use network::transport::hub::{TransportHub, TransportPolicy};

    use super::*;
    use crate::{
        model::{MAX_LEVELS_PER_SIDE, SHALLOW_WINDOW_SIZES},
        service::{OrderBookEventSubscription, OrderBookSource, OrderBookStreamError},
    };

    /// A source whose subscription never resolves, so a test drives every state
    /// transition itself.
    #[derive(Clone, Copy)]
    struct PendingSource;

    struct PendingSubscription;

    impl OrderBookSource for PendingSource {
        type Subscription = PendingSubscription;

        fn subscribe_order_book(
            &self,
            _listing: VenueListingKey,
            _depth: u16,
        ) -> impl std::future::Future<Output = Result<Self::Subscription, MarketError>> + Send
        {
            std::future::pending()
        }
    }

    /// A source that records that a book was asked for.
    ///
    /// The request the book sends crosses the service, so the seam is the only place a
    /// test can see that the precheck let it through.
    #[derive(Clone, Default)]
    struct AnsweringOrderBookSource {
        subscriptions: Arc<std::sync::Mutex<usize>>,
    }

    impl AnsweringOrderBookSource {
        fn subscriptions(&self) -> usize {
            *self
                .subscriptions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    impl OrderBookSource for AnsweringOrderBookSource {
        type Subscription = PendingSubscription;

        async fn subscribe_order_book(
            &self,
            _listing: VenueListingKey,
            _depth: u16,
        ) -> Result<Self::Subscription, MarketError> {
            *self
                .subscriptions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
            std::future::pending().await
        }
    }

    impl OrderBookEventSubscription for PendingSubscription {
        fn metadata(&self) -> OrderBookMetadata {
            metadata()
        }

        fn synchronize(
            &mut self,
        ) -> impl std::future::Future<Output = Result<(), OrderBookStreamError>> + Send {
            std::future::pending()
        }

        fn recv(
            &mut self,
        ) -> impl std::future::Future<Output = Option<Result<(), OrderBookStreamError>>> + Send
        {
            std::future::pending()
        }

        fn snapshot(&self) -> Option<OrderBookSnapshot> {
            None
        }
    }

    /// A source that resolves a subscription through the catalogue the fixture publishes, at the
    /// moment it is asked for.
    ///
    /// A stream's resolution is what a venue reload replaces, and for a book that resolution *is*
    /// the metadata the ladder is rounded and labelled with: reading it out of the catalogue here
    /// is what lets a test see which generation the stream it opened was resolved at.
    #[derive(Clone)]
    struct ResolvingOrderBookSource {
        catalog: Arc<Catalog>,
        subscriptions: Arc<std::sync::Mutex<usize>>,
    }

    struct ResolvingSubscription {
        metadata: OrderBookMetadata,
    }

    impl OrderBookEventSubscription for ResolvingSubscription {
        fn metadata(&self) -> OrderBookMetadata {
            self.metadata.clone()
        }

        fn synchronize(
            &mut self,
        ) -> impl std::future::Future<Output = Result<(), OrderBookStreamError>> + Send {
            std::future::pending()
        }

        fn recv(
            &mut self,
        ) -> impl std::future::Future<Output = Option<Result<(), OrderBookStreamError>>> + Send
        {
            std::future::pending()
        }

        fn snapshot(&self) -> Option<OrderBookSnapshot> {
            None
        }
    }

    impl OrderBookSource for ResolvingOrderBookSource {
        type Subscription = ResolvingSubscription;

        async fn subscribe_order_book(
            &self,
            listing: VenueListingKey,
            _depth: u16,
        ) -> Result<Self::Subscription, MarketError> {
            *self
                .subscriptions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
            let published = self
                .catalog
                .cached_listing(&listing)
                .expect("the fixture publishes the listing the book follows");
            Ok(ResolvingSubscription {
                metadata: OrderBookMetadata::of(&published.snapshot.rules, FIXTURE_DEPTH),
            })
        }
    }

    /// What the fixture venue publishes for the pane to ask about.
    enum Published {
        /// The fixture route lists these base assets.
        Listings(Vec<&'static str>),
        /// The fixture route lists one listing whose status is terminal.
        Terminal(&'static str),
        /// The fixture route always fails.
        Failing,
    }

    fn tick(value: &str) -> Tick {
        Tick::from_str(value).expect("tick should parse")
    }

    /// The fixture listing key of one base asset.
    fn listing(base: &str) -> VenueListingKey {
        market_participants_connector::catalog::test_support::listing_key(
            &market_participants_connector::catalog::test_support::spot_route(),
            &symbol(base),
        )
    }

    fn symbol(base: &str) -> String {
        format!("{base}USDT")
    }

    fn entry(base: &str) -> CatalogEntry {
        market_participants_connector::catalog::test_support::entry(
            &market_participants_connector::catalog::test_support::spot_route(),
            market_participants_connector::catalog::test_support::spot_token(
                "binance", base, "USDT",
            ),
            &symbol(base),
        )
    }

    /// The fixture route's BTC listing again, published at another price tick.
    ///
    /// It is the shape a production load commits: the load re-fetches the catalogue, and what it
    /// publishes is the listing restated at the venue's own increments.
    fn restated_entry(price_tick: &str) -> CatalogEntry {
        let mut restated = entry("BTC");
        restated.snapshot.rules.common.price.tick = tick(price_tick);
        restated
    }

    /// One scope a test can follow.
    fn selection(base: &str) -> InstrumentSelection {
        InstrumentSelection {
            listing: listing(base),
        }
    }

    /// A config that already follows the fixture BTC/USDT listing.
    fn config() -> OrderBookConfig {
        OrderBookConfig {
            selection: Some(selection("BTC")),
            ..OrderBookConfig::default()
        }
    }

    /// The depth the fixture route's venue publishes per side, which is the bound
    /// the pane offers its row limits from.
    const FIXTURE_DEPTH: u16 = 5_000;

    fn metadata() -> OrderBookMetadata {
        OrderBookMetadata {
            precision: ListingPrecision {
                price_tick: tick("0.01"),
                quantity_tick: tick("0.001"),
            },
            quote_asset: market_participants_connector::catalog::test_support::token(
                "binance", "USDT",
            ),
            quantity_unit: QuantityUnit::AssetUnits {
                asset: market_participants_connector::catalog::test_support::token(
                    "binance", "BTC",
                ),
            },
            depth: FIXTURE_DEPTH,
        }
    }

    /// One side's levels, written the way a venue prints them.
    fn levels(levels: &[(&str, u64)]) -> Vec<ListingOrderBookLevel> {
        levels
            .iter()
            .map(|(price, quantity)| ListingOrderBookLevel {
                price: MarketPrice {
                    value: Price::from_str(price).expect("test price should parse"),
                    quote_asset: market_participants_connector::catalog::test_support::token(
                        "binance", "USDT",
                    ),
                    basis: PriceBasis::PerAssetUnit,
                },
                quantity: MarketQuantity {
                    value: Quantity::from(*quantity),
                    unit: QuantityUnit::AssetUnits {
                        asset: market_participants_connector::catalog::test_support::token(
                            "binance", "BTC",
                        ),
                    },
                },
            })
            .collect()
    }

    fn from_levels(
        sequence: u64,
        asks: Vec<ListingOrderBookLevel>,
        bids: Vec<ListingOrderBookLevel>,
    ) -> OrderBookSnapshot {
        let book = ListingOrderBook {
            listing: listing("BTC"),
            sequence: MarketSequence::Numeric(sequence),
            timestamp: None,
            bids,
            asks,
        };
        OrderBookSnapshot::from_market_participant(&book, metadata(), MAX_LEVELS_PER_SIDE)
    }

    fn snapshot(sequence: u64, asks: &[(&str, u64)], bids: &[(&str, u64)]) -> OrderBookSnapshot {
        from_levels(sequence, levels(asks), levels(bids))
    }

    /// A book that differs from `snapshot(1, …)` in one price.
    fn moved_snapshot(sequence: u64) -> OrderBookSnapshot {
        snapshot(
            sequence,
            &[("102", 1), ("103", 2)],
            &[("100", 1), ("99", 2)],
        )
    }

    fn disconnect() -> OrderBookStreamError {
        MarketError::Disconnected(DisconnectReason::Normal).into()
    }

    /// Installs the market participants root the header's picker and the pane's own
    /// capability question read.
    ///
    /// The root is wired to `catalog`, which is the directory's own catalogue: the
    /// pane's gate asks the directory and the service behind it resolves listings
    /// against whatever this root was wired to, so two different catalogues would let a
    /// mismatch between them pass unnoticed.
    fn install_connector_with(
        cx: &mut App,
        catalog: &Arc<market_participants_connector::catalog::store::Catalog>,
        loaded: bool,
    ) {
        let snapshot = Arc::new(MarketParticipantNetworkSnapshot::default());
        let (_, receiver) = tokio::sync::watch::channel(snapshot);
        let connector = MarketParticipantsConnector::new_with_catalog(
            MarketParticipantsConnectorInput::new(
                Arc::new(TransportHub::new(TransportPolicy::default())),
                receiver,
            )
            .with_runtime(global::handle(cx)),
            Arc::clone(catalog),
        )
        .expect("the market participants root should build from typed runtime inputs");
        // The pane follows the venue commits this root announces, so the fixture attaches the
        // boundary the application's own install path attaches.
        connector.attach_events(cx);
        if loaded {
            connector
                .load_fixture(
                    &market_participants_connector::test_support::definitions(
                        &connector.supported_venues(),
                    ),
                    &[],
                    &[],
                )
                .expect("the venue's routes load");
        }
        cx.set_global(connector);
    }

    /// Creates a widget the way the framework does: status first, then one load.
    ///
    /// The fixture route the pane asks about publishes the listing the default config
    /// follows, so the pane really is reading: the market has to be in memory before it
    /// starts anything, and a test about what the pane does while it is reading has to
    /// hand it one. A test about the waiting state builds its own fixture instead.
    fn widget_with(cx: &mut TestAppContext, config: OrderBookConfig) -> Entity<OrderBookWidget> {
        widget_over(cx, config, Published::Listings(vec!["BTC"]))
    }

    /// The same, over a fixture route that publishes `published`.
    fn widget_over(
        cx: &mut TestAppContext,
        config: OrderBookConfig,
        published: Published,
    ) -> Entity<OrderBookWidget> {
        widget_over_source(cx, config, PendingSource, published)
    }

    /// The same, over a source a test drives: the requests the book sends are only
    /// observable through the seam it sends them to.
    fn widget_over_source<S>(
        cx: &mut TestAppContext,
        config: OrderBookConfig,
        source: S,
        published: Published,
    ) -> Entity<OrderBookWidget>
    where
        S: OrderBookSource,
    {
        widget_over_source_reporting(cx, config, source, published).0
    }

    /// The same, handing back the report the pane publishes its config through.
    fn widget_over_source_reporting<S>(
        cx: &mut TestAppContext,
        config: OrderBookConfig,
        source: S,
        published: Published,
    ) -> (Entity<OrderBookWidget>, std::rc::Rc<Cell<usize>>)
    where
        S: OrderBookSource,
    {
        widget_over_market(cx, config, source, published, true)
    }

    /// The same, with the fixture venue loaded or left out of the loaded venues.
    fn widget_over_market<S>(
        cx: &mut TestAppContext,
        config: OrderBookConfig,
        source: S,
        published: Published,
        loaded: bool,
    ) -> (Entity<OrderBookWidget>, std::rc::Rc<Cell<usize>>)
    where
        S: OrderBookSource,
    {
        widget_over_market_with(cx, config, published, loaded, move |_| source)
    }

    /// The same, for a source built from the catalogue the fixture installs.
    ///
    /// A stream's own resolution is what a venue reload invalidates, so a test that asserts the
    /// book reopened its stream needs a source that reads the generation in memory at the moment
    /// it is asked: the catalogue the fixture publishes is handed to `build` rather than copied.
    fn widget_over_market_with<S>(
        cx: &mut TestAppContext,
        config: OrderBookConfig,
        published: Published,
        loaded: bool,
        build: impl FnOnce(&Arc<Catalog>) -> S,
    ) -> (Entity<OrderBookWidget>, std::rc::Rc<Cell<usize>>)
    where
        S: OrderBookSource,
    {
        cx.executor().allow_parking();
        let reports = std::rc::Rc::new(Cell::new(0));
        // The market is installed and its commit flushed before the book is built, which is the
        // order production has: the user loads the market, and a book opened afterwards reads the
        // generation that load published rather than a commit about a market it was not reading.
        let source = cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            global::install(cx).expect("test Tokio runtime should build");
            let runtime = global::handle(cx);
            let _runtime = runtime.enter();
            // The fixture the entries are published through is built from the test's own
            // `published`; the market participants root is then wired to the very catalogue it holds
            // rather than to a second, empty one, so the pane's gate and its reads look at
            // one directory. The publication itself has to follow the registration: a
            // snapshot records the route revision it was fetched under, and registering the
            // route moves that revision on — a catalogue published before it would have
            // nothing to serve.
            let entries = match &published {
                Published::Listings(bases) => {
                    Some(bases.iter().map(|base| entry(base)).collect::<Vec<_>>())
                }
                Published::Terminal(base) => Some(vec![
                    market_participants_connector::catalog::test_support::with_status(
                        entry(base),
                        ListingStatus::Delisted,
                    ),
                ]),
                Published::Failing => None,
            };
            let fixture = match &entries {
                Some(entries) => {
                    market_participants_connector::catalog::test_support::Fixture::new(
                        entries.clone(),
                    )
                }
                None => market_participants_connector::catalog::test_support::Fixture::failing(),
            };
            let catalog = Arc::clone(fixture.catalog_arc());
            install_connector_with(cx, &catalog, loaded);
            if let Some(entries) = &entries {
                market_participants_connector::catalog::test_support::publish_fixture_route(
                    &catalog,
                    &market_participants_connector::catalog::test_support::spot_route(),
                    entries.clone(),
                )
                .expect("the fixture route should publish its catalog");
            }
            build(&catalog)
        });
        cx.run_until_parked();
        let widget = cx.update(|cx| {
            let change = WidgetConfigChange::new({
                let reports = reports.clone();
                move |_| reports.set(reports.get() + 1)
            });
            let widget = cx.new(|cx| {
                let status = cx.new(|_| WidgetStatus::new());
                let weak: WeakEntity<OrderBookWidget> = cx.weak_entity();
                let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
                    let _ = weak.update(cx, |widget, cx| widget.load(cx));
                });

                OrderBookWidget::new_with_source(config, source, status, change, recovery, cx)
            });
            widget.update(cx, |widget, cx| widget.load(cx));
            widget
        });
        (widget, reports)
    }

    fn widget(cx: &mut TestAppContext) -> Entity<OrderBookWidget> {
        widget_with(cx, config())
    }

    /// The same, over a route whose catalog the directory published.
    ///
    /// The market a pane reads has to be in memory before it starts anything, so a
    /// test about what the pane does once it is reading builds its fixture over a
    /// published route.
    fn loaded_widget(cx: &mut TestAppContext) -> Entity<OrderBookWidget> {
        widget_over(cx, config(), Published::Listings(vec!["BTC"]))
    }

    /// The same, over a route whose catalog is published but whose venue is not
    /// loaded.
    fn widget_over_unloaded(
        cx: &mut TestAppContext,
        config: OrderBookConfig,
        published: Published,
    ) -> Entity<OrderBookWidget> {
        widget_over_source_reporting_unloaded(cx, config, PendingSource, published).0
    }

    /// The same, with the venue left out of the registry.
    fn widget_over_source_reporting_unloaded<S>(
        cx: &mut TestAppContext,
        config: OrderBookConfig,
        source: S,
        published: Published,
    ) -> (Entity<OrderBookWidget>, std::rc::Rc<Cell<usize>>)
    where
        S: OrderBookSource,
    {
        widget_over_market(cx, config, source, published, false)
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

    /// Lets the widget and the application runtime take a turn each until `ready`.
    fn settle_until(cx: &mut TestAppContext, what: &str, ready: impl Fn(&App) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let handle = cx.update(|cx| global::handle(cx));
            handle
                .handle()
                .block_on(async { tokio::task::yield_now().await });
            if cx.update(|cx| ready(cx)) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{what} should happen");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Reports a book as the stream would.
    ///
    /// Cancelling the binding is what makes the hand-driven book the only input a
    /// test has to reason about, and it also fails the load the binding was still
    /// holding: the cancelled runtime answers that call whenever it gets to it.
    fn accept_book(
        widget: &Entity<OrderBookWidget>,
        snapshot: OrderBookSnapshot,
        cx: &mut App,
    ) -> bool {
        widget.update(cx, |widget, context| {
            widget.binding.cancel();
            widget.apply_latest(
                widget
                    .current_key()
                    .expect("the test widget follows a listing"),
                Arc::new(OrderBookStreamMessage::Snapshot(snapshot)),
                context,
            )
        })
    }

    fn report_status(
        widget: &Entity<OrderBookWidget>,
        status: OrderBookStatus,
        cx: &mut App,
    ) -> bool {
        widget.update(cx, |widget, context| {
            widget.apply_reliable(
                widget
                    .current_key()
                    .expect("the test widget follows a listing"),
                Arc::new(OrderBookStreamMessage::Status(status)),
                context,
            )
        })
    }

    fn report_failure(widget: &Entity<OrderBookWidget>, cx: &mut App) {
        widget.update(cx, |widget, context| {
            let recovery = widget.recovery.clone();
            widget.status.report(
                WidgetState::failed(Arc::new(OrderBookFailure::Stream(disconnect())), recovery),
                context,
            );
        });
    }

    fn status(widget: &Entity<OrderBookWidget>, cx: &App) -> WidgetState {
        let status = widget.read(cx).status.clone();
        status.state(cx)
    }

    fn ladder_rows(widget: &Entity<OrderBookWidget>, cx: &App) -> (usize, usize) {
        let book = widget.read(cx);
        let grouped = book.grouped.as_ref().expect("the ladder should be grouped");
        (grouped.asks.len(), grouped.bids.len())
    }

    #[gpui_kit::test]
    fn a_widget_without_a_book_should_report_connecting_and_have_no_content(
        cx: &mut TestAppContext,
    ) {
        let widget = loaded_widget(cx);

        cx.update(|cx| {
            assert!(!widget.read(cx).has_content());
            assert_eq!(status(&widget, cx), WidgetState::Connecting);
        });
    }

    /// A market the user has not loaded leaves the pane waiting for it, with the
    /// choice they stored left alone (⑮).
    #[gpui_kit::test]
    fn an_unloaded_market_should_leave_the_pane_waiting(cx: &mut TestAppContext) {
        let widget = widget_over_unloaded(cx, config(), Published::Listings(vec!["BTC"]));

        cx.update(|cx| {
            assert!(!widget.read(cx).has_content());
            assert_eq!(widget.read(cx).config.selection, Some(selection("BTC")));
            let status = status(&widget, cx);
            assert!(
                status.failure().is_none(),
                "an unloaded market is not a failure"
            );
            assert_eq!(
                status,
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: vec![listing("BTC").route.venue],
                    supported: OrderBookWidget::listed_venues(),
                }),
                "waiting is not working"
            );
            let notice = status.notice().expect("the pane waits for its market");
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("Binance"),
                "the notice names the market: {sentence}"
            );
        });
    }

    /// The notice is a reference into the catalogue rather than a sentence, so a pane that is
    /// already waiting follows the language without being reloaded and without re-reporting
    /// anything itself: the frame that draws it resolves it in the language showing then.
    #[gpui_kit::test]
    fn a_language_change_should_relabel_the_notice_a_waiting_pane_shows(cx: &mut TestAppContext) {
        let widget = widget_over_unloaded(cx, config(), Published::Listings(vec!["BTC"]));

        let (notice, english) = cx.update(|cx| {
            let notice = status(&widget, cx)
                .notice()
                .cloned()
                .expect("the pane waits for its market");
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("Binance"),
                "the notice starts out in English: {sentence}"
            );

            (notice, sentence.to_string())
        });

        cx.update(|cx| cx.set_global(LanguageCode::ZhCn));

        cx.update(|cx| {
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("币安"),
                "and it says the same thing in Chinese now: {sentence}"
            );
            assert_ne!(
                sentence, english,
                "it is not the copy of the language the interface left: {sentence}"
            );
            assert_eq!(
                status(&widget, cx).notice(),
                Some(&notice),
                "the reference is what the pane reported; nothing re-reported it"
            );
        });
    }

    /// The pane that has nothing chosen is waiting too, and its prompt is a reference of
    /// the same kind: the pane does not care which of the two notices stands on screen.
    #[gpui_kit::test]
    fn a_language_change_should_relabel_the_prompt_of_a_pane_with_nothing_chosen(
        cx: &mut TestAppContext,
    ) {
        let widget = widget_with(cx, OrderBookConfig::default());

        let (notice, english) = cx.update(|cx| {
            let notice = status(&widget, cx)
                .notice()
                .cloned()
                .expect("the pane asks for an instrument");
            assert_eq!(
                notice,
                Notice::message("widget.order_book.select_instrument_prompt"),
                "the pane asks with the catalogue's own prompt"
            );

            let english = notice.resolve(cx);

            (notice, english)
        });

        cx.update(|cx| cx.set_global(LanguageCode::ZhCn));

        cx.update(|cx| {
            let prompt = notice.resolve(cx);
            assert_eq!(
                prompt,
                t!(cx, "widget.order_book.select_instrument_prompt"),
                "and it asks in the language showing now"
            );
            assert_ne!(
                prompt, english,
                "which is not the copy of the language the interface left"
            );
        });
    }

    /// The directory answering "this route's catalog is not in memory" is a wait, not a
    /// failure: the pane's own gate asked the same question a moment earlier, so
    /// this is the answer changing underneath the request — a venue re-discovering its
    /// scopes is what does it — and loading the market again is what brings the book
    /// back. Reporting it as `failed` would leave the pane terminal over a route the next
    /// publication brings back, and no later event may revive a failed pane.
    #[gpui_kit::test]
    fn a_directory_that_has_not_loaded_a_catalog_should_leave_the_pane_waiting(
        cx: &mut TestAppContext,
    ) {
        let widget = loaded_widget(cx);

        cx.update(|cx| {
            // The route is really there and its catalog is really published, so the answer
            // this test delivers is one only a revocation mid-request produces.
            assert!(
                cx.global::<MarketParticipantsConnector>()
                    .catalog()
                    .cached_listings(&market_participants_connector::catalog::test_support::spot_route())
                    .is_some(),
                "the fixture market is loaded before the directory's answer arrives"
            );

            report_status(
                &widget,
                OrderBookStatus::Error(OrderBookStreamError::MarketParticipant(MarketError::Catalog(
                    Box::new(
                        market_participants_connector::catalog::error::CatalogError::CatalogNotLoaded {
                            route: market_participants_connector::catalog::test_support::spot_route(),
                        },
                    ),
                ))),
                cx,
            );

            let status = status(&widget, cx);
            assert!(
                status.failure().is_none(),
                "a catalog that has not loaded is a wait, not a failure"
            );
            assert_eq!(
                status,
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: vec![listing("BTC").route.venue],
                    supported: OrderBookWidget::listed_venues(),
                }),
                "waiting is not working"
            );
            let notice = status
                .notice()
                .expect("the pane reports the neutral notice");
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("Binance"),
                "the notice names the market: {sentence}"
            );
            assert!(!widget.read(cx).has_content());
        });
    }

    /// A market a shrinking commit takes away while the ladder is reading it is a wait,
    /// not a failure.
    ///
    /// The pane's own gate asked the loaded venues a moment earlier, so a scope that
    /// left them is an answer the gate could not have seen: the request already in
    /// flight is refused with "this market is not loaded". That is the same wait a
    /// catalog that has not loaded is — loading the market again is what
    /// brings the ladder back — so the pane reports the neutral notice, keeps the
    /// selection it was given and writes nothing back, instead of failing terminally
    /// over a market one load brings back.
    #[gpui_kit::test]
    fn a_market_taken_away_mid_flight_should_leave_the_pane_waiting(cx: &mut TestAppContext) {
        let (widget, reports) = widget_over_source_reporting(
            cx,
            config(),
            PendingSource,
            Published::Listings(vec!["BTC"]),
        );
        let route = market_participants_connector::catalog::test_support::spot_route();

        cx.update(|cx| {
            // The market is really loaded and the ladder is really reading it, so the
            // answer this test delivers is one only a revocation mid-request produces.
            assert!(
                cx.global::<MarketParticipantsConnector>()
                    .venues()
                    .scope(&route)
                    .is_some(),
                "the fixture market is loaded before the refusal arrives"
            );
            assert_eq!(
                widget.read(cx).config.selection,
                config().selection,
                "the ladder follows what the user chose"
            );
            let before = reports.get();

            report_status(
                &widget,
                OrderBookStatus::Error(OrderBookStreamError::MarketParticipant(
                    MarketError::Network(MarketNetworkError::from(
                        MarketParticipantError::VenueNotLoaded {
                            venue: route.venue,
                            scope: route.scope.clone(),
                        },
                    )),
                )),
                cx,
            );

            assert_eq!(
                widget.read(cx).config.selection,
                config().selection,
                "a market that went away does not unmake the user's choice"
            );
            assert_eq!(
                reports.get(),
                before,
                "nothing about the choice changed, so nothing is written back"
            );
            assert!(
                widget.read(cx).awaiting_market,
                "the ladder waits for its market"
            );
            assert!(!widget.read(cx).has_content());
            let status = status(&widget, cx);
            assert!(
                status.failure().is_none(),
                "a market the user can load again is not a failure"
            );
            assert_eq!(
                status,
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: vec![route.venue],
                    supported: OrderBookWidget::listed_venues(),
                }),
                "waiting is not working"
            );
            let notice = status
                .notice()
                .expect("the pane reports the neutral notice");
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("Binance"),
                "the notice names the market: {sentence}"
            );
        });
    }

    /// Loading the venue is what a waiting pane recovers from, by itself and
    /// without being reopened (⑯).
    #[gpui_kit::test]
    fn loading_the_venue_should_let_the_waiting_pane_start(cx: &mut TestAppContext) {
        let widget = widget_over_unloaded(cx, config(), Published::Listings(vec!["BTC"]));
        cx.update(|cx| {
            assert!(
                status(&widget, cx).notice().is_some(),
                "the pane waits for a market nobody loaded"
            );

            let venues = cx
                .global::<MarketParticipantsConnector>()
                .supported_venues();
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&venues),
                    &[],
                    &[],
                )
                .expect("the venue's routes load");
            // A publication is what emits the directory's change signal; a load alone
            // does not.
            market_participants_connector::catalog::test_support::publish_fixture_catalogs(
                cx,
                &[(
                    market_participants_connector::catalog::test_support::spot_route(),
                    vec![entry("BTC")],
                )],
            );
        });
        // The directory signals its publications through its own task, so the event
        // that re-runs the load arrives on the executor.
        cx.run_until_parked();

        cx.update(|cx| {
            let status = status(&widget, cx);
            assert!(
                status.notice().is_none(),
                "the pane left the notice without being reopened"
            );
            assert_eq!(status, WidgetState::Connecting);
        });
    }

    /// No market that could answer with depth is loaded and nothing is chosen: there is nothing to
    /// draw *and* nothing to choose from, so the pane waits for the market participant's metadata rather than
    /// asking for an instrument its picker could not offer (AGENTS.md §8).
    #[gpui_kit::test]
    fn a_book_with_nothing_chosen_and_no_market_loaded_should_wait_for_one(
        cx: &mut TestAppContext,
    ) {
        let (widget, _) = widget_over_market(
            cx,
            OrderBookConfig::default(),
            PendingSource,
            Published::Listings(Vec::new()),
            false,
        );

        cx.update(|cx| {
            assert!(!widget.read(cx).has_content(), "there is no ladder to draw");
            assert!(
                widget.read(cx).awaiting_market,
                "and no market to draw one from either"
            );
            assert_eq!(
                status(&widget, cx),
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: OrderBookWidget::listed_venues(),
                    supported: OrderBookWidget::listed_venues(),
                }),
                "a book nobody could choose from waits for the market participant's metadata"
            );
        });
    }

    /// The commit of a market the book was waiting for is what takes it off that notice: a market
    /// with nothing chosen is the prompt's own business, and the pane says so without being
    /// reopened (a book waiting with nothing chosen reads nothing, so only the wait itself can tell
    /// that it is over).
    #[gpui_kit::test]
    fn loading_a_market_should_turn_the_wait_back_into_the_prompt(cx: &mut TestAppContext) {
        let (widget, _) = widget_over_market(
            cx,
            OrderBookConfig::default(),
            PendingSource,
            Published::Listings(Vec::new()),
            false,
        );

        cx.update(|cx| {
            assert!(
                widget.read(cx).awaiting_market,
                "the book starts out waiting"
            );
            let venues = cx
                .global::<MarketParticipantsConnector>()
                .supported_venues();
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&venues),
                    &[],
                    &[],
                )
                .expect("the venue's routes load");
            // A publication is what emits the directory's change signal; a load alone
            // does not.
            market_participants_connector::catalog::test_support::publish_fixture_catalogs(
                cx,
                &[(
                    market_participants_connector::catalog::test_support::spot_route(),
                    vec![entry("BTC")],
                )],
            );
        });
        cx.run_until_parked();

        cx.update(|cx| {
            assert!(
                !widget.read(cx).awaiting_market,
                "the book left the wait on the commit alone"
            );
            assert_eq!(
                status(&widget, cx),
                WidgetState::waiting(Notice::message(
                    "widget.order_book.select_instrument_prompt"
                )),
                "a market the user can choose from asks for an instrument"
            );
        });
    }

    /// A book nobody has pointed at an instrument asks for one instead of drawing
    /// a default nobody asked for, and it opens no stream until it has one.
    #[gpui_kit::test]
    fn a_widget_without_a_selection_should_ask_for_one(cx: &mut TestAppContext) {
        let widget = widget_with(cx, OrderBookConfig::default());

        cx.update(|cx| {
            assert!(!widget.read(cx).has_content());
            assert!(
                widget.read(cx).current_key().is_none(),
                "a book that follows nothing has nothing to stream"
            );
            let status = status(&widget, cx);
            assert_eq!(
                status,
                WidgetState::waiting(Notice::message(
                    "widget.order_book.select_instrument_prompt"
                )),
                "a pane that follows nothing asks for an instrument and is not working"
            );
        });
    }

    /// A listing the published catalog does not hold is gone, so it goes back to
    /// nothing chosen and the pane reports the change.
    #[gpui_kit::test]
    fn a_selection_missing_from_the_catalog_should_be_cleared(cx: &mut TestAppContext) {
        let (widget, reports) = widget_over_source_reporting(
            cx,
            config(),
            PendingSource,
            Published::Listings(vec!["ETH"]),
        );
        settle_until(cx, "the route to answer", |cx| {
            widget.read(cx).config.selection.is_none()
        });

        cx.update(|cx| {
            assert_eq!(widget.read(cx).config.selection, None);
            assert!(!widget.read(cx).has_content());
            assert_eq!(
                status(&widget, cx),
                WidgetState::waiting(Notice::message(
                    "widget.order_book.select_instrument_prompt"
                ))
            );
            assert_eq!(
                reports.get(),
                1,
                "the replacement is what the pane writes back"
            );
        });
    }

    /// A route that is not loaded is not one this pane can read depth from, so
    /// the listing it named goes back to nothing chosen.
    #[gpui_kit::test]
    fn a_selection_on_a_route_that_left_the_registry_should_be_cleared(cx: &mut TestAppContext) {
        let widget = widget(cx);
        let unregistered = InstrumentSelection {
            listing: market_participants_connector::catalog::test_support::listing_key(
                &market_participants_connector::catalog::test_support::perp_route(),
                "BTCUSDT",
            ),
        };
        let routes =
            cx.update(|cx| LoadedScope::all(cx.global::<MarketParticipantsConnector>().venues()));

        cx.update(|cx| {
            assert!(
                !offers_route(
                    &routes,
                    &unregistered.listing.route,
                    market_participants_model::routing::CapabilityId::OrderBook
                ),
                "the fixture route is not one the loaded venues answer"
            );
            widget.update(cx, |widget, _| {
                widget.config.selection = Some(unregistered.clone());
            });
            let support = listing_support(
                cx.global::<MarketParticipantsConnector>().catalog(),
                &routes,
                &unregistered.listing,
            );
            assert_eq!(support, ListingSupport::Unsupported);
            widget.update(cx, |widget, cx| {
                assert!(!widget.settle_selection(&unregistered, support, cx));
            });

            assert_eq!(
                widget.read(cx).config.selection,
                None,
                "a route the registry does not declare is unsupported"
            );
        });
    }

    /// A listing whose status is terminal is not one the pane can read depth from,
    /// so it goes back to nothing chosen as well.
    #[gpui_kit::test]
    fn a_selection_whose_listing_is_terminal_should_be_cleared(cx: &mut TestAppContext) {
        let (widget, reports) =
            widget_over_source_reporting(cx, config(), PendingSource, Published::Terminal("BTC"));
        settle_until(cx, "the route to answer", |cx| {
            widget.read(cx).config.selection.is_none()
        });

        cx.update(|cx| {
            assert_eq!(widget.read(cx).config.selection, None);
            assert_eq!(reports.get(), 1);
            assert_eq!(
                status(&widget, cx),
                WidgetState::waiting(Notice::message(
                    "widget.order_book.select_instrument_prompt"
                ))
            );
        });
    }

    /// A route that did not answer proves nothing, so the selection is left alone and
    /// the request for it reports its own error.
    #[gpui_kit::test]
    fn a_catalog_lookup_failure_should_preserve_the_selection(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.update(|cx| {
            widget.update(cx, |widget, cx| {
                assert!(widget.settle_selection(&selection("BTC"), ListingSupport::Unknown, cx));
            });

            assert_eq!(widget.read(cx).config.selection, Some(selection("BTC")));
        });
    }

    /// The lookup is advisory, and the loaded venue is the prerequisite: a book whose
    /// market was never loaded asks for nothing rather than requesting depth from a
    /// route nobody has loaded.
    ///
    /// The fixture is the failing directory with no load, because a load that fails
    /// leaves no venue behind at all: "the venue is loaded" and "its catalog is
    /// here" are one statement, so a venue whose directory failed is a venue the
    /// pane waits for.
    #[gpui_kit::test]
    fn a_market_that_is_not_in_memory_should_not_ask_for_the_book(cx: &mut TestAppContext) {
        let source = AnsweringOrderBookSource::default();
        let (widget, reports) =
            widget_over_source_reporting_unloaded(cx, config(), source.clone(), Published::Failing);

        cx.update(|cx| {
            assert_eq!(
                source.subscriptions(),
                0,
                "a market the user has not loaded is not asked for depth"
            );
            assert_eq!(widget.read(cx).config.selection, Some(selection("BTC")));
            let status = status(&widget, cx);
            assert!(
                status.failure().is_none(),
                "waiting for a market is not a failure of the book"
            );
            assert_eq!(
                status,
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: vec![listing("BTC").route.venue],
                    supported: OrderBookWidget::listed_venues(),
                }),
                "the pane waits for the market to load"
            );
            assert!(
                status
                    .notice()
                    .is_some_and(|notice| notice.resolve(cx).contains("Binance")),
                "the pane says which market to load"
            );
            assert_eq!(
                reports.get(),
                0,
                "waiting for a market changes no configuration"
            );
        });
    }

    /// A book the user has moved away from is not the answer's to clear.
    #[gpui_kit::test]
    fn a_late_catalog_result_should_not_clear_a_new_selection(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.update(|cx| {
            assert!(widget.update(cx, |widget, cx| {
                widget.select_selection(selection("ETH"), cx)
            }));
            // The answer is about BTC, which the user has already moved away from.
            widget.update(cx, |widget, cx| {
                assert!(!widget.settle_selection(
                    &selection("BTC"),
                    ListingSupport::Unsupported,
                    cx
                ));
            });

            assert_eq!(widget.read(cx).config.selection, Some(selection("ETH")));
        });
    }

    /// Choosing an instrument is what starts the book, and it also clears the
    /// prompt the widget was standing in with.
    #[gpui_kit::test]
    fn choosing_an_instrument_should_start_the_book_and_clear_the_prompt(cx: &mut TestAppContext) {
        let widget = widget_with(cx, OrderBookConfig::default());

        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                assert!(widget.select_selection(selection("ETH"), context));
            });

            let chosen = selection("ETH");
            assert_eq!(widget.read(cx).config.selection, Some(chosen.clone()));
            assert_eq!(
                widget.read(cx).current_key(),
                Some(OrderBookStreamKey::new(
                    chosen.listing,
                    PRICE_MULTIPLIERS[0],
                    WINDOW_SIZES[0]
                ))
            );
            let status = status(&widget, cx);
            assert_eq!(status, WidgetState::Connecting);
            assert!(status.notice().is_none());
        });
    }

    /// Two listings of the same canonical instrument are two scopes: the pane follows
    /// the venue's own listing, so picking the other one is a different book.
    #[gpui_kit::test]
    fn two_listings_of_one_instrument_should_stay_apart(cx: &mut TestAppContext) {
        let widget = widget(cx);
        let other = InstrumentSelection {
            listing: market_participants_connector::catalog::test_support::listing_key(
                &market_participants_connector::catalog::test_support::route(
                    market_participants_connector::catalog::test_support::VENUE,
                    "core-spot",
                ),
                "BTCUSDT",
            ),
        };

        cx.update(|cx| {
            let first = widget.read(cx).config.selection.clone().expect("a scope");
            assert_ne!(
                first.listing, other.listing,
                "the two keys name different listings"
            );
            assert_ne!(
                first.listing.route, other.listing.route,
                "and different routes"
            );

            assert!(widget.update(cx, |widget, cx| {
                widget.select_selection(other.clone(), cx)
            }));

            let widget = widget.read(cx);
            assert_eq!(widget.config.selection, Some(other.clone()));
            assert_eq!(
                widget.current_key(),
                Some(OrderBookStreamKey::new(
                    other.listing.clone(),
                    PRICE_MULTIPLIERS[0],
                    WINDOW_SIZES[0]
                )),
                "the stream key follows the other listing"
            );
        });
    }

    /// A complete book is not the same thing as the status that says it is
    /// complete: the widget reports ready where the two meet.
    #[gpui_kit::test]
    fn an_accepted_book_should_become_content_and_report_ready_at_the_ready_revision(
        cx: &mut TestAppContext,
    ) {
        let widget = widget(cx);

        cx.update(|cx| {
            report_status(&widget, OrderBookStatus::LoadingSnapshot(metadata()), cx);
            assert!(accept_book(
                &widget,
                snapshot(1, &[("101", 1)], &[("100", 1)]),
                cx
            ));
            assert!(widget.read(cx).has_content());
            assert_eq!(
                status(&widget, cx),
                WidgetState::Loading,
                "a book with no status behind it is not a promise that it is complete"
            );

            report_status(
                &widget,
                OrderBookStatus::Ready {
                    revision: MarketSequence::Numeric(1),
                },
                cx,
            );
            assert_eq!(status(&widget, cx), WidgetState::Ready);
        });
    }

    #[gpui_kit::test]
    fn a_ready_status_that_overtakes_its_book_should_wait_for_it(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.update(|cx| {
            report_status(
                &widget,
                OrderBookStatus::Ready {
                    revision: MarketSequence::Numeric(3),
                },
                cx,
            );
            assert_eq!(status(&widget, cx), WidgetState::Loading);

            accept_book(&widget, snapshot(2, &[("101", 1)], &[("100", 1)]), cx);
            assert_eq!(
                status(&widget, cx),
                WidgetState::Loading,
                "a book older than the status named is not the book that was promised"
            );

            accept_book(&widget, moved_snapshot(3), cx);
            assert_eq!(status(&widget, cx), WidgetState::Ready);
        });
    }

    /// Connecting successfully is enough to show the book, and an empty book is
    /// content — a ladder with nothing in it — not a state to report: the pane says
    /// the ladder holds nothing itself.
    #[gpui_kit::test]
    fn an_empty_book_should_be_content_and_report_nothing(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.update(|cx| {
            accept_book(&widget, snapshot(1, &[], &[]), cx);
            report_status(
                &widget,
                OrderBookStatus::Ready {
                    revision: MarketSequence::Numeric(1),
                },
                cx,
            );

            assert!(widget.read(cx).has_content());
            assert_eq!(ladder_rows(&widget, cx), (0, 0));
            let status = status(&widget, cx);
            assert_eq!(status, WidgetState::Ready);
            assert!(
                status.notice().is_none(),
                "the empty state belongs to the pane's own ladder, not to the framework's placeholder"
            );
        });
    }

    #[gpui_kit::test]
    fn a_failure_with_content_should_be_reported_and_keep_the_ladder(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.update(|cx| {
            accept_book(&widget, snapshot(1, &[("101", 1)], &[("100", 1)]), cx);
            report_failure(&widget, cx);

            assert!(widget.read(cx).has_content());
            assert_eq!(ladder_rows(&widget, cx), (1, 1));
            let status = status(&widget, cx);
            assert!(status.is_failed());
            assert_eq!(
                status.failure().map(|failure| failure.action().label_key()),
                Some("common.retry"),
                "a failure always offers the framework's own way out"
            );
        });
    }

    #[gpui_kit::test]
    fn a_failure_without_content_should_be_reported_with_no_content(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.update(|cx| {
            report_failure(&widget, cx);

            assert!(!widget.read(cx).has_content());
            assert!(status(&widget, cx).is_failed());
        });
    }

    #[gpui_kit::test]
    fn a_stream_failure_should_become_the_reported_state(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.update(|cx| {
            report_status(&widget, OrderBookStatus::LoadingSnapshot(metadata()), cx);
            assert_eq!(status(&widget, cx), WidgetState::Loading);

            report_status(&widget, OrderBookStatus::Error(disconnect()), cx);
            let status = status(&widget, cx);
            assert!(status.is_failed());
            assert!(
                status
                    .failure()
                    .is_some_and(|failure| failure.text(cx) == disconnect().localized_message(cx))
            );
        });
    }

    /// A stream that is still alive after its market went away must not put the ladder
    /// back over the notice: the pane waits, and a book already in flight belongs to a
    /// market the pane no longer reports reading. Without the guard the book is accepted,
    /// `has_content()` turns true again and the framework draws the ladder instead of the
    /// notice the user is supposed to act on.
    #[gpui_kit::test]
    fn a_late_book_after_the_market_went_away_should_not_repaint_over_the_notice(
        cx: &mut TestAppContext,
    ) {
        let widget = loaded_widget(cx);

        cx.update(|cx| {
            // The pane is reading, which is the state a revocation interrupts.
            let key = widget
                .read(cx)
                .current_key()
                .expect("the fixture config follows a listing");
            accept_book(&widget, snapshot(1, &[("101", 1)], &[("100", 1)]), cx);
            assert!(widget.read(cx).has_content(), "the ladder is what arrived");

            // The venue loaded again and published a scope set without this one: the pane's own
            // gate is what notices, and it reports the neutral notice.
            let venue = market_participants_model::identifier::VenueId::Binance;
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(&[], &[(venue, Vec::new())], &[])
                .expect("an empty scope set unloads the venue's slot");
            widget.update(cx, |widget, cx| widget.load(cx));

            let waiting = status(&widget, cx);
            assert!(
                waiting.notice().is_some(),
                "the pane reports the neutral notice"
            );
            assert!(waiting.failure().is_none(), "waiting is not a failure");

            widget.update(cx, |widget, context| {
                widget.apply_latest(
                    key,
                    Arc::new(OrderBookStreamMessage::Snapshot(moved_snapshot(9))),
                    context,
                );
            });

            assert!(
                !widget.read(cx).has_content(),
                "a book from the market that went away must not become content again"
            );
            assert!(
                status(&widget, cx).notice().is_some(),
                "the notice is still what the pane reports"
            );
        });
    }

    /// A book opened with nothing chosen reads nothing, and the instrument the user then picks is
    /// what moves what it reads — not a venue commit. A commit of another venue therefore cannot
    /// move the book's scope, so the pane leaves the stream it started and the ladder it accepted
    /// exactly as they are rather than starting over what the user is watching.
    ///
    /// The commit is proved to have reached the pane by a probe subscribed after it did, and what
    /// proves the pane did not start over is its own directory generation beside the ladder it
    /// was drawing and the status it reported for it.
    #[gpui_kit::test]
    fn choosing_an_instrument_should_leave_the_book_alone_on_another_venues_commit(
        cx: &mut TestAppContext,
    ) {
        let source = AnsweringOrderBookSource::default();
        let (widget, _reports) = widget_over_source_reporting(
            cx,
            OrderBookConfig::default(),
            source.clone(),
            Published::Listings(vec!["BTC"]),
        );
        // The commits the fixture itself made — the venue loading, and the catalog publication —
        // are drained before anything is chosen: what this test is about is the commit below, and
        // one still in flight would be handled as a commit of a market the book had only just
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
                widget.read(cx).config.selection.is_none(),
                "the pane is opened with nothing chosen and nothing to read"
            );
            assert!(
                widget.update(cx, |widget, context| widget
                    .select_selection(selection("BTC"), context)),
                "the picker offers the listing the loaded venue publishes"
            );
        });
        settle_until(cx, "the book to read the instrument the user chose", |_| {
            source.subscriptions() > 0
        });
        let generation = cx.read(|cx| widget.read(cx).directory_generation);

        // The book is reading: the ladder it accepted is the whole of what it draws.
        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                widget.apply_latest(
                    widget
                        .current_key()
                        .expect("the book follows the instrument the user chose"),
                    Arc::new(OrderBookStreamMessage::Snapshot(snapshot(
                        1,
                        &[("101", 1)],
                        &[("100", 1)],
                    ))),
                    context,
                );
            });
            report_status(
                &widget,
                OrderBookStatus::Ready {
                    revision: MarketSequence::Numeric(1),
                },
                cx,
            );
            assert!(
                widget.read(cx).has_content(),
                "the book is reading a ladder"
            );
            assert_eq!(
                status(&widget, cx),
                WidgetState::Ready,
                "the pane reported the ladder it accepted as ready"
            );
        });

        // Another venue commits: nothing about this book's scope moved, so the read it is in the
        // middle of is left exactly as it is.
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
                "a commit of a venue this book does not follow must not start it over"
            );
            assert!(
                widget.read(cx).snapshot.is_some(),
                "the ladder the user is reading must not be dropped"
            );
            assert_eq!(
                status(&widget, cx),
                WidgetState::Ready,
                "and the pane must not go back to connecting"
            );
        });
    }

    /// A refresh of the venue the book reads replaces the catalogue generation its live stream
    /// resolved the listing's rules, units and price steps against, while the stream keeps what it
    /// resolved when it was opened. So the book starts over — the ladder goes, because nothing on
    /// screen can be vouched for — and the request it sends is the one that opens a new stream, at
    /// the generation the load published.
    ///
    /// What proves both halves is the source's own record: the subscription the service was asked
    /// for when it opened the stream, and the metadata that stream was resolved at — which is the
    /// price step the ladder is grouped and labelled by.
    #[gpui_kit::test]
    fn a_refresh_of_the_books_own_venue_should_reopen_its_stream(cx: &mut TestAppContext) {
        let subscriptions = Arc::new(std::sync::Mutex::new(0_usize));
        let opened = {
            let subscriptions = Arc::clone(&subscriptions);
            move || {
                *subscriptions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            }
        };
        let (widget, _reports) = widget_over_market_with(
            cx,
            config(),
            Published::Listings(vec!["BTC"]),
            true,
            move |catalog| ResolvingOrderBookSource {
                catalog: Arc::clone(catalog),
                subscriptions,
            },
        );
        settle_until(cx, "the book's stream to be opened", |_| opened() == 1);
        let generation = cx.read(|cx| widget.read(cx).directory_generation);

        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                widget.apply_latest(
                    widget
                        .current_key()
                        .expect("the fixture config follows a listing"),
                    Arc::new(OrderBookStreamMessage::Snapshot(snapshot(
                        1,
                        &[("101", 1)],
                        &[("100", 1)],
                    ))),
                    context,
                );
            });
            assert!(
                widget.read(cx).has_content(),
                "the book is reading a ladder"
            );

            // The same venue loads again and the catalogue it republishes restates the listing's
            // price tick, which is the shape a production load has: it re-fetches the catalogue.
            let venue = market_participants_model::identifier::VenueId::from_name("binance")
                .expect("the fixture venue is canonical");
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(
                        std::slice::from_ref(&venue),
                    ),
                    &[],
                    &[],
                )
                .expect("the book's venue loads again");
            market_participants_connector::catalog::test_support::publish_fixture_catalogs(
                cx,
                &[(
                    market_participants_connector::catalog::test_support::spot_route(),
                    vec![restated_entry("0.5")],
                )],
            );
        });
        settle_until(cx, "the book to open a new stream", |_| opened() == 2);

        cx.update(|cx| {
            assert_eq!(
                widget.read(cx).directory_generation,
                generation + 1,
                "a refresh of the venue the book reads starts it over"
            );
            assert!(
                !widget.read(cx).has_content(),
                "and the ladder is gone: it was grouped at the generation that was replaced"
            );
            assert_eq!(
                widget
                    .read(cx)
                    .metadata
                    .as_ref()
                    .expect("the stream that was opened published its metadata")
                    .precision
                    .price_tick,
                tick("0.5"),
                "the stream that was opened resolved at the generation this load published"
            );
        });
    }

    /// The other direction of the same rule: a commit that takes the book's own market away is a
    /// market it can no longer read, so the pane goes back to waiting for it — without being
    /// reopened, and without losing the choice the user made.
    ///
    /// A pane that is waiting with nothing chosen is not the case here: the book was reading, and
    /// the commit is what moves its scope out of memory.
    #[gpui_kit::test]
    fn a_commit_that_takes_the_books_own_market_away_should_send_it_back_to_waiting(
        cx: &mut TestAppContext,
    ) {
        let widget = loaded_widget(cx);
        cx.update(|cx| {
            assert!(
                !widget.read(cx).awaiting_market,
                "the book is reading the market it was built over"
            );
            widget.update(cx, |widget, context| {
                widget.apply_latest(
                    widget
                        .current_key()
                        .expect("the fixture config follows a listing"),
                    Arc::new(OrderBookStreamMessage::Snapshot(snapshot(
                        1,
                        &[("101", 1)],
                        &[("100", 1)],
                    ))),
                    context,
                );
            });
            report_status(
                &widget,
                OrderBookStatus::Ready {
                    revision: MarketSequence::Numeric(1),
                },
                cx,
            );
            assert!(
                widget.read(cx).has_content(),
                "the book is reading a ladder"
            );
        });

        // The venue loads again and publishes no scope at all, which is what a shrinking commit
        // does: the market behind the ladder is not in memory any more.
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &[],
                    &[(
                        market_participants_model::identifier::VenueId::Binance,
                        Vec::new(),
                    )],
                    &[],
                )
                .expect("the shrinking commit lands");
        });
        cx.run_until_parked();

        cx.update(|cx| {
            assert!(
                widget.read(cx).awaiting_market,
                "the book waits for the market it lost"
            );
            assert!(
                !widget.read(cx).has_content(),
                "and the ladder it was drawing is not backed by a market any more"
            );
            assert_eq!(
                widget.read(cx).config.selection,
                config().selection,
                "a market that went away does not unmake the user's choice"
            );
            assert_eq!(
                status(&widget, cx),
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: vec![
                        market_participants_connector::catalog::test_support::spot_route().venue
                    ],
                    supported: OrderBookWidget::listed_venues(),
                }),
                "waiting is not working"
            );
        });
    }

    /// A failure is terminal: a venue commit does not revive a failed book, even when the commit
    /// takes the market its ladder came from away and puts it back.
    ///
    /// The first commit is the one a book without the terminal guard would act on: it takes the
    /// book's own market away, so an unguarded handler would replace the failure with the neutral
    /// notice. The second puts the market back, which a handler that judged "did what I read
    /// change" without the guard would read as a scope that appeared and would start over for.
    #[gpui_kit::test]
    fn a_venue_commit_should_not_revive_a_failed_pane(cx: &mut TestAppContext) {
        let widget = loaded_widget(cx);
        let failure = cx.update(|cx| {
            // The failure is reported without cancelling the binding: a cancelled call answers
            // with an error of its own, which would be a second failure over this one and nothing
            // to do with the commits below.
            report_failure(&widget, cx);

            let state = status(&widget, cx);
            assert!(state.is_failed(), "the book is in its terminal failure");
            state
        });
        let venue = market_participants_model::identifier::VenueId::Binance;

        let deliveries = Arc::new(AtomicUsize::new(0));
        let _probe = cx.update(|cx| {
            let events = cx.global::<MarketParticipantsConnector>().venues().events();
            cx.new(|cx| CommitProbe::new(cx, &events, Arc::clone(&deliveries)))
        });
        // The market the book follows leaves the loaded venues...
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
                "a venue commit must not revive a failed book"
            );
            assert!(
                !widget.read(cx).awaiting_market,
                "a book in its terminal failure is not a book waiting for a market"
            );
        });
    }

    /// Failure is terminal: the widget decides when to ask for a replacement
    /// generation, so a book that arrives behind a failure is not drawn.
    #[gpui_kit::test]
    fn a_book_after_a_failure_should_not_change_the_status_or_the_ladder(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.update(|cx| {
            accept_book(&widget, snapshot(1, &[("101", 1)], &[("100", 1)]), cx);
            report_status(&widget, OrderBookStatus::Error(disconnect()), cx);
            let failed = status(&widget, cx);

            assert!(
                !accept_book(&widget, moved_snapshot(9), cx),
                "a late book asks for no repaint"
            );
            assert!(status(&widget, cx).is_failed());
            assert_eq!(
                widget
                    .read(cx)
                    .snapshot
                    .as_ref()
                    .map(|book| book.sequence.clone()),
                Some(MarketSequence::Numeric(1))
            );
            assert_eq!(status(&widget, cx).failure(), failed.failure());
        });
    }

    #[gpui_kit::test]
    fn a_book_for_another_scope_should_be_ignored(cx: &mut TestAppContext) {
        let widget = widget(cx);
        let other = OrderBookStreamKey::new(listing("ETH"), PRICE_MULTIPLIERS[0], WINDOW_SIZES[0]);

        cx.update(|cx| {
            let applied = widget.update(cx, |widget, context| {
                widget.apply_latest(
                    other,
                    Arc::new(OrderBookStreamMessage::Snapshot(moved_snapshot(1))),
                    context,
                )
            });

            assert!(!applied);
            assert!(!widget.read(cx).has_content());
        });
    }

    #[gpui_kit::test]
    fn a_book_the_stream_has_overtaken_should_not_roll_the_ladder_back(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.update(|cx| {
            accept_book(&widget, moved_snapshot(4), cx);

            assert!(!accept_book(
                &widget,
                snapshot(2, &[("101", 9)], &[("100", 9)]),
                cx
            ));
            assert_eq!(
                widget
                    .read(cx)
                    .snapshot
                    .as_ref()
                    .map(|book| book.sequence.clone()),
                Some(MarketSequence::Numeric(4))
            );
            assert_eq!(
                widget
                    .read(cx)
                    .grouped
                    .as_ref()
                    .and_then(|book| book.mid_price),
                Some(Price::from(101_u64))
            );
        });
    }

    /// A venue publishes far more often than the visible ladder changes, and the
    /// body is cached: a book that draws the same pixels asks for no frame.
    #[gpui_kit::test]
    fn a_book_that_draws_the_same_ladder_should_not_ask_for_a_repaint(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.update(|cx| {
            assert!(accept_book(
                &widget,
                snapshot(1, &[("101", 1)], &[("100", 1)]),
                cx
            ));

            assert!(
                !accept_book(&widget, snapshot(2, &[("101", 1)], &[("100", 1)]), cx),
                "the same visible ladder is not worth a frame"
            );
            assert_eq!(
                widget
                    .read(cx)
                    .snapshot
                    .as_ref()
                    .map(|book| book.sequence.clone()),
                Some(MarketSequence::Numeric(2)),
                "the book is still the one the stream last published"
            );
        });
    }

    #[gpui_kit::test]
    fn an_explicit_reload_should_start_over_from_connecting(cx: &mut TestAppContext) {
        let widget = loaded_widget(cx);

        cx.update(|cx| {
            accept_book(&widget, snapshot(1, &[("101", 1)], &[("100", 1)]), cx);
            widget.update(cx, |widget, context| widget.load(context));

            assert!(!widget.read(cx).has_content());
            assert_eq!(status(&widget, cx), WidgetState::Connecting);
            assert!(widget.read(cx).grouped.is_none());
        });
    }

    #[gpui_kit::test]
    fn a_retry_after_a_failure_should_keep_the_ladder(cx: &mut TestAppContext) {
        let widget = loaded_widget(cx);

        cx.update(|cx| {
            accept_book(&widget, snapshot(1, &[("101", 1)], &[("100", 1)]), cx);
            report_failure(&widget, cx);

            widget.update(cx, |widget, context| widget.load(context));

            assert!(
                widget.read(cx).has_content(),
                "a retry keeps what is on screen"
            );
            assert_eq!(ladder_rows(&widget, cx), (1, 1));
            assert_eq!(status(&widget, cx), WidgetState::Connecting);
        });
    }

    #[gpui_kit::test]
    fn selecting_another_instrument_should_switch_the_scope_and_drop_the_ladder(
        cx: &mut TestAppContext,
    ) {
        let widget = widget(cx);

        cx.update(|cx| {
            accept_book(&widget, snapshot(1, &[("101", 1)], &[("100", 1)]), cx);

            assert!(widget.update(cx, |widget, context| {
                widget.select_selection(selection("ETH"), context)
            }));

            let widget = widget.read(cx);
            assert_eq!(widget.config.selection, Some(selection("ETH")));
            assert!(!widget.has_content());
            assert!(widget.grouped.is_none());
        });
    }

    #[gpui_kit::test]
    fn selecting_the_scope_that_is_already_shown_should_only_retry_a_failure(
        cx: &mut TestAppContext,
    ) {
        let widget = widget(cx);

        cx.update(|cx| {
            assert!(
                !widget.update(cx, |widget, context| widget
                    .select_selection(selection("BTC"), context)),
                "picking what is already shown is not a change"
            );

            report_failure(&widget, cx);
            assert!(widget.update(cx, |widget, context| {
                widget.select_selection(selection("BTC"), context)
            }));
            assert!(!status(&widget, cx).is_failed());
        });
    }

    /// A selection names the exact listing, so two listings of one canonical
    /// instrument are two scopes and the pane follows the one the user picked.
    #[gpui_kit::test]
    fn a_selection_of_another_listing_should_switch_the_scope(cx: &mut TestAppContext) {
        let widget = widget(cx);
        let other = selection("ETH");

        cx.update(|cx| {
            assert!(widget.update(cx, |widget, context| {
                widget.select_selection(other.clone(), context)
            }));
            assert_eq!(widget.read(cx).config.selection, Some(other));
        });
    }

    #[gpui_kit::test]
    fn changing_the_grouping_should_regroup_the_book_without_dropping_it(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.update(|cx| {
            accept_book(
                &widget,
                // Two asks the venue's own tick keeps apart and one coarser step
                // does not.
                snapshot(1, &[("101.01", 1), ("101.02", 1)], &[("100", 1)]),
                cx,
            );
            assert_eq!(ladder_rows(&widget, cx).0, 2);

            widget.update(cx, |widget, context| {
                widget.select_price_multiplier(10, context)
            });

            assert_eq!(widget.read(cx).config.price_multiplier, 10);
            assert_eq!(
                ladder_rows(&widget, cx).0,
                1,
                "a coarser step groups the two asks into one bucket"
            );
            assert_eq!(
                widget
                    .read(cx)
                    .snapshot
                    .as_ref()
                    .map(|book| book.sequence.clone()),
                Some(MarketSequence::Numeric(1)),
                "regrouping reads the book the widget already has"
            );
        });
    }

    #[gpui_kit::test]
    fn changing_the_row_limit_should_regroup_the_book(cx: &mut TestAppContext) {
        let widget = widget(cx);
        let asks = (1..=30_u64)
            .map(|price| {
                let mut level = levels(&[("1", 1)]).remove(0);
                level.price.value = Price::from(price);
                level
            })
            .collect::<Vec<_>>();

        cx.update(|cx| {
            accept_book(&widget, from_levels(1, asks, levels(&[("100", 1)])), cx);
            assert_eq!(ladder_rows(&widget, cx).0, WINDOW_SIZES[0]);

            widget.update(cx, |widget, context| {
                widget.select_window_size(WINDOW_SIZES[1], context);
            });

            assert_eq!(ladder_rows(&widget, cx).0, 30);
        });
    }

    /// The ladder is grouped to what the pane can actually draw, so a pane that
    /// was resized regroups without asking the venue for anything.
    #[gpui_kit::test]
    fn the_laid_out_height_should_limit_the_grouped_rows(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.update(|cx| {
            accept_book(
                &widget,
                snapshot(1, &[("101", 1), ("102", 1), ("103", 1)], &[("100", 1)]),
                cx,
            );

            widget.update(cx, |widget, context| {
                widget.paint_caches.set_drawable_rows(1);
                assert!(widget.regroup(context));
            });

            assert_eq!(ladder_rows(&widget, cx).0, 1);
        });
    }

    /// A shallow book only offers the rows it can fill: a row limit the venue is
    /// too shallow for is refused, and one of its own is taken.
    #[gpui_kit::test]
    fn a_shallow_book_should_only_take_the_row_limits_it_can_fill(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.update(|cx| {
            let book = ListingOrderBook {
                listing: listing("BTC"),
                sequence: MarketSequence::Numeric(1),
                timestamp: None,
                bids: levels(&[("100", 1)]),
                asks: levels(&[("101", 1)]),
            };
            let shallow = OrderBookSnapshot::from_market_participant(
                &book,
                OrderBookMetadata {
                    depth: 20,
                    ..metadata()
                },
                MAX_LEVELS_PER_SIDE,
            );
            accept_book(&widget, shallow, cx);

            widget.update(cx, |widget, context| {
                widget.select_window_size(WINDOW_SIZES[2], context);
                assert_eq!(
                    widget.config.window_size, WINDOW_SIZES[0],
                    "a row limit the book cannot fill is not offered"
                );

                widget.select_window_size(SHALLOW_WINDOW_SIZES[1], context);
                assert_eq!(
                    widget.config.window_size, SHALLOW_WINDOW_SIZES[1],
                    "a row limit the book can fill is taken"
                );
            });
        });
    }

    /// The depth the route publishes is read before the stream opens, so a stored
    /// row limit that route cannot fill is replaced before it can reach the header
    /// or the request.
    #[gpui_kit::test]
    fn a_row_limit_the_route_cannot_fill_should_be_settled_before_the_stream_opens(
        cx: &mut TestAppContext,
    ) {
        let (widget, reports) = widget_over_source_reporting(
            cx,
            OrderBookConfig {
                window_size: SHALLOW_WINDOW_SIZES[0],
                ..config()
            },
            PendingSource,
            Published::Listings(vec!["BTC"]),
        );

        assert_eq!(
            cx.read(|cx| widget.read(cx).config.window_size),
            WINDOW_SIZES[0],
            "the route's own depth settles the stored choice"
        );
        assert!(
            reports.get() > 0,
            "the replacement is reported so it is written back"
        );
    }

    #[gpui_kit::test]
    fn reduce_motion_should_put_the_bars_where_they_belong(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.update(|cx| {
            cx.set_reduce_motion(true);
            accept_book(&widget, snapshot(1, &[("101", 1)], &[("100", 1)]), cx);
            accept_book(&widget, moved_snapshot(2), cx);

            let widget = widget.read(cx);
            assert_eq!(widget.depth_animation.progress, 1.0);
            assert!(
                !widget.depth_animation.is_active(),
                "a movement nobody asked to see asks for no frames"
            );
        });
    }

    /// The header is the widget's own; the pane decides where the two groups go.
    #[gpui_kit::test]
    fn the_header_should_offer_the_instrument_control_and_the_drawing_controls(
        cx: &mut TestAppContext,
    ) {
        let widget = widget(cx);
        let (_, cx) = cx.add_window_view(|_, _| HeaderMarker);

        let header =
            cx.update(|window, cx| widget.update(cx, |widget, cx| widget.header(window, cx)));

        assert!(header.left.is_some(), "the instrument control belongs left");
        assert!(
            header.right.is_some(),
            "the grouping and row-limit pickers belong right"
        );
    }

    /// Draws one more frame with every cache missed, so what a debug selector reports is what this
    /// frame painted rather than what the frame before it left behind. A paint action is what fills
    /// a selector, and a frame that hits the body cache reuses the last paint range without drawing
    /// (AGENTS.md §10); `window.refresh()` is the one frame that cannot hit it.
    fn repaint(window: &mut VisualTestContext) {
        window.update(|window, _| window.refresh());
        window.run_until_parked();
    }

    /// The body is one canvas per side plus a handful of rows, and nothing else
    /// measures it: rendering it once is what proves the frame is well formed.
    ///
    /// A resize is what also exercises the regroup a canvas asks for from inside
    /// its own layout pass, which is the one place the widget may not update
    /// itself directly.
    #[gpui_kit::test]
    fn the_body_should_draw_the_ladder_it_was_given(cx: &mut TestAppContext) {
        let widget = widget(cx);
        cx.update(|cx| {
            accept_book(&widget, snapshot(1, &[("101", 1)], &[("100", 1)]), cx);
        });

        let (_, cx) = cx.add_window_view({
            let widget = widget.clone();
            move |_, _| BodyHarness(widget)
        });
        cx.simulate_resize(size(px(320.0), px(240.0)));
        cx.run_until_parked();
        // The ladder is drawn inside the body, which is the one cached layer, so the frame this
        // assertion reads has to be one that could not have hit the cache (AGENTS.md §10).
        repaint(cx);

        assert!(
            cx.debug_bounds("ORDER_BOOK_LADDER").is_some(),
            "the body draws its ladder"
        );
        assert_eq!(
            cx.read(|cx| widget.read(cx).effective_window_size()),
            4,
            "the canvas reported the rows a 240px pane leaves for each side"
        );
        assert_eq!(cx.read(|cx| ladder_rows(&widget, cx)), (1, 1));
    }

    /// The window root the body test renders into: the widget's own body, with
    /// none of the framework around it.
    struct BodyHarness(Entity<OrderBookWidget>);

    impl Render for BodyHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.0.clone())
        }
    }

    /// The window root the header test renders into.
    struct HeaderMarker;

    impl Render for HeaderMarker {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// The debug selector of the picker's own panel.
    ///
    /// A test outside `instrument_selector_ui` has no other way to see that the picker is up, and
    /// seeing it is what makes the drop below evidence rather than an accident.
    const SELECTOR_PANEL: &str = "INSTRUMENT_SELECTOR";

    /// The window root the picker test renders into: the pane's own header control, with none of
    /// the pane around it.
    ///
    /// What it holds of the widget is a weak handle, because a root holding the widget strongly
    /// would be the thing keeping it alive and there would be nothing left to measure.
    struct SelectorHarness(WeakEntity<OrderBookWidget>);

    impl Render for SelectorHarness {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .children(
                    self.0
                        .update(cx, |widget, cx| widget.instrument_button(cx))
                        .ok(),
                )
        }
    }

    /// Lets the picker's own fade finish and repaints, so what is drawn is what has settled.
    fn settle(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    /// The picker is handed a weak handle on the pane, so the dialog being up is not what keeps
    /// the pane alive: a pane released with its own picker still showing goes away.
    ///
    /// The pane would otherwise be held for as long as the selector is open — and a pane that
    /// cannot be released while its picker is up is a pane whose stream, tables and buffers all
    /// outlive the closed workspace.
    #[gpui_kit::test]
    fn an_open_selector_should_not_keep_the_pane_alive(cx: &mut TestAppContext) {
        let widget = widget(cx);
        let weak = widget.downgrade();
        let (_, cx) = cx.add_window_view({
            let weak = weak.clone();
            move |_, cx| Layer::new(cx.new(move |_| SelectorHarness(weak)))
        });
        cx.simulate_resize(size(px(400.0), px(120.0)));
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(SELECTOR_PANEL).is_none(),
            "nothing is open before the header control is clicked"
        );
        cx.simulate_click(point(px(200.0), px(60.0)), Modifiers::none());
        settle(cx);
        assert!(
            cx.debug_bounds(SELECTOR_PANEL).is_some(),
            "the header control should have opened the picker on the pane's own scope"
        );

        // The pane goes with this drop: the picker is open, and what it holds of the pane is the
        // weak handle its confirmation callback captured.
        drop(widget);
        settle(cx);

        assert!(
            weak.upgrade().is_none(),
            "an open picker should not be what keeps the pane it was opened from alive"
        );
    }

    #[test]
    fn a_config_should_round_trip_through_its_persisted_form() {
        let config = OrderBookConfig {
            price_multiplier: PRICE_MULTIPLIERS[3],
            window_size: WINDOW_SIZES[1],
            ..config()
        };
        let envelope = VersionedWidgetConfig::encode(&config).expect("config should encode");
        let decoded = envelope.decode::<OrderBookConfig>();

        assert_eq!(decoded, config);
    }

    #[test]
    fn an_unselected_config_should_round_trip_through_its_persisted_form() {
        let config = OrderBookConfig::default();
        let envelope = VersionedWidgetConfig::encode(&config).expect("config should encode");
        let decoded = envelope.decode::<OrderBookConfig>();

        assert_eq!(decoded.selection, None);
        assert_eq!(decoded, config);
    }

    /// The payload a build before the listing migration wrote named a route and an
    /// instrument separately, which is not a listing this build can read. Reading
    /// never fails: the whole config comes back as its defaults, and the framework
    /// writes that back.
    #[test]
    fn an_old_payload_should_decode_to_the_defaults() {
        let stored = r#"{"schema_version":1,"payload":"{\"selection\":{\"route\":{\"venue\":\"binance\",\"market\":\"spot\"},\"instrument\":{\"kind\":\"spot\"}},\"price_multiplier\":10,\"window_size\":50}"}"#;
        let envelope: VersionedWidgetConfig =
            serde_json::from_str(stored).expect("the stored envelope should parse");

        let decoded = envelope.decode::<OrderBookConfig>();

        assert_eq!(decoded, OrderBookConfig::default());
        assert_eq!(decoded.selection, None);
    }

    #[test]
    fn an_unsupported_grouping_should_reset_to_the_default() {
        let mut config = OrderBookConfig {
            price_multiplier: 7,
            ..OrderBookConfig::default()
        };

        assert!(
            config.settle(None),
            "a replaced choice is reported as a change"
        );

        assert_eq!(
            config.price_multiplier,
            OrderBookConfig::default().price_multiplier
        );
    }

    #[test]
    fn an_unsupported_row_limit_should_reset_to_the_default() {
        let mut config = OrderBookConfig {
            window_size: 60,
            ..OrderBookConfig::default()
        };

        assert!(config.settle(Some(FIXTURE_DEPTH)));

        assert_eq!(config.window_size, OrderBookConfig::default().window_size);
    }

    #[test]
    fn a_supported_choice_should_survive_settling() {
        let mut config = OrderBookConfig {
            price_multiplier: PRICE_MULTIPLIERS[4],
            window_size: WINDOW_SIZES[2],
            ..OrderBookConfig::default()
        };

        assert!(
            !config.settle(Some(FIXTURE_DEPTH)),
            "nothing was replaced, so nothing is reported"
        );

        assert_eq!(config.price_multiplier, PRICE_MULTIPLIERS[4]);
        assert_eq!(config.window_size, WINDOW_SIZES[2]);
    }

    /// A route whose venue publishes at most twenty levels per side cannot fill the
    /// standard row limits, so a choice from the deep ladder goes back to the default
    /// the shallow ladder starts with.
    #[test]
    fn a_row_limit_a_shallow_route_cannot_fill_should_reset_to_its_own_default() {
        let mut config = OrderBookConfig {
            window_size: WINDOW_SIZES[2],
            ..OrderBookConfig::default()
        };

        assert!(config.settle(Some(20)));

        assert_eq!(config.window_size, SHALLOW_WINDOW_SIZES[2]);
    }

    /// A pane that follows nothing has no route to judge its row limit against, so
    /// the stored choice is left alone until one is chosen.
    #[test]
    fn a_row_limit_should_survive_settling_without_a_route() {
        let mut config = OrderBookConfig {
            window_size: SHALLOW_WINDOW_SIZES[0],
            ..OrderBookConfig::default()
        };

        assert!(!config.settle(None));

        assert_eq!(config.window_size, SHALLOW_WINDOW_SIZES[0]);
    }

    #[test]
    fn the_mid_price_should_take_the_colour_of_the_direction_it_moved() {
        let theme = Theme::default();
        let chart = &theme.components.chart;
        let lower = Some(Price::from(100_u64));
        let higher = Some(Price::from(101_u64));
        let (up_color, up_direction) = mid_price_direction(higher, lower, &theme);
        let (down_color, down_direction) = mid_price_direction(lower, higher, &theme);
        let (flat_color, flat_direction) = mid_price_direction(lower, lower, &theme);
        let (first_color, first_direction) = mid_price_direction(lower, None, &theme);

        assert_eq!(up_color, chart.bullish.hsla);
        assert!(matches!(up_direction, Some(IconKind::ArrowUp)));
        assert_eq!(down_color, chart.bearish.hsla);
        assert!(matches!(down_direction, Some(IconKind::ArrowDown)));
        assert_eq!(flat_color, theme.roles.market.flat.hsla);
        assert!(flat_direction.is_none());
        assert_eq!(first_color, theme.roles.market.flat.hsla);
        assert!(first_direction.is_none());
    }

    #[test]
    fn an_imperceptible_movement_should_not_request_frames() {
        let mut animation = DepthAnimationState::settled();

        animation.restart(DEPTH_ANIMATION_SETTLE_EPSILON / 2.0);

        assert_eq!(animation.progress, 1.0);
        assert!(!animation.is_active());
    }

    #[test]
    fn a_settled_animation_should_advance_without_a_frame() {
        let mut animation = DepthAnimationState::settled();
        animation.restart(1.0);

        let started_at = Instant::now();
        assert_eq!(animation.advance(started_at), 0.0);
        let progress = animation.advance(started_at + Duration::from_secs(1));

        assert_eq!(progress, 1.0);
        assert!(!animation.is_active());
    }

    /// Both columns of the ladder are named by the display codes the published
    /// generation decided on.
    ///
    /// The column labels are user-visible text, and neither the venue's own symbol
    /// nor a canonical identity is one of them.
    #[gpui_kit::test]
    fn the_ladder_should_name_published_codes(cx: &mut TestAppContext) {
        let _widget = widget_over(cx, config(), Published::Listings(vec!["BTC"]));
        settle_until(cx, "the fixture route to publish its listing", |cx| {
            cx.global::<MarketParticipantsConnector>()
                .catalog()
                .cached_listing_instrument(&listing("BTC"))
                .is_ok()
        });

        cx.update(|cx| {
            let labels = OrderBookUnitLabels::of(&metadata(), cx);

            assert_eq!(
                labels.price.as_ref(),
                "USDT",
                "the price column is quoted in the code the generation published"
            );
            assert_eq!(
                labels.quantity.as_ref(),
                "BTC",
                "the quantity column counts the code the generation published"
            );
        });
    }

    /// An asset the published generation does not name is the localized unknown
    /// rather than the identity the book's metadata carries.
    ///
    /// A digest is not a name, and a code invented here would disagree with every
    /// other pane about the same asset.
    #[gpui_kit::test]
    fn an_unpublished_asset_should_be_named_as_unknown(cx: &mut TestAppContext) {
        let _widget = widget_over(cx, config(), Published::Listings(vec!["BTC"]));
        settle_until(cx, "the fixture route to publish its listing", |cx| {
            cx.global::<MarketParticipantsConnector>()
                .catalog()
                .cached_listing_instrument(&listing("BTC"))
                .is_ok()
        });

        cx.update(|cx| {
            let unpublished =
                market_participants_connector::catalog::test_support::token("binance", "NOWHERE");
            let labels = OrderBookUnitLabels::of(
                &OrderBookMetadata {
                    quote_asset: unpublished.clone(),
                    quantity_unit: QuantityUnit::AssetUnits {
                        asset: unpublished.clone(),
                    },
                    ..metadata()
                },
                cx,
            );
            let identity = unpublished.to_string();
            let unknown = t!(cx, "error.display.unknown_asset");

            assert_eq!(labels.price, unknown);
            assert_eq!(labels.quantity, unknown);
            assert_ne!(
                labels.price.as_ref(),
                identity.as_str(),
                "a canonical identity is not a name"
            );
            assert!(
                !labels.price.contains("crypto-token") && !labels.price.contains("v1/"),
                "the column states a name, not an identity: {}",
                labels.price
            );
        });
    }
}

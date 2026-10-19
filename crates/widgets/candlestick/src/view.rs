//! The candlestick chart widget.
//!
//! The widget owns one exact listing and one interval, and draws whatever the
//! live candle stream publishes for that scope. Everything the chart *does*
//! around a frame — connecting, loading, paginating, failing — is reported
//! through the framework's status handle; the body only ever draws the candles
//! it has.
//!
//! The scope is settled before it is requested: the listing is asked about in the
//! published directory, and the interval is the request's own business because
//! the descriptor a widget may read names a capability's transports rather than
//! its intervals. A scope either answer proves unsupported goes back to its
//! default and is reported, so a stored value the venue no longer serves is
//! never what the chart keeps failing on.
//!
//! Three pieces of local state make the chart feel like a chart rather than a
//! picture, and each of them is deliberately kept out of the status:
//!
//! - **the viewport**, which is a candle width plus an optional right-hand
//!   anchor. Following the live edge is expressed as *no anchor*, so a chart
//!   that is following keeps following when a new candle arrives;
//! - **the countdown**, which ticks once a second and repaints the body only
//!   when the label it draws actually changes;
//! - **pagination**, which is a pair of in-flight flags rather than a status,
//!   because a page arriving changes the candles rather than what the widget is
//!   reporting.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    empty_state::EmptyState,
    interactive::InteractiveVariantExt,
    menu::{MenuItem, MenuItemAction, MenuItemGroup},
    select::Select,
    size::UnifiedSizable,
};
use gpui_kit::{
    AnyElement, App, Context, CursorStyle, ElementId, Entity, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Pixels, Point, Render, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement, Styled, Task, Window, base::Disableable, div,
    prelude::FluentBuilder, px,
};
use i18n::t;
use instrument_selector_ui::{
    instrument_button::InstrumentButton,
    instrument_selector::{
        InstrumentFilters, InstrumentKindFilter, InstrumentStatusFilter, LoadedScope,
    },
};
use market_participants_connector::{
    catalog::error::CatalogError,
    connector::MarketParticipantsConnector,
    error::MarketParticipantError,
    market::{
        error::MarketError,
        timeframe::{Adjustment, TimeFrame},
    },
    venue::loaded::VenuesUpdated,
};
use market_participants_model::{
    identifier::VenueId, listing::route::VenueRoute, market::selection::InstrumentSelection,
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
    time_zone::notify_on_data_time_zone_change,
    view::{WidgetHeader, WidgetView},
};
use widget_tokio_runtime::global;

use super::{
    chart::{
        geometry::{CandleGeometry, ChartLayout, ChartViewport, visible_range},
        paint::{
            ChartCanvasOptions, ChartPaintCaches, chart_canvas, latest_price_indicator_is_visible,
        },
        text::HoverLabelCache,
    },
    model::CandlestickSeriesSnapshot,
    service::{
        CandlestickProtocol, CandlestickRequest, CandlestickResponse, CandlestickService,
        CandlestickServiceError, CandlestickSource, CandlestickStatus, CandlestickStreamKey,
        CandlestickStreamMessage,
    },
    time::{
        TIMEFRAME_GROUPS, countdown_refresh_delay, current_timestamp_millis,
        next_second_boundary_delay, offered_timeframes, remaining_time_label, tick_and_unit,
        timeframe_of_label,
    },
};

/// How many candles the chart keeps. Older ones are evicted as newer arrive.
const MAXIMUM_CANDLES: usize = 1_500;
/// How much history one page holds.
const HISTORY_PAGE_SIZE: u16 = 300;
/// How close to the edge of the loaded range panning has to come before the next
/// page is requested.
const PREFETCH_MARGIN: usize = 120;
const MIN_CANDLE_WIDTH: f32 = 5.0;
const MAX_CANDLE_WIDTH: f32 = 25.0;
/// One wheel step changes the candle width by one pixel, so a single frame's
/// accumulated steps only have to cover the whole zoom range to be enough.
const MAX_ZOOM_STEPS_PER_FRAME: i32 = (MAX_CANDLE_WIDTH - MIN_CANDLE_WIDTH) as i32;

/// The scope a chart follows, persisted with the widget.
///
/// The selection is optional because a chart can exist before the user has
/// chosen anything: it then asks for an instrument instead of charting a default
/// one nobody asked for.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CandlestickConfig {
    selection: Option<InstrumentSelection>,
    timeframe: TimeFrame,
    adjustment: Adjustment,
}

impl Default for CandlestickConfig {
    fn default() -> Self {
        Self {
            selection: None,
            timeframe: TimeFrame::Minute1,
            adjustment: Adjustment::None,
        }
    }
}

impl WidgetConfig for CandlestickConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CandlestickFailure {
    Market(MarketError),
    Rpc(RpcCallError<CandlestickServiceError>),
}

impl LocalizedError for CandlestickFailure {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Market(error) => error.localized_message(cx),
            Self::Rpc(error) => error.localized_message(cx),
        }
    }
}

/// Whether a market rejection says the interval itself is not published.
///
/// The route's own capability descriptor is what decides which intervals it
/// publishes, and a descriptor a widget may read names its capability's
/// transports rather than its intervals: a request is the only answer the chart
/// can get, so its rejection is read here rather than guessed from a static list.
fn is_unsupported_timeframe(error: &MarketError) -> bool {
    matches!(
        error,
        MarketError::Network(network)
            if matches!(network.error(), MarketParticipantError::UnsupportedTimeFrame { .. })
    )
}

/// Whether a market rejection says the adjustment itself is not published.
///
/// The same reading as [`is_unsupported_timeframe`], for the other dimension of a
/// candlestick request: the route's own descriptor decides which conventions it
/// publishes, and its rejection is the only answer the chart can get.
fn is_unsupported_adjustment(error: &MarketError) -> bool {
    matches!(
        error,
        MarketError::Network(network)
            if matches!(network.error(), MarketParticipantError::UnsupportedAdjustment { .. })
    )
}

/// Whether a market rejection says the route does not serve candlesticks at all.
fn is_unsupported_capability(error: &MarketError) -> bool {
    matches!(
        error,
        MarketError::UnsupportedCapability {
            capability: CapabilityId::Candlesticks,
            ..
        }
    )
}

/// The message key one price-adjustment convention is named by.
///
/// Every convention a route can publish is named here, so a convention this build grows
/// without a word for it is a compile error rather than a blank entry in the control.
const fn adjustment_key(adjustment: Adjustment) -> &'static str {
    match adjustment {
        Adjustment::None => "widget.candlestick.adjust_none",
        Adjustment::Forward => "widget.candlestick.adjust_forward",
    }
}

/// The venues the waiting notice lists: every venue whose loader can answer with candles.
///
/// A chart reads one venue — the one behind the instrument the user followed — so the
/// markets it waits for follow the selection. What it can read at all does not: that is
/// this capability's venue set off the definition table, which is the same answer whether
/// or not anything is loaded and whichever instrument is followed.
///
/// The venues are handed over as identities rather than as names: the sentence that names
/// them is resolved by the framework, in the language showing on the frame that draws it.
fn listed_venues() -> Vec<VenueId> {
    market_participants_connector::venue::definition::venues_supporting(CapabilityId::Candlesticks)
}

/// Whether any venue that can answer with candles is in memory.
///
/// This is the first half of the pane's precondition, and it is asked before the selection is
/// looked at on purpose: the picker a chart with nothing followed draws is built from the loaded
/// venues, so with none of them loaded there is nothing to choose from, and "nothing is chosen"
/// and "the market is not loaded" are one and the same thing to the user. Once one is loaded, a
/// chart that still follows nothing is the "select an instrument" notice's business.
fn any_market_loaded(cx: &App) -> bool {
    let venues = cx.global::<MarketParticipantsConnector>().venues();
    listed_venues()
        .iter()
        .any(|venue| venues.loaded(venue).is_some())
}

/// Whether the directory proved that the listing is not chartable.
///
/// A route that is no longer registered, a listing that is absent from the
/// published route and a listing of another route are answers; a catalog that
/// could not be fetched is not, and neither is an operation the runtime lost.
fn is_definitely_unsupported(error: &CatalogError) -> bool {
    matches!(
        error,
        CatalogError::RouteNotRegistered { .. }
            | CatalogError::ListingNotFound { .. }
            | CatalogError::ListingRouteMismatch { .. }
    )
}

/// The countdown under the latest price.
///
/// The label only moves once a second, so the task that drives it compares
/// against the previous label and reports whether anything a frame would draw
/// actually changed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CountdownState {
    candle_timestamp: Option<i64>,
    label: SharedString,
}

impl Default for CountdownState {
    fn default() -> Self {
        Self {
            candle_timestamp: None,
            label: SharedString::new_static("00:00"),
        }
    }
}

impl CountdownState {
    fn clear(&mut self) {
        *self = Self::default();
    }

    /// Adopts a candle from a snapshot, ignoring price updates to the candle the
    /// timer is already counting down.
    fn observe_candle(
        &mut self,
        candle_timestamp: i64,
        timeframe: TimeFrame,
        venue: &VenueId,
        now_timestamp: i64,
    ) -> bool {
        if self.candle_timestamp == Some(candle_timestamp) {
            return false;
        }
        self.candle_timestamp = Some(candle_timestamp);
        self.update_label(candle_timestamp, timeframe, venue, now_timestamp)
    }

    fn tick(
        &mut self,
        candle_timestamp: i64,
        timeframe: TimeFrame,
        venue: &VenueId,
        now_timestamp: i64,
    ) -> bool {
        self.candle_timestamp = Some(candle_timestamp);
        self.update_label(candle_timestamp, timeframe, venue, now_timestamp)
    }

    fn update_label(
        &mut self,
        candle_timestamp: i64,
        timeframe: TimeFrame,
        venue: &VenueId,
        now_timestamp: i64,
    ) -> bool {
        let label = SharedString::new(
            remaining_time_label(candle_timestamp, timeframe, venue, now_timestamp).as_str(),
        );
        if self.label == label {
            return false;
        }
        self.label = label;
        true
    }
}

/// One pointer position, waiting for the next frame.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PointerInput {
    position: Point<Pixels>,
    dragging: bool,
}

/// The pointer input of one frame.
///
/// Mouse moves arrive faster than frames, so only the newest position is kept:
/// what the frame does with it is idempotent, and applying each event would
/// rebuild the chart several times per frame.
#[derive(Default)]
struct PointerFrameState {
    pending: Option<PointerInput>,
    scheduled: bool,
}

impl PointerFrameState {
    fn queue(&mut self, input: PointerInput) -> bool {
        self.pending = Some(input);
        if self.scheduled {
            return false;
        }
        self.scheduled = true;
        true
    }

    fn take(&mut self) -> Option<PointerInput> {
        self.scheduled = false;
        self.pending.take()
    }

    fn clear_pending(&mut self) {
        self.pending = None;
    }
}

/// One wheel event, waiting for the next frame.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ScrollInput {
    position: Point<Pixels>,
    steps: i32,
}

/// The wheel input of one frame.
///
/// Wheel steps accumulate rather than overwrite, because each one is a zoom
/// amount rather than a position, and they accumulate relative to the newest
/// anchor so that zooming under a moving pointer still follows it.
#[derive(Default)]
struct ScrollFrameState {
    pending: Option<ScrollInput>,
    scheduled: bool,
}

impl ScrollFrameState {
    fn queue(&mut self, input: ScrollInput) -> bool {
        if let Some(pending) = self.pending.as_mut() {
            pending.position = input.position;
            pending.steps = pending
                .steps
                .saturating_add(input.steps)
                .clamp(-MAX_ZOOM_STEPS_PER_FRAME, MAX_ZOOM_STEPS_PER_FRAME);
        } else {
            self.pending = Some(ScrollInput {
                steps: input
                    .steps
                    .clamp(-MAX_ZOOM_STEPS_PER_FRAME, MAX_ZOOM_STEPS_PER_FRAME),
                ..input
            });
        }
        if self.scheduled {
            return false;
        }
        self.scheduled = true;
        true
    }

    fn take(&mut self) -> Option<ScrollInput> {
        self.scheduled = false;
        self.pending.take()
    }

    fn clear_pending(&mut self) {
        self.pending = None;
    }
}

pub struct CandlestickWidget {
    config: CandlestickConfig,
    binding: WidgetBinding<CandlestickProtocol>,
    status: Entity<WidgetStatus>,
    /// The recovery the framework offers for every failure this widget reports.
    recovery: WidgetAction,
    config_change: WidgetConfigChange,
    directory_generation: u64,
    snapshot: Option<CandlestickSeriesSnapshot>,
    countdown: CountdownState,
    countdown_task: Option<Task<()>>,
    viewport: ChartViewport,
    /// Filled in by the chart's layout pass, so that pointer events — which
    /// arrive outside any layout — can be hit-tested.
    chart_layout: Rc<Cell<Option<ChartLayout>>>,
    hover_label_cache: Rc<RefCell<HoverLabelCache>>,
    chart_paint_caches: Rc<ChartPaintCaches>,
    pointer_frame: PointerFrameState,
    scroll_frame: ScrollFrameState,
    dragging: bool,
    drag_last_x: Option<Pixels>,
    drag_remainder: f32,
    fetching_history: bool,
    fetching_newer: bool,
    /// Whether the chart is waiting for its market rather than reading it.
    ///
    /// The pane keeps its stream while it waits — the alternative is cancelling the
    /// binding, which is terminal, and this pane has to be able to read the market
    /// again once the user loads it — so a message that was in flight when the
    /// route went away can still arrive. Accepting it would put a snapshot back and
    /// make the pane report content over the notice the framework is drawing, so every
    /// message entry refuses while this is set. The stream keys cannot do this job:
    /// they say which scope a message belongs to, and this one belongs to the scope
    /// the notice is about.
    awaiting_market: bool,
    /// Whether the venue this chart reads was loaded or refreshed again.
    ///
    /// Such a commit replaces the catalog generation the live stream resolved its rules against,
    /// while the stream itself keeps the rules, status and native identifiers it resolved when it
    /// was opened. So the chart has to start over, and the request it sends has to be the one that
    /// opens a new stream: [`Self::load`] reads this after its own gate — a chart a shrinking
    /// commit sent back to waiting keeps it, so the load that brings the market back reopens the
    /// stream too — and it is what makes the request a `Retry` rather than the idempotent `Load`,
    /// which the service answers without touching a stream for that scope that is already live.
    market_reloaded: bool,
    /// The intervals the chart's route publishes, asked before anything is subscribed.
    ///
    /// The service owns the source, so the chart keeps this one question from it: which
    /// intervals a route serves is what decides whether the interval the chart persisted is
    /// used at all, and the answer has to arrive before the request does. The question is a
    /// lookup in the route's own descriptor, not a request to the venue.
    published_timeframes: Rc<dyn Fn(&VenueRoute) -> Result<Vec<TimeFrame>, MarketError>>,
    /// The price-adjustment conventions the chart's route publishes, asked before anything is
    /// subscribed.
    ///
    /// The other dimension of a candlestick request, asked the same way and for the same
    /// reason: a convention this build persisted may have stopped being published, and the
    /// route's own declaration is what answers whether it still is.
    published_adjustments: Rc<dyn Fn(&VenueRoute) -> Result<Vec<Adjustment>, MarketError>>,
    /// Wakes the chart when the zone its timestamps are read in moves.
    _time_zone_task: Option<Task<()>>,
}

impl WidgetView for CandlestickWidget {
    type Config = CandlestickConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.market.candlestick");
    const DISPLAY_NAME: &'static str = "widget.names.candlestick";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;
    const DESCRIPTION: &'static str = "Live and historical OHLCV chart";
    const ICON: Option<&'static str> = Some("widgets/candlestick.png");

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

    fn load(&mut self, cx: &mut Context<Self>) {
        // The market this chart follows has to be loaded before anything else
        // happens here, and the check is first on purpose: a route the user never
        // loaded, or one a shrinking commit has since taken away, leaves the
        // pane waiting for it instead of reaching the branches below, which settle
        // the selection the user stored.
        if !self.market_ready(cx) {
            self.wait_for_market(cx);
            return;
        }
        // Read past the gate: a commit on the chart's own venue that took its market away leaves
        // this set while the stream that resolved against the retired generation is still alive,
        // and the load that brings the market back has to reopen that stream.
        let market_reloaded = std::mem::take(&mut self.market_reloaded);
        let retry = self.status.is_failed(cx);
        self.resolve_timeframe(cx);
        self.resolve_adjustment(cx);
        self.start_current(retry, market_reloaded, cx);
    }

    /// The options this widget persists.
    ///
    /// The framework asks for these to write the pane back, so this answers with the
    /// field itself rather than computing anything.
    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    /// The chart is content as soon as a snapshot has been accepted — including
    /// an empty one. An empty chart is what "this instrument has no candles" looks
    /// like: the axes are still there, and there is nothing to report about it.
    fn has_content(&self) -> bool {
        self.snapshot.is_some()
    }

    /// The scope on the left; the interval and the adjustment convention on the right.
    ///
    /// Both controls on the right read the route behind the chart, so neither is any use before
    /// the market participant's metadata is: the interval picker refuses input while the chart waits for
    /// its market, exactly as it does with nothing chosen — every interval it would offer is one
    /// the route publishes, and a route whose market is not in memory cannot be asked — and the
    /// adjustment control is not drawn, because what it would state is that same answer. The
    /// instrument picker on the left disables itself whenever the loaded venues offer it nothing
    /// to select from.
    fn header(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> WidgetHeader {
        let controls = div()
            .flex()
            .items_center()
            .gap_1()
            .children(self.adjustment_select(cx))
            .child(self.timeframe_select(cx));
        WidgetHeader::empty()
            .left(self.instrument_button(cx))
            .right(controls)
    }
}

impl CandlestickWidget {
    fn new_with_source<S>(
        config: CandlestickConfig,
        source: S,
        status: Entity<WidgetStatus>,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: CandlestickSource,
    {
        // The service takes the source, so the chart keeps these questions from it: the
        // intervals and the adjustment conventions a route publishes are asked before every
        // subscription, and a clone of the source is what answers both.
        let published_timeframes = {
            let source = source.clone();
            Rc::new(move |route: &VenueRoute| source.supported_timeframes(route))
        };
        let published_adjustments = {
            let source = source.clone();
            Rc::new(move |route: &VenueRoute| source.supported_adjustments(route))
        };
        let service = CandlestickService::new(source, HISTORY_PAGE_SIZE, MAXIMUM_CANDLES);
        let binding = WidgetBinding::spawn(
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
            countdown: CountdownState::default(),
            countdown_task: None,
            viewport: ChartViewport::default(),
            chart_layout: Rc::new(Cell::new(None)),
            hover_label_cache: Rc::new(RefCell::new(HoverLabelCache::default())),
            chart_paint_caches: Rc::new(ChartPaintCaches::default()),
            pointer_frame: PointerFrameState::default(),
            scroll_frame: ScrollFrameState::default(),
            dragging: false,
            drag_last_x: None,
            drag_remainder: 0.0,
            fetching_history: false,
            fetching_newer: false,
            awaiting_market: false,
            market_reloaded: false,
            published_timeframes,
            published_adjustments,
            _time_zone_task: notify_on_data_time_zone_change(cx),
        }
    }

    /// Whether what this chart needs is in memory: at least one market that answers with candles,
    /// and the route of the stored selection among them.
    ///
    /// A chart that follows nothing still needs a market: the picker is drawn from the loaded
    /// venues, so with none of them loaded there is nothing to choose from — the pane waits for
    /// the market participant's metadata and says so, rather than asking for a choice that is not there.
    /// Once one is loaded, a chart that still follows nothing is the "select an instrument"
    /// notice's business and this answers `true` for it.
    ///
    /// Reading a market is only possible once the user has loaded it, and "not
    /// loaded" is a wait rather than a failure. One question answers each route: a scope in
    /// the loaded venues is a scope whose catalog and metadata committed with it
    ///, so there is no "in play but the directory has not arrived" state left to
    /// ask about.
    fn market_ready(&self, cx: &App) -> bool {
        if !any_market_loaded(cx) {
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

    /// Reports the neutral notice for the market this chart follows, and drops the
    /// chart it was drawing.
    ///
    /// The route behind that chart is gone, so nothing on screen is backed by a
    /// market any more, and the framework only draws the notice while the pane
    /// reports no content — which is why the pane also stops accepting messages: the
    /// stream it was reading is still alive, and a snapshot that was already in flight
    /// would otherwise put content back over the notice. The stored selection is left
    /// alone: it is the user's, and loading the market again is what brings it back.
    fn wait_for_market(&mut self, cx: &mut Context<Self>) {
        self.awaiting_market = true;
        self.clear_loaded_state();
        self.report_notice(cx);
        cx.notify();
    }

    /// Reports the notice the pane is standing in with: the market it waits for, or the
    /// instrument it has nothing chosen for.
    ///
    /// The list the sentence ends with is every venue that answers with candles — the
    /// capability this pane is defined by, read off the definition table rather than off
    /// what happens to be loaded — and the venue it waits for is the one behind the
    /// selection. A chart that is waiting with nothing chosen has no single venue to
    /// name, because no choice has narrowed the wait down to one: the whole list is
    /// what it is waiting for.
    ///
    /// Which of the two references applies is what the wait itself says: the paths that
    /// drop the chart for a market set the flag, and the one that starts reading again
    /// clears it, so a chart that reached [`Self::start_current`] past its own gate is
    /// asking for an instrument rather than waiting for a venue.
    ///
    /// This is the one place that reference is produced — [`Self::wait_for_market`] and
    /// [`Self::start_current`] are the two that stand in one of them — and the sentence is
    /// the framework's to resolve, in the language showing on the frame that draws it.
    fn report_notice(&self, cx: &mut Context<Self>) {
        let notice = if self.awaiting_market {
            let venues = self
                .config
                .selection
                .as_ref()
                .map(|selection| vec![selection.listing.route.venue])
                .unwrap_or_else(listed_venues);
            Notice::MarketNotLoaded {
                venues,
                supported: listed_venues(),
            }
        } else {
            Notice::message("widget.candlestick.select_instrument_prompt")
        };
        self.status.report(WidgetState::waiting(notice), cx);
    }

    /// Runs the load a venue commit may have unblocked.
    ///
    /// A commit can matter to this chart for either of two reasons. The first is that it changes
    /// whether the market it needs is in memory, and the pane already holds that answer in the two
    /// things it maintains anyway: `awaiting_market` says it is not reading because a market it
    /// needs is missing, and `market_ready` says everything it needs is there now. A pane that is
    /// waiting for one and can now read starts over rather than re-checking; a pane that was
    /// reading one that is gone goes back to waiting.
    ///
    /// The second is that it landed on the venue behind the stored selection, which is the user
    /// loading or refreshing the market this chart reads: the catalog generation the live stream
    /// resolved its rules and precision against is replaced, so the chart starts over — see
    /// `market_reloaded`, which is what makes the request the one that opens a new stream. A commit
    /// of any other venue is neither, and leaves the pane and its stream exactly as they are. A
    /// failure is terminal: the event does not revive a failed pane, only the user's own recovery
    /// does.
    fn on_venues_updated(&mut self, event: &VenuesUpdated, cx: &mut Context<Self>) {
        if self.status.is_failed(cx) {
            return;
        }
        // A commit on the venue the stored selection is on is this chart's own market being
        // loaded again, so the chart cannot keep reading the stream it holds.
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

    /// The stream scope this chart would accept messages from.
    ///
    /// Absent until an instrument is chosen: nothing is streaming, so nothing is
    /// in scope.
    fn current_key(&self) -> Option<CandlestickStreamKey> {
        self.config.selection.as_ref().map(|selection| {
            CandlestickStreamKey::new(
                selection.listing.clone(),
                self.config.timeframe,
                self.config.adjustment,
            )
        })
    }

    /// Drops everything that was derived from the previous scope.
    ///
    /// The caches hold shaped text and resolved boxes, and both are keyed on
    /// inputs that stop meaning anything once the listing or interval changes.
    fn clear_loaded_state(&mut self) {
        self.snapshot = None;
        self.countdown.clear();
        self.pointer_frame.clear_pending();
        self.scroll_frame.clear_pending();
        self.chart_layout.set(None);
        self.hover_label_cache.borrow_mut().clear();
        self.chart_paint_caches.clear();
        self.viewport.right_timestamp = None;
        self.viewport.cursor = None;
        self.fetching_history = false;
        self.fetching_newer = false;
    }

    /// Returns the chart to "nothing chosen", reporting it.
    ///
    /// The default scope is what a value the venue no longer serves goes back to,
    /// and it is reported so the pane is written back with the scope the chart is
    /// actually following instead of the one it stored.
    fn clear_selection(&mut self, reason: &str, cx: &mut Context<Self>) {
        log::warn!(
            "the chart went back to having nothing chosen: {reason}; \
             it had been following {:?}",
            self.config
                .selection
                .as_ref()
                .map(|selection| &selection.listing)
        );
        self.config.selection = None;
        self.config_change.publish(cx);
        self.start_current(false, false, cx);
    }

    /// Settles a scope the route's own answer proved unsupported.
    ///
    /// Only an answer the request itself produced reaches this: an interval or an
    /// adjustment convention the route's descriptor does not publish, or a route that
    /// does not serve candlesticks at all. The one item the rejection names goes back
    /// to its default — a neighbouring interval is not a substitute the user asked for —
    /// and the chart is restarted, so the rejection is never also reported as a
    /// failure. Anything else is a stream failure and stays one.
    ///
    /// The adjustment branch and [`Self::resolve_adjustment`] reset to the same neutral
    /// convention for the same reason: every route that serves candlesticks publishes it,
    /// so the value the chart falls back to is one the route answers. A route that
    /// published only `Adjustment::Forward` would make this branch land on a convention
    /// the route does not serve; `AdapterLimits::adjustments` and
    /// `every_candlestick_route_publishes_the_neutral_convention` are what rule that out.
    ///
    /// The adjustment is read before the capability because it is the narrower statement:
    /// a route that refuses one convention may still serve candles, and only the request
    /// that asked for that convention can tell the two apart.
    ///
    /// Returns whether the chart was restarted.
    fn recover_unsupported_scope(&mut self, error: &MarketError, cx: &mut Context<Self>) -> bool {
        let default = CandlestickConfig::default();
        if is_unsupported_timeframe(error) && self.config.timeframe != default.timeframe {
            log::warn!(
                "the interval {:?} is not published for the listing the chart follows; \
                 the chart fell back to {:?}",
                self.config.timeframe,
                default.timeframe
            );
            self.config.timeframe = default.timeframe;
            self.config_change.publish(cx);
            self.start_current(false, false, cx);
            return true;
        }
        if is_unsupported_adjustment(error) && self.config.adjustment != default.adjustment {
            log::warn!(
                "the adjustment {:?} is not published for the listing the chart follows; \
                 the chart fell back to {:?}",
                self.config.adjustment,
                default.adjustment
            );
            self.config.adjustment = default.adjustment;
            self.config_change.publish(cx);
            self.start_current(false, false, cx);
            return true;
        }
        if is_unsupported_capability(error) && self.config.selection.is_some() {
            self.clear_selection("the route does not serve candlesticks", cx);
            return true;
        }
        false
    }

    /// Resets an interval the chart's route does not publish, before anything subscribes.
    ///
    /// Rule 2 of the widget contract: an interval this build persisted may have stopped being
    /// published — the venue retired it, or the listing moved to a route that serves other
    /// intervals — and the chart only ever asks for an interval the route's own descriptor
    /// answers. The reset happens before the subscription, so the service never receives an
    /// interval the route does not serve, and it is reported, so the pane is written back with
    /// the interval the chart is actually following.
    ///
    /// Only a definite answer resets anything: a route this build cannot ask about proves
    /// nothing, so the interval is kept and the subscription reports its own failure with the
    /// recovery the pane already offers. The default interval is never re-written, because a
    /// reset to the value that is already stored is not a change.
    fn resolve_timeframe(&mut self, cx: &mut Context<Self>) {
        let default = CandlestickConfig::default();
        if self.config.timeframe == default.timeframe {
            return;
        }
        let Some(selection) = self.config.selection.as_ref() else {
            return;
        };
        match (self.published_timeframes)(&selection.listing.route) {
            Ok(published) if !published.contains(&self.config.timeframe) => {
                log::warn!(
                    "the interval {:?} is not published for {:?}; the chart follows {:?} \
                     instead of subscribing to an interval the route does not serve",
                    self.config.timeframe,
                    selection.listing,
                    default.timeframe
                );
                self.config.timeframe = default.timeframe;
                self.config_change.publish(cx);
            }
            Ok(_) => {}
            Err(error) => log::warn!(
                "asking which intervals {} publishes failed; the chart asks for {:?} anyway: \
                 {error}",
                selection.listing.route,
                self.config.timeframe
            ),
        }
    }

    /// Resets an adjustment convention the chart's route does not publish, before anything
    /// subscribes.
    ///
    /// Rule 2 of the widget contract, for the other dimension of a candlestick request: a
    /// convention this build persisted may have stopped being published — the venue retired
    /// it, or the listing moved to a route whose history is served another way — and the
    /// chart only ever asks for a convention the route's own descriptor answers. The reset
    /// happens before the subscription, so the service never receives a convention the route
    /// does not serve, and it is reported, so the pane is written back with the convention the
    /// chart is actually asking with.
    ///
    /// Only a definite answer resets anything: a route this build cannot ask about proves
    /// nothing, so the convention is kept and the subscription reports its own failure with
    /// the recovery the pane already offers. The default convention is never re-written,
    /// because a reset to the value that is already stored is not a change.
    ///
    /// The reset lands on the neutral convention because every route that serves
    /// candlesticks publishes it (`AdapterLimits::adjustments` says so, and
    /// `every_candlestick_route_publishes_the_neutral_convention` keeps it true), so this
    /// is a value the route answers rather than this widget's guess.
    fn resolve_adjustment(&mut self, cx: &mut Context<Self>) {
        let default = CandlestickConfig::default();
        if self.config.adjustment == default.adjustment {
            return;
        }
        let Some(selection) = self.config.selection.as_ref() else {
            return;
        };
        match (self.published_adjustments)(&selection.listing.route) {
            Ok(published) if !published.contains(&self.config.adjustment) => {
                log::warn!(
                    "the adjustment {:?} is not published for {:?}; the chart asks for {:?} \
                     instead of subscribing to a convention the route does not serve",
                    self.config.adjustment,
                    selection.listing,
                    default.adjustment
                );
                self.config.adjustment = default.adjustment;
                self.config_change.publish(cx);
            }
            Ok(_) => {}
            Err(error) => log::warn!(
                "asking which adjustment conventions {} publishes failed; the chart asks for \
                 {:?} anyway: {error}",
                selection.listing.route,
                self.config.adjustment
            ),
        }
    }

    /// Starts the stream, or replaces it for a scope the chart is starting over.
    ///
    /// The two booleans are two separate decisions. `retry` keeps the candles the user is still
    /// reading — blanking the chart to reconnect would flash a screen of data away. `market_reloaded`
    /// is a commit on the venue this chart reads: the user loaded that market again, so the chart
    /// cannot vouch for candles written at the generation the commit replaced and they go, while
    /// the request still has to be the replacing one.
    fn start_current(&mut self, retry: bool, market_reloaded: bool, cx: &mut Context<Self>) {
        // The pane reached here only past its own gate, so the market it was waiting
        // for is in memory again and messages belong to it once more.
        self.awaiting_market = false;
        self.directory_generation = self.directory_generation.wrapping_add(1);
        let generation = self.directory_generation;
        let Some(selection) = self.config.selection.clone() else {
            // There is nothing to chart yet, so there is no market to read.
            // The widget has no content of its own, so it says what it is waiting
            // for instead of reporting work it is not doing.
            self.clear_loaded_state();
            self.countdown_task.take();
            self.report_notice(cx);
            cx.notify();
            return;
        };
        if retry {
            // A retry keeps the candles the user is still reading — blanking the
            // chart to reconnect would flash a screen of data away — but the
            // pages that were in flight belonged to the generation being
            // replaced and will never be answered.
            self.fetching_history = false;
            self.fetching_newer = false;
        } else {
            // An explicit load starts over, because the widget cannot vouch for
            // what it was showing.
            self.clear_loaded_state();
        }
        self.status.report(WidgetState::Connecting, cx);
        // What the directory still publishes is asked before the chart reads, but
        // only as advice: a listing the route no longer lists goes back to "nothing
        // chosen" instead of being charted and failing. The lookup is not the
        // authority on what can be requested — the request below is, because it
        // resolves the listing inside one catalog generation and checks the interval
        // against the route's own descriptor. A lookup that failed therefore proves
        // nothing and is only logged: the chart asks for the stream anyway, and a
        // catalog or network failure comes back as the failure it is instead of being
        // dressed up as a listing that is not there.
        let catalog = cx.global::<MarketParticipantsConnector>().catalog_arc();
        let offered = catalog.contains(&selection.listing);
        cx.spawn(async move |this, cx| {
            let offered = offered;
            let _ = this.update(cx, |this, cx| {
                if this.directory_generation != generation
                    || this.config.selection.as_ref() != Some(&selection)
                {
                    return;
                }
                match &offered {
                    Ok(true) => {
                        // A listing that is settled or delisted is one no market call
                        // reads, so it is reset here the way a missing one is.
                        let unreadable = catalog
                            .cached_listing(&selection.listing)
                            .is_some_and(|entry| entry.snapshot.status.is_terminal());
                        if unreadable {
                            this.clear_selection("the listing no longer trades", cx);
                            return;
                        }
                    }
                    Ok(false) => {
                        this.clear_selection("the route does not publish it", cx);
                        return;
                    }
                    Err(error) if is_definitely_unsupported(error) => {
                        this.clear_selection("the directory does not serve it", cx);
                        return;
                    }
                    Err(error) => {
                        log::warn!(
                            "asking whether `{:?}` is published failed; the chart asks for the \
                             stream anyway: {error}",
                            selection.listing
                        );
                    }
                }
                this.start_countdown_task(cx);
                let request = if retry || market_reloaded {
                    CandlestickRequest::Retry {
                        listing: selection.listing,
                        timeframe: this.config.timeframe,
                        adjustment: this.config.adjustment,
                    }
                } else {
                    CandlestickRequest::Load {
                        listing: selection.listing,
                        timeframe: this.config.timeframe,
                        adjustment: this.config.adjustment,
                    }
                };
                this.binding.call(request, cx, Self::apply_rpc_result);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn apply_rpc_result(
        &mut self,
        result: WidgetRpcResult<CandlestickProtocol>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(
                CandlestickResponse::StreamStarted
                | CandlestickResponse::HistoryQueued
                | CandlestickResponse::NewerQueued,
            ) => {}
            Err(error) => {
                let recovery = self.recovery.clone();
                self.status.report(
                    WidgetState::failed(Arc::new(CandlestickFailure::Rpc(error)), recovery),
                    cx,
                );
            }
        }
    }

    fn apply_reliable(
        &mut self,
        key: CandlestickStreamKey,
        message: Arc<CandlestickStreamMessage>,
        cx: &mut Context<Self>,
    ) {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx)
            || self.awaiting_market
        {
            return;
        }
        match message.as_ref() {
            CandlestickStreamMessage::Status(status) => match status {
                CandlestickStatus::Connecting => {
                    self.status.report(WidgetState::Connecting, cx);
                }
                CandlestickStatus::LoadingHistory => {
                    self.status.report(WidgetState::Loading, cx);
                }
                // Every page ends with Ready, so this is also where a pagination
                // request stops being in flight.
                CandlestickStatus::Ready => {
                    self.fetching_history = false;
                    self.fetching_newer = false;
                    self.status.report(WidgetState::Ready, cx);
                }
                CandlestickStatus::Error(error) => {
                    self.countdown_task.take();
                    // A market the loaded venues no longer hold is the notice's answer,
                    // not a failure and not a scope that stopped serving candlesticks:
                    // the commit landed between this pane's gate and the request it was
                    // already reading, so the chart waits for the market again with the
                    // user's selection left alone.
                    if error.is_venue_not_loaded() {
                        self.wait_for_market(cx);
                        return;
                    }
                    // A rejection the request itself produced is a scope the venue does
                    // not serve, not a stream that failed: the item it names goes back
                    // to its default and the chart is restarted before anything is
                    // reported. Every other failure is the failure it is.
                    if self.recover_unsupported_scope(error, cx) {
                        return;
                    }
                    // The directory answering "this route's catalog is not in memory"
                    // is a wait rather than a failure. The gate above asked the
                    // same question a moment earlier, so this is the answer changing
                    // underneath the request — a venue re-discovering its scopes is what
                    // does it — and loading the market again is what brings it back.
                    if market_is_catalog_not_loaded(error) {
                        self.wait_for_market(cx);
                        return;
                    }
                    let recovery = self.recovery.clone();
                    self.status.report(
                        WidgetState::failed(
                            Arc::new(CandlestickFailure::Market(error.clone())),
                            recovery,
                        ),
                        cx,
                    );
                }
            },
            CandlestickStreamMessage::Snapshot(_) => {}
        }
    }

    fn apply_latest(
        &mut self,
        key: CandlestickStreamKey,
        message: Arc<CandlestickStreamMessage>,
        cx: &mut Context<Self>,
    ) {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx)
            || self.awaiting_market
        {
            return;
        }
        if let CandlestickStreamMessage::Snapshot(snapshot) = message.as_ref() {
            self.apply_snapshot(key.venue(), snapshot);
        }
    }

    /// Accepts a snapshot as the whole of what the chart draws.
    fn apply_snapshot(&mut self, venue: &VenueId, snapshot: &CandlestickSeriesSnapshot) {
        self.snapshot = Some(snapshot.clone());
        if !snapshot.at_live_edge() {
            // The chart is showing a historical window, so no candle on screen
            // is still being built and there is nothing to count down to.
            self.countdown.clear();
            return;
        }
        if let Some(candle) = snapshot.latest() {
            self.countdown.observe_candle(
                candle.timestamp,
                self.config.timeframe,
                venue,
                current_timestamp_millis(),
            );
        } else {
            self.countdown.clear();
        }
        // Following the live edge is the absence of an anchor, so once the
        // newest candle is back on screen the anchor is dropped rather than
        // pinned to it.
        if self.viewport.right_timestamp.is_some_and(|timestamp| {
            snapshot
                .latest_timestamp()
                .is_some_and(|latest| timestamp >= latest)
        }) {
            self.viewport.right_timestamp = None;
        }
    }

    /// Applies a selector result as one configuration transition.
    ///
    /// Returns whether the selection was accepted.
    fn select_selection(&mut self, selection: InstrumentSelection, cx: &mut Context<Self>) -> bool {
        // A scope this chart may follow is a scope the loaded venues answer with, and
        // that one answer is the whole of what the selector offered: the selector is
        // handed the same loaded scopes, so a scope it could draw is a scope this
        // accepts, and a scope that is not loaded is refused rather than carried into
        // a stream nothing will publish.
        if let Some(connector) = cx.try_global::<MarketParticipantsConnector>()
            && !connector
                .venues()
                .scope(&selection.listing.route)
                .is_some_and(|scope| {
                    scope
                        .capabilities
                        .iter()
                        .any(|support| support.capability == CapabilityId::Candlesticks)
                })
        {
            return false;
        }
        if self.config.selection.as_ref() == Some(&selection) && !self.status.is_failed(cx) {
            return false;
        }
        self.config.selection = Some(selection);
        self.config_change.publish(cx);
        self.start_current(false, false, cx);
        true
    }

    fn select_interval(&mut self, timeframe: TimeFrame, cx: &mut Context<Self>) {
        if self.config.timeframe == timeframe && !self.status.is_failed(cx) {
            return;
        }
        self.config.timeframe = timeframe;
        self.config_change.publish(cx);
        self.start_current(false, false, cx);
    }

    /// Applies the adjustment control's result as one configuration transition.
    ///
    /// The convention is a dimension of the stream's scope, so switching it opens a new stream
    /// through the same path the interval picker uses rather than reusing the one whose candles
    /// are priced the other way.
    fn select_adjustment(&mut self, adjustment: Adjustment, cx: &mut Context<Self>) {
        if self.config.adjustment == adjustment && !self.status.is_failed(cx) {
            return;
        }
        self.config.adjustment = adjustment;
        self.config_change.publish(cx);
        self.start_current(false, false, cx);
    }

    /// The control that opens the instrument selector.
    ///
    /// Before anything is chosen it offers the choice rather than reporting a
    /// scope, because there is no scope yet. What it states once a listing is
    /// followed — the published name and the route it is on — is the control's own
    /// text, so the chart and the dialog cannot disagree about which listing is
    /// followed.
    fn instrument_button(&self, cx: &Context<Self>) -> AnyElement {
        let widget = cx.entity().downgrade();

        InstrumentButton::new(
            ElementId::Name(SharedString::new_static("CandlestickInstrument")),
            self.config.selection.clone(),
            InstrumentFilters {
                kind_filter: InstrumentKindFilter::All,
                status_filter: InstrumentStatusFilter::Any,
                required_capability: CapabilityId::Candlesticks,
                entry_predicate: None,
                routes: loaded_scopes(cx),
            },
            "widget.candlestick.select_instrument",
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

    /// The route this chart may ask what it publishes, if there is one to ask.
    ///
    /// A route's own descriptor is what answers, and it is readable only once its market is in
    /// memory: a chart that follows nothing, and one the pane is waiting for its market with,
    /// have no route to put the question to.
    fn asked_route(&self) -> Option<&VenueRoute> {
        self.config
            .selection
            .as_ref()
            .filter(|_| !self.awaiting_market)
            .map(|selection| &selection.listing.route)
    }

    /// The intervals the picker offers for the market the chart follows.
    ///
    /// The route's own declaration decides which of the closed set it serves
    /// ([`offered_timeframes`]), and a chart with no route to ask — nothing followed, or a market
    /// the pane is waiting for — offers none of them.
    fn offered_intervals(&self) -> Vec<TimeFrame> {
        let Some(route) = self.asked_route() else {
            return Vec::new();
        };
        let published = (self.published_timeframes)(route);
        offered_timeframes(published.as_deref().ok())
    }

    /// The price-adjustment conventions the adjustment control offers for the market the chart
    /// follows.
    ///
    /// These are the ones the route declares, in the order it declares them. A chart with no
    /// route to ask — nothing followed, or a market the pane is waiting for — offers none, and a
    /// route this build cannot ask offers none either: an answer nobody gave proves nothing about
    /// what such a route serves, so there is no list to put in front of the user, and the chart
    /// keeps asking with the convention it stored.
    fn offered_adjustments(&self) -> Vec<Adjustment> {
        let Some(route) = self.asked_route() else {
            return Vec::new();
        };
        (self.published_adjustments)(route).unwrap_or_default()
    }

    /// The adjustment control: the conventions the route publishes, when there is a choice.
    ///
    /// The conventions offered are the route's own declaration rather than a list of venues, so
    /// a route that publishes one of them has nothing to switch between and draws no control at
    /// all, while a venue that starts publishing two gets one without this widget naming it. The
    /// control states the convention the chart follows, the way the interval picker states its
    /// interval, so the two cannot disagree about what is being asked for.
    fn adjustment_select(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let offered = self.offered_adjustments();
        if offered.len() <= 1 {
            return None;
        }
        let select = offered.iter().copied().fold(
            Select::new(ElementId::Name(SharedString::new_static(
                "CandlestickAdjustment",
            )))
            .placeholder(t!(cx, adjustment_key(self.config.adjustment)))
            .ghost()
            .small()
            .selected_item(Some(SharedString::new_static(
                self.config.adjustment.label(),
            )))
            .aria_label(t!(cx, adjustment_key(self.config.adjustment))),
            |select, adjustment| {
                select.menu_item(MenuItem::Action(MenuItemAction::new(
                    SharedString::new_static(adjustment.label()),
                    t!(cx, adjustment_key(adjustment)),
                )))
            },
        );

        Some(
            div()
                .debug_selector(|| "TEST_CANDLESTICK_ADJUSTMENT".into())
                .occlude()
                .child(select.on_change({
                    let widget = cx.entity();
                    move |choice, _, app| {
                        // A choice is identified by the label it shows, which is the
                        // convention's own spelling, and the control can only hand back one
                        // it offered.
                        let Some(adjustment) = choice.as_deref().and_then(|label| {
                            offered
                                .iter()
                                .copied()
                                .find(|candidate| candidate.label() == label)
                        }) else {
                            return;
                        };
                        widget.update(app, |this, cx| this.select_adjustment(adjustment, cx));
                    }
                }))
                .into_any_element(),
        )
    }

    /// The interval picker, grouped by unit.
    ///
    /// The intervals offered are the ones the route behind the chart publishes, read out of the
    /// closed set a request can carry in this widget's own order ([`offered_timeframes`]); a
    /// route that cannot be asked offers the closed set, because the choice stays the user's
    /// there and the venue's own refusal is what answers a request it does not serve. A chart
    /// with nothing to offer — nothing followed, a market that is not loaded, or a route that
    /// publishes none of the intervals this build can ask for — refuses input, because the route
    /// is the thing that answers.
    fn timeframe_select(&self, cx: &Context<Self>) -> AnyElement {
        let offered = self.offered_intervals();
        let disabled = offered.is_empty();
        let mut select = Select::new(ElementId::Name(SharedString::new_static(
            "CandlestickTimeframe",
        )))
        .placeholder(SharedString::from(self.config.timeframe.label()))
        .ghost()
        .small()
        .aria_label(t!(cx, "widget.candlestick.timeframe"))
        .disabled(disabled)
        .selected_item(Some(SharedString::from(self.config.timeframe.label())));
        if !disabled {
            for unit in TIMEFRAME_GROUPS {
                let mut intervals = offered
                    .iter()
                    .copied()
                    .filter(|timeframe| tick_and_unit(*timeframe).1 == unit)
                    .peekable();
                if intervals.peek().is_none() {
                    continue;
                }
                let unit_label = t!(cx, unit.group_key());
                select = select.menu_item(MenuItem::Group(MenuItemGroup::new(unit_label.clone())));
                for timeframe in intervals {
                    let (tick, _) = tick_and_unit(timeframe);
                    select = select.menu_item(MenuItem::Action(MenuItemAction::new(
                        SharedString::from(timeframe.label()),
                        SharedString::new(format_smolstr!("{tick} {unit_label}").as_str()),
                    )));
                }
            }
        }

        div()
            .debug_selector(|| "TEST_CANDLESTICK_TIMEFRAME".into())
            .occlude()
            .child(select.on_change({
                let widget = cx.entity();
                move |timeframe, _, app| {
                    // The picker identifies an interval by the label it shows,
                    // which is the interval's own spelling.
                    let Some(timeframe) = timeframe.as_deref().and_then(timeframe_of_label) else {
                        return;
                    };
                    widget.update(app, |this, cx| this.select_interval(timeframe, cx));
                }
            }))
            .into_any_element()
    }

    /// Keeps the countdown moving without a frame per second of its own.
    ///
    /// The task wakes exactly when the label would change, and repaints only
    /// when it did: a chart at a historical window has nothing to count down to
    /// and therefore costs nothing.
    fn start_countdown_task(&mut self, cx: &mut Context<Self>) {
        if self.countdown_task.is_some() {
            return;
        }
        self.countdown_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let refresh_delay = match this.update(cx, |this, cx| {
                    let now_timestamp = current_timestamp_millis();
                    if this.tick_countdown(now_timestamp) && this.should_refresh_countdown() {
                        cx.notify();
                    }
                    this.next_countdown_refresh_delay(now_timestamp)
                }) {
                    Ok(refresh_delay) => refresh_delay,
                    Err(_) => break,
                };
                cx.background_executor().timer(refresh_delay).await;
            }
        }));
    }

    fn tick_countdown(&mut self, now_timestamp: i64) -> bool {
        let Some(candle_timestamp) = self
            .snapshot
            .as_ref()
            .filter(|snapshot| snapshot.at_live_edge())
            .and_then(|snapshot| snapshot.latest())
            .map(|candle| candle.timestamp)
        else {
            return false;
        };
        let Some(venue) = venue_of(&self.config) else {
            return false;
        };
        self.countdown.tick(
            candle_timestamp,
            self.config.timeframe,
            venue,
            now_timestamp,
        )
    }

    fn next_countdown_refresh_delay(&self, now_timestamp: i64) -> Duration {
        let Some(venue) = venue_of(&self.config) else {
            return next_second_boundary_delay(now_timestamp);
        };
        self.snapshot
            .as_ref()
            .filter(|snapshot| snapshot.at_live_edge())
            .and_then(|snapshot| snapshot.latest())
            .map_or_else(
                || next_second_boundary_delay(now_timestamp),
                |candle| {
                    countdown_refresh_delay(
                        candle.timestamp,
                        self.config.timeframe,
                        venue,
                        now_timestamp,
                    )
                },
            )
    }

    /// Whether the countdown is on screen, and therefore worth repainting for.
    fn should_refresh_countdown(&self) -> bool {
        let Some(snapshot) = self
            .snapshot
            .as_ref()
            .filter(|snapshot| snapshot.at_live_edge())
        else {
            return false;
        };
        let candles = snapshot.candles();
        if candles.is_empty() {
            return false;
        }
        let visible_end = self
            .viewport
            .right_timestamp
            .map_or(candles.len(), |timestamp| {
                candles.partition_point(|candle| candle.timestamp <= timestamp)
            })
            .clamp(1, candles.len());
        latest_price_indicator_is_visible(candles.len(), visible_end, true)
    }

    fn begin_drag(&mut self, x: Pixels) -> bool {
        let changed = !self.dragging || self.viewport.cursor.is_some();
        self.dragging = true;
        self.drag_last_x = Some(x);
        self.drag_remainder = 0.0;
        self.viewport.cursor = None;
        self.pointer_frame.clear_pending();
        changed
    }

    fn end_drag(&mut self, position: Option<Point<Pixels>>) -> bool {
        let changed = self.dragging || self.viewport.cursor != position;
        self.dragging = false;
        self.drag_last_x = None;
        self.drag_remainder = 0.0;
        self.viewport.cursor = position;
        self.pointer_frame.clear_pending();
        changed
    }

    fn queue_mouse_move(
        &mut self,
        position: Point<Pixels>,
        dragging: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self
            .pointer_frame
            .queue(PointerInput { position, dragging })
        {
            return;
        }
        cx.on_next_frame(window, |this, _, cx| {
            let Some(input) = this.pointer_frame.take() else {
                return;
            };
            if this.apply_pointer_input(input, cx) {
                cx.notify();
            }
        });
    }

    fn apply_pointer_input(&mut self, input: PointerInput, cx: &mut Context<Self>) -> bool {
        if self.dragging && input.dragging {
            let Some(last_x) = self.drag_last_x.replace(input.position.x) else {
                return false;
            };
            // Panning is measured in candles, and the remainder carries the
            // sub-candle part forward so that a slow drag still moves.
            self.drag_remainder += f32::from(input.position.x - last_x);
            let candle_delta = (self.drag_remainder / self.viewport.candle_width).trunc() as isize;
            if candle_delta != 0 {
                self.drag_remainder -= candle_delta as f32 * self.viewport.candle_width;
                return self.pan_by(-candle_delta, cx);
            }
            false
        } else if self.viewport.cursor == Some(input.position) {
            false
        } else {
            self.viewport.cursor = Some(input.position);
            true
        }
    }

    fn pan_by(&mut self, candle_delta: isize, cx: &mut Context<Self>) -> bool {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return false;
        };
        let candles = snapshot.candles();
        if candles.is_empty() {
            return false;
        }
        let current_end = self
            .viewport
            .right_timestamp
            .map_or(candles.len(), |timestamp| {
                candles.partition_point(|candle| candle.timestamp <= timestamp)
            });
        let new_end = current_end
            .saturating_add_signed(candle_delta)
            .clamp(1, candles.len());
        let at_live_edge = new_end == candles.len() && snapshot.at_live_edge();
        let right_timestamp = if at_live_edge {
            None
        } else {
            Some(candles[new_end - 1].timestamp)
        };
        if self.viewport.right_timestamp == right_timestamp {
            return false;
        }
        self.viewport.right_timestamp = right_timestamp;
        self.prefetch_if_needed(cx);
        true
    }

    fn handle_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(layout) = self.chart_layout.get() else {
            return;
        };
        if self.snapshot.is_none() || !layout.contains_data_point(&event.position) {
            return;
        }
        let delta = event.delta.pixel_delta(window.line_height()).y;
        if delta == px(0.0) {
            return;
        }
        let input = ScrollInput {
            position: event.position,
            steps: if delta > px(0.0) { 1 } else { -1 },
        };
        if !self.scroll_frame.queue(input) {
            return;
        }
        cx.on_next_frame(window, |this, _, cx| {
            let Some(input) = this.scroll_frame.take() else {
                return;
            };
            if this.apply_scroll_input(input, cx) {
                cx.notify();
            }
        });
    }

    /// Zooms around the candle under the pointer.
    ///
    /// The candle under the cursor is found *before* the width changes and
    /// anchored after it, so the chart grows away from the pointer instead of
    /// sliding out from under it.
    fn apply_scroll_input(&mut self, input: ScrollInput, cx: &mut Context<Self>) -> bool {
        let Some(layout) = self.chart_layout.get() else {
            return false;
        };
        let Some(snapshot) = self.snapshot.as_ref() else {
            return false;
        };
        if !layout.contains_data_point(&input.position) {
            return false;
        }
        let old_width = self.viewport.candle_width;
        let new_width = (old_width + input.steps as f32).clamp(MIN_CANDLE_WIDTH, MAX_CANDLE_WIDTH);
        if new_width == old_width {
            return false;
        }

        let candles = snapshot.candles();
        let old_range = visible_range(candles, self.viewport, layout.price_plot.size.width);
        if old_range.is_empty() {
            return false;
        }
        let geometry = CandleGeometry::new(layout.price_plot, old_range.len(), old_width);
        let Some(hover_slot) = geometry.slot_at(input.position.x) else {
            return false;
        };
        let hover_index = old_range.start + hover_slot;
        let slots_right = ((f32::from(layout.price_plot.right() - input.position.x) / new_width)
            .ceil() as usize)
            .saturating_add(1);
        let new_end = hover_index
            .saturating_add(slots_right)
            .clamp(1, candles.len());
        self.viewport.candle_width = new_width;
        self.viewport.right_timestamp = if new_end == candles.len() && snapshot.at_live_edge() {
            None
        } else {
            Some(candles[new_end - 1].timestamp)
        };
        self.viewport.cursor = Some(input.position);
        self.prefetch_if_needed(cx);
        true
    }

    /// Asks for the page the user is about to need.
    ///
    /// The margin is measured against the widest the plot can be, so the request
    /// goes out slightly before the edge is reached rather than after the chart
    /// has already run out of candles.
    ///
    /// Called after every gesture that moves the viewport, and after every change
    /// of the chart's width — which needs no gesture at all.
    fn prefetch_if_needed(&mut self, cx: &mut Context<Self>) {
        // Failure is terminal: the stream that would answer is gone with it, so
        // asking now would only replace the failure being reported with a cheaper
        // one about a stream nobody is holding open.
        if self.status.is_failed(cx) {
            return;
        }
        let Some(layout) = self.chart_layout.get() else {
            return;
        };
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let range = visible_range(
            snapshot.candles(),
            self.viewport,
            layout.maximum_plot_width(),
        );
        if range.start <= PREFETCH_MARGIN
            && !snapshot.older_exhausted()
            && !self.fetching_history
            && let Some(before_timestamp) = snapshot.earliest_timestamp()
        {
            self.fetching_history = true;
            self.binding.call(
                CandlestickRequest::FetchHistory { before_timestamp },
                cx,
                Self::apply_rpc_result,
            );
        } else if range.end.saturating_add(PREFETCH_MARGIN) >= snapshot.candles().len()
            && !snapshot.at_live_edge()
            && !self.fetching_newer
            && let Some(after_timestamp) = snapshot.latest_timestamp()
        {
            self.fetching_newer = true;
            self.binding.call(
                CandlestickRequest::FetchNewer { after_timestamp },
                cx,
                Self::apply_rpc_result,
            );
        }
    }

    /// The ready-state body.
    ///
    /// The body is cached by the pane, so nothing here needs to place a cache
    /// boundary. What it must do instead is notify when what it draws changes,
    /// which the binding already does for every accepted snapshot.
    fn ready_body(&self, cx: &Context<Self>) -> AnyElement {
        let Some(snapshot) = self.snapshot.clone() else {
            return div().size_full().min_h_0().into_any_element();
        };
        // Whether the snapshot carries candles is read before the canvas takes it,
        // because a snapshot with none is what the empty state stands for.
        let has_candles = !snapshot.candles().is_empty();
        let widget = cx.entity();
        // The chart reports its own geometry, because a pane that was split or
        // dragged wider shows more candles than the page it holds and no pointer
        // event ever asks for the rest. Prepaint runs while this entity may still
        // be on the stack, so the reaction waits for the effect cycle to end.
        let on_layout_change = {
            let widget = widget.downgrade();
            move |cx: &mut App| {
                cx.defer(move |cx| {
                    let _ = widget.update(cx, |widget, cx| widget.prefetch_if_needed(cx));
                });
            }
        };
        let chart = chart_canvas(
            snapshot,
            ChartCanvasOptions {
                viewport: self.viewport,
                interval: self.config.timeframe,
                countdown_label: self.countdown.label.clone(),
            },
            self.chart_layout.clone(),
            self.hover_label_cache.clone(),
            self.chart_paint_caches.clone(),
            on_layout_change,
        );

        div()
            .id(ElementId::Name(SharedString::new_static(
                "CandlestickChart",
            )))
            .aria_label(t!(cx, "widget.candlestick.chart"))
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .cursor(if self.dragging {
                CursorStyle::ClosedHand
            } else {
                CursorStyle::Crosshair
            })
            .on_mouse_down(MouseButton::Left, {
                let widget = widget.clone();
                move |event, _, app| {
                    widget.update(app, |this, cx| {
                        if this.begin_drag(event.position.x) {
                            cx.notify();
                        }
                    });
                    // The pane treats any mouse down as "this pane was clicked";
                    // a drag inside the chart is not that.
                    app.stop_propagation();
                }
            })
            .on_mouse_up(MouseButton::Left, {
                let widget = widget.clone();
                move |event, _, app| {
                    widget.update(app, |this, cx| {
                        if this.end_drag(Some(event.position)) {
                            cx.notify();
                        }
                    });
                }
            })
            .on_mouse_up_out(MouseButton::Left, {
                let widget = widget.clone();
                move |_, _, app| {
                    widget.update(app, |this, cx| {
                        if this.end_drag(None) {
                            cx.notify();
                        }
                    });
                }
            })
            .on_mouse_move({
                let widget = widget.clone();
                move |event, window, app| {
                    widget.update(app, |this, cx| {
                        this.queue_mouse_move(event.position, event.dragging(), window, cx);
                    });
                }
            })
            .on_hover({
                let widget = widget.clone();
                move |hovering, _, app| {
                    if !*hovering {
                        widget.update(app, |this, cx| {
                            if this.end_drag(None) {
                                cx.notify();
                            }
                        });
                    }
                }
            })
            .on_scroll_wheel(move |event, window, app| {
                widget.update(app, |this, cx| {
                    this.handle_scroll(event, window, cx);
                });
                // The chart zooms rather than letting the page scroll.
                app.stop_propagation();
            })
            // A snapshot with no candles draws axes and no series, which leaves the
            // plot and a chart that failed to load looking the same. The snapshot is
            // still the pane's content — the venue published it — so the pane says
            // what it holds over the plot rather than letting the framework's
            // placeholder speak for it.
            .child(
                div()
                    .relative()
                    .size_full()
                    .min_h_0()
                    .child(chart)
                    .when(!has_candles, |this| {
                        this.child(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(EmptyState::new(
                                    "CandlestickEmpty",
                                    t!(cx, "common.no_data"),
                                )),
                        )
                    }),
            )
            .into_any_element()
    }
}

impl Render for CandlestickWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The body is only embedded while the widget has content, so this is the
        // ready state and nothing else.
        div()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .child(self.ready_body(cx))
    }
}

/// The venue a chart's countdown belongs to.
///
/// A chart that follows nothing describes nothing: it offers no countdown, and
/// the picker decides on its own where to open.
fn venue_of(config: &CandlestickConfig) -> Option<&VenueId> {
    config
        .selection
        .as_ref()
        .map(|selection| &selection.listing.route.venue)
}

/// The scopes a picker may offer, as the loaded venues answer them now.
///
/// Read when the picker opens rather than kept: a scope loaded after this chart was
/// created is a scope the user may chart, and one a shrinking commit withdrew is not.
fn loaded_scopes(cx: &App) -> Arc<[LoadedScope]> {
    LoadedScope::all(cx.global::<MarketParticipantsConnector>().venues())
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        rc::Rc,
        str::FromStr,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use app_config::widget::VersionedWidgetConfig;
    use app_config_provider::notification::ConfigNotification;
    use gpui_kit::{
        AppContext, Entity, Modifiers, MouseButton, Render, TestAppContext, VisualTestContext,
        WeakEntity, Window, div, size,
    };
    use i18n::language::codes::LanguageCode;
    use market_participants_connector::{
        catalog::{
            batch::CatalogEntry,
            store::Catalog,
            test_support::{self, Fixture},
        },
        connector::{MarketParticipantsConnector, MarketParticipantsConnectorInput},
        error::MarketParticipantError,
        market::{domain::DisconnectReason, error::MarketNetworkError},
        runtime::MarketParticipantNetworkSnapshot,
        venue::definition::{CapabilitySupport, ScopeEntry},
    };
    use market_participants_model::{
        decimal::{Price, Quantity, Tick},
        identifier::VenueScopeId,
        instrument::Instrument,
        listing::{
            route::{VenueListingKey, VenueRoute},
            rules::ListingPrecision,
            venue::PriceBasis,
        },
        market::{
            book::ListingCandlestick,
            price::MarketPrice,
            units::{MarketQuantity, QuantityUnit},
        },
    };
    use network::transport::hub::{TransportHub, TransportPolicy};
    use widget::venue::VenueDisplay;

    use super::*;
    use crate::{
        model::CandlestickSeries,
        service::{CandlestickEventSubscription, CandlestickPageQuery},
        time::TIMEFRAMES,
    };

    /// The symbol the fixture route publishes the charted listing under.
    const SPOT_SYMBOL: &str = "BTCUSDT";

    /// The lines the waiting notice should end with: every venue that answers with candles,
    /// each written the way the sentence writes it — the venue's logo at the size the
    /// sentence names, then its name in bold.
    ///
    /// It is built rather than copied: the pane's own answer is what the list has to be, so a
    /// fixture that spelled the names out would pass while the pane listed the wrong venues.
    fn expected_market_list(cx: &App) -> Vec<String> {
        listed_venues()
            .iter()
            .map(|venue| {
                format!(
                    "![](file://{}?size=16) **{}**",
                    venue.asset_path(),
                    venue.display_string(cx)
                )
            })
            .collect()
    }

    /// Loads the fixture venue again with no scope at all, the way a shrinking commit
    /// takes a scope away: a venue that publishes nothing is not a loaded venue.
    fn unload_venue(cx: &mut App) {
        let venue =
            VenueId::from_name(test_support::VENUE).expect("the fixture venue is canonical");
        cx.global::<MarketParticipantsConnector>()
            .load_fixture(&[], &[(venue, Vec::new())], &[])
            .expect("the shrinking commit lands");
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

    fn route() -> VenueRoute {
        test_support::spot_route()
    }

    fn instrument() -> Instrument {
        test_support::spot_token(test_support::VENUE, "BTC", "USDT")
    }

    fn listing() -> VenueListingKey {
        test_support::listing_key(&route(), SPOT_SYMBOL)
    }

    fn selection() -> InstrumentSelection {
        InstrumentSelection { listing: listing() }
    }

    /// A config that already follows the fixture route's published listing.
    fn config() -> CandlestickConfig {
        CandlestickConfig {
            selection: Some(selection()),
            timeframe: TimeFrame::Minute1,
            adjustment: Adjustment::None,
        }
    }

    /// The scope the connection action discovers at runtime.
    ///
    /// A builder-deployed DEX is not in this build's static venue list, so it is the
    /// clearest case of a route that only the registry knows about.
    const DISCOVERED_SCOPE: &str = "perp-dex/fixture";

    /// A route of that discovered scope.
    fn discovered_route() -> VenueRoute {
        test_support::route(test_support::VENUE, DISCOVERED_SCOPE)
    }

    /// The entry the load of a discovered scope commits: it publishes a catalog, so
    /// the scope is one the selector offers, and it is in no static table, so only a
    /// load can vouch for it.
    fn discovered_scope_entry() -> ScopeEntry {
        ScopeEntry {
            scope: VenueScopeId::new(DISCOVERED_SCOPE).expect("the fixture scope is canonical"),
            catalog_source: Some(
                market_participants_connector::catalog::source::CatalogSourceId::new(
                    "fixture-catalog",
                )
                .expect("the fixture source id is canonical"),
            ),
            normalization: test_support::case_insensitive_symbols(),
            capabilities: vec![
                CapabilitySupport {
                    capability: CapabilityId::MarketCatalog,
                    needs_auth: false,
                },
                CapabilitySupport {
                    capability: CapabilityId::Candlesticks,
                    needs_auth: false,
                },
            ],
        }
    }

    /// The selection of a listing on that discovered route.
    fn discovered_selection() -> InstrumentSelection {
        InstrumentSelection {
            listing: test_support::listing_key(&discovered_route(), SPOT_SYMBOL),
        }
    }

    /// One catalog entry under another symbol of the same route.
    fn entry(symbol: &str) -> CatalogEntry {
        test_support::entry(&route(), instrument(), symbol)
    }

    /// The charted listing again, published at another price tick.
    ///
    /// It is the shape a production load commits: the load re-fetches the catalogue, and what it
    /// publishes is the listing restated at the venue's own increments.
    fn restated_entry(price_tick: &str) -> CatalogEntry {
        let mut restated = entry(SPOT_SYMBOL);
        restated.snapshot.rules.common.price.tick = price_tick
            .parse()
            .expect("the restated price tick should parse");
        restated
    }

    fn test_price_tick() -> Tick {
        Tick::from_str("0.01").expect("price tick should parse")
    }

    /// The precision the pending subscription reports for its listing.
    fn test_precision() -> ListingPrecision {
        ListingPrecision {
            price_tick: test_price_tick(),
            quantity_tick: Tick::from_str("0.001").expect("quantity step should parse"),
        }
    }
    fn market_price(close: u64) -> MarketPrice {
        MarketPrice {
            value: Price::from(close),
            quote_asset: test_support::currency("USD"),
            basis: PriceBasis::PerAssetUnit,
        }
    }

    fn candle(timestamp: i64, close: u64) -> ListingCandlestick {
        let price = market_price(close);
        ListingCandlestick {
            listing: listing(),
            open: price.clone(),
            high: price.clone(),
            low: price.clone(),
            close: price,
            traded_quantity: Some(MarketQuantity {
                value: Quantity::from(1_u64),
                unit: QuantityUnit::AssetUnits {
                    asset: test_support::token(test_support::VENUE, "BTC"),
                },
            }),
            traded_notional: None,
            timestamp,
        }
    }

    fn snapshot_of(
        candles: impl IntoIterator<Item = ListingCandlestick>,
    ) -> CandlestickSeriesSnapshot {
        let mut series = CandlestickSeries::new(100, test_precision());
        series.merge_initial(candles);
        series.snapshot()
    }

    fn disconnect() -> MarketError {
        MarketError::Disconnected(DisconnectReason::Normal)
    }

    /// A source whose subscription never resolves, so a test drives every state
    /// transition itself.
    #[derive(Clone, Copy)]
    struct PendingSource;

    struct PendingSubscription;

    impl CandlestickSource for PendingSource {
        type Subscription = PendingSubscription;

        fn supported_timeframes(&self, _route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError> {
            Ok(TIMEFRAMES.to_vec())
        }

        fn supported_adjustments(
            &self,
            _route: &VenueRoute,
        ) -> Result<Vec<Adjustment>, MarketError> {
            Ok(vec![Adjustment::None])
        }

        fn fetch_candlesticks(
            &self,
            _query: CandlestickPageQuery,
        ) -> impl std::future::Future<Output = Result<Vec<ListingCandlestick>, MarketError>> + Send
        {
            std::future::pending()
        }

        fn subscribe_candlesticks(
            &self,
            _listing: VenueListingKey,
            _timeframe: TimeFrame,
            _adjustment: Adjustment,
        ) -> impl std::future::Future<Output = Result<Self::Subscription, MarketError>> + Send
        {
            std::future::pending()
        }
    }

    impl CandlestickEventSubscription for PendingSubscription {
        fn precision(&self) -> ListingPrecision {
            test_precision()
        }

        fn recv(
            &mut self,
        ) -> impl std::future::Future<Output = Option<Result<Arc<ListingCandlestick>, MarketError>>> + Send
        {
            std::future::pending()
        }
    }

    /// A source that records how many candle streams it was asked for.
    ///
    /// The request the chart sends crosses the service, so the seam is the only
    /// place a test can see that the chart asked at all.
    #[derive(Clone, Default)]
    struct CountingSource {
        subscriptions: Arc<AtomicUsize>,
    }

    impl CandlestickSource for CountingSource {
        type Subscription = PendingSubscription;

        fn supported_timeframes(&self, _route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError> {
            Ok(TIMEFRAMES.to_vec())
        }

        fn supported_adjustments(
            &self,
            _route: &VenueRoute,
        ) -> Result<Vec<Adjustment>, MarketError> {
            Ok(vec![Adjustment::None])
        }

        fn fetch_candlesticks(
            &self,
            _query: CandlestickPageQuery,
        ) -> impl std::future::Future<Output = Result<Vec<ListingCandlestick>, MarketError>> + Send
        {
            std::future::pending()
        }

        fn subscribe_candlesticks(
            &self,
            _listing: VenueListingKey,
            _timeframe: TimeFrame,
            _adjustment: Adjustment,
        ) -> impl std::future::Future<Output = Result<Self::Subscription, MarketError>> + Send
        {
            self.subscriptions.fetch_add(1, Ordering::AcqRel);
            std::future::pending()
        }
    }

    /// A source that resolves a subscription through the catalogue the fixture publishes, at the
    /// moment it is asked for, and records the increments that listing was published at then.
    ///
    /// A stream's resolution is what a venue reload replaces, so a test about a reload has to see
    /// both that the chart asked for a new stream and which generation's rules that stream was
    /// opened at: this is the one seam where the two are a single record.
    #[derive(Clone)]
    struct ResolvingSource {
        catalog: Arc<Catalog>,
        resolutions: Arc<Mutex<Vec<ListingPrecision>>>,
    }

    /// The increments every subscription one resolving source was asked for was published at.
    fn resolutions_of(resolutions: &Arc<Mutex<Vec<ListingPrecision>>>) -> Vec<ListingPrecision> {
        resolutions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    impl CandlestickSource for ResolvingSource {
        type Subscription = PendingSubscription;

        fn supported_timeframes(&self, _route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError> {
            Ok(TIMEFRAMES.to_vec())
        }

        fn supported_adjustments(
            &self,
            _route: &VenueRoute,
        ) -> Result<Vec<Adjustment>, MarketError> {
            Ok(vec![Adjustment::None])
        }

        fn fetch_candlesticks(
            &self,
            _query: CandlestickPageQuery,
        ) -> impl std::future::Future<Output = Result<Vec<ListingCandlestick>, MarketError>> + Send
        {
            std::future::pending()
        }

        fn subscribe_candlesticks(
            &self,
            listing: VenueListingKey,
            _timeframe: TimeFrame,
            _adjustment: Adjustment,
        ) -> impl std::future::Future<Output = Result<Self::Subscription, MarketError>> + Send
        {
            let published = self
                .catalog
                .cached_listing(&listing)
                .expect("the fixture publishes the listing the chart follows");
            self.resolutions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(ListingPrecision::of(&published.snapshot.rules));
            std::future::pending()
        }
    }

    /// A source that rejects every interval but the default one.
    ///
    /// Which intervals a route publishes is its own descriptor's answer, and a venue may still
    /// refuse an interval its descriptor lists. The route therefore answers that it publishes
    /// every interval the model can express, so what this source models is the refusal itself:
    /// the chart has to read the rejection as the interval being unsupported.
    #[derive(Clone, Default)]
    struct RejectingTimeframeSource {
        subscriptions: Arc<AtomicUsize>,
    }

    impl CandlestickSource for RejectingTimeframeSource {
        type Subscription = PendingSubscription;

        fn supported_timeframes(&self, _route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError> {
            Ok(TIMEFRAMES.to_vec())
        }

        fn supported_adjustments(
            &self,
            _route: &VenueRoute,
        ) -> Result<Vec<Adjustment>, MarketError> {
            Ok(vec![Adjustment::None])
        }

        fn fetch_candlesticks(
            &self,
            _query: CandlestickPageQuery,
        ) -> impl std::future::Future<Output = Result<Vec<ListingCandlestick>, MarketError>> + Send
        {
            std::future::pending()
        }

        fn subscribe_candlesticks(
            &self,
            listing: VenueListingKey,
            timeframe: TimeFrame,
            _adjustment: Adjustment,
        ) -> impl std::future::Future<Output = Result<Self::Subscription, MarketError>> + Send
        {
            self.subscriptions.fetch_add(1, Ordering::AcqRel);
            let unsupported = timeframe != CandlestickConfig::default().timeframe;
            async move {
                if unsupported {
                    return Err(MarketError::from(
                        MarketParticipantError::UnsupportedTimeFrame {
                            route: listing.route,
                            timeframe: format_smolstr!("{timeframe:?}"),
                        },
                    ));
                }
                std::future::pending().await
            }
        }
    }

    /// A source that rejects every adjustment convention but the default one.
    ///
    /// The same shape as [`RejectingTimeframeSource`], for the other dimension of a
    /// candlestick request: the route declares both conventions and still refuses the
    /// adjusted one, so what this source models is the refusal itself — the chart has to
    /// read the rejection as the convention being unsupported.
    #[derive(Clone, Default)]
    struct RejectingAdjustmentSource {
        subscribed: Arc<Mutex<Vec<Adjustment>>>,
    }

    impl CandlestickSource for RejectingAdjustmentSource {
        type Subscription = PendingSubscription;

        fn supported_timeframes(&self, _route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError> {
            Ok(TIMEFRAMES.to_vec())
        }

        fn supported_adjustments(
            &self,
            _route: &VenueRoute,
        ) -> Result<Vec<Adjustment>, MarketError> {
            Ok(vec![Adjustment::None, Adjustment::Forward])
        }

        fn fetch_candlesticks(
            &self,
            _query: CandlestickPageQuery,
        ) -> impl std::future::Future<Output = Result<Vec<ListingCandlestick>, MarketError>> + Send
        {
            std::future::pending()
        }

        fn subscribe_candlesticks(
            &self,
            listing: VenueListingKey,
            _timeframe: TimeFrame,
            adjustment: Adjustment,
        ) -> impl std::future::Future<Output = Result<Self::Subscription, MarketError>> + Send
        {
            self.subscribed
                .lock()
                .expect("the subscribed conventions should not be poisoned")
                .push(adjustment);
            let unsupported = adjustment != CandlestickConfig::default().adjustment;
            async move {
                if unsupported {
                    return Err(MarketError::from(
                        MarketParticipantError::UnsupportedAdjustment {
                            route: listing.route,
                            adjustment: smol_str::SmolStr::new(adjustment.label()),
                        },
                    ));
                }
                std::future::pending().await
            }
        }
    }

    /// A source whose subscription is answered when the market it was about is gone.
    ///
    /// The commit that takes the scope away lands between the pane's gate and the
    /// request the service is already making, so the answer the service reads is "this
    /// market is not loaded": the real source refuses exactly that way, because the
    /// builder asks the route's own bounds before it subscribes. The test releases the
    /// answer itself, so the pane is provably past its gate and reading when the commit
    /// arrives.
    #[derive(Clone, Default)]
    struct RevokedMarketSource {
        answer: Arc<tokio::sync::Notify>,
    }

    impl CandlestickSource for RevokedMarketSource {
        type Subscription = PendingSubscription;

        fn supported_timeframes(&self, _route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError> {
            Ok(TIMEFRAMES.to_vec())
        }

        fn supported_adjustments(
            &self,
            _route: &VenueRoute,
        ) -> Result<Vec<Adjustment>, MarketError> {
            Ok(vec![Adjustment::None])
        }

        fn fetch_candlesticks(
            &self,
            _query: CandlestickPageQuery,
        ) -> impl std::future::Future<Output = Result<Vec<ListingCandlestick>, MarketError>> + Send
        {
            std::future::pending()
        }

        fn subscribe_candlesticks(
            &self,
            listing: VenueListingKey,
            _timeframe: TimeFrame,
            _adjustment: Adjustment,
        ) -> impl std::future::Future<Output = Result<Self::Subscription, MarketError>> + Send
        {
            let answer = Arc::clone(&self.answer);
            async move {
                answer.notified().await;
                Err(MarketError::Network(MarketNetworkError::from(
                    MarketParticipantError::VenueNotLoaded {
                        venue: listing.route.venue,
                        scope: listing.route.scope,
                    },
                )))
            }
        }
    }

    /// A source whose subscription rejects the route's candlestick capability.
    #[derive(Clone, Copy)]
    struct UnsupportedCapabilitySource;

    impl CandlestickSource for UnsupportedCapabilitySource {
        type Subscription = PendingSubscription;

        fn supported_timeframes(&self, route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError> {
            Err(MarketError::UnsupportedCapability {
                route: route.clone(),
                capability: CapabilityId::Candlesticks,
            })
        }

        fn supported_adjustments(
            &self,
            route: &VenueRoute,
        ) -> Result<Vec<Adjustment>, MarketError> {
            Err(MarketError::UnsupportedCapability {
                route: route.clone(),
                capability: CapabilityId::Candlesticks,
            })
        }

        fn fetch_candlesticks(
            &self,
            _query: CandlestickPageQuery,
        ) -> impl std::future::Future<Output = Result<Vec<ListingCandlestick>, MarketError>> + Send
        {
            std::future::pending()
        }

        async fn subscribe_candlesticks(
            &self,
            listing: VenueListingKey,
            _timeframe: TimeFrame,
            _adjustment: Adjustment,
        ) -> Result<Self::Subscription, MarketError> {
            Err(MarketError::UnsupportedCapability {
                route: listing.route,
                capability: CapabilityId::Candlesticks,
            })
        }
    }

    /// A source whose route publishes exactly the intervals and adjustment conventions a
    /// test names, and which records every question the chart asks it.
    ///
    /// The questions are the point: which intervals and conventions the route publishes is
    /// what decides whether the values the chart persisted are used at all, and the order of
    /// the questions is what a test reads to see that nothing is subscribed before the
    /// answer.
    #[derive(Clone)]
    struct PublishingSource {
        /// The intervals the route publishes, and the ones its subscription accepts.
        published: Arc<Vec<TimeFrame>>,
        /// The adjustment conventions the route publishes, and the ones its subscription
        /// accepts.
        published_adjustments: Arc<Vec<Adjustment>>,
        /// Whether the route answers the questions about what it publishes at all.
        unknown: bool,
        queries: Arc<AtomicUsize>,
        subscribed: Arc<Mutex<Vec<TimeFrame>>>,
        subscribed_adjustments: Arc<Mutex<Vec<Adjustment>>>,
    }

    impl PublishingSource {
        /// A route that publishes exactly these intervals and the neutral convention.
        fn publishing(published: impl IntoIterator<Item = TimeFrame>) -> Self {
            Self::new(published, [Adjustment::None], false)
        }

        /// A route that publishes exactly these intervals and these conventions.
        fn publishing_with_adjustments(
            published: impl IntoIterator<Item = TimeFrame>,
            adjustments: impl IntoIterator<Item = Adjustment>,
        ) -> Self {
            Self::new(published, adjustments, false)
        }

        /// A route this build cannot ask what it publishes.
        fn unknown(published: impl IntoIterator<Item = TimeFrame>) -> Self {
            Self::new(published, [Adjustment::None], true)
        }

        /// A route this build cannot ask what it publishes, whose subscription accepts
        /// these conventions.
        ///
        /// A route that publishes no table answers no question about what it serves, and
        /// the request is then the only answer there is: this is the seam a test uses to see
        /// that the chart asks with the convention it stored instead of resetting it.
        fn unknown_accepting(
            published: impl IntoIterator<Item = TimeFrame>,
            accepted: impl IntoIterator<Item = Adjustment>,
        ) -> Self {
            Self::new(published, accepted, true)
        }

        fn new(
            published: impl IntoIterator<Item = TimeFrame>,
            adjustments: impl IntoIterator<Item = Adjustment>,
            unknown: bool,
        ) -> Self {
            Self {
                published: Arc::new(published.into_iter().collect()),
                published_adjustments: Arc::new(adjustments.into_iter().collect()),
                unknown,
                queries: Arc::new(AtomicUsize::new(0)),
                subscribed: Arc::new(Mutex::new(Vec::new())),
                subscribed_adjustments: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// How many times the route was asked which intervals it publishes.
        fn queries(&self) -> usize {
            self.queries.load(Ordering::Acquire)
        }

        /// The interval every stream was opened for, in the order they were opened.
        fn subscribed(&self) -> Vec<TimeFrame> {
            self.subscribed
                .lock()
                .expect("the subscribed intervals should not be poisoned")
                .clone()
        }

        /// The adjustment convention every stream was opened for, in the order they were
        /// opened.
        fn subscribed_adjustments(&self) -> Vec<Adjustment> {
            self.subscribed_adjustments
                .lock()
                .expect("the subscribed conventions should not be poisoned")
                .clone()
        }
    }

    impl CandlestickSource for PublishingSource {
        type Subscription = PendingSubscription;

        fn supported_timeframes(&self, _route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError> {
            self.queries.fetch_add(1, Ordering::AcqRel);
            if self.unknown {
                return Err(MarketError::ServiceClosed);
            }
            Ok((*self.published).clone())
        }

        fn supported_adjustments(
            &self,
            _route: &VenueRoute,
        ) -> Result<Vec<Adjustment>, MarketError> {
            if self.unknown {
                return Err(MarketError::ServiceClosed);
            }
            Ok((*self.published_adjustments).clone())
        }

        fn fetch_candlesticks(
            &self,
            _query: CandlestickPageQuery,
        ) -> impl std::future::Future<Output = Result<Vec<ListingCandlestick>, MarketError>> + Send
        {
            std::future::pending()
        }

        fn subscribe_candlesticks(
            &self,
            listing: VenueListingKey,
            timeframe: TimeFrame,
            adjustment: Adjustment,
        ) -> impl std::future::Future<Output = Result<Self::Subscription, MarketError>> + Send
        {
            self.subscribed
                .lock()
                .expect("the subscribed intervals should not be poisoned")
                .push(timeframe);
            self.subscribed_adjustments
                .lock()
                .expect("the subscribed conventions should not be poisoned")
                .push(adjustment);
            let published = self.published.contains(&timeframe);
            let adjustment_published = self.published_adjustments.contains(&adjustment);
            async move {
                if !published {
                    return Err(MarketError::from(
                        MarketParticipantError::UnsupportedTimeFrame {
                            route: listing.route,
                            timeframe: format_smolstr!("{timeframe:?}"),
                        },
                    ));
                }
                if !adjustment_published {
                    return Err(MarketError::from(
                        MarketParticipantError::UnsupportedAdjustment {
                            route: listing.route,
                            adjustment: smol_str::SmolStr::new(adjustment.label()),
                        },
                    ));
                }
                std::future::pending().await
            }
        }
    }

    /// Everything one test drives the widget with.
    struct WidgetUnderTest {
        widget: Entity<CandlestickWidget>,
        /// Counts the reports the widget published for its persisted config.
        config_reports: Rc<Cell<usize>>,
    }

    impl WidgetUnderTest {
        /// How many times the widget reported its persisted config as changed.
        fn reports(&self) -> usize {
            self.config_reports.get()
        }
    }

    /// Installs the market participants root the framework installs before it builds a widget.
    ///
    /// The header's instrument control states the route the charted listing is on, so building a
    /// header asks the registry which routes a venue has; a test that builds a header therefore
    /// builds the widget the way the framework does, with that root already in place.
    ///
    /// The root is wired to the directory's own catalogue rather than to a second, empty one: the
    /// pane's gate asks the directory and the service behind it resolves listings against whatever
    /// this root was wired to, so two different catalogues would let a mismatch between them pass
    /// unnoticed.
    fn install_connector_with(
        cx: &mut App,
        catalog: &Arc<market_participants_connector::catalog::store::Catalog>,
        loaded: bool,
    ) {
        let snapshot = Arc::new(MarketParticipantNetworkSnapshot::default());
        let notification = ConfigNotification::new(snapshot);
        let connector = MarketParticipantsConnector::new_with_catalog(
            MarketParticipantsConnectorInput::new(
                Arc::new(TransportHub::new(TransportPolicy::default())),
                notification,
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

    /// Creates a widget the way the framework does: directory, status, then one load.
    fn widget_over<S>(
        cx: &mut TestAppContext,
        config: CandlestickConfig,
        entries: Vec<CatalogEntry>,
        source: S,
    ) -> WidgetUnderTest
    where
        S: CandlestickSource,
    {
        widget_over_market(cx, config, entries, source, true)
    }

    /// The same, over a market that is published but not loaded.
    fn widget_over_unloaded<S>(
        cx: &mut TestAppContext,
        entries: Vec<CatalogEntry>,
        source: S,
    ) -> WidgetUnderTest
    where
        S: CandlestickSource,
    {
        widget_over_market(cx, config(), entries, source, false)
    }

    /// The same, with the venue loaded or not.
    ///
    /// The directory the pane reads is built first and the market participants root is wired to the very
    /// catalogue it holds: the gate asks the directory for the snapshot and the service resolves
    /// listings against the root's own wiring, so a fixture that gave them two different
    /// catalogues could not tell a gate/read mismatch from a working pane.
    fn widget_over_market<S>(
        cx: &mut TestAppContext,
        config: CandlestickConfig,
        entries: Vec<CatalogEntry>,
        source: S,
        loaded: bool,
    ) -> WidgetUnderTest
    where
        S: CandlestickSource,
    {
        widget_over_market_with(cx, config, entries, loaded, move |_| source)
    }

    /// The same, for a source built from the catalogue the fixture installs.
    ///
    /// A stream's own resolution is what a venue reload invalidates, so a test that asserts the
    /// chart reopened its stream needs a source that reads the generation in memory at the moment
    /// it is asked: the catalogue the fixture publishes is handed to `build` rather than copied.
    fn widget_over_market_with<S>(
        cx: &mut TestAppContext,
        config: CandlestickConfig,
        entries: Vec<CatalogEntry>,
        loaded: bool,
        build: impl FnOnce(&Arc<Catalog>) -> S,
    ) -> WidgetUnderTest
    where
        S: CandlestickSource,
    {
        cx.executor().allow_parking();
        let config_reports = Rc::new(Cell::new(0));
        let reports = config_reports.clone();
        // The market is installed and its commit flushed before the chart is built, which is the
        // order production has: the user loads the market, and a chart opened afterwards reads the
        // generation that load published rather than a commit about a market it was not reading.
        let source = cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            global::install(cx).expect("test Tokio runtime should build");
            let fixture = Fixture::new(entries.clone());
            let catalog = Arc::clone(fixture.catalog_arc());
            install_connector_with(cx, &catalog, loaded);
            // The publication goes through the same catalogue the root is wired to, so the pane's
            // gate and its reads look at one directory rather than two.
            if loaded {
                test_support::publish_fixture_catalogs(cx, &[(route(), entries)]);
            }
            build(&catalog)
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let config_change = WidgetConfigChange::new(move |_| {
                reports.set(reports.get() + 1);
            });
            let widget = cx.new(move |cx| {
                let status = cx.new(|_| WidgetStatus::new());
                let weak: WeakEntity<CandlestickWidget> = cx.weak_entity();
                let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
                    let _ = weak.update(cx, |widget, cx| widget.load(cx));
                });

                CandlestickWidget::new_with_source(
                    config,
                    source,
                    status,
                    config_change,
                    recovery,
                    cx,
                )
            });
            widget.update(cx, |widget, cx| widget.load(cx));
            WidgetUnderTest {
                widget,
                config_reports,
            }
        })
    }

    /// The same, over a directory that publishes the charted listing.
    fn widget(cx: &mut TestAppContext) -> WidgetUnderTest {
        let built = widget_over(cx, config(), vec![entry(SPOT_SYMBOL)], PendingSource);
        answered(cx, &built.widget);
        built
    }

    /// Waits for the directory's answer to be applied.
    ///
    /// It comes from the application runtime, so `run_until_parked` alone does not
    /// wait for it. A chart that accepted its selection starts its countdown; one
    /// whose listing the directory does not publish goes back to having nothing
    /// chosen, and either way the settle is behind the widget.
    fn answered(cx: &mut TestAppContext, widget: &Entity<CandlestickWidget>) {
        settle_until(cx, "the directory to answer", |cx| {
            let widget = widget.read(cx);
            widget.countdown_task.is_some() || widget.config.selection.is_none()
        });
    }

    /// Lets the chart and the application runtime take a turn each until `ready`.
    ///
    /// A request crosses both executors — the chart sends it from GPUI's, the service
    /// answers on the application runtime — so waiting for one of them alone waits for
    /// nothing.
    fn settle_until(cx: &mut TestAppContext, what: &str, ready: impl Fn(&App) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let handle = cx.update(|cx| global::handle(cx).handle().clone());
            handle.block_on(async { tokio::task::yield_now().await });
            if cx.update(|cx| ready(cx)) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{what} should happen");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Reports a snapshot as the stream would, then returns the widget.
    ///
    /// Cancelling the binding is what makes the hand-driven snapshot the only input
    /// a test has to reason about, and it also fails the load the binding was still
    /// holding: the cancelled runtime answers that call whenever it gets to it. A
    /// test that ticks after accepting therefore has to expect that answer, which
    /// arrives as a terminal failure — the relayout tests below are the ones that
    /// do.
    fn accept_snapshot(
        widget: &Entity<CandlestickWidget>,
        snapshot: CandlestickSeriesSnapshot,
        cx: &mut App,
    ) {
        widget.update(cx, |widget, context| {
            widget.binding.cancel();
            let Some(key) = widget.current_key() else {
                return;
            };
            widget.apply_latest(
                key,
                Arc::new(CandlestickStreamMessage::Snapshot(snapshot)),
                context,
            );
        });
    }

    fn status(widget: &Entity<CandlestickWidget>, cx: &App) -> WidgetState {
        let status = widget.read(cx).status.clone();
        status.state(cx)
    }

    /// A market the user has not loaded leaves the pane waiting for it, with the
    /// choice they stored left alone (⑮).
    #[gpui_kit::test]
    fn an_unloaded_market_should_leave_the_pane_waiting(cx: &mut TestAppContext) {
        let built = widget_over_unloaded(cx, vec![entry(SPOT_SYMBOL)], PendingSource);

        cx.update(|cx| {
            assert!(
                !built.widget.read(cx).has_content(),
                "there is nothing to chart"
            );
            assert_eq!(
                built.widget.read(cx).config.selection,
                config().selection,
                "a market that is not loaded does not unmake the user's choice"
            );
            let status = status(&built.widget, cx);
            assert!(
                status.failure().is_none(),
                "an unloaded market is not a failure"
            );
            assert_eq!(
                status,
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: vec![route().venue],
                    supported: listed_venues(),
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
        let built = widget_over_unloaded(cx, vec![entry(SPOT_SYMBOL)], PendingSource);

        let (notice, english) = cx.update(|cx| {
            let notice = status(&built.widget, cx)
                .notice()
                .cloned()
                .expect("the pane waits for its market");
            let sentence = notice.resolve(cx);
            assert!(
                sentence.contains("Binance"),
                "the notice starts out in English: {sentence}"
            );
            // The list the sentence ends with is the pane's capability, not its selection: a
            // chart following this venue still names every venue that answers with candles,
            // one per line, so a reader learns what they could load instead. A line is the
            // venue's logo and its bold name, which is what this fixture writes out.
            assert!(
                expected_market_list(cx)
                    .iter()
                    .all(|line| sentence.contains(line)),
                "the notice lists every venue that answers with candles: {sentence:?}"
            );

            (notice, sentence.to_string())
        });

        cx.update(|cx| cx.set_global(LanguageCode::ZhCn));

        cx.update(|cx| {
            let sentence = notice.resolve(cx);
            assert!(
                expected_market_list(cx)
                    .iter()
                    .all(|line| sentence.contains(line)),
                "and it says the same thing in Chinese now, list included: {sentence}"
            );
            assert!(
                sentence.contains("币安") && sentence.contains("欧易"),
                "the names are the ones this language gives: {sentence}"
            );
            assert_ne!(
                sentence, english,
                "the reference is resolved again in the language showing now, so the words moved: \
                 {sentence}"
            );
            assert_eq!(
                status(&built.widget, cx).notice(),
                Some(&notice),
                "the reference is what the pane reported; nothing re-reported it"
            );
        });
    }

    /// The pane that has nothing chosen is waiting too, and its prompt is a reference of the
    /// same kind: the pane does not care which of the two notices stands on screen.
    #[gpui_kit::test]
    fn a_language_change_should_relabel_the_prompt_of_a_pane_with_nothing_chosen(
        cx: &mut TestAppContext,
    ) {
        let built = widget_over(cx, CandlestickConfig::default(), Vec::new(), PendingSource);

        let (notice, english) = cx.update(|cx| {
            let notice = status(&built.widget, cx)
                .notice()
                .cloned()
                .expect("the pane asks for an instrument");
            assert_eq!(
                notice,
                Notice::message("widget.candlestick.select_instrument_prompt"),
                "the pane asks with the catalogue's own prompt"
            );
            let english = notice.resolve(cx);

            (notice, english)
        });

        cx.update(|cx| cx.set_global(LanguageCode::ZhCn));

        cx.update(|cx| {
            assert_eq!(
                notice.resolve(cx),
                t!(cx, "widget.candlestick.select_instrument_prompt"),
                "and it asks in the language showing now"
            );
            assert_ne!(
                notice.resolve(cx),
                english,
                "which is not the copy the interface left"
            );
        });
    }

    /// Loading the venue is what a waiting pane recovers from, by itself and
    /// without being reopened (⑯).
    #[gpui_kit::test]
    fn loading_the_venue_should_let_the_waiting_pane_start(cx: &mut TestAppContext) {
        let built = widget_over_unloaded(cx, vec![entry(SPOT_SYMBOL)], PendingSource);
        cx.update(|cx| {
            assert!(
                status(&built.widget, cx).notice().is_some(),
                "the pane waits for a market nobody loaded"
            );

            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(
                        &cx.global::<MarketParticipantsConnector>()
                            .supported_venues(),
                    ),
                    &[],
                    &[],
                )
                .expect("the venue's routes load");
            // A publication is what emits the directory's change signal; a load alone
            // does not.
            test_support::publish_fixture_catalogs(cx, &[(route(), vec![entry(SPOT_SYMBOL)])]);
        });
        // The directory signals its publications through its own task, so the event
        // that re-runs the load arrives on the executor.
        cx.run_until_parked();

        cx.update(|cx| {
            let status = status(&built.widget, cx);
            assert!(
                status.notice().is_none(),
                "the pane left the notice without being reopened"
            );
            assert!(status.failure().is_none());
            assert_eq!(status, WidgetState::Connecting);
        });
    }

    #[gpui_kit::test]
    fn a_widget_without_a_snapshot_should_report_connecting_and_have_no_content(
        cx: &mut TestAppContext,
    ) {
        let built = widget(cx);
        let widget = built.widget;

        cx.update(|cx| {
            assert!(!widget.read(cx).has_content());
            assert_eq!(status(&widget, cx), WidgetState::Connecting);
        });
    }

    /// A listing the directory does not publish is a scope that cannot be charted, so
    /// the chart goes back to having nothing chosen and reports the reset.
    #[gpui_kit::test]
    fn a_selection_missing_from_the_directory_should_be_cleared_and_reported(
        cx: &mut TestAppContext,
    ) {
        let built = widget_over(cx, config(), vec![entry("ETHUSDT")], PendingSource);
        answered(cx, &built.widget);

        cx.update(|cx| {
            assert_eq!(built.widget.read(cx).config.selection, None);
            assert!(
                built.reports() >= 1,
                "the reset has to be written back, or the next start resets it again"
            );
            assert!(!built.widget.read(cx).has_content());
            assert_eq!(
                status(&built.widget, cx),
                WidgetState::waiting(Notice::message(
                    "widget.candlestick.select_instrument_prompt"
                )),
                "a chart with nothing chosen asks for one"
            );
        });
    }

    /// A route a shrinking commit took away leaves the chart waiting for the user to
    /// load it again instead of clearing the choice they made: the notice is where a
    /// withdrawn scope goes, and the stored selection is not the pane's to throw away.
    #[gpui_kit::test]
    fn a_revoked_route_should_leave_the_selection_alone_and_be_reported(cx: &mut TestAppContext) {
        let built = widget_over(cx, config(), vec![entry(SPOT_SYMBOL)], PendingSource);
        answered(cx, &built.widget);
        cx.update(|cx| {
            assert_eq!(
                built.widget.read(cx).config.selection.as_ref(),
                Some(&selection())
            );
        });
        let before = built.reports();

        cx.update(unload_venue);
        cx.update(|cx| built.widget.update(cx, |widget, cx| widget.load(cx)));

        cx.update(|cx| {
            assert_eq!(
                built.widget.read(cx).config.selection.as_ref(),
                Some(&selection()),
                "a market that went away does not unmake the user's choice"
            );
            assert_eq!(
                built.reports(),
                before,
                "nothing about the choice changed, so nothing is written back"
            );
            assert!(!built.widget.read(cx).has_content());
            let status = status(&built.widget, cx);
            assert_eq!(
                status,
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: vec![route().venue],
                    supported: listed_venues(),
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

    /// A market a shrinking commit takes away while the chart is already reading is a
    /// wait, not a scope that stopped serving candlesticks.
    ///
    /// The pane passed its gate and asked the service for a stream, so the answer that
    /// comes back is the one the request gives when the scope left the loaded venues in
    /// between: "this market is not loaded". Settling that like a route whose descriptor
    /// lacks candlesticks would throw the user's own choice away, and reporting it as a
    /// failure would leave a pane failed over a market one load brings back: the
    /// chart waits for it, with the selection it was given.
    #[gpui_kit::test]
    fn a_market_taken_away_mid_flight_should_leave_the_pane_waiting(cx: &mut TestAppContext) {
        let source = RevokedMarketSource::default();
        let built = widget_over(cx, config(), vec![entry(SPOT_SYMBOL)], source.clone());

        // The pane is past its gate: the market is loaded, the selection is the user's,
        // and the stream the service is opening is the one the commit lands on.
        cx.update(|cx| {
            assert_eq!(
                built.widget.read(cx).config.selection.as_ref(),
                Some(&selection()),
                "the chart follows what the user chose"
            );
            assert_eq!(
                status(&built.widget, cx),
                WidgetState::Connecting,
                "and it is reading its market"
            );
        });
        let before = built.reports();

        // The commit lands on the request that is already in flight.
        source.answer.notify_one();
        settle_until(cx, "the chart to notice its market went away", |cx| {
            status(&built.widget, cx).notice().is_some()
        });

        cx.update(|cx| {
            assert_eq!(
                built.widget.read(cx).config.selection.as_ref(),
                Some(&selection()),
                "a market that went away does not unmake the user's choice"
            );
            assert_eq!(
                built.reports(),
                before,
                "nothing about the choice changed, so nothing is written back"
            );
            assert!(!built.widget.read(cx).has_content());
            let status = status(&built.widget, cx);
            assert!(
                status.failure().is_none(),
                "a market the user can load again is not a failure"
            );
            assert_eq!(
                status,
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: vec![route().venue],
                    supported: listed_venues(),
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

        // The commit that took the market away reaches the pane as well, and the notice
        // is where it stays: still not a failure, and still nothing written back.
        cx.update(unload_venue);
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                built.widget.read(cx).config.selection.as_ref(),
                Some(&selection())
            );
            assert_eq!(built.reports(), before, "and still nothing is written back");
            let status = status(&built.widget, cx);
            assert!(status.failure().is_none(), "the notice is not a failure");
            assert!(status.notice().is_some(), "the pane is still waiting");
        });
    }

    /// A market whose load never committed is a market the pane waits for, and waiting
    /// is not a reason to drop what the user chose: the selection is kept and nothing
    /// about it is written back.
    ///
    /// The fixture is the failing directory with nothing loaded, because a load that
    /// fails leaves no venue behind at all: "the venue is loaded" and "its catalog
    /// is here" are one statement, so a venue whose directory failed is a venue
    /// the pane waits for.
    #[gpui_kit::test]
    fn a_directory_failure_should_preserve_the_selection(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let config_reports = Rc::new(Cell::new(0));
        let reports = config_reports.clone();
        let widget = cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            global::install(cx).expect("test Tokio runtime should build");
            let installation = test_support::install_failing(cx);
            let catalog = Arc::clone(installation.catalog_arc());
            install_connector_with(cx, &catalog, false);
            let widget = cx.new(move |cx| {
                let status = cx.new(|_| WidgetStatus::new());
                let weak: WeakEntity<CandlestickWidget> = cx.weak_entity();
                let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
                    let _ = weak.update(cx, |widget, cx| widget.load(cx));
                });

                CandlestickWidget::new_with_source(
                    config(),
                    PendingSource,
                    status,
                    WidgetConfigChange::new(move |_| reports.set(reports.get() + 1)),
                    recovery,
                    cx,
                )
            });
            widget.update(cx, |widget, cx| widget.load(cx));
            widget
        });

        cx.update(|cx| {
            assert_eq!(widget.read(cx).config.selection, config().selection);
            assert_eq!(
                config_reports.get(),
                0,
                "a market that is not in memory must not change the persisted config"
            );
            let status = status(&widget, cx);
            assert_eq!(
                status,
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: vec![route().venue],
                    supported: listed_venues(),
                }),
                "the pane waits for its market rather than reading nothing"
            );
            assert!(
                status.failure().is_none(),
                "an unloaded market is not a failure of the chart"
            );
            assert!(
                status
                    .notice()
                    .is_some_and(|notice| notice.resolve(cx).contains("Binance")),
                "the pane says which market to load"
            );
        });
    }

    /// The lookup is advisory, and the loaded venue is the prerequisite: a chart whose
    /// market was never loaded starts no stream at all rather than subscribing to a
    /// route nobody has loaded.
    #[gpui_kit::test]
    fn a_market_that_is_not_in_memory_should_not_start_a_stream(cx: &mut TestAppContext) {
        let source = CountingSource::default();
        let demands = Arc::clone(&source.subscriptions);
        cx.executor().allow_parking();
        let widget = cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            global::install(cx).expect("test Tokio runtime should build");
            let installation = test_support::install_failing(cx);
            let catalog = Arc::clone(installation.catalog_arc());
            install_connector_with(cx, &catalog, false);
            let widget = cx.new(move |cx| {
                let status = cx.new(|_| WidgetStatus::new());
                let weak: WeakEntity<CandlestickWidget> = cx.weak_entity();
                let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
                    let _ = weak.update(cx, |widget, cx| widget.load(cx));
                });

                CandlestickWidget::new_with_source(
                    config(),
                    source,
                    status,
                    WidgetConfigChange::default(),
                    recovery,
                    cx,
                )
            });
            widget.update(cx, |widget, cx| widget.load(cx));
            widget
        });

        cx.update(|cx| {
            assert_eq!(
                demands.load(Ordering::Acquire),
                0,
                "a market the user has not loaded is not asked for candles"
            );
            assert_eq!(widget.read(cx).config.selection, config().selection);
            let status = status(&widget, cx);
            assert_eq!(
                status,
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: vec![route().venue],
                    supported: listed_venues(),
                }),
                "waiting is not working"
            );
            assert!(
                status.failure().is_none(),
                "waiting for a market is not a failure of the chart"
            );
        });
    }

    /// Keeping the listing and switching the interval asks for a new stream over the
    /// scope the user picked.
    #[gpui_kit::test]
    fn switching_the_interval_should_request_the_new_scope(cx: &mut TestAppContext) {
        let source = CountingSource::default();
        let demands = Arc::clone(&source.subscriptions);
        cx.executor().allow_parking();
        let widget = cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            global::install(cx).expect("test Tokio runtime should build");
            let fixture = Fixture::new(vec![entry(SPOT_SYMBOL)]);
            let catalog = Arc::clone(fixture.catalog_arc());
            install_connector_with(cx, &catalog, true);
            test_support::publish_fixture_catalogs(cx, &[(route(), vec![entry(SPOT_SYMBOL)])]);
            let widget = cx.new(move |cx| {
                let status = cx.new(|_| WidgetStatus::new());
                let weak: WeakEntity<CandlestickWidget> = cx.weak_entity();
                let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
                    let _ = weak.update(cx, |widget, cx| widget.load(cx));
                });

                CandlestickWidget::new_with_source(
                    config(),
                    source,
                    status,
                    WidgetConfigChange::default(),
                    recovery,
                    cx,
                )
            });
            widget.update(cx, |widget, cx| widget.load(cx));
            widget
        });
        settle_until(cx, "the first scope to be requested", |_| {
            demands.load(Ordering::Acquire) == 1
        });

        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                widget.select_interval(TimeFrame::Minute5, context);
            });
        });
        settle_until(cx, "the new interval to be requested", |_| {
            demands.load(Ordering::Acquire) == 2
        });

        cx.update(|cx| {
            let chart = widget.read(cx);
            assert_eq!(chart.config.timeframe, TimeFrame::Minute5);
            assert_eq!(chart.config.selection, config().selection);
            assert_eq!(
                chart.current_key(),
                Some(CandlestickStreamKey::new(
                    listing(),
                    TimeFrame::Minute5,
                    Adjustment::None
                )),
                "the chart is streaming the interval the user picked"
            );
            let status = status(&widget, cx);
            assert_eq!(status, WidgetState::Connecting);
            assert!(
                status.failure().is_none(),
                "switching an interval is not a failure"
            );
        });
    }

    /// Two listings of one canonical instrument are two scopes: the second one
    /// replaces the first in the config and in the stream key, and neither is folded
    /// into the other by their shared instrument identity.
    #[gpui_kit::test]
    fn two_listings_of_one_instrument_should_stay_distinct_scopes(cx: &mut TestAppContext) {
        let entries = vec![entry(SPOT_SYMBOL), entry("BTCUSDT_ALT")];
        assert_eq!(
            entries[0].instrument.id(),
            entries[1].instrument.id(),
            "the fixture publishes one instrument under two listings"
        );
        assert_ne!(entries[0].listing.key, entries[1].listing.key);
        let built = widget_over(cx, config(), entries, PendingSource);
        answered(cx, &built.widget);
        let first_key = cx.update(|cx| {
            built
                .widget
                .read(cx)
                .current_key()
                .expect("the chart follows a listing")
        });

        let second = InstrumentSelection {
            listing: test_support::listing_key(&route(), "BTCUSDT_ALT"),
        };
        cx.update(|cx| {
            assert!(built.widget.update(cx, |widget, cx| {
                widget.select_selection(second.clone(), cx)
            }));
        });

        cx.update(|cx| {
            let chart = built.widget.read(cx);
            assert_eq!(
                chart.config.selection.as_ref(),
                Some(&second),
                "the second listing of the instrument is the configured scope"
            );
            assert_ne!(
                chart.current_key(),
                Some(first_key),
                "another listing of the same instrument is another stream"
            );
            assert_eq!(
                chart.current_key(),
                Some(CandlestickStreamKey::new(
                    second.listing.clone(),
                    TimeFrame::Minute1,
                    Adjustment::None
                )),
                "the stream key carries the listing, not the instrument"
            );
        });
    }

    /// The persisted form keeps the two apart as well: a config that names one listing
    /// never decodes into the other.
    #[test]
    fn two_listings_of_one_instrument_should_persist_apart() {
        let first = config();
        let mut second = config();
        second.selection = Some(InstrumentSelection {
            listing: test_support::listing_key(&route(), "BTCUSDT_ALT"),
        });

        let stored_first = VersionedWidgetConfig::encode(&first).expect("config should encode");
        let stored_second = VersionedWidgetConfig::encode(&second).expect("config should encode");

        assert_ne!(stored_first, stored_second);
        assert_eq!(
            stored_first.decode::<CandlestickConfig>().selection,
            first.selection
        );
        assert_eq!(
            stored_second.decode::<CandlestickConfig>().selection,
            second.selection
        );
    }

    /// A payload written before the listing shape existed names a route and an
    /// instrument, which is not a listing: it cannot decode, so the chart reads the
    /// defaults and the framework writes them back.
    #[test]
    fn an_old_payload_should_decode_to_the_default_config() {
        let legacy = concat!(
            r#"{"selection":{"route":{"venue":"binance","scope":"spot"},"#,
            r#""instrument":{"symbol":"BTCUSDT"}},"timeframe":"1m"}"#,
        );
        let stored = serde_json::from_value::<VersionedWidgetConfig>(serde_json::json!({
            "schema_version": 1,
            "payload": legacy,
        }))
        .expect("the envelope should decode");

        let decoded = stored.decode::<CandlestickConfig>();

        assert_eq!(decoded.selection, None);
        assert_eq!(decoded.timeframe, CandlestickConfig::default().timeframe);
    }

    #[gpui_kit::test]
    fn an_accepted_snapshot_should_become_content_and_clear_the_status(cx: &mut TestAppContext) {
        let built = widget(cx);
        let widget = built.widget;

        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                widget.status.report(WidgetState::Loading, context);
            });
            accept_snapshot(&widget, snapshot_of([candle(1, 100)]), cx);

            assert!(widget.read(cx).has_content());
            assert_eq!(
                widget.read(cx).countdown.candle_timestamp,
                Some(1),
                "the chart counts down to the newest candle it was given"
            );
            widget.update(cx, |widget, context| {
                widget.status.report(WidgetState::Ready, context);
            });
            assert_eq!(status(&widget, cx), WidgetState::Ready);
            assert!(status(&widget, cx).failure().is_none());
        });
    }

    /// Connecting successfully is enough to show the chart, and an empty
    /// snapshot is content — a chart with no candles — not a state to report: the
    /// pane says the plot holds nothing itself.
    #[gpui_kit::test]
    fn an_empty_snapshot_should_be_content_and_report_nothing(cx: &mut TestAppContext) {
        let built = widget(cx);
        let widget = built.widget;

        cx.update(|cx| {
            accept_snapshot(&widget, snapshot_of([]), cx);

            assert!(widget.read(cx).has_content());
            widget.update(cx, |widget, context| {
                widget.status.report(WidgetState::Ready, context);
            });
            let status = status(&widget, cx);
            assert_eq!(status, WidgetState::Ready);
            assert!(status.failure().is_none());
            assert!(
                status.notice().is_none(),
                "the empty state belongs to the pane's own body, not to the framework's placeholder"
            );
        });
    }

    #[gpui_kit::test]
    fn a_failure_with_content_should_be_reported_and_keep_the_chart(cx: &mut TestAppContext) {
        let built = widget(cx);
        let widget = built.widget;

        cx.update(|cx| {
            accept_snapshot(&widget, snapshot_of([candle(1, 100)]), cx);
            widget.update(cx, |widget, context| {
                let recovery = widget.recovery.clone();
                widget.status.report(
                    WidgetState::failed(
                        Arc::new(CandlestickFailure::Market(disconnect())),
                        recovery,
                    ),
                    context,
                );
            });

            assert!(
                widget.read(cx).has_content(),
                "a failure must not throw away candles the widget already has"
            );
            let status = status(&widget, cx);
            assert!(status.is_failed());
            assert_eq!(
                status.failure().map(|failure| failure.text(cx)),
                Some(disconnect().localized_message(cx))
            );
        });
    }

    #[gpui_kit::test]
    fn a_failure_without_content_should_be_reported_with_no_content(cx: &mut TestAppContext) {
        let built = widget(cx);
        let widget = built.widget;

        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                let recovery = widget.recovery.clone();
                widget.status.report(
                    WidgetState::failed(
                        Arc::new(CandlestickFailure::Market(disconnect())),
                        recovery,
                    ),
                    context,
                );
            });

            assert!(!widget.read(cx).has_content());
            assert!(status(&widget, cx).is_failed());
        });
    }

    #[gpui_kit::test]
    fn a_stream_failure_should_become_the_reported_state_and_stop_the_countdown(
        cx: &mut TestAppContext,
    ) {
        let built = widget(cx);
        let widget = built.widget;

        cx.update(|cx| {
            accept_snapshot(&widget, snapshot_of([candle(1, 100)]), cx);
            widget.update(cx, |widget, context| {
                assert!(widget.countdown_task.is_some());
                let key = widget
                    .current_key()
                    .expect("an accepted snapshot implies a scope");
                widget.apply_reliable(
                    key,
                    Arc::new(CandlestickStreamMessage::Status(CandlestickStatus::Error(
                        disconnect(),
                    ))),
                    context,
                );

                assert!(widget.status.is_failed(context));
                assert!(
                    widget.countdown_task.is_none(),
                    "nothing is being built once the stream is gone"
                );
            });
        });
    }

    #[gpui_kit::test]
    fn every_failure_should_offer_the_recovery_that_reloads_the_widget(cx: &mut TestAppContext) {
        let built = widget(cx);
        let widget = built.widget;

        cx.update(|cx| {
            accept_snapshot(&widget, snapshot_of([candle(1, 100)]), cx);
            widget.update(cx, |widget, context| {
                let recovery = widget.recovery.clone();
                widget.status.report(
                    WidgetState::failed(
                        Arc::new(CandlestickFailure::Market(disconnect())),
                        recovery,
                    ),
                    context,
                );
            });

            let recovery = status(&widget, cx)
                .failure()
                .map(|failure| failure.action().clone())
                .expect("a failure should offer a way out");
            assert_eq!(recovery.label(cx).as_ref(), "Retry");
            assert!(recovery.is_enabled());

            recovery.run(cx);

            assert_eq!(status(&widget, cx), WidgetState::Connecting);
            assert!(status(&widget, cx).failure().is_none());
        });
    }

    #[gpui_kit::test]
    fn a_late_stream_message_after_a_failure_should_not_change_the_status(cx: &mut TestAppContext) {
        let built = widget(cx);
        let widget = built.widget;

        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                widget.binding.cancel();
                let recovery = widget.recovery.clone();
                widget.status.report(
                    WidgetState::failed(
                        Arc::new(CandlestickFailure::Market(disconnect())),
                        recovery,
                    ),
                    context,
                );
                let key = widget
                    .current_key()
                    .expect("the test config follows a listing");

                widget.apply_reliable(
                    key.clone(),
                    Arc::new(CandlestickStreamMessage::Status(CandlestickStatus::Ready)),
                    context,
                );
                widget.apply_latest(
                    key,
                    Arc::new(CandlestickStreamMessage::Snapshot(snapshot_of([candle(
                        1, 100,
                    )]))),
                    context,
                );

                assert!(widget.status.is_failed(context));
                assert!(widget.snapshot.is_none());
            });
        });
    }

    #[gpui_kit::test]
    fn a_snapshot_for_another_scope_should_be_ignored(cx: &mut TestAppContext) {
        let built = widget(cx);
        let widget = built.widget;

        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                widget.binding.cancel();
                let other = CandlestickStreamKey::new(
                    test_support::listing_key(&test_support::perp_route(), "BTCUSDT"),
                    TimeFrame::Minute5,
                    Adjustment::None,
                );
                widget.apply_latest(
                    other,
                    Arc::new(CandlestickStreamMessage::Snapshot(snapshot_of([candle(
                        1, 100,
                    )]))),
                    context,
                );

                assert!(
                    widget.snapshot.is_none(),
                    "a message from the previous scope must not be drawn"
                );
            });
        });
    }

    /// An explicit reload drops what it had and starts over from connecting.
    #[gpui_kit::test]
    fn an_explicit_reload_should_start_over_from_connecting(cx: &mut TestAppContext) {
        let built = widget(cx);
        let widget = built.widget;

        cx.update(|cx| {
            accept_snapshot(&widget, snapshot_of([candle(1, 100)]), cx);
            assert!(widget.read(cx).has_content());

            widget.update(cx, |widget, context| widget.load(context));

            assert!(!widget.read(cx).has_content());
            assert_eq!(status(&widget, cx), WidgetState::Connecting);
            assert!(status(&widget, cx).failure().is_none());
        });
    }

    /// The framework also calls `load`, and the two cases must not be confused:
    /// a retry keeps the chart, an explicit reload does not.
    #[gpui_kit::test]
    fn a_retry_after_a_failure_should_keep_the_chart_and_drop_the_pages_in_flight(
        cx: &mut TestAppContext,
    ) {
        let built = widget(cx);
        let widget = built.widget;

        cx.update(|cx| {
            accept_snapshot(&widget, snapshot_of([candle(1, 100)]), cx);
            widget.update(cx, |widget, context| {
                widget.fetching_history = true;
                widget.fetching_newer = true;
                let recovery = widget.recovery.clone();
                widget.status.report(
                    WidgetState::failed(
                        Arc::new(CandlestickFailure::Market(disconnect())),
                        recovery,
                    ),
                    context,
                );
            });

            let recovery = status(&widget, cx)
                .failure()
                .map(|failure| failure.action().clone())
                .expect("a failure should offer a way out");
            recovery.run(cx);

            let reported = status(&widget, cx);
            let widget = widget.read(cx);
            assert!(
                widget.has_content(),
                "a retry must not blank the chart the user is still reading"
            );
            assert_eq!(reported, WidgetState::Connecting);
            assert!(
                !widget.fetching_history && !widget.fetching_newer,
                "the pages that were in flight belonged to the replaced generation"
            );
        });
    }

    #[gpui_kit::test]
    fn selecting_another_listing_should_switch_the_scope_and_drop_the_chart(
        cx: &mut TestAppContext,
    ) {
        let entries = vec![entry(SPOT_SYMBOL), entry("ETHUSDT")];
        let built = widget_over(cx, config(), entries, PendingSource);
        answered(cx, &built.widget);

        cx.update(|cx| {
            accept_snapshot(&built.widget, snapshot_of([candle(1, 100)]), cx);
            let selection = InstrumentSelection {
                listing: test_support::listing_key(&route(), "ETHUSDT"),
            };

            let accepted = built.widget.update(cx, |widget, context| {
                widget.select_selection(selection.clone(), context)
            });

            assert!(accepted);
            let widget = built.widget.read(cx);
            assert_eq!(widget.config.selection.as_ref(), Some(&selection));
            assert!(
                widget.snapshot.is_none(),
                "candles from the previous listing must not be drawn under the new one"
            );
        });
    }

    #[gpui_kit::test]
    fn selecting_a_scope_that_is_already_shown_should_be_rejected(cx: &mut TestAppContext) {
        let built = widget(cx);

        cx.update(|cx| {
            let selection = built
                .widget
                .read(cx)
                .config
                .selection
                .clone()
                .expect("the test config follows a listing");

            let accepted = built.widget.update(cx, |widget, context| {
                widget.select_selection(selection, context)
            });

            assert!(!accepted);
        });
    }

    /// A stream that is still alive after its market went away must not put content
    /// back over the notice: the pane waits, and a snapshot already in flight belongs
    /// to a market the pane no longer reports reading. Without the guard the snapshot
    /// is accepted, `has_content()` turns true again and the framework draws the chart
    /// instead of the notice the user is supposed to act on.
    #[gpui_kit::test]
    fn a_late_snapshot_after_the_market_went_away_should_not_repaint_over_the_notice(
        cx: &mut TestAppContext,
    ) {
        let built = widget(cx);
        let widget = built.widget;
        cx.update(|cx| {
            accept_snapshot(&widget, snapshot_of([candle(1, 100)]), cx);
            assert!(widget.read(cx).has_content(), "the chart has candles");
            let key = widget
                .read(cx)
                .current_key()
                .expect("an accepted snapshot implies a scope");

            // The venue loaded again and published a scope set without this one: the
            // pane's own gate is what notices, and it reports the neutral notice.
            unload_venue(cx);
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
                    Arc::new(CandlestickStreamMessage::Snapshot(snapshot_of([candle(
                        2, 200,
                    )]))),
                    context,
                );
            });

            assert!(
                !widget.read(cx).has_content(),
                "a snapshot from the market that went away must not become content again"
            );
            assert!(
                status(&widget, cx).notice().is_some(),
                "the notice is still what the pane reports"
            );
        });
    }

    /// A scope the connection action discovered at runtime is in no static table, and
    /// the selector offers it anyway, because the selector is handed the loaded
    /// venues. Confirming a listing on such a scope therefore has to be accepted: a
    /// check that asked the static venue list as well would drop the click and say nothing,
    /// leaving the user with a control that silently refuses what it just offered.
    #[gpui_kit::test]
    fn a_discovered_scope_absent_from_the_static_list_should_be_accepted(cx: &mut TestAppContext) {
        let built = widget_over_unloaded(cx, vec![], PendingSource);
        let listing = discovered_selection();
        let venue =
            VenueId::from_name(test_support::VENUE).expect("the fixture venue is canonical");
        cx.update(|cx| {
            assert!(
                cx.global::<MarketParticipantsConnector>()
                    .venues()
                    .scope(&discovered_route())
                    .is_none(),
                "the fixture scope is in no static table and no load has published it"
            );
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(&[], &[(venue, vec![discovered_scope_entry()])], &[])
                .expect("the discovered scope loads");
            assert!(
                cx.global::<MarketParticipantsConnector>()
                    .venues()
                    .scope(&discovered_route())
                    .is_some(),
                "the load is what made the scope a scope"
            );
        });

        let accepted = cx.update(|cx| {
            built.widget.update(cx, |widget, context| {
                widget.select_selection(listing, context)
            })
        });

        assert!(
            accepted,
            "a discovered scope the selector offered must not be refused when confirmed"
        );
    }

    /// A commit of a venue this chart does not follow is not a change to the markets it reads:
    /// the pane neither re-runs its load nor restarts the stream it already has, so the candles
    /// the user is watching stay the ones being read.
    ///
    /// What proves the pane did not start over is the chart's own directory generation — the
    /// token [`CandlestickWidget::start_current`] mints once per start — beside the source's
    /// subscription count: a handler that loaded on every commit would bump both, whether or not
    /// the service found a reason to subscribe again.
    #[gpui_kit::test]
    fn a_single_venue_pane_wakes_only_for_its_own_venue(cx: &mut TestAppContext) {
        let source = CountingSource::default();
        let demands = Arc::clone(&source.subscriptions);
        let built = widget_over_unloaded(cx, vec![entry(SPOT_SYMBOL)], source);
        let venue =
            VenueId::from_name(test_support::VENUE).expect("the fixture venue is canonical");
        let elsewhere = VenueId::Okx;
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[venue]),
                    &[],
                    &[],
                )
                .expect("the chart's own venue loads");
            test_support::publish_fixture_catalogs(cx, &[(route(), vec![entry(SPOT_SYMBOL)])]);
        });
        settle_until(cx, "the chart to read its market", |_| {
            demands.load(Ordering::Acquire) == 1
        });
        let generation = cx.read(|cx| built.widget.read(cx).directory_generation);

        // Another venue commits: nothing about this chart's market moved, so the read it is in
        // the middle of is left exactly as it is.
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[elsewhere]),
                    &[],
                    &[],
                )
                .expect("another venue loads");
        });
        cx.run_until_parked();

        cx.update(|cx| {
            assert!(
                built.widget.read(cx).config.selection.is_some(),
                "the chart still follows the listing it was reading"
            );
            assert_eq!(
                built.widget.read(cx).directory_generation,
                generation,
                "a commit of a venue this chart does not follow must not start it over"
            );
            assert_eq!(
                demands.load(Ordering::Acquire),
                1,
                "a commit of a venue this chart does not follow must not restart its stream"
            );
        });
    }

    /// A refresh of the venue the chart is reading replaces the catalogue generation its live
    /// stream resolved the interval's increments against, while that stream keeps what it resolved
    /// when it was opened. So the chart starts over — the candles go, because nothing on screen can
    /// be vouched for — and the request it sends is the one that opens a new stream, at the
    /// generation the load published.
    ///
    /// What proves both halves is the source's own record: the subscription it was asked for at the
    /// moment the service opened it, and the increments that listing was published at then.
    #[gpui_kit::test]
    fn a_refresh_of_the_charts_own_venue_should_reopen_its_stream(cx: &mut TestAppContext) {
        let resolutions = Arc::new(Mutex::new(Vec::new()));
        let built = widget_over_market_with(cx, config(), vec![entry(SPOT_SYMBOL)], true, {
            let resolutions = Arc::clone(&resolutions);
            move |catalog| ResolvingSource {
                catalog: Arc::clone(catalog),
                resolutions,
            }
        });
        let venue =
            VenueId::from_name(test_support::VENUE).expect("the fixture venue is canonical");
        settle_until(cx, "the chart's stream to be opened", |_| {
            resolutions_of(&resolutions).len() == 1
        });
        let generation = cx.read(|cx| built.widget.read(cx).directory_generation);

        cx.update(|cx| {
            built.widget.update(cx, |widget, context| {
                let Some(key) = widget.current_key() else {
                    panic!("the chart follows the listing it was built with");
                };
                widget.apply_latest(
                    key,
                    Arc::new(CandlestickStreamMessage::Snapshot(snapshot_of([candle(
                        1, 100,
                    )]))),
                    context,
                );
            });
            assert!(built.widget.read(cx).has_content(), "the chart is reading");

            // The same venue loads again and the catalogue it republishes restates the listing's
            // price tick, which is the shape a production load has: the load re-fetches the
            // catalogue, and what it commits is a listing restated at the venue's own increments.
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(
                        std::slice::from_ref(&venue),
                    ),
                    &[],
                    &[],
                )
                .expect("the chart's venue loads again");
            test_support::publish_fixture_catalogs(cx, &[(route(), vec![restated_entry("0.01")])]);
        });
        settle_until(cx, "the chart to open a new stream", |_| {
            resolutions_of(&resolutions).len() == 2
        });

        cx.update(|cx| {
            assert_eq!(
                built.widget.read(cx).directory_generation,
                generation + 1,
                "a refresh of the venue the chart reads starts it over"
            );
            assert!(
                !built.widget.read(cx).has_content(),
                "and the candles are gone: they were written at the generation that was replaced"
            );
            assert_eq!(
                resolutions_of(&resolutions)[1].price_tick,
                Tick::from_str("0.01").expect("the restated tick should parse"),
                "the stream that was opened resolved at the generation this load published"
            );
        });
    }

    /// A chart opened with nothing chosen reads nothing, and the instrument the user then picks is
    /// what moves what it reads — not a venue commit. A commit of another venue therefore cannot
    /// move the chart's scope, so the pane leaves the stream it started and the candles it accepted
    /// exactly as they are rather than starting over what the user is watching.
    ///
    /// The commit is proved to have reached the pane by a probe subscribed after it did, and what
    /// proves the pane did not start over is its own directory generation beside the candles it
    /// was drawing and the status it reported for them.
    #[gpui_kit::test]
    fn choosing_an_instrument_should_leave_the_chart_alone_on_another_venues_commit(
        cx: &mut TestAppContext,
    ) {
        let source = CountingSource::default();
        let demands = Arc::clone(&source.subscriptions);
        let built = widget_over(
            cx,
            CandlestickConfig::default(),
            vec![entry(SPOT_SYMBOL)],
            source,
        );
        let widget = built.widget;

        // The commit the fixture itself made — the venue loading — is drained before anything is
        // chosen: what this test is about is the commit below, and one still in flight would be
        // handled as a commit of a market the chart had only just started following.
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
                    .select_selection(selection(), context)),
                "the picker offers the listing the loaded venue publishes"
            );
        });
        settle_until(
            cx,
            "the chart to read the instrument the user chose",
            |_| demands.load(Ordering::Acquire) == 1,
        );
        let generation = cx.read(|cx| widget.read(cx).directory_generation);

        // The chart is reading: the candles it accepted are the whole of what it draws.
        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                let Some(key) = widget.current_key() else {
                    panic!("the chart follows the instrument the user chose");
                };
                widget.apply_latest(
                    key,
                    Arc::new(CandlestickStreamMessage::Snapshot(snapshot_of([candle(
                        1, 100,
                    )]))),
                    context,
                );
            });
            widget.update(cx, |widget, context| {
                widget.status.report(WidgetState::Ready, context);
            });
            assert!(
                widget.read(cx).has_content(),
                "the chart is reading candles"
            );
        });

        // Another venue commits: nothing about this chart's scope moved, so the read it is in the
        // middle of is left exactly as it is.
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[VenueId::Okx]),
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
                "a commit of a venue this chart does not follow must not start it over"
            );
            assert!(
                widget.read(cx).snapshot.is_some(),
                "the candles the user is reading must not be dropped"
            );
            assert_eq!(
                status(&widget, cx),
                WidgetState::Ready,
                "and the pane must not go back to connecting"
            );
        });
    }

    /// The other direction of the same rule: a commit that takes the chart's own market away is a
    /// market it can no longer read, so the pane goes back to waiting for it — without being
    /// reopened, and without losing the choice the user made.
    ///
    /// A pane that is waiting with nothing chosen is not the case here: the chart was reading, and
    /// the commit is what moves its scope out of memory.
    #[gpui_kit::test]
    fn a_commit_that_takes_the_charts_own_market_away_should_send_it_back_to_waiting(
        cx: &mut TestAppContext,
    ) {
        let source = CountingSource::default();
        let demands = Arc::clone(&source.subscriptions);
        let built = widget_over(cx, config(), vec![entry(SPOT_SYMBOL)], source);
        let widget = built.widget;
        settle_until(cx, "the chart to read its market", |_| {
            demands.load(Ordering::Acquire) == 1
        });
        cx.update(|cx| {
            widget.update(cx, |widget, context| {
                let Some(key) = widget.current_key() else {
                    panic!("the chart follows the listing it was built with");
                };
                widget.apply_latest(
                    key,
                    Arc::new(CandlestickStreamMessage::Snapshot(snapshot_of([candle(
                        1, 100,
                    )]))),
                    context,
                );
            });
            assert!(widget.read(cx).has_content(), "the chart is reading");
        });

        let deliveries = Arc::new(AtomicUsize::new(0));
        let _probe = cx.update(|cx| {
            let events = cx.global::<MarketParticipantsConnector>().venues().events();
            cx.new(|cx| CommitProbe::new(cx, &events, Arc::clone(&deliveries)))
        });
        // The venue loads again and publishes no scope at all, which is what a shrinking commit
        // does: the chart's market is not in memory any more.
        cx.update(unload_venue);
        settle_until(cx, "the shrinking commit to reach its subscribers", |_| {
            deliveries.load(Ordering::Acquire) >= 1
        });

        cx.update(|cx| {
            assert!(
                widget.read(cx).awaiting_market,
                "the chart waits for the market it lost"
            );
            assert!(
                !widget.read(cx).has_content(),
                "and what it was drawing is not backed by a market any more"
            );
            assert_eq!(
                widget.read(cx).config.selection,
                config().selection,
                "a market that went away does not unmake the user's choice"
            );
            assert_eq!(
                status(&widget, cx),
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: vec![route().venue],
                    supported: listed_venues(),
                }),
                "waiting is not working"
            );
        });
    }

    /// The market a shrinking commit took away and the user then loads again brings the chart back
    /// — and that load reopens the stream, because the stream it still holds was opened at the
    /// generation the shrinking commit replaced.
    ///
    /// What this does not pin is *where* the flag is read. The predicate that sets it is the venue
    /// behind the stored selection, and a wait does not clear that selection, so the commit that
    /// brings the market back sets the flag again by itself. `force_orders`' twin is the one whose
    /// returning commit cannot — waiting empties the set it compares — and it is there that reading
    /// the flag past the gate is load-bearing.
    #[gpui_kit::test]
    fn a_market_that_comes_back_after_a_shrinking_commit_should_reopen_the_stream(
        cx: &mut TestAppContext,
    ) {
        let resolutions = Arc::new(Mutex::new(Vec::new()));
        let built = widget_over_market_with(cx, config(), vec![entry(SPOT_SYMBOL)], true, {
            let resolutions = Arc::clone(&resolutions);
            move |catalog| ResolvingSource {
                catalog: Arc::clone(catalog),
                resolutions,
            }
        });
        settle_until(cx, "the chart's stream to be opened", |_| {
            resolutions_of(&resolutions).len() == 1
        });

        cx.update(unload_venue);
        settle_until(cx, "the chart to wait for the market it lost", |cx| {
            built.widget.read(cx).awaiting_market
        });
        let generation = cx.read(|cx| built.widget.read(cx).directory_generation);

        let venue =
            VenueId::from_name(test_support::VENUE).expect("the fixture venue is canonical");
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(
                        std::slice::from_ref(&venue),
                    ),
                    &[],
                    &[],
                )
                .expect("the chart's venue loads again");
            test_support::publish_fixture_catalogs(cx, &[(route(), vec![restated_entry("0.01")])]);
        });
        settle_until(cx, "the chart to open a new stream", |_| {
            resolutions_of(&resolutions).len() == 2
        });

        cx.update(|cx| {
            assert!(
                !built.widget.read(cx).awaiting_market,
                "the chart is reading its market again"
            );
            assert_eq!(
                built.widget.read(cx).directory_generation,
                generation + 1,
                "the load the returning market unblocked starts the chart over"
            );
            assert_eq!(
                resolutions_of(&resolutions)[1].price_tick,
                Tick::from_str("0.01").expect("the restated tick should parse"),
                "and it reopens the stream rather than keeping the one the shrinking commit left"
            );
        });
    }

    /// A failure is terminal: a venue commit does not revive a failed chart, even when the market
    /// it follows leaves the loaded venues and comes back — a set that changed twice.
    ///
    /// The first commit is the one a pane without the terminal guard would act on: it takes the
    /// chart's own scope away, so an unguarded handler would replace the failure with the neutral
    /// notice. The second puts the market back, which an unguarded handler would read as a set
    /// that grew and would start a stream for.
    #[gpui_kit::test]
    fn a_venue_commit_should_not_revive_a_failed_pane(cx: &mut TestAppContext) {
        let source = CountingSource::default();
        let demands = Arc::clone(&source.subscriptions);
        let built = widget_over_unloaded(cx, vec![entry(SPOT_SYMBOL)], source);
        let venue =
            VenueId::from_name(test_support::VENUE).expect("the fixture venue is canonical");
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(
                        std::slice::from_ref(&venue),
                    ),
                    &[],
                    &[],
                )
                .expect("the chart's venue loads");
            test_support::publish_fixture_catalogs(cx, &[(route(), vec![entry(SPOT_SYMBOL)])]);
        });
        settle_until(cx, "the chart to read its market", |_| {
            demands.load(Ordering::Acquire) == 1
        });
        let generation = cx.read(|cx| built.widget.read(cx).directory_generation);

        // The chart fails, and a failure is the state only the user's own recovery leaves.
        cx.update(|cx| {
            built.widget.update(cx, |widget, context| {
                let recovery = widget.recovery.clone();
                widget.status.report(
                    WidgetState::failed(Arc::new(disconnect()), recovery),
                    context,
                );
            });
            assert!(
                status(&built.widget, cx).failure().is_some(),
                "the chart is in its terminal failure"
            );
        });

        let deliveries = Arc::new(AtomicUsize::new(0));
        let _probe = cx.update(|cx| {
            let events = cx.global::<MarketParticipantsConnector>().venues().events();
            cx.new(|cx| CommitProbe::new(cx, &events, Arc::clone(&deliveries)))
        });
        // The market the chart follows leaves the loaded venues...
        cx.update(unload_venue);
        settle_until(cx, "the shrinking commit to reach its subscribers", |_| {
            deliveries.load(Ordering::Acquire) >= 1
        });
        // ...and comes back, with the catalog it published before.
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(
                        std::slice::from_ref(&venue),
                    ),
                    &[],
                    &[],
                )
                .expect("the venue loads again");
            test_support::publish_fixture_catalogs(cx, &[(route(), vec![entry(SPOT_SYMBOL)])]);
        });
        settle_until(
            cx,
            "the commit that puts the market back to reach its subscribers",
            |_| deliveries.load(Ordering::Acquire) >= 2,
        );

        cx.update(|cx| {
            assert!(
                status(&built.widget, cx).failure().is_some(),
                "a venue commit does not revive a failed pane"
            );
            assert_eq!(
                built.widget.read(cx).directory_generation,
                generation,
                "the failed chart did not start over"
            );
            assert_eq!(
                demands.load(Ordering::Acquire),
                1,
                "the failed chart started no stream"
            );
        });
    }

    /// A scope the venue deployed is a scope the user may chart, and following one must
    /// not be undone by the pane's own settling: the loaded venues are the one answer
    /// to what may be followed, and the commit that published the scope is what
    /// makes it followable — nothing else is asked, and nothing about it is cleared.
    ///
    /// This is the regression defence of the read-point convergence: while the gate
    /// asked the static venue list beside the loaded venues, a deployed scope was
    /// dropped by the very pane whose selector had just offered it.
    #[gpui_kit::test]
    fn a_selection_never_clears_on_a_loadable_scope(cx: &mut TestAppContext) {
        let source = CountingSource::default();
        let demands = Arc::clone(&source.subscriptions);
        let built = widget_over_unloaded(cx, vec![], source);
        let venue =
            VenueId::from_name(test_support::VENUE).expect("the fixture venue is canonical");
        cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(&[], &[(venue, vec![discovered_scope_entry()])], &[])
                .expect("the deployed scope loads");
            test_support::publish_fixture_catalogs(
                cx,
                &[(
                    discovered_route(),
                    vec![test_support::entry(
                        &discovered_route(),
                        instrument(),
                        SPOT_SYMBOL,
                    )],
                )],
            );
        });
        cx.run_until_parked();

        let accepted = cx.update(|cx| {
            built.widget.update(cx, |widget, context| {
                widget.select_selection(discovered_selection(), context)
            })
        });
        assert!(
            accepted,
            "a scope the loaded venues answer is a scope this chart follows"
        );

        settle_until(cx, "the deployed scope to be requested", |_| {
            demands.load(Ordering::Acquire) >= 1
        });

        cx.update(|cx| {
            assert_eq!(
                built.widget.read(cx).config.selection,
                Some(discovered_selection()),
                "the selection of a loadable scope is never cleared by the pane's own settling"
            );
        });
    }

    /// A route that does not serve candlesticks is a route this chart cannot follow:
    /// the request says so in its own words, and the one item it names goes back to
    /// the default and is written back.
    #[gpui_kit::test]
    fn a_capability_rejection_should_clear_the_selection_and_report_it(cx: &mut TestAppContext) {
        let built = widget_over(
            cx,
            config(),
            vec![entry(SPOT_SYMBOL)],
            UnsupportedCapabilitySource,
        );

        settle_until(cx, "the rejected capability to be reported", |cx| {
            built.widget.read(cx).config.selection.is_none()
        });

        cx.update(|cx| {
            assert!(built.reports() >= 1, "the reset has to be written back");
            let status = status(&built.widget, cx);
            assert!(
                status.failure().is_none(),
                "a scope the route does not serve is not a stream failure to recover from"
            );
            assert_eq!(
                status,
                WidgetState::waiting(Notice::message(
                    "widget.candlestick.select_instrument_prompt"
                )),
                "a chart whose scope went away asks for another one"
            );
        });
    }

    /// An interval the route does not publish is replaced by the default one rather
    /// than by a neighbouring interval the user never asked for, and the replacement
    /// is reported so the pane is written back with the interval it is charting.
    #[gpui_kit::test]
    fn an_unsupported_interval_should_fall_back_to_the_default_and_be_reported(
        cx: &mut TestAppContext,
    ) {
        let source = RejectingTimeframeSource::default();
        let demands = Arc::clone(&source.subscriptions);
        let mut rejected = config();
        // Second candles are an interval the model can express, and not every route
        // publishes one.
        rejected.timeframe = TimeFrame::Second1;
        let built = widget_over(cx, rejected, vec![entry(SPOT_SYMBOL)], source);
        let default = CandlestickConfig::default().timeframe;

        settle_until(cx, "the default interval to be requested", |_| {
            demands.load(Ordering::Acquire) == 2
        });

        cx.update(|cx| {
            let widget = built.widget.read(cx);
            assert_eq!(widget.config.timeframe, default);
            assert!(
                built.reports() >= 1,
                "the replacement has to be written back"
            );
            assert_eq!(
                widget.current_key(),
                Some(CandlestickStreamKey::new(
                    listing(),
                    default,
                    Adjustment::None
                )),
                "the stream is requested at the interval the route serves"
            );
        });
    }

    /// The adjustment refusal is recognized, and it is recognized as the narrower
    /// statement rather than as a route that serves no candles.
    ///
    /// Both halves of the recovery read the typed error the market layer carries, and
    /// nothing else pins that: a future edit to the matching — or to the error the market
    /// layer wraps a refusal in — could stop recognizing it while every other test still
    /// passed, because the chart would simply report the refusal as a stream failure
    /// instead of resetting the convention. The precedence is asserted here too: a route
    /// that refuses one convention still serves candles, so the adjustment branch has to
    /// answer first, and the capability branch must not claim the same refusal.
    #[test]
    fn the_adjustment_refusal_is_read_as_an_unsupported_adjustment() {
        let route = route();
        let refused = MarketError::from(MarketParticipantError::UnsupportedAdjustment {
            route: route.clone(),
            adjustment: smol_str::SmolStr::new(Adjustment::Forward.label()),
        });
        let no_candles = MarketError::UnsupportedCapability {
            route,
            capability: CapabilityId::Candlesticks,
        };

        assert!(
            is_unsupported_adjustment(&refused),
            "the refusal the market layer wraps is the adjustment refusal: {refused:?}"
        );
        assert!(
            !is_unsupported_timeframe(&refused),
            "one refusal is not read as the other dimension's"
        );
        assert!(
            !is_unsupported_capability(&refused),
            "a route that refuses one convention is not a route that serves no candles"
        );
        assert!(
            is_unsupported_capability(&no_candles),
            "the capability refusal keeps its own reading"
        );
        assert!(
            !is_unsupported_adjustment(&no_candles),
            "and the adjustment reading does not claim it"
        );
    }

    /// An interval this build persisted may have stopped being published, and the chart asks
    /// its route before it subscribes: the interval goes back to the default, the reset is
    /// reported, and the stream is opened for the interval the route serves instead of being
    /// refused after the fact.
    #[gpui_kit::test]
    fn an_unpublished_interval_should_go_back_to_the_default_before_the_subscription(
        cx: &mut TestAppContext,
    ) {
        let default = CandlestickConfig::default().timeframe;
        let source = PublishingSource::publishing([default]);
        let mut stale = config();
        stale.timeframe = TimeFrame::Minute15;
        let built = widget_over(cx, stale, vec![entry(SPOT_SYMBOL)], source.clone());
        answered(cx, &built.widget);
        settle_until(cx, "the stream to be opened", |_| {
            !source.subscribed().is_empty()
        });

        cx.update(|cx| {
            assert_eq!(built.widget.read(cx).config.timeframe, default);
            assert!(
                status(&built.widget, cx).failure().is_none(),
                "the corrected interval is one the route serves"
            );
        });
        assert_eq!(built.reports(), 1, "the reset has to be written back");
        assert_eq!(
            source.queries(),
            1,
            "the route is asked which intervals it publishes"
        );
        assert_eq!(
            source.subscribed(),
            vec![default],
            "the stream is opened for the corrected interval, never for the one that is gone"
        );
    }

    /// An interval the route publishes is used as it was stored: nothing is reset and nothing
    /// is reported, so a chart whose interval is served is not written back on every load.
    #[gpui_kit::test]
    fn a_published_interval_should_be_subscribed_as_it_was_stored(cx: &mut TestAppContext) {
        let source = PublishingSource::publishing([TimeFrame::Minute1, TimeFrame::Minute15]);
        let mut stored = config();
        stored.timeframe = TimeFrame::Minute15;
        let built = widget_over(cx, stored, vec![entry(SPOT_SYMBOL)], source.clone());
        answered(cx, &built.widget);
        settle_until(cx, "the stream to be opened", |_| {
            !source.subscribed().is_empty()
        });

        cx.update(|cx| {
            assert_eq!(built.widget.read(cx).config.timeframe, TimeFrame::Minute15);
        });
        assert_eq!(built.reports(), 0, "a published interval is not a change");
        assert_eq!(source.queries(), 1);
        assert_eq!(source.subscribed(), vec![TimeFrame::Minute15]);
    }

    /// A route this build cannot ask proves nothing, so the interval is kept and the stream
    /// asks for it: a refusal is the venue's own answer to the request, which the pane already
    /// offers recovery for, rather than a reset nobody asked for.
    #[gpui_kit::test]
    fn an_unanswerable_route_should_keep_the_interval_it_was_stored_with(cx: &mut TestAppContext) {
        let source = PublishingSource::unknown([TimeFrame::Minute15]);
        let mut stored = config();
        stored.timeframe = TimeFrame::Minute15;
        let built = widget_over(cx, stored, vec![entry(SPOT_SYMBOL)], source.clone());
        answered(cx, &built.widget);
        settle_until(cx, "the stream to be opened", |_| {
            !source.subscribed().is_empty()
        });

        cx.update(|cx| {
            assert_eq!(built.widget.read(cx).config.timeframe, TimeFrame::Minute15);
        });
        assert_eq!(built.reports(), 0, "an answer nobody gave is not a reset");
        assert_eq!(source.queries(), 1);
        assert_eq!(source.subscribed(), vec![TimeFrame::Minute15]);
    }

    /// The picker offers the intervals the route publishes, and only those: a venue that serves
    /// a handful of the twenty-five intervals a request can carry must not be offered the rest,
    /// and the ones it does serve are offered in the widget's own order rather than in whatever
    /// order the route declared them.
    #[gpui_kit::test]
    fn the_interval_picker_should_offer_the_intervals_the_route_publishes(cx: &mut TestAppContext) {
        // A route shaped like the venues this build serves: a couple of the intervals the
        // request can carry, declared in an order of its own.
        let source = PublishingSource::publishing([TimeFrame::Minute5, TimeFrame::Minute1]);
        let built = widget_over(cx, config(), vec![entry(SPOT_SYMBOL)], source);
        answered(cx, &built.widget);

        let offered = cx.update(|cx| built.widget.read(cx).offered_intervals());

        assert_eq!(
            offered.len(),
            2,
            "the route publishes two intervals, so the picker offers two: {offered:?}"
        );
        assert_eq!(
            offered,
            vec![TimeFrame::Minute1, TimeFrame::Minute5],
            "the published intervals are offered in the widget's own order"
        );
    }

    /// A route this build cannot ask proves nothing about what it serves, so the picker keeps
    /// offering every interval a request can carry: the choice stays the user's, and the venue's
    /// own refusal of a request is what the pane's recovery settles.
    #[gpui_kit::test]
    fn an_unanswerable_route_should_leave_every_interval_to_the_picker(cx: &mut TestAppContext) {
        let source = PublishingSource::unknown([TimeFrame::Minute1]);
        let built = widget_over(cx, config(), vec![entry(SPOT_SYMBOL)], source);
        answered(cx, &built.widget);

        let offered = cx.update(|cx| built.widget.read(cx).offered_intervals());

        assert_eq!(
            offered.len(),
            TIMEFRAMES.len(),
            "an answer nobody gave offers the whole closed set: {offered:?}"
        );
        assert_eq!(
            offered,
            TIMEFRAMES.to_vec(),
            "and it is the closed set in the order the picker draws it"
        );
    }

    /// An adjustment convention this build persisted may have stopped being published, and the
    /// chart asks its route before it subscribes: the convention goes back to the neutral
    /// default, the reset is reported, and the stream is opened for a convention the route
    /// serves instead of being refused after the fact.
    #[gpui_kit::test]
    fn an_unpublished_adjustment_should_go_back_to_the_default_before_the_subscription(
        cx: &mut TestAppContext,
    ) {
        let source = PublishingSource::publishing([CandlestickConfig::default().timeframe]);
        let mut stale = config();
        stale.adjustment = Adjustment::Forward;
        let built = widget_over(cx, stale, vec![entry(SPOT_SYMBOL)], source.clone());
        answered(cx, &built.widget);
        settle_until(cx, "the stream to be opened", |_| {
            !source.subscribed_adjustments().is_empty()
        });

        cx.update(|cx| {
            assert_eq!(
                built.widget.read(cx).config.adjustment,
                Adjustment::None,
                "a convention the route does not publish is not the one it is asked for"
            );
        });
        assert_eq!(built.reports(), 1, "the reset has to be written back");
        assert_eq!(
            source.subscribed_adjustments(),
            vec![Adjustment::None],
            "the stream is opened for the convention the route serves, never for the one that is gone"
        );
    }

    /// A convention the route does publish is used as it was stored, and it reaches the stream:
    /// nothing is reset, nothing is reported, and the subscription asks for exactly the
    /// convention the chart persisted.
    #[gpui_kit::test]
    fn a_published_adjustment_should_be_subscribed_as_it_was_stored(cx: &mut TestAppContext) {
        let source = PublishingSource::publishing_with_adjustments(
            [CandlestickConfig::default().timeframe],
            [Adjustment::None, Adjustment::Forward],
        );
        let mut stored = config();
        stored.adjustment = Adjustment::Forward;
        let built = widget_over(cx, stored, vec![entry(SPOT_SYMBOL)], source.clone());
        answered(cx, &built.widget);
        settle_until(cx, "the stream to be opened", |_| {
            !source.subscribed_adjustments().is_empty()
        });

        cx.update(|cx| {
            assert_eq!(built.widget.read(cx).config.adjustment, Adjustment::Forward);
        });
        assert_eq!(built.reports(), 0, "a published convention is not a change");
        assert_eq!(
            source.subscribed_adjustments(),
            vec![Adjustment::Forward],
            "the stream carries the convention the chart stored"
        );
    }

    /// A route may refuse a convention its declaration lists, and that refusal is read as the
    /// convention being unsupported: it goes back to the neutral default, the reset is
    /// reported, and the chart is restarted instead of failing.
    #[gpui_kit::test]
    fn a_refused_adjustment_should_fall_back_to_the_default(cx: &mut TestAppContext) {
        let source = RejectingAdjustmentSource::default();
        let subscribed = Arc::clone(&source.subscribed);
        let mut stored = config();
        stored.adjustment = Adjustment::Forward;
        let built = widget_over(cx, stored, vec![entry(SPOT_SYMBOL)], source);

        settle_until(cx, "the default convention to be requested", |_| {
            subscribed
                .lock()
                .expect("the subscribed conventions should not be poisoned")
                .len()
                == 2
        });

        cx.update(|cx| {
            let widget = built.widget.read(cx);
            assert_eq!(widget.config.adjustment, Adjustment::None);
            assert!(
                status(&built.widget, cx).failure().is_none(),
                "a convention the route refuses is not a stream failure to recover from"
            );
        });
        assert!(built.reports() >= 1, "the reset has to be written back");
        assert_eq!(
            subscribed
                .lock()
                .expect("the subscribed conventions should not be poisoned")
                .clone(),
            vec![Adjustment::Forward, Adjustment::None],
            "the chart restarted with the convention the route serves"
        );
    }

    /// A route this build cannot ask proves nothing, so the convention is kept and the stream
    /// asks for it: a refusal is the venue's own answer to the request, rather than a reset
    /// nobody asked for.
    #[gpui_kit::test]
    fn an_unanswerable_route_should_keep_the_adjustment_it_was_stored_with(
        cx: &mut TestAppContext,
    ) {
        let default = CandlestickConfig::default().timeframe;
        let source =
            PublishingSource::unknown_accepting([default], [Adjustment::None, Adjustment::Forward]);
        let mut stored = config();
        stored.adjustment = Adjustment::Forward;
        let built = widget_over(cx, stored, vec![entry(SPOT_SYMBOL)], source.clone());
        answered(cx, &built.widget);
        settle_until(cx, "the stream to be opened", |_| {
            !source.subscribed_adjustments().is_empty()
        });

        cx.update(|cx| {
            assert_eq!(built.widget.read(cx).config.adjustment, Adjustment::Forward);
        });
        assert_eq!(built.reports(), 0, "an answer nobody gave is not a reset");
        assert_eq!(
            source.subscribed_adjustments(),
            vec![Adjustment::Forward],
            "the stream asks with the convention the chart stored"
        );
    }

    /// The adjustment is a dimension of the stream's scope: switching it opens a new stream
    /// rather than reusing the one whose candles are priced the other way.
    #[gpui_kit::test]
    fn a_changed_adjustment_should_be_subscribed_as_a_new_stream(cx: &mut TestAppContext) {
        let default = CandlestickConfig::default().timeframe;
        let source = PublishingSource::publishing_with_adjustments(
            [default],
            [Adjustment::None, Adjustment::Forward],
        );
        let built = widget_over(cx, config(), vec![entry(SPOT_SYMBOL)], source.clone());
        answered(cx, &built.widget);
        settle_until(cx, "the first stream to be opened", |_| {
            !source.subscribed_adjustments().is_empty()
        });

        built.widget.update(cx, |widget, cx| {
            widget.config.adjustment = Adjustment::Forward;
            widget.load(cx);
        });
        settle_until(cx, "the replacement stream to be opened", |_| {
            source.subscribed_adjustments().len() == 2
        });

        assert_eq!(
            source.subscribed_adjustments(),
            vec![Adjustment::None, Adjustment::Forward],
            "the replacement stream asks for the convention the chart switched to"
        );
    }

    /// The control offers the conventions the route publishes, in the order the route declares
    /// them: a route that publishes two gives the user a choice between them.
    #[gpui_kit::test]
    fn the_adjustment_control_should_offer_the_conventions_the_route_publishes(
        cx: &mut TestAppContext,
    ) {
        let default = CandlestickConfig::default().timeframe;
        let source = PublishingSource::publishing_with_adjustments(
            [default],
            [Adjustment::None, Adjustment::Forward],
        );
        let built = widget_over(cx, config(), vec![entry(SPOT_SYMBOL)], source);
        answered(cx, &built.widget);

        assert_eq!(
            cx.update(|cx| built.widget.read(cx).offered_adjustments()),
            vec![Adjustment::None, Adjustment::Forward],
            "a route that publishes both conventions offers both"
        );
    }

    /// A route that serves its history one way has nothing to switch between, so the control
    /// offers the one convention it publishes and is not drawn at all.
    #[gpui_kit::test]
    fn a_route_that_publishes_one_convention_should_offer_only_that_one(cx: &mut TestAppContext) {
        let source = PublishingSource::publishing([CandlestickConfig::default().timeframe]);
        let built = widget_over(cx, config(), vec![entry(SPOT_SYMBOL)], source);
        answered(cx, &built.widget);

        assert_eq!(
            cx.update(|cx| built.widget.read(cx).offered_adjustments()),
            vec![Adjustment::None],
            "one convention is what such a route publishes"
        );
    }

    /// A route this build cannot ask proves nothing about what it serves, so the control offers
    /// nothing at all: the chart keeps asking with the convention it stored, and a control that
    /// listed conventions nobody confirmed would be this widget guessing.
    #[gpui_kit::test]
    fn an_unanswerable_route_should_offer_no_convention(cx: &mut TestAppContext) {
        let default = CandlestickConfig::default().timeframe;
        let source =
            PublishingSource::unknown_accepting([default], [Adjustment::None, Adjustment::Forward]);
        let mut stored = config();
        stored.adjustment = Adjustment::Forward;
        let built = widget_over(cx, stored, vec![entry(SPOT_SYMBOL)], source.clone());
        answered(cx, &built.widget);
        settle_until(cx, "the stream to be opened", |_| {
            !source.subscribed_adjustments().is_empty()
        });

        assert_eq!(
            cx.update(|cx| built.widget.read(cx).offered_adjustments()),
            Vec::new(),
            "an answer nobody gave offers no convention"
        );
        assert_eq!(
            source.subscribed_adjustments(),
            vec![Adjustment::Forward],
            "and the stream still asks with the convention the chart stored"
        );
    }

    /// Choosing a convention is one configuration transition: it is written back, and the stream
    /// is opened again for it — the convention is a dimension of the stream's scope, so the
    /// candles priced the other way are not reused.
    #[gpui_kit::test]
    fn choosing_a_convention_should_restart_the_stream_with_it(cx: &mut TestAppContext) {
        let default = CandlestickConfig::default().timeframe;
        let source = PublishingSource::publishing_with_adjustments(
            [default],
            [Adjustment::None, Adjustment::Forward],
        );
        let built = widget_over(cx, config(), vec![entry(SPOT_SYMBOL)], source.clone());
        answered(cx, &built.widget);
        settle_until(cx, "the first stream to be opened", |_| {
            !source.subscribed_adjustments().is_empty()
        });
        assert_eq!(
            source.subscribed_adjustments(),
            vec![Adjustment::None],
            "the chart starts on the convention it stored"
        );

        // What the control's own change handler does with the choice it was handed.
        built.widget.update(cx, |widget, cx| {
            widget.select_adjustment(Adjustment::Forward, cx)
        });

        settle_until(cx, "the replacement stream to be opened", |_| {
            source.subscribed_adjustments().len() == 2
        });
        cx.update(|cx| {
            assert_eq!(
                built.widget.read(cx).config.adjustment,
                Adjustment::Forward,
                "the chart follows the convention the user picked"
            );
        });
        assert_eq!(
            built.reports(),
            1,
            "the switch has to be written back, or it lives only in memory"
        );
        assert_eq!(
            source.subscribed_adjustments(),
            vec![Adjustment::None, Adjustment::Forward],
            "and the replacement stream asks for it"
        );
    }

    /// The control's two labels are what the user reads, so each of them is defined and renders
    /// in both languages, and the two states do not read the same.
    #[gpui_kit::test]
    fn every_adjustment_label_should_render_in_every_language(cx: &mut TestAppContext) {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("embedded locales should load");

            for language in LanguageCode::all() {
                cx.set_global(language);
                let neutral = t!(cx, adjustment_key(Adjustment::None));
                let forward = t!(cx, adjustment_key(Adjustment::Forward));

                assert!(
                    !neutral.is_empty(),
                    "{} renders empty in {language}",
                    adjustment_key(Adjustment::None)
                );
                assert!(
                    !forward.is_empty(),
                    "{} renders empty in {language}",
                    adjustment_key(Adjustment::Forward)
                );
                assert_ne!(
                    neutral, forward,
                    "the two states do not read the same in {language}: {neutral:?}"
                );
            }
        });
    }

    /// A chart with nothing chosen has nothing of its own to show, so it says
    /// what it is waiting for instead of reading an instrument nobody
    /// picked.
    #[gpui_kit::test]
    fn a_widget_without_a_selection_should_ask_for_one(cx: &mut TestAppContext) {
        let built = widget_over(cx, CandlestickConfig::default(), Vec::new(), PendingSource);
        let widget = built.widget;

        cx.update(|cx| {
            assert!(!widget.read(cx).has_content(), "there is nothing to chart");
            let status = status(&widget, cx);
            assert_eq!(
                status,
                WidgetState::waiting(Notice::message(
                    "widget.candlestick.select_instrument_prompt"
                )),
                "it is not connecting"
            );
            assert!(status.failure().is_none(), "asking is not a failure");
            assert!(widget.read(cx).countdown_task.is_none());
            assert!(widget.read(cx).current_key().is_none());
        });
    }

    /// No market that could answer with candles is loaded and nothing is chosen: there is nothing
    /// to chart *and* nothing to choose from, so the pane waits for the market participant's metadata rather
    /// than asking for an instrument its picker could not offer (AGENTS.md §8).
    #[gpui_kit::test]
    fn a_chart_with_nothing_chosen_and_no_market_loaded_should_wait_for_one(
        cx: &mut TestAppContext,
    ) {
        let built = widget_over_market(
            cx,
            CandlestickConfig::default(),
            Vec::new(),
            PendingSource,
            false,
        );
        let widget = built.widget;

        cx.update(|cx| {
            assert!(!widget.read(cx).has_content(), "there is nothing to chart");
            assert!(
                widget.read(cx).awaiting_market,
                "and no market to chart from either"
            );
            assert_eq!(
                status(&widget, cx),
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues: listed_venues(),
                    supported: listed_venues(),
                }),
                "a chart nobody could choose from waits for the market participant's metadata"
            );
        });
    }

    /// The commit of a market the chart was waiting for is what takes it off that notice: a market
    /// with nothing chosen is the prompt's own business, and the pane says so without being
    /// reopened (a chart waiting with nothing chosen reads nothing, so only the wait itself can
    /// tell that it is over).
    #[gpui_kit::test]
    fn loading_a_market_should_turn_the_wait_back_into_the_prompt(cx: &mut TestAppContext) {
        let built = widget_over_market(
            cx,
            CandlestickConfig::default(),
            Vec::new(),
            PendingSource,
            false,
        );
        let widget = built.widget;

        cx.update(|cx| {
            assert!(
                widget.read(cx).awaiting_market,
                "the chart starts out waiting"
            );
            let venue =
                VenueId::from_name(test_support::VENUE).expect("the fixture venue is canonical");
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[venue]),
                    &[],
                    &[],
                )
                .expect("the fixture venue's routes should load");
            // A publication is what emits the directory's change signal; a load alone does not.
            test_support::publish_fixture_catalogs(cx, &[(route(), vec![entry(SPOT_SYMBOL)])]);
        });
        cx.run_until_parked();

        cx.update(|cx| {
            assert!(
                !widget.read(cx).awaiting_market,
                "the chart left the wait on the commit alone"
            );
            assert_eq!(
                status(&widget, cx),
                WidgetState::waiting(Notice::message(
                    "widget.candlestick.select_instrument_prompt"
                )),
                "a market the user can choose from asks for an instrument"
            );
        });
    }

    /// The interval picker in the header refuses input while the chart waits for its market, and
    /// is live on the frame that market is there: every interval it would offer is one the route
    /// behind the chart publishes, and a route whose market is not in memory cannot be asked.
    #[gpui_kit::test]
    fn the_interval_picker_should_refuse_input_while_the_market_is_not_loaded(
        cx: &mut TestAppContext,
    ) {
        let built = widget_over_unloaded(cx, vec![entry(SPOT_SYMBOL)], PendingSource);
        let (_, window) = cx.add_window_view(|_, _| HeaderHarness {
            widget: built.widget.clone(),
        });
        window.run_until_parked();

        press(window, "TEST_CANDLESTICK_TIMEFRAME");
        assert!(
            window.debug_bounds("TEST_DROPDOWN_PANEL").is_none(),
            "a picker for a market that is not loaded offers no intervals"
        );

        let venue =
            VenueId::from_name(test_support::VENUE).expect("the fixture venue is canonical");
        window.cx.update(|cx| {
            cx.global::<MarketParticipantsConnector>()
                .load_fixture(
                    &market_participants_connector::test_support::definitions(&[venue]),
                    &[],
                    &[],
                )
                .expect("the chart's own venue loads");
            test_support::publish_fixture_catalogs(cx, &[(route(), vec![entry(SPOT_SYMBOL)])]);
        });
        window.run_until_parked();
        assert!(
            !window.cx.update(|cx| built.widget.read(cx).awaiting_market),
            "the pane left the wait when its market arrived"
        );

        press(window, "TEST_CANDLESTICK_TIMEFRAME");
        assert!(
            window.debug_bounds("TEST_DROPDOWN_PANEL").is_some(),
            "the intervals of the route that is now loaded are what the control opens"
        );
    }

    /// The adjustment control is in the header when the route publishes more than one
    /// convention, and the interval picker is beside it: the two are one row because they are
    /// one question — which candles this chart asks for.
    #[gpui_kit::test]
    fn a_route_with_two_conventions_should_draw_the_adjustment_control(cx: &mut TestAppContext) {
        let default = CandlestickConfig::default().timeframe;
        let source = PublishingSource::publishing_with_adjustments(
            [default],
            [Adjustment::None, Adjustment::Forward],
        );
        let built = widget_over(cx, config(), vec![entry(SPOT_SYMBOL)], source);
        answered(cx, &built.widget);
        let (_, window) = cx.add_window_view(|_, _| HeaderHarness {
            widget: built.widget.clone(),
        });
        window.run_until_parked();

        assert!(
            window.debug_bounds("TEST_CANDLESTICK_ADJUSTMENT").is_some(),
            "a route that publishes two conventions gives the user something to switch between"
        );
        assert!(
            window.debug_bounds("TEST_CANDLESTICK_TIMEFRAME").is_some(),
            "and the interval picker is drawn beside it"
        );
    }

    /// A route that publishes one convention has nothing to switch between, so the control is
    /// not drawn at all: the venue's name decides nothing here, only what it publishes does.
    #[gpui_kit::test]
    fn a_route_with_one_convention_should_draw_no_adjustment_control(cx: &mut TestAppContext) {
        let source = PublishingSource::publishing([CandlestickConfig::default().timeframe]);
        let built = widget_over(cx, config(), vec![entry(SPOT_SYMBOL)], source);
        answered(cx, &built.widget);
        let (_, window) = cx.add_window_view(|_, _| HeaderHarness {
            widget: built.widget.clone(),
        });
        window.run_until_parked();

        assert!(
            window.debug_bounds("TEST_CANDLESTICK_ADJUSTMENT").is_none(),
            "one convention is not a choice, so there is no control"
        );
        assert!(
            window.debug_bounds("TEST_CANDLESTICK_TIMEFRAME").is_some(),
            "the header is drawn; only the adjustment control is missing"
        );
    }

    /// Presses what `selector` names, the way a window delivers a press: the button goes down,
    /// the frame that follows is drawn, and only then does it come up.
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

    /// The pane's header on its own, drawn in a window so that the controls in it are somewhere a
    /// click can land: the header belongs to the framework's pane, which a test does not build.
    struct HeaderHarness {
        widget: Entity<CandlestickWidget>,
    }

    impl Render for HeaderHarness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let header = self
                .widget
                .update(cx, |widget, cx| widget.header(window, cx));
            div().size_full().children(header.right)
        }
    }

    #[gpui_kit::test]
    fn choosing_an_instrument_should_start_the_chart_and_clear_the_prompt(cx: &mut TestAppContext) {
        let built = widget_over(
            cx,
            CandlestickConfig::default(),
            vec![entry(SPOT_SYMBOL)],
            PendingSource,
        );
        let widget = built.widget;

        cx.update(|cx| {
            let accepted = widget.update(cx, |widget, context| {
                widget.select_selection(selection(), context)
            });

            assert!(accepted);
            assert_eq!(
                widget.read(cx).config.selection.as_ref(),
                Some(&selection())
            );
            let status = status(&widget, cx);
            assert_eq!(status, WidgetState::Connecting);
            assert!(
                status.notice().is_none(),
                "the prompt belongs only to a chart that has nothing to chart"
            );
        });
    }

    /// The header is the widget's own; the pane decides where the two groups go.
    #[gpui_kit::test]
    fn the_header_should_offer_the_instrument_control_and_the_interval_picker(
        cx: &mut TestAppContext,
    ) {
        let built = widget(cx);
        let widget = built.widget;
        let (_, cx) = cx.add_window_view(|_, _| HeaderMarker);

        let header =
            cx.update(|window, cx| widget.update(cx, |widget, cx| widget.header(window, cx)));

        assert!(header.left.is_some(), "the instrument control belongs left");
        assert!(header.right.is_some(), "the interval picker belongs right");
    }

    /// The window root the header test renders into.
    struct HeaderMarker;

    impl Render for HeaderMarker {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// The window root the relayout tests render into: the widget's own body,
    /// with none of the framework around it.
    struct BodyHarness(Entity<CandlestickWidget>);

    impl Render for BodyHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.0.clone())
        }
    }

    /// One page of candles: more than a narrow chart can show, far fewer than a
    /// wide one.
    fn page_snapshot() -> CandlestickSeriesSnapshot {
        let mut series = CandlestickSeries::new(MAXIMUM_CANDLES, test_precision());
        series.merge_initial((1..=i64::from(HISTORY_PAGE_SIZE)).map(|index| candle(index, 100)));
        series.snapshot()
    }

    /// Widening the chart from the outside — splitting the pane, or dragging a
    /// neighbour's edge — makes room for candles the loaded page does not hold,
    /// and none of that is a gesture inside the chart.
    #[gpui_kit::test]
    fn a_widened_chart_should_ask_for_the_history_it_now_needs(cx: &mut TestAppContext) {
        let built = widget(cx);
        let widget = built.widget;
        cx.update(|cx| accept_snapshot(&widget, page_snapshot(), cx));

        let window = cx.open_window(size(px(320.0), px(220.0)), {
            let widget = widget.clone();
            move |_, _| BodyHarness(widget)
        });
        cx.run_until_parked();

        assert!(
            cx.read(|cx| widget.read(cx).chart_layout.get().is_some()),
            "there is no visible range to measure until the chart is laid out"
        );
        assert!(
            !cx.read(|cx| widget.read(cx).fetching_history),
            "a page that fills the chart leaves nothing to ask for"
        );

        // Cancelling the binding to hand-drive the stream also fails the load it was
        // holding, and whether that answer has already arrived is a race the harness
        // cannot win — it depends on when the cancelled runtime gets to it. It is not
        // this test's subject either: a chart that has been told its stream is gone
        // never asks for history again, which is what
        // `a_relayout_after_a_failure_should_not_ask_for_history` measures. So the
        // chart is put back where a live stream leaves it before the widening is
        // measured, whether or not that answer landed in the meantime.
        widget.update(cx, |widget, context| {
            widget.status.report(WidgetState::Ready, context);
        });

        cx.simulate_window_resize(*window, size(px(2400.0), px(220.0)));
        cx.run_until_parked();

        assert!(
            cx.read(|cx| widget.read(cx).fetching_history),
            "a chart wider than its page has to ask for the candles it now shows"
        );
    }

    /// Failure is terminal, so a relayout must not trade the failure being
    /// reported for a cheaper one: the stream that would answer is gone.
    #[gpui_kit::test]
    fn a_relayout_after_a_failure_should_not_ask_for_history(cx: &mut TestAppContext) {
        let built = widget(cx);
        let widget = built.widget;
        cx.update(|cx| {
            accept_snapshot(&widget, page_snapshot(), cx);
            widget.update(cx, |widget, context| {
                let recovery = widget.recovery.clone();
                widget.status.report(
                    WidgetState::failed(
                        Arc::new(CandlestickFailure::Market(disconnect())),
                        recovery,
                    ),
                    context,
                );
            });
        });

        let _window = cx.open_window(size(px(2400.0), px(220.0)), {
            let widget = widget.clone();
            move |_, _| BodyHarness(widget)
        });
        cx.run_until_parked();

        assert!(
            cx.read(|cx| widget.read(cx).chart_layout.get().is_some()),
            "a chart that failed while holding candles is still laid out"
        );
        assert!(
            !cx.read(|cx| widget.read(cx).fetching_history),
            "there is no stream left to ask for a page"
        );
    }

    /// The default interval is what a chart falls back to, so the picker has to
    /// offer it: a default it cannot pick is a chart that can never recover.
    #[test]
    fn the_default_interval_should_be_one_the_picker_offers() {
        assert!(
            TIMEFRAMES.contains(&CandlestickConfig::default().timeframe),
            "the interval the chart falls back to is not one it can ask for"
        );
    }

    /// A rejection is read as the item it names, and nothing else is.
    #[test]
    fn a_rejection_should_be_read_as_the_scope_it_names() {
        let interval = MarketError::from(MarketParticipantError::UnsupportedTimeFrame {
            route: route(),
            timeframe: format_smolstr!("1s"),
        });
        let capability = MarketError::UnsupportedCapability {
            route: route(),
            capability: CapabilityId::Candlesticks,
        };
        let other = MarketError::UnsupportedCapability {
            route: route(),
            capability: CapabilityId::Tickers,
        };

        assert!(is_unsupported_timeframe(&interval));
        assert!(!is_unsupported_capability(&interval));
        assert!(is_unsupported_capability(&capability));
        assert!(!is_unsupported_timeframe(&capability));
        assert!(!is_unsupported_capability(&other));
        assert!(!is_unsupported_timeframe(&disconnect()));
        assert!(!is_unsupported_capability(&disconnect()));
    }

    /// Only an answer the directory gave counts as one: a fetch that failed and an
    /// operation the runtime lost prove nothing about the listing.
    #[test]
    fn only_a_directory_answer_should_count_as_unsupported() {
        use market_participants_connector::catalog::error::CatalogError;

        let missing = CatalogError::ListingNotFound {
            listing: Box::new(listing()),
        };
        let revoked = CatalogError::RouteNotRegistered { route: route() };
        let failed = CatalogError::Source(Box::new(
            market_participants_connector::catalog::error::SourceError::Network {
                reason: format_smolstr!("the test catalog is unreachable"),
            },
        ));

        assert!(is_definitely_unsupported(&missing));
        assert!(is_definitely_unsupported(&revoked));
        assert!(!is_definitely_unsupported(&failed));
    }

    #[test]
    fn pointer_frame_should_keep_only_the_latest_input() {
        let mut frame = PointerFrameState::default();
        let first = PointerInput {
            position: gpui_kit::point(px(10.0), px(20.0)),
            dragging: false,
        };
        let latest = PointerInput {
            position: gpui_kit::point(px(30.0), px(40.0)),
            dragging: true,
        };

        assert!(frame.queue(first));
        assert!(!frame.queue(latest));
        assert_eq!(frame.take(), Some(latest));
        assert!(frame.queue(first));
        assert_eq!(frame.take(), Some(first));
    }

    #[test]
    fn scroll_frame_should_accumulate_steps_and_keep_the_latest_anchor() {
        let mut frame = ScrollFrameState::default();
        let first = ScrollInput {
            position: gpui_kit::point(px(10.0), px(20.0)),
            steps: 1,
        };
        let latest = ScrollInput {
            position: gpui_kit::point(px(30.0), px(40.0)),
            steps: 1,
        };

        assert!(frame.queue(first));
        assert!(!frame.queue(latest));
        assert_eq!(
            frame.take(),
            Some(ScrollInput {
                position: latest.position,
                steps: 2,
            })
        );
    }

    #[test]
    fn scroll_frame_should_clamp_one_frame_to_the_whole_zoom_range() {
        let mut frame = ScrollFrameState::default();

        frame.queue(ScrollInput {
            position: gpui_kit::point(px(0.0), px(0.0)),
            steps: MAX_ZOOM_STEPS_PER_FRAME * 4,
        });

        assert_eq!(
            frame.take().map(|input| input.steps),
            Some(MAX_ZOOM_STEPS_PER_FRAME)
        );
    }

    #[test]
    fn countdown_state_should_ignore_price_updates_for_the_same_candle() {
        let mut countdown = CountdownState::default();
        countdown.observe_candle(0, TimeFrame::Minute1, &venue("binance"), 0);

        let changed = countdown.observe_candle(0, TimeFrame::Minute1, &venue("binance"), 30_000);

        assert_eq!((changed, countdown.label.as_ref()), (false, "01:00"));
    }

    #[test]
    fn countdown_state_tick_should_advance_the_same_candle() {
        let mut countdown = CountdownState::default();
        countdown.observe_candle(0, TimeFrame::Minute1, &venue("binance"), 0);

        let changed = countdown.tick(0, TimeFrame::Minute1, &venue("binance"), 1_000);

        assert_eq!((changed, countdown.label.as_ref()), (true, "00:59"));
    }

    #[test]
    fn countdown_state_should_reset_for_a_new_candle() {
        let mut countdown = CountdownState::default();
        countdown.observe_candle(0, TimeFrame::Minute1, &venue("binance"), 59_000);

        let changed =
            countdown.observe_candle(60_000, TimeFrame::Minute1, &venue("binance"), 60_000);

        assert_eq!((changed, countdown.label.as_ref()), (true, "01:00"));
    }

    #[test]
    fn clearing_the_countdown_should_restore_the_initial_label() {
        let mut countdown = CountdownState::default();
        countdown.observe_candle(0, TimeFrame::Minute1, &venue("binance"), 0);

        countdown.clear();

        assert_eq!(
            (countdown.candle_timestamp, countdown.label.as_ref()),
            (None, "00:00")
        );
    }

    #[test]
    fn the_default_config_should_follow_nothing() {
        let config = CandlestickConfig::default();

        assert_eq!(
            config.selection, None,
            "a new chart waits for the user to choose instead of charting a default"
        );
        assert_eq!(config.timeframe, TimeFrame::Minute1);
    }

    #[test]
    fn a_chart_that_follows_nothing_should_describe_no_venue() {
        // Nothing is assumed for a chart nobody has pointed anywhere: no venue for
        // the countdown, and the picker opens wherever its own filters admit.
        assert_eq!(venue_of(&CandlestickConfig::default()), None);
        assert_eq!(venue_of(&config()), Some(&venue("binance")));
    }

    #[test]
    fn a_config_should_round_trip_through_its_persisted_form() {
        let config = config();
        let envelope = VersionedWidgetConfig::encode(&config).expect("config should encode");
        let decoded = envelope.decode::<CandlestickConfig>();

        assert_eq!(decoded.selection, config.selection);
        assert_eq!(decoded.timeframe, config.timeframe);
    }

    #[test]
    fn an_unselected_config_should_round_trip_through_its_persisted_form() {
        let config = CandlestickConfig::default();
        let envelope = VersionedWidgetConfig::encode(&config).expect("config should encode");
        let decoded = envelope.decode::<CandlestickConfig>();

        assert_eq!(decoded.selection, None);
        assert_eq!(decoded.timeframe, config.timeframe);
    }

    fn venue(name: &str) -> VenueId {
        VenueId::from_name(name).expect("the fixture venue should be canonical")
    }
}

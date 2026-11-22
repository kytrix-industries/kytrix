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
//! # The body is the ready state
//!
//! The framework draws the placeholder, the banner and the spinner, and it embeds
//! this body only while [`CandlestickWidget::has_content`] answers `true`. There is
//! therefore no state branch in the body: an empty snapshot draws the axes and a
//! "no data" line over the plot, because the venue publishing no candles is
//! content the pane holds rather than a state the framework should speak for.
//!
//! # What the frame draws
//!
//! One canvas for the plot (see [`crate::chart::paint::ChartCanvas`]) and, beside it,
//! the indicator settings panel the user may open. The canvas node is built once and
//! its content is replaced by `sync` only when what it would draw moved — the box, the
//! viewport, a candle, the countdown label, the indicators, the panel width, the theme,
//! the language, the data time zone, the text raster scale or a dead atlas lease. A
//! frame that changes none of those writes nothing and asks for nothing (I3).
//!
//! # The layout is resolved once per frame, not through the frame loop
//!
//! The old painter ran the chart layout through three fixed-point iterations *inside
//! the paint closure* because the y-axis width depends on the widest axis label, which
//! depends on the plot width, which depends on the axis width. That loop was pure
//! measurement — it never needed a second element tree — so it stays, but it runs once
//! per `sync` with the engine's text measurement: the labels of the candidate axes are
//! shaped through the text cache, the widest is taken, and [`ChartLayout`] is built from
//! it. There is no next-frame self-registration and no second layout pass; the only
//! feedback the widget takes from the engine is the canvas node's own measured box, which
//! the frame that laid it out has already written when `sync` runs.
//!
//! # Interactions are applied in the dispatch hook
//!
//! Pointer moves, wheel steps and drags arrive through [`WidgetView::dispatch`]. The old
//! view queued them in `Rc<RefCell<_>>` buffers and drained them in a next-frame
//! callback, because it could not mutate itself from inside a paint closure. The engine's
//! dispatch hook *has* the widget, so each event is applied where it arrives: the viewport
//! moves, a frame is requested, and the next `sync` paints. A wheel burst therefore costs
//! one frame per event rather than one coalesced frame; the coalescing buffer and its
//! `MAX_ZOOM_STEPS_PER_FRAME` clamp are gone (the clamp only existed to bound the buffer).
//! The zoom-anchor maths — find the candle under the pointer before the width changes and
//! anchor the right edge after it — is unchanged.
//!
//! # Three pieces of local state, each kept out of the status
//!
//! - **the viewport**, which is a candle width plus an optional right-hand anchor.
//!   Following the live edge is expressed as *no anchor*, so a chart that is following
//!   keeps following when a new candle arrives;
//! - **the countdown**, which ticks once a second and repaints the plot only when the
//!   label it draws actually changes. It is a one-shot engine timer (`App::after_scoped`)
//!   re-armed by `sync`, not a detached task;
//! - **pagination**, which is a pair of in-flight flags plus a last-requested marker per
//!   direction, because a page arriving changes the candles rather than what the widget is
//!   reporting. A request is finished when the stream says so with a `PageSettled` message,
//!   not when a status arrives: the candles travel on the latest channel and the settle on
//!   the reliable one, so neither can stand in for the other.
//!
//! # Header controls and static copy
//!
//! The instrument control, the interval picker and the adjustment picker are kit
//! components whose text and item lists are chosen when they are built. Their *selection*
//! is dynamic (`set_chosen`), but a language switch or a route whose published intervals
//! changed rebuilds them in the group the pane gave the widget (§15.1). Rebuilding first
//! checks that the node the control was built under is still alive: a pane that rebuilt
//! its header row took the control with it, and touching a stale node would fail the whole
//! synchronization.
//!
//! # Scope settlement and directory generations
//!
//! A load starts with the market precondition (§8.3): if no venue that answers with
//! candles is in memory the pane reports the neutral notice and waits. The directory's
//! published generation is compared in `sync`; a change either unblocks a pane that was
//! waiting, sends a pane whose market went away back to waiting, or — when the pane was
//! reading — reopens the stream, because the catalogue generation the live stream resolved
//! its rules against was replaced. The comparison is the only wake-up path: no connector
//! subscription, no polling.

use std::{rc::Rc, sync::Arc, time::Duration};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    kit::{
        button::Button,
        empty_state::EmptyState,
        select::{Select, SelectItem, SelectOptions},
        separator::Separator,
        size::{UnifiedSizable, UnifiedSize},
    },
    style_builder::StyleBuilder,
};
use i18n::{t_smol, translate::Translate};
use instrument_selector_ui::{
    host::SelectorHost,
    instrument_button::{InstrumentButton, InstrumentButtonHandle},
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
};
use market_participants_model::{
    identifier::VenueId, listing::route::VenueRoute, market::selection::InstrumentSelection,
    routing::CapabilityId,
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use ui_core::{
    app::{App, TimerId},
    error::EngineError,
    event::{Event, InputEvent, PointerButton},
    math::Point,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::style::StylePatch,
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
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

use super::{
    chart::{
        geometry::{CandleGeometry, ChartViewport, visible_range},
        paint::{ChartCanvas, ChartFrame, latest_price_indicator_is_visible},
    },
    indicator::{CandlestickIndicatorConfig, normalize_indicator},
    model::{CandlestickSeriesSnapshot, NewestCandle},
    panel::{IndicatorPanel, PANEL_WIDTH, PanelRequest},
    service::{
        CandlestickProtocol, CandlestickRequest, CandlestickService, CandlestickServiceError,
        CandlestickSource, CandlestickStatus, CandlestickStreamKey, CandlestickStreamMessage,
        PageDirection,
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
    #[serde(deserialize_with = "normalize_indicator")]
    indicator: CandlestickIndicatorConfig,
}

impl Default for CandlestickConfig {
    fn default() -> Self {
        Self {
            selection: None,
            timeframe: TimeFrame::Minute1,
            adjustment: Adjustment::None,
            indicator: CandlestickIndicatorConfig::default(),
        }
    }
}

impl WidgetConfig for CandlestickConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// Why the chart reports a failure: either the market refused the stream or the
/// RPC that asked for it was answered with an error.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CandlestickFailure {
    Market(MarketError),
    Rpc(RpcCallError<CandlestickServiceError>),
}

impl TranslatedError for CandlestickFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Market(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
}

/// What a header control asks the pane to do.
///
/// A control's handler runs with an application context and no widget context, so it
/// cannot change the pane: it writes the request and asks for a frame, and the next
/// `sync` applies it.
#[derive(Clone, Debug, PartialEq)]
enum HeaderRequest {
    /// The user picked an interval.
    Timeframe(TimeFrame),
    /// The user picked a price-adjustment convention.
    Adjustment(Adjustment),
    /// The user toggled the indicator settings panel.
    ToggleSettings,
}

/// Whether a market rejection says the interval itself is not published.
fn is_unsupported_timeframe(error: &MarketError) -> bool {
    matches!(
        error,
        MarketError::Network(network)
            if matches!(network.error(), MarketParticipantError::UnsupportedTimeFrame { .. })
    )
}

/// Whether a market rejection says the adjustment itself is not published.
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
const fn adjustment_key(adjustment: Adjustment) -> &'static str {
    match adjustment {
        Adjustment::None => "widget.candlestick.adjust_none",
        Adjustment::Forward => "widget.candlestick.adjust_forward",
    }
}

/// The venues the waiting notice lists: every venue whose loader can answer with candles.
fn listed_venues() -> Vec<VenueId> {
    market_participants_connector::venue::definition::venues_supporting(CapabilityId::Candlesticks)
}

/// Whether the directory proved that the listing is not chartable.
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
/// The label only moves once a second, so the timer that drives it compares against
/// the previous label and reports whether anything a frame would draw actually changed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CountdownState {
    candle_timestamp: Option<i64>,
    label: SmolStr,
}

impl Default for CountdownState {
    fn default() -> Self {
        Self {
            candle_timestamp: None,
            label: SmolStr::new_static("00:00"),
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
        let label = SmolStr::new(remaining_time_label(
            candle_timestamp,
            timeframe,
            venue,
            now_timestamp,
        ));
        if self.label == label {
            return false;
        }
        self.label = label;
        true
    }
}

/// One command this chart sent and has not seen the answer to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    /// The load that starts the stream.
    Load,
    /// The load that replaces a stream whose catalogue generation moved.
    Retry,
    /// The page asked for towards older candles.
    FetchHistory,
    /// The page asked for towards newer candles.
    FetchNewer,
}

/// What the pane was built to keep of its header controls.
///
/// A kit control's static text and item lists are chosen when it is built, so a
/// language switch or a route whose published intervals changed rebuilds it. The
/// state recorded here is everything the build depended on.
#[derive(Clone, Debug, PartialEq)]
struct HeaderState {
    language: i18n::language::codes::LanguageCode,
    selection: Option<InstrumentSelection>,
    generation: u64,
}

/// The controls this widget built into the pane's right header group.
struct HeaderControls {
    /// The row the widget built inside the group the pane made.
    root: NodeId,
    /// The group node the row was built under, for a rebuild and its survival guard.
    parent: NodeId,
    settings: Button,
    adjustment: Option<Select>,
    timeframe: Select,
    /// Everything the controls' static copy and items were built from.
    built: HeaderState,
}

/// The instrument control and the group it was built into.
struct HeaderInstrument {
    parent: NodeId,
    control: InstrumentButtonHandle,
    built: HeaderState,
}

/// The background service a pane reads its candles through, and the two questions only the
/// source behind it can answer.
struct Service {
    /// Absent on a host with no market directory: such a pane reports the neutral notice
    /// and never subscribes.
    binding: Option<WidgetBinding<CandlestickProtocol>>,
    published_timeframes: Option<Rc<dyn Fn(&VenueRoute) -> Result<Vec<TimeFrame>, MarketError>>>,
    published_adjustments: Option<Rc<dyn Fn(&VenueRoute) -> Result<Vec<Adjustment>, MarketError>>>,
}

impl Service {
    /// No directory root: nothing to subscribe to and no route to ask.
    const fn none() -> Self {
        Self {
            binding: None,
            published_timeframes: None,
            published_adjustments: None,
        }
    }
}

/// One pane: the candles a stream publishes, the viewport they are drawn through, and
/// the settings panel that edits the indicators.
pub struct CandlestickWidget {
    config: CandlestickConfig,
    binding: Option<WidgetBinding<CandlestickProtocol>>,
    status: WidgetStatus,
    /// The recovery the framework offers for every failure this widget reports.
    recovery: WidgetAction,
    config_change: WidgetConfigChange,
    /// The command in flight, keyed by the identity the binding returned.
    pending: Pending<PendingCommand>,
    /// The iteration of the scope, bumped whenever the pane starts over. An answer
    /// that was already in flight across the restart belongs to the generation it
    /// replaced.
    generation: u64,
    /// The published directory generation this pane last acted on.
    seen_generation: u64,
    snapshot: Option<CandlestickSeriesSnapshot>,
    countdown: CountdownState,
    /// The one-shot timer that wakes the countdown, if one is armed.
    countdown_timer: Option<TimerId>,
    /// What the countdown timer writes: the wall-clock instant it fired at.
    countdown_wake: Signal<Option<i64>>,
    viewport: ChartViewport,
    /// The canvas node, its caches and the layout the last frame resolved.
    chart: ChartCanvas,
    /// The body root, once the body exists.
    body_root: Option<NodeId>,
    /// The container the panel is built into, once the body exists.
    panel_parent: Option<NodeId>,
    /// The separator between the plot and the panel, while the panel is open.
    panel_separator: Option<NodeId>,
    /// Whether the settings panel is open. Not persisted: a pane always starts closed.
    panel_open: bool,
    /// The panel, and the only strong reference to it.
    panel: Option<IndicatorPanel>,
    /// The overlay that says the instrument has published no candles.
    empty_container: Option<NodeId>,
    empty_visible: Signal<bool>,
    empty_text: Signal<SmolStr>,
    /// The language the empty state's text was last built for.
    empty_language: Option<i18n::language::codes::LanguageCode>,
    /// Where a header control hands over what the user did.
    requests: Signal<Option<HeaderRequest>>,
    /// Where the panel hands over what the user committed.
    panel_requests: Signal<Option<PanelRequest>>,
    /// Where the instrument selector hands over the listing the user confirmed.
    selection_requests: Signal<Option<InstrumentSelection>>,
    instrument: Option<HeaderInstrument>,
    controls: Option<HeaderControls>,
    /// The pointer position the frame's crosshair was drawn at, while the pointer is
    /// inside the plot.
    dragging: bool,
    drag_last_x: Option<f32>,
    drag_remainder: f32,
    fetching_history: bool,
    fetching_newer: bool,
    /// The oldest candle the history page in flight was asked from.
    ///
    /// Purely a "do not ask twice for the same page" guard: the in-flight flag says a
    /// request is out, and this says which page it asked for, so a prefetch that runs
    /// again before the page lands cannot repeat it.
    history_requested_before: Option<i64>,
    /// The newest candle the newer page in flight was asked from, guarded the same way.
    newer_requested_after: Option<i64>,
    /// Whether the chart is waiting for its market rather than reading it.
    awaiting_market: bool,
    /// Whether the venue this chart reads was loaded or refreshed again.
    market_reloaded: bool,
    /// The intervals the chart's route publishes, asked before anything is subscribed.
    published_timeframes: Option<Rc<dyn Fn(&VenueRoute) -> Result<Vec<TimeFrame>, MarketError>>>,
    /// The price-adjustment conventions the chart's route publishes, asked the same way.
    published_adjustments: Option<Rc<dyn Fn(&VenueRoute) -> Result<Vec<Adjustment>, MarketError>>>,
    /// The newest-candle policy the last history request carried (view-test read point).
    #[cfg(test)]
    prefetched_history_newest: Option<NewestCandle>,
    /// How many history pages this view has asked for (view-test read point).
    #[cfg(test)]
    history_requests: usize,
    /// The answers a view test injects in place of a live stream.
    #[cfg(test)]
    test_inbox: Vec<Inbound<CandlestickProtocol>>,
}

impl WidgetView for CandlestickWidget {
    type Config = CandlestickConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.candlestick");
    const DISPLAY_NAME: &'static str = "widget.names.candlestick";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.candlestick");
    const ICON: Option<&'static str> = Some("widgets/candlestick.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: WidgetStatus,
        _reload: ReloadHandle,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError> {
        Ok(Self::assemble(config, status, config_change, recovery, cx))
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        // The market this chart follows has to be loaded before anything else happens
        // here, and the check is first on purpose: a route the user never loaded, or one
        // a shrinking commit has since taken away, leaves the pane waiting for it instead
        // of reaching the branches below, which settle the selection the user stored.
        if !self.market_ready(cx) {
            self.wait_for_market(cx);
            return;
        }
        // Read past the gate: a commit on the chart's own venue that took its market away
        // leaves this set while the stream that resolved against the retired generation is
        // still alive, and the load that brings the market back has to reopen that stream.
        let market_reloaded = std::mem::take(&mut self.market_reloaded);
        let retry = self.status.is_failed(cx.app());
        self.resolve_timeframe(cx);
        self.resolve_adjustment(cx);
        self.start_current(retry, market_reloaded, cx);
    }

    /// The options this widget persists.
    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    /// The chart is content as soon as a snapshot has been accepted — including an empty
    /// one. An empty chart is what "this instrument has no candles" looks like: the axes
    /// are still there, and there is nothing to report about it.
    fn has_content(&self) -> bool {
        self.snapshot.is_some()
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if self.body_root.is_some() {
            // The framework builds a body once; a second call would double every node.
            return Ok(());
        }
        let root = NodeBuilder::boxed()
            .id(BODY_SELECTOR)
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .flex_row()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        // The plot column: the canvas, with the "no data" line over it.
        let plot = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .flex_col()
                    .min_width_0()
                    .min_height_0()
                    .relative()
                    .build(),
            )
            .build(cx.tree_mut(), root)?;
        self.chart.build(cx, plot)?;
        let empty_container = NodeBuilder::boxed()
            .id(EMPTY_SELECTOR)
            .style(
                StyleBuilder::new()
                    .absolute()
                    .top(0.0)
                    .left(0.0)
                    .right(0.0)
                    .bottom(0.0)
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .build(),
            )
            .bind_style({
                let visible = self.empty_visible;
                move |read| StylePatch {
                    visible: Some(*read.read(&visible)),
                    ..StylePatch::default()
                }
            })
            .build(cx.tree_mut(), plot)?;
        let theme = cx.theme().clone();
        EmptyState::new(theme, "CandlestickEmpty")
            .bind_text(self.empty_text)
            .build(cx.tree_mut(), empty_container)?;
        self.empty_container = Some(empty_container);
        self.body_root = Some(root);
        // The canvas node has no box until the frame that lays this body out, so the
        // chart cannot be painted on the frame that builds it: ask for the one that can.
        cx.request_frame()?;
        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.drain_inbox(cx);

        // The directory's published generation is the only wake-up path for a market that
        // appears, disappears or is loaded again (§8.3). A change either unblocks a pane
        // that was waiting, sends a pane whose market went away back to waiting, or
        // reopens the stream of a pane that was reading — the catalogue generation the
        // live stream resolved its rules against is gone.
        let generation = *cx.read(&cx.services().catalog_generation);
        if generation != self.seen_generation {
            self.seen_generation = generation;
            if self.status.is_failed(cx.app()) {
                // A failure is terminal: the directory moving does not revive the pane.
            } else if self.awaiting_market {
                if self.market_ready(cx) {
                    self.load(cx);
                }
            } else if !self.market_ready(cx) {
                self.wait_for_market(cx);
            } else if self.config.selection.is_some() {
                self.market_reloaded = true;
                self.load(cx);
            }
        }

        // A header control owns no `&mut self`; it asks for the transition it was used for.
        let request = cx.read(&self.requests).clone();
        if request.is_some() {
            cx.write(&self.requests, None);
        }
        if let Some(request) = request {
            self.apply_header_request(request, cx);
        }
        let selection = cx.read(&self.selection_requests).clone();
        if selection.is_some() {
            cx.write(&self.selection_requests, None);
        }
        if let Some(selection) = selection {
            self.select_selection(selection, cx);
        }

        self.sync_panel(cx)?;
        // The panel writes its committed edits during its own `sync`, so they are drained
        // after it: a colour or period the user committed is applied in the frame that drew
        // it rather than one frame later.
        let panel_request = cx.read(&self.panel_requests).clone();
        if panel_request.is_some() {
            cx.write(&self.panel_requests, None);
        }
        if let Some(panel_request) = panel_request {
            self.apply_panel_request(panel_request, cx);
        }
        self.sync_header(cx)?;
        self.sync_empty_state(cx)?;
        self.sync_countdown(cx)?;
        self.sync_chart(cx)?;
        // A body that first became ready in this hook is one frame away: the framework
        // sampled `has_content` before running it.
        if self.has_content() && self.body_root.is_none() {
            cx.request_frame()?;
        }
        self.prefetch_if_needed(cx);
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
                // The pane is building its row; the control this widget built into the
                // previous one went with it. Only the component is the widget's to take
                // down.
                self.release_instrument(cx, false);
                self.build_instrument(cx, parent)
            }
            HeaderGroup::Right => {
                self.release_controls(cx, false);
                self.build_controls(cx, parent)
            }
        }
    }

    fn dispatch(&mut self, event: &Event, cx: &mut WidgetCx<'_>) -> bool {
        // The settings panel's inputs and pickers own the events over their own nodes; the
        // panel is offered the event first, and the chart only acts on positions inside its
        // own canvas box (the two share one surface).
        if let Some(panel) = self.panel.as_mut()
            && panel.dispatch(cx, event)
        {
            return true;
        }
        let Event::Input(input) = event else {
            return false;
        };
        // A cancelled or leaving pointer ends the drag wherever it is: the position it
        // carries may already be outside the canvas.
        if matches!(
            input,
            InputEvent::PointerCancel { .. } | InputEvent::PointerLeave { .. }
        ) {
            if self.end_drag(None) {
                let _ = cx.request_frame();
            }
            return true;
        }
        let position = pointer_position(input);
        if !position.is_some_and(|position| self.chart_contains(position)) {
            return false;
        }
        match input {
            InputEvent::PointerDown {
                position, button, ..
            } => {
                if *button != PointerButton::Primary {
                    return false;
                }
                if self.begin_drag(position.x) {
                    let _ = cx.request_frame();
                }
                true
            }
            InputEvent::PointerUp { position, .. } => {
                if self.end_drag(Some(*position)) {
                    let _ = cx.request_frame();
                }
                true
            }
            InputEvent::PointerMove { position, .. } => {
                if self.apply_pointer(*position) {
                    let _ = cx.request_frame();
                }
                true
            }
            InputEvent::Scroll {
                position, delta, ..
            } => {
                if self.zoom_at(*position, delta.y) {
                    let _ = cx.request_frame();
                }
                // The chart zooms rather than letting the page scroll.
                true
            }
            _ => false,
        }
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        // Dropping the bridge aborts its forwarding tasks and cancels the service.
        if let Some(mut binding) = self.binding.take() {
            binding.cancel();
        }
        self.release_instrument(cx, false);
        self.release_controls(cx, false);
        self.release_panel(cx);
        // The canvas node lives in the body, which the framework removes after this hook;
        // the node, the geometry it registered and its caches are this canvas' own.
        self.chart.release(cx);
        self.cancel_countdown(cx);
        self.body_root = None;
        self.panel_parent = None;
        self.empty_container = None;
        self.snapshot = None;
        self.pending.clear();
        self.countdown.clear();
        self.viewport.cursor = None;
    }
}

impl CandlestickWidget {
    /// Builds a widget over the collaborators the context hands out.
    fn assemble(
        config: CandlestickConfig,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let connector = cx.services().market_participants.clone();
        // The service takes the source, so the chart keeps these two questions from it: the
        // intervals and the adjustment conventions a route publishes are asked before every
        // subscription, and a clone of the source answers both.
        let service = connector.as_ref().map_or_else(Service::none, |connector| {
            Self::follow_source(connector.market(), cx)
        });
        Self::assemble_with(config, service, status, config_change, recovery, cx)
    }

    /// Starts the service for one source and keeps the questions only it can answer.
    fn follow_source<S: CandlestickSource>(source: S, cx: &mut WidgetCx<'_>) -> Service {
        let published_timeframes: Rc<dyn Fn(&VenueRoute) -> Result<Vec<TimeFrame>, MarketError>> = {
            let source = source.clone();
            Rc::new(move |route: &VenueRoute| {
                CandlestickSource::supported_timeframes(&source, route)
            })
        };
        let published_adjustments: Rc<dyn Fn(&VenueRoute) -> Result<Vec<Adjustment>, MarketError>> = {
            let source = source.clone();
            Rc::new(move |route: &VenueRoute| {
                CandlestickSource::supported_adjustments(&source, route)
            })
        };
        let service = CandlestickService::new(source, HISTORY_PAGE_SIZE, MAXIMUM_CANDLES);
        let binding = WidgetBinding::spawn(cx, service);
        Service {
            binding: Some(binding),
            published_timeframes: Some(published_timeframes),
            published_adjustments: Some(published_adjustments),
        }
    }

    /// Builds a widget over an already-decided set of collaborators.
    ///
    /// The framework's own construction goes through [`Self::assemble`]; a test reaches this
    /// one directly so it can hand the pane a source that records what it was asked for
    /// instead of a live connection.
    fn assemble_with(
        config: CandlestickConfig,
        service: Service,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let Service {
            binding,
            published_timeframes,
            published_adjustments,
        } = service;
        let requests = cx.signal(None::<HeaderRequest>);
        let panel_requests = cx.signal(None::<PanelRequest>);
        let selection_requests = cx.signal(None::<InstrumentSelection>);
        let empty_visible = cx.signal(false);
        let empty_text = cx.signal(SmolStr::default());
        let countdown_wake = cx.signal(None::<i64>);
        let generation = cx.services().catalog_generation;
        let seen_generation = *cx.read(&generation);
        Self {
            config,
            binding,
            status,
            recovery,
            config_change,
            pending: Pending::new(),
            generation: 0,
            seen_generation,
            snapshot: None,
            countdown: CountdownState::default(),
            countdown_timer: None,
            countdown_wake,
            viewport: ChartViewport::default(),
            chart: ChartCanvas::new(),
            body_root: None,
            panel_parent: None,
            panel_separator: None,
            panel_open: false,
            panel: None,
            empty_container: None,
            empty_visible,
            empty_text,
            empty_language: None,
            requests,
            panel_requests,
            selection_requests,
            instrument: None,
            controls: None,
            dragging: false,
            drag_last_x: None,
            drag_remainder: 0.0,
            fetching_history: false,
            fetching_newer: false,
            history_requested_before: None,
            newer_requested_after: None,
            awaiting_market: false,
            market_reloaded: false,
            published_timeframes,
            published_adjustments,
            #[cfg(test)]
            prefetched_history_newest: None,
            #[cfg(test)]
            history_requests: 0,
            #[cfg(test)]
            test_inbox: Vec::new(),
        }
    }

    /// The directory root the pane reads its market through, if the host has one.
    fn connector<'a>(&self, cx: &'a WidgetCx<'_>) -> Option<&'a MarketParticipantsConnector> {
        cx.services().market_participants.as_ref()
    }

    /// Builds a pane over a test's own candle source.
    ///
    /// A headless host has no live market connection for the pane to read, so an interface
    /// test constructs the widget with the fixture it wants and drives the hooks itself.
    #[cfg(test)]
    fn with_source<S: CandlestickSource>(
        config: CandlestickConfig,
        source: S,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let service = Self::follow_source(source, cx);
        Self::assemble_with(config, service, status, config_change, recovery, cx)
    }

    /// Whether what this chart needs is in memory: at least one market that answers with
    /// candles, and the route of the stored selection among them.
    fn market_ready(&self, cx: &WidgetCx<'_>) -> bool {
        let Some(connector) = self.connector(cx) else {
            return false;
        };
        let venues = connector.venues();
        if !listed_venues()
            .iter()
            .any(|venue| venues.loaded(venue).is_some())
        {
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
        venues.scope(route).is_some()
    }

    /// Reports the neutral notice for the market this chart follows, and drops the chart
    /// it was drawing.
    fn wait_for_market(&mut self, cx: &mut WidgetCx<'_>) {
        self.awaiting_market = true;
        self.clear_loaded_state(cx);
        // The settings panel edits a configuration that only means something while a
        // chart is reading a market, so it goes with the chart rather than floating over
        // the notice.
        self.close_indicator_panel(cx);
        self.report_notice(cx);
        let _ = cx.request_frame();
    }

    /// Reports the notice the pane is standing in with: the market it waits for, or the
    /// instrument it has nothing chosen for.
    fn report_notice(&self, cx: &mut WidgetCx<'_>) {
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
        self.status
            .report(cx.app_mut(), WidgetState::waiting(notice));
    }

    /// The stream scope this chart would accept messages from.
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
    fn clear_loaded_state(&mut self, cx: &mut WidgetCx<'_>) {
        self.snapshot = None;
        self.countdown.clear();
        self.cancel_countdown(cx);
        self.chart.clear();
        self.viewport.right_timestamp = None;
        self.viewport.cursor = None;
        self.dragging = false;
        self.drag_last_x = None;
        self.drag_remainder = 0.0;
        self.fetching_history = false;
        self.fetching_newer = false;
        self.history_requested_before = None;
        self.newer_requested_after = None;
        self.pending.clear();
    }

    /// Returns the chart to "nothing chosen", reporting it.
    fn clear_selection(&mut self, reason: &str, cx: &mut WidgetCx<'_>) {
        log::warn!(
            "the chart went back to having nothing chosen: {reason}; it had been following {:?}",
            self.config
                .selection
                .as_ref()
                .map(|selection| &selection.listing)
        );
        self.config.selection = None;
        self.config_change.publish(cx.app_mut());
        self.start_current(false, false, cx);
    }

    /// Settles a scope the route's own answer proved unsupported.
    fn recover_unsupported_scope(&mut self, error: &MarketError, cx: &mut WidgetCx<'_>) -> bool {
        let default = CandlestickConfig::default();
        if is_unsupported_timeframe(error) && self.config.timeframe != default.timeframe {
            log::warn!(
                "the interval {:?} is not published for the listing the chart follows; \
                 the chart fell back to {:?}",
                self.config.timeframe,
                default.timeframe
            );
            self.config.timeframe = default.timeframe;
            self.config_change.publish(cx.app_mut());
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
            self.config_change.publish(cx.app_mut());
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
    fn resolve_timeframe(&mut self, cx: &mut WidgetCx<'_>) {
        let default = CandlestickConfig::default();
        if self.config.timeframe == default.timeframe {
            return;
        }
        let Some(selection) = self.config.selection.as_ref() else {
            return;
        };
        let Some(published_timeframes) = self.published_timeframes.as_ref() else {
            return;
        };
        match published_timeframes(&selection.listing.route) {
            Ok(published) if !published.contains(&self.config.timeframe) => {
                log::warn!(
                    "the interval {:?} is not published for {:?}; the chart follows {:?} \
                     instead of subscribing to an interval the route does not serve",
                    self.config.timeframe,
                    selection.listing,
                    default.timeframe
                );
                self.config.timeframe = default.timeframe;
                self.config_change.publish(cx.app_mut());
            }
            Ok(_) => {}
            Err(error) => log::warn!(
                "asking which intervals {} publishes failed; the chart asks for {:?} anyway: {error}",
                selection.listing.route,
                self.config.timeframe
            ),
        }
    }

    /// Resets an adjustment convention the chart's route does not publish, before
    /// anything subscribes.
    fn resolve_adjustment(&mut self, cx: &mut WidgetCx<'_>) {
        let default = CandlestickConfig::default();
        if self.config.adjustment == default.adjustment {
            return;
        }
        let Some(selection) = self.config.selection.as_ref() else {
            return;
        };
        let Some(published_adjustments) = self.published_adjustments.as_ref() else {
            return;
        };
        match published_adjustments(&selection.listing.route) {
            Ok(published) if !published.contains(&self.config.adjustment) => {
                log::warn!(
                    "the adjustment {:?} is not published for {:?}; the chart asks for {:?} \
                     instead of subscribing to a convention the route does not serve",
                    self.config.adjustment,
                    selection.listing,
                    default.adjustment
                );
                self.config.adjustment = default.adjustment;
                self.config_change.publish(cx.app_mut());
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
    fn start_current(&mut self, retry: bool, market_reloaded: bool, cx: &mut WidgetCx<'_>) {
        // The pane reached here only past its own gate, so the market it was waiting for
        // is in memory again and messages belong to it once more.
        self.awaiting_market = false;
        self.generation = self.generation.wrapping_add(1);
        let Some(selection) = self.config.selection.clone() else {
            // There is nothing to chart yet, so there is no market to read.
            self.clear_loaded_state(cx);
            self.report_notice(cx);
            let _ = cx.request_frame();
            return;
        };
        if retry {
            // A retry keeps the candles the user is still reading — blanking the chart to
            // reconnect would flash a screen of data away — but the pages that were in
            // flight belonged to the generation being replaced and will never be answered.
            self.fetching_history = false;
            self.fetching_newer = false;
            self.history_requested_before = None;
            self.newer_requested_after = None;
            self.pending.clear();
        } else {
            // An explicit load starts over, because the widget cannot vouch for what it was
            // showing.
            self.clear_loaded_state(cx);
        }
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        // What the directory still publishes is asked before the chart reads, but only as
        // advice: a listing the route no longer lists goes back to "nothing chosen"
        // instead of being charted and failing. A lookup that failed proves nothing and is
        // only logged: the request below resolves the listing inside one catalogue
        // generation and its rejection comes back as the failure it is.
        if let Some(connector) = self.connector(cx) {
            let catalog = connector.catalog_arc();
            match catalog.contains(&selection.listing) {
                Ok(true) => {
                    // A listing that is settled or delisted is one no market call reads,
                    // so it is reset here the way a missing one is.
                    let unreadable = catalog
                        .cached_listing(&selection.listing)
                        .is_some_and(|entry| entry.snapshot.status.is_terminal());
                    if unreadable {
                        self.clear_selection("the listing no longer trades", cx);
                        return;
                    }
                }
                Ok(false) => {
                    self.clear_selection("the route does not publish it", cx);
                    return;
                }
                Err(error) if is_definitely_unsupported(&error) => {
                    self.clear_selection("the directory does not serve it", cx);
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
        }
        let request = if retry || market_reloaded {
            CandlestickRequest::Retry {
                listing: selection.listing,
                timeframe: self.config.timeframe,
                adjustment: self.config.adjustment,
            }
        } else {
            CandlestickRequest::Load {
                listing: selection.listing,
                timeframe: self.config.timeframe,
                adjustment: self.config.adjustment,
            }
        };
        let command = if retry || market_reloaded {
            PendingCommand::Retry
        } else {
            PendingCommand::Load
        };
        let Some(binding) = self.binding.as_mut() else {
            // A pane with no binding has no Kytrix source to read: `load` refuses this
            // case before it gets here (the market precondition fails), so this is the
            // "host that went away" corner rather than a failure to report.
            return;
        };
        let request_id = binding.call(cx, request);
        self.pending.track(request_id, command);
        let _ = cx.request_frame();
    }

    /// Applies what a header control handed over.
    fn apply_header_request(&mut self, request: HeaderRequest, cx: &mut WidgetCx<'_>) {
        match request {
            HeaderRequest::Timeframe(timeframe) => self.select_timeframe(timeframe, cx),
            HeaderRequest::Adjustment(adjustment) => self.select_adjustment(adjustment, cx),
            HeaderRequest::ToggleSettings => {
                if self.indicator_settings_available(cx) {
                    self.toggle_indicator_panel(cx);
                }
            }
        }
    }

    /// Applies what the indicator panel committed.
    fn apply_panel_request(&mut self, request: PanelRequest, cx: &mut WidgetCx<'_>) {
        match request {
            PanelRequest::SetIndicator(indicator) => self.set_indicator_config(indicator, cx),
        }
    }

    /// Applies a selector result as one configuration transition.
    fn select_selection(&mut self, selection: InstrumentSelection, cx: &mut WidgetCx<'_>) {
        // A scope this chart may follow is a scope the loaded venues answer with, and that
        // one answer is the whole of what the selector offered: a scope it could not draw
        // is refused rather than carried into a stream nothing will publish.
        if let Some(connector) = self.connector(cx)
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
            return;
        }
        if self.config.selection.as_ref() == Some(&selection) && !self.status.is_failed(cx.app()) {
            return;
        }
        self.config.selection = Some(selection);
        self.config_change.publish(cx.app_mut());
        self.start_current(false, false, cx);
    }

    fn select_timeframe(&mut self, timeframe: TimeFrame, cx: &mut WidgetCx<'_>) {
        if self.config.timeframe == timeframe && !self.status.is_failed(cx.app()) {
            return;
        }
        self.config.timeframe = timeframe;
        self.config_change.publish(cx.app_mut());
        self.start_current(false, false, cx);
    }

    /// Applies the adjustment control's result as one configuration transition.
    fn select_adjustment(&mut self, adjustment: Adjustment, cx: &mut WidgetCx<'_>) {
        if self.config.adjustment == adjustment && !self.status.is_failed(cx.app()) {
            return;
        }
        self.config.adjustment = adjustment;
        self.config_change.publish(cx.app_mut());
        self.start_current(false, false, cx);
    }

    /// Replaces the indicator settings and writes them back.
    fn set_indicator_config(
        &mut self,
        indicator: CandlestickIndicatorConfig,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.config.indicator == indicator {
            return;
        }
        self.config.indicator = indicator;
        self.config_change.publish(cx.app_mut());
        let _ = cx.request_frame();
    }

    /// Whether the indicator settings are something the user can open.
    fn indicator_settings_available(&self, cx: &WidgetCx<'_>) -> bool {
        self.market_ready(cx) && self.config.selection.is_some()
    }

    /// Opens the settings panel, or closes and drops it.
    fn toggle_indicator_panel(&mut self, cx: &mut WidgetCx<'_>) {
        if self.panel_open {
            self.close_indicator_panel(cx);
        } else {
            self.panel_open = true;
        }
        let _ = cx.request_frame();
    }

    /// Drops the settings panel.
    fn close_indicator_panel(&mut self, cx: &mut WidgetCx<'_>) {
        self.panel_open = false;
        let _ = cx.request_frame();
    }

    /// Builds, keeps or drops the panel according to whether the user has it open.
    ///
    /// The panel's column is built when the user opens it and removed when it closes, so the
    /// plot takes back the width: hiding a column would leave its flex space reserved (the
    /// engine's visibility removes paint and hit-testing, not layout).
    fn sync_panel(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(root) = self.body_root else {
            return Ok(());
        };
        if self.panel_open {
            if self.panel_parent.is_none() {
                if cx.tree().get(root).is_err() {
                    return Ok(());
                }
                let separator = Separator::vertical(cx.theme().clone());
                let separator_parent = NodeBuilder::boxed()
                    .style(StyleBuilder::new().flex_none().height_full().build())
                    .build(cx.tree_mut(), root)?;
                separator.build(cx.tree_mut(), separator_parent)?;
                let parent = NodeBuilder::boxed()
                    .id(PANEL_PARENT_SELECTOR)
                    .style(
                        StyleBuilder::new()
                            .flex_none()
                            .width(PANEL_WIDTH)
                            .height_full()
                            .min_height_0()
                            .build(),
                    )
                    .build(cx.tree_mut(), root)?;
                self.panel_separator = Some(separator_parent);
                self.panel_parent = Some(parent);
            }
            let Some(parent) = self.panel_parent else {
                return Ok(());
            };
            if self.panel.is_none() {
                if cx.tree().get(parent).is_err() {
                    return Ok(());
                }
                let mut panel = IndicatorPanel::new(self.panel_requests);
                panel.build(cx, parent)?;
                self.panel = Some(panel);
                let _ = cx.request_frame();
            }
            if let Some(panel) = self.panel.as_mut()
                && panel.sync(cx, &self.config.indicator)?
            {
                let _ = cx.request_frame();
            }
        } else if self.panel_parent.is_some() || self.panel.is_some() {
            self.release_panel(cx);
            let _ = cx.request_frame();
        }
        Ok(())
    }

    /// Releases the panel, and removes the column it was built into.
    fn release_panel(&mut self, cx: &mut WidgetCx<'_>) {
        if let Some(mut panel) = self.panel.take() {
            panel.release(cx);
        }
        for node in [self.panel_parent.take(), self.panel_separator.take()]
            .into_iter()
            .flatten()
        {
            if cx.tree().get(node).is_ok() {
                let window = cx.window();
                let _ = cx.app_mut().remove_node(window, node);
            }
        }
    }

    /// Keeps the "no data" line over the plot current.
    fn sync_empty_state(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(container) = self.empty_container else {
            return Ok(());
        };
        if cx.tree().get(container).is_err() {
            self.empty_container = None;
            return Ok(());
        }
        let has_candles = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| !snapshot.candles().is_empty());
        let visible = self.snapshot.is_some() && !has_candles;
        let changed = cx.write(&self.empty_visible, visible);
        let language = *cx.read(&cx.language());
        if self.empty_language != Some(language) {
            self.empty_language = Some(language);
            let translate = cx.translate().clone();
            cx.write(&self.empty_text, t_smol!(translate, "common.no_data"));
            let _ = cx.request_frame();
        }
        if changed {
            let _ = cx.request_frame();
        }
        Ok(())
    }

    /// Pumps the countdown: applies what the timer wrote, then arms the next tick.
    fn sync_countdown(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let wake = *cx.read(&self.countdown_wake);
        let mut changed = false;
        if let Some(now_timestamp) = wake {
            self.countdown_timer = None;
            cx.write(&self.countdown_wake, None);
            changed = self.tick_countdown(now_timestamp);
        }
        if self.should_refresh_countdown() {
            if self.countdown_timer.is_none() {
                self.arm_countdown(cx);
            }
        } else {
            self.cancel_countdown(cx);
        }
        if changed {
            let _ = cx.request_frame();
        }
        Ok(())
    }

    /// Arms the one-shot timer that wakes the countdown at the next label change.
    fn arm_countdown(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(scope) = self.body_root else {
            return;
        };
        let now_timestamp = current_timestamp_millis();
        let delay = self.next_countdown_refresh_delay(now_timestamp);
        let Some(delay) = delay else {
            return;
        };
        let wake = self.countdown_wake;
        let window = cx.window();
        match cx.app_mut().after_scoped(window, scope, delay, move |app| {
            app.write(&wake, Some(current_timestamp_millis()));
            let _ = app.request_repaint(window);
        }) {
            Ok(id) => self.countdown_timer = Some(id),
            Err(error) => {
                log::error!("the candlestick countdown could not arm its tick: {error}");
            }
        }
    }

    /// Stops the countdown timer, because there is nothing to count down to.
    fn cancel_countdown(&mut self, cx: &mut WidgetCx<'_>) {
        if let Some(id) = self.countdown_timer.take() {
            cx.app_mut().cancel_timer(&id);
        }
    }

    /// The refresh delay until the next label would change, if the chart has one.
    ///
    /// `None` means the chart is not counting down at all (no live-edge candle, no
    /// venue), so no timer has to be armed.
    fn next_countdown_refresh_delay(&self, now_timestamp: i64) -> Option<Duration> {
        if !self.should_refresh_countdown() {
            return None;
        }
        let venue = venue_of(&self.config)?;
        Some(
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
                ),
        )
    }

    /// Recomputes the countdown label for `now`, reporting whether it moved.
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

    /// Whether a window-space point is inside the chart canvas' last measured box.
    ///
    /// The panel and the plot share one widget surface, so every pointer event arrives at
    /// this widget; the canvas box is what separates the chart's interactions from the
    /// panel's.
    fn chart_contains(&self, position: Point) -> bool {
        self.chart
            .bounds()
            .is_some_and(|bounds| bounds.contains(position))
    }

    /// Paints the chart for the frame, if anything it would draw moved.
    fn sync_chart(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return Ok(());
        };
        let frame = ChartFrame {
            snapshot,
            viewport: self.viewport,
            interval: self.config.timeframe,
            countdown_label: &self.countdown.label,
            indicators: &self.config.indicator,
            panel_open: self.panel_open,
            data_time_zone: cx.data_time_zone(),
        };
        if self.chart.sync(cx, &frame)? {
            let _ = cx.request_frame();
        }
        Ok(())
    }

    /// Accepts a snapshot as the whole of what the chart draws.
    fn apply_snapshot(&mut self, venue: &VenueId, snapshot: &CandlestickSeriesSnapshot) {
        self.snapshot = Some(snapshot.clone());
        // A marker names the page that was asked for, so it stops meaning anything once
        // the buffer has moved past it.
        if let Some(earliest) = snapshot.earliest_timestamp()
            && self
                .history_requested_before
                .is_some_and(|before_timestamp| earliest < before_timestamp)
        {
            self.history_requested_before = None;
        }
        if let Some(latest) = snapshot.latest_timestamp()
            && self
                .newer_requested_after
                .is_some_and(|after_timestamp| latest > after_timestamp)
        {
            self.newer_requested_after = None;
        }
        if !snapshot.at_live_edge() {
            // The chart is showing a historical window, so no candle on screen is still
            // being built and there is nothing to count down to.
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
        // Following the live edge is the absence of an anchor, so once the newest candle
        // is back on screen the anchor is dropped rather than pinned to it.
        if self.viewport.right_timestamp.is_some_and(|timestamp| {
            snapshot
                .latest_timestamp()
                .is_some_and(|latest| timestamp >= latest)
        }) {
            self.viewport.right_timestamp = None;
        }
    }

    /// Asks for the page the user is about to need.
    ///
    /// The margin is measured against the widest the plot can be, so the request goes out
    /// slightly before the edge is reached rather than after the chart has already run out
    /// of candles.
    fn prefetch_if_needed(&mut self, cx: &mut WidgetCx<'_>) {
        // Failure is terminal: the stream that would answer is gone with it.
        if self.status.is_failed(cx.app()) {
            return;
        }
        let Some(maximum_plot_width) = self
            .chart
            .layout()
            .map(crate::chart::geometry::ChartLayout::maximum_plot_width)
        else {
            return;
        };
        if maximum_plot_width <= 0.0 {
            return;
        }
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let range = visible_range(snapshot.candles(), self.viewport, maximum_plot_width);
        // Warmup is a history need: the left margin also has to cover it, or a page that
        // only covered the plot's width would leave the leftmost trustworthy value outside
        // the buffer once it arrived. The newer side keeps the plain margin.
        let margin = PREFETCH_MARGIN.max(self.config.indicator.warmup_depth());
        let following_the_live_edge =
            self.viewport.right_timestamp.is_none() && snapshot.at_live_edge();
        let room_for_a_page = MAXIMUM_CANDLES.saturating_sub(snapshot.candles().len())
            >= usize::from(HISTORY_PAGE_SIZE);
        // `WidgetBinding::call` owns a single RPC slot and replaces whatever was pending in
        // it, so each branch is gated on the other's flag: at most one pagination request is
        // ever out.
        if range.start <= margin
            && (!following_the_live_edge || room_for_a_page)
            && !snapshot.older_exhausted()
            && !self.fetching_history
            && !self.fetching_newer
            && let Some(before_timestamp) = snapshot.earliest_timestamp()
            && self.history_requested_before != Some(before_timestamp)
        {
            self.fetching_history = true;
            self.history_requested_before = Some(before_timestamp);
            // The candle at the live edge is on screen exactly when the viewport follows it;
            // that is what decides whether the page may evict it.
            let newest = if self.viewport.right_timestamp.is_none() {
                NewestCandle::Kept
            } else {
                NewestCandle::Evicted
            };
            #[cfg(test)]
            {
                self.prefetched_history_newest = Some(newest);
                self.history_requests += 1;
            }
            self.call(
                cx,
                CandlestickRequest::FetchHistory {
                    before_timestamp,
                    newest,
                },
                PendingCommand::FetchHistory,
            );
        } else if range.end.saturating_add(PREFETCH_MARGIN) >= snapshot.candles().len()
            && !snapshot.at_live_edge()
            && !self.fetching_newer
            && !self.fetching_history
            && let Some(after_timestamp) = snapshot.latest_timestamp()
            && self.newer_requested_after != Some(after_timestamp)
        {
            self.fetching_newer = true;
            self.newer_requested_after = Some(after_timestamp);
            self.call(
                cx,
                CandlestickRequest::FetchNewer { after_timestamp },
                PendingCommand::FetchNewer,
            );
        }
    }

    /// Sends one command and records the identity its answer will carry.
    fn call(
        &mut self,
        cx: &mut WidgetCx<'_>,
        request: CandlestickRequest,
        command: PendingCommand,
    ) {
        let Some(binding) = self.binding.as_mut() else {
            return;
        };
        let request_id = binding.call(cx, request);
        self.pending.track(request_id, command);
    }

    /// Consumes what the binding delivered, in arrival order.
    fn drain_inbox(&mut self, cx: &mut WidgetCx<'_>) {
        #[cfg(test)]
        let messages = {
            let mut messages = std::mem::take(&mut self.test_inbox);
            messages.extend(
                self.binding
                    .as_ref()
                    .map(WidgetBinding::take)
                    .unwrap_or_default(),
            );
            messages
        };
        #[cfg(not(test))]
        let messages = self
            .binding
            .as_ref()
            .map(WidgetBinding::take)
            .unwrap_or_default();
        for inbound in messages {
            match inbound {
                Inbound::Reliable { key, message } => self.apply_reliable(key, message, cx),
                Inbound::Latest { key, message } => self.apply_latest(key, message, cx),
                Inbound::Rpc { request_id, result } => self.apply_rpc(request_id, result, cx),
            }
        }
    }

    /// Applies one RPC reply, once its request identity says which command it answers.
    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<CandlestickProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        let Some(command) = self.pending.take(request_id) else {
            log::debug!(
                "the candlestick pane ignored reply {request_id}: it answers no command in flight"
            );
            return;
        };
        let Err(error) = result else {
            return;
        };
        // A page that could not be fetched leaves its direction's flag set only if the
        // failure is ignored; the failure is terminal, so the stream is gone as well and
        // there is no direction left to unblock.
        match command {
            PendingCommand::Load
            | PendingCommand::Retry
            | PendingCommand::FetchHistory
            | PendingCommand::FetchNewer => self.fail_rpc(error, cx),
        }
    }

    /// Reports the failure an RPC answer carried.
    fn fail_rpc(&mut self, error: RpcCallError<CandlestickServiceError>, cx: &mut WidgetCx<'_>) {
        if self.status.is_failed(cx.app()) {
            return;
        }
        let recovery = self.recovery.clone();
        self.status.report(
            cx.app_mut(),
            WidgetState::failed(Arc::new(CandlestickFailure::Rpc(error)), recovery),
        );
    }

    /// Applies the failure the reliable stream carried.
    fn apply_reliable(
        &mut self,
        key: CandlestickStreamKey,
        message: Arc<CandlestickStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx.app())
            || self.awaiting_market
        {
            return;
        }
        match message.as_ref() {
            CandlestickStreamMessage::Status(status) => match status {
                CandlestickStatus::Connecting => {
                    self.status.report(cx.app_mut(), WidgetState::Connecting);
                }
                CandlestickStatus::LoadingHistory => {
                    self.status.report(cx.app_mut(), WidgetState::Loading);
                }
                // The pane is ready; the candles are what it draws. Pagination is not a
                // status — a page is announced by `PageSettled` — so this arm only reports
                // the pane's own state and touches nothing else.
                CandlestickStatus::Ready => {
                    self.status.report(cx.app_mut(), WidgetState::Ready);
                }
                CandlestickStatus::Error(error) => {
                    // A market the loaded venues no longer hold is the notice's answer, not
                    // a failure and not a scope that stopped serving candlesticks.
                    if error.is_venue_not_loaded() {
                        self.wait_for_market(cx);
                        return;
                    }
                    // A rejection the request itself produced is a scope the venue does not
                    // serve, not a stream that failed.
                    if self.recover_unsupported_scope(error, cx) {
                        return;
                    }
                    // The directory answering "this route's catalogue is not in memory" is a
                    // wait rather than a failure.
                    if market_is_catalog_not_loaded(error) {
                        self.wait_for_market(cx);
                        return;
                    }
                    let recovery = self.recovery.clone();
                    self.status.report(
                        cx.app_mut(),
                        WidgetState::failed(
                            Arc::new(CandlestickFailure::Market(error.clone())),
                            recovery,
                        ),
                    );
                }
            },
            // One pagination request is over, so its direction's flag can go: the page
            // either landed or never will. The prefetch then runs again, because a settle is
            // also the news that the window this chart shows may still not be covered.
            CandlestickStreamMessage::PageSettled { direction } => match direction {
                PageDirection::History => self.fetching_history = false,
                PageDirection::Newer => self.fetching_newer = false,
            },
            CandlestickStreamMessage::Snapshot(_) => {}
        }
    }

    /// Applies one push of the latest stream.
    fn apply_latest(
        &mut self,
        key: CandlestickStreamKey,
        message: Arc<CandlestickStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx.app())
            || self.awaiting_market
        {
            return;
        }
        if let CandlestickStreamMessage::Snapshot(snapshot) = message.as_ref() {
            self.apply_snapshot(key.venue(), snapshot);
            let _ = cx.request_frame();
        }
    }

    /// The pointer moved: pan when dragging, otherwise move the crosshair.
    fn apply_pointer(&mut self, position: Point) -> bool {
        if self.dragging {
            let Some(last_x) = self.drag_last_x.replace(position.x) else {
                return false;
            };
            // Panning is measured in candles, and the remainder carries the sub-candle part
            // forward so that a slow drag still moves.
            self.drag_remainder += position.x - last_x;
            let candle_delta = (self.drag_remainder / self.viewport.candle_width).trunc() as isize;
            if candle_delta != 0 {
                self.drag_remainder -= candle_delta as f32 * self.viewport.candle_width;
                return self.pan_by(-candle_delta);
            }
            false
        } else if self.viewport.cursor == Some(position) {
            false
        } else {
            self.viewport.cursor = Some(position);
            true
        }
    }

    fn begin_drag(&mut self, x: f32) -> bool {
        let changed = !self.dragging || self.viewport.cursor.is_some();
        self.dragging = true;
        self.drag_last_x = Some(x);
        self.drag_remainder = 0.0;
        self.viewport.cursor = None;
        changed
    }

    fn end_drag(&mut self, position: Option<Point>) -> bool {
        let changed = self.dragging || self.viewport.cursor != position;
        self.dragging = false;
        self.drag_last_x = None;
        self.drag_remainder = 0.0;
        self.viewport.cursor = position;
        changed
    }

    /// Scrolls the viewport by whole candles, keeping the live edge when it is showing.
    fn pan_by(&mut self, candle_delta: isize) -> bool {
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
        true
    }

    /// Zooms around the candle under the pointer.
    ///
    /// The candle under the cursor is found *before* the width changes and anchored after
    /// it, so the chart grows away from the pointer instead of sliding out from under it.
    fn zoom_at(&mut self, position: Point, delta_y: f32) -> bool {
        let Some(layout) = self.chart.layout() else {
            return false;
        };
        if self.snapshot.is_none() || !layout.contains_data_point(position) {
            return false;
        }
        let plot = layout.price_plot;
        let Some(snapshot) = self.snapshot.as_ref() else {
            return false;
        };
        let steps = if delta_y > 0.0 { 1.0 } else { -1.0 };
        let old_width = self.viewport.candle_width;
        let new_width = (old_width + steps).clamp(MIN_CANDLE_WIDTH, MAX_CANDLE_WIDTH);
        if (new_width - old_width).abs() < f32::EPSILON {
            return false;
        }
        let candles = snapshot.candles();
        let old_range = visible_range(candles, self.viewport, plot.size.width);
        if old_range.is_empty() {
            return false;
        }
        let geometry = CandleGeometry::new(plot, old_range.len(), old_width);
        let Some(hover_slot) = geometry.slot_at(position.x) else {
            return false;
        };
        let hover_index = old_range.start + hover_slot;
        let slots_right =
            (((plot.max().x - position.x) / new_width).ceil() as usize).saturating_add(1);
        let new_end = hover_index
            .saturating_add(slots_right)
            .clamp(1, candles.len());
        self.viewport.candle_width = new_width;
        self.viewport.right_timestamp = if new_end == candles.len() && snapshot.at_live_edge() {
            None
        } else {
            Some(candles[new_end - 1].timestamp)
        };
        self.viewport.cursor = Some(position);
        true
    }

    /// Builds the instrument control into the header's left group.
    fn build_instrument(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let host = SelectorHost::new(Rc::clone(cx.services_rc()));
        let selection = self.config.selection.clone();
        let filters = InstrumentFilters {
            kind_filter: InstrumentKindFilter::All,
            status_filter: InstrumentStatusFilter::Any,
            required_capability: CapabilityId::Candlesticks,
            entry_predicate: None,
            routes: self.loaded_scopes(cx),
        };
        let requests = self.selection_requests;
        let button = InstrumentButton::new(
            cx.unique_key(INSTRUMENT_KEY_SUFFIX),
            selection,
            filters,
            "widget.candlestick.select_instrument",
            move |selection: InstrumentSelection, app: &mut App, window: WindowId| {
                app.write(&requests, Some(selection));
                let _ = app.request_repaint(window);
            },
        );
        let window = cx.window();
        let control = button.build(&host, cx.app_mut(), window, parent)?;
        self.instrument = Some(HeaderInstrument {
            parent,
            control,
            built: self.header_state(cx),
        });
        Ok(())
    }

    /// The scopes a picker may offer, as the loaded venues answer them now.
    fn loaded_scopes(&self, cx: &WidgetCx<'_>) -> Arc<[LoadedScope]> {
        self.connector(cx).map_or_else(Arc::default, |connector| {
            LoadedScope::all(connector.venues())
        })
    }

    /// Builds the settings button and the two pickers into the header's right group.
    fn build_controls(&mut self, cx: &mut WidgetCx<'_>, parent: NodeId) -> Result<(), EngineError> {
        let row = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .gap(4.0)
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let requests = self.requests;
        let available = self.indicator_settings_available(cx);
        let label = t_smol!(translate, "widget.candlestick.indicator.settings");
        let key = cx.unique_key(SETTINGS_KEY_SUFFIX);
        let window = cx.window();
        let mut settings = Button::new(cx.app_mut(), theme.clone(), key)
            .ghost()
            .with_unified_size(UnifiedSize::Small)
            .icon(base_ui::kit::icon::IconKind::Faders)
            .a11y_label(label.clone())
            .tooltip_text(label)
            .selected(self.panel_open)
            .disabled(!available);
        if available {
            settings = settings.on_click(move |action| {
                // The handler only records the request: the widget's `sync` applies it
                // where it can see the whole state (and re-checks availability).
                let window = action.window();
                let app = action.app_mut();
                app.write(&requests, Some(HeaderRequest::ToggleSettings));
                let _ = app.request_repaint(window);
            });
        }
        settings.build(cx.app_mut(), window, row)?;
        let adjustment = self.build_adjustment_select(cx, row, &theme)?;
        let timeframe = self.build_timeframe_select(cx, row, &theme)?;
        self.controls = Some(HeaderControls {
            root: row,
            parent,
            settings,
            adjustment,
            timeframe,
            built: self.header_state(cx),
        });
        Ok(())
    }

    fn header_state(&self, cx: &WidgetCx<'_>) -> HeaderState {
        HeaderState {
            language: *cx.read(&cx.language()),
            selection: self.config.selection.clone(),
            generation: self.seen_generation,
        }
    }

    fn build_adjustment_select(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        theme: &base_ui::theme::handle::ThemeHandle,
    ) -> Result<Option<Select>, EngineError> {
        let translate = cx.translate().clone();
        let offered = self.offered_adjustments();
        if offered.len() <= 1 {
            return Ok(None);
        }
        let requests = self.requests;
        let window = cx.window();
        let key = cx.unique_key(ADJUSTMENT_KEY_SUFFIX);
        let items = offered
            .iter()
            .map(|adjustment| {
                SelectItem::new(
                    SmolStr::new_static(adjustment.label()),
                    t_smol!(translate, adjustment_key(*adjustment)),
                )
            })
            .collect::<Vec<_>>();
        let options = SelectOptions::default()
            .size(UnifiedSize::Small)
            .placeholder(t_smol!(translate, adjustment_key(self.config.adjustment)));
        // The composition root's singletons are cloned before the mutable app borrow:
        // `WidgetCx` hands out one borrow at a time.
        let layer = base_ui::kit::layer::Layer::clone(cx.layer());
        let router = base_ui::kit::overlay::AnimationRouter::clone(cx.animation());
        let select = Select::new(cx.app_mut(), theme.clone(), layer, router, key, options)
            .on_change(move |app, chosen| {
                let Some(id) = chosen.first() else {
                    return;
                };
                let Some(adjustment) = adjustment_of_label(id.as_str()) else {
                    return;
                };
                app.write(&requests, Some(HeaderRequest::Adjustment(adjustment)));
                let _ = app.request_repaint(window);
            });
        select
            .set_items(cx.app_mut(), items)
            .unwrap_or_else(|error| {
                log::warn!(
                    "the candlestick adjustment picker could not state its choices: {error}"
                );
            });
        select.set_chosen(cx.app_mut(), [self.config.adjustment.label()], false);
        select.build(cx.app_mut(), window, parent)?;
        Ok(Some(select))
    }

    fn build_timeframe_select(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        theme: &base_ui::theme::handle::ThemeHandle,
    ) -> Result<Select, EngineError> {
        let translate = cx.translate().clone();
        let requests = self.requests;
        let window = cx.window();
        let key = cx.unique_key(TIMEFRAME_KEY_SUFFIX);
        let offered = self.offered_intervals();
        let disabled = offered.is_empty();
        // The picker offers the intervals in the unit groups the old menu headed, in the
        // same order; the kit select draws a flat list, so the groups survive as ordering
        // rather than as headings (a registered kit gap).
        let mut items: Vec<SelectItem> = Vec::with_capacity(offered.len());
        if !disabled {
            for unit in TIMEFRAME_GROUPS {
                for timeframe in offered
                    .iter()
                    .filter(|timeframe| tick_and_unit(**timeframe).1 == unit)
                {
                    let tick = tick_and_unit(*timeframe).0;
                    items.push(SelectItem::new(
                        SmolStr::new_static(timeframe.label()),
                        t_smol!(translate, unit.label_key(tick)),
                    ));
                }
            }
        }
        let options = SelectOptions::default()
            .size(UnifiedSize::Small)
            .label(t_smol!(translate, "widget.candlestick.timeframe"))
            .placeholder(SmolStr::new_static(self.config.timeframe.label()));
        // The composition root's singletons are cloned before the mutable app borrow:
        // `WidgetCx` hands out one borrow at a time.
        let layer = base_ui::kit::layer::Layer::clone(cx.layer());
        let router = base_ui::kit::overlay::AnimationRouter::clone(cx.animation());
        let select = Select::new(cx.app_mut(), theme.clone(), layer, router, key, options)
            .on_change(move |app, chosen| {
                // The picker identifies an interval by the label it shows, which is the
                // interval's own spelling.
                let Some(id) = chosen.first() else {
                    return;
                };
                let Some(timeframe) = timeframe_of_label(id.as_str()) else {
                    return;
                };
                app.write(&requests, Some(HeaderRequest::Timeframe(timeframe)));
                let _ = app.request_repaint(window);
            });
        select
            .set_items(cx.app_mut(), items)
            .unwrap_or_else(|error| {
                log::warn!("the candlestick interval picker could not state its choices: {error}");
            });
        select.set_chosen(cx.app_mut(), [self.config.timeframe.label()], false);
        select.set_disabled(cx.app_mut(), disabled);
        select.build(cx.app_mut(), window, parent)?;
        Ok(select)
    }

    /// The intervals the picker offers for the market the chart follows.
    fn offered_intervals(&self) -> Vec<TimeFrame> {
        let Some(route) = self.asked_route() else {
            return Vec::new();
        };
        let Some(published_timeframes) = self.published_timeframes.as_ref() else {
            return offered_timeframes(None);
        };
        let published = published_timeframes(route);
        offered_timeframes(published.as_deref().ok())
    }

    /// The price-adjustment conventions the adjustment control offers.
    fn offered_adjustments(&self) -> Vec<Adjustment> {
        let Some(route) = self.asked_route() else {
            return Vec::new();
        };
        self.published_adjustments
            .as_ref()
            .and_then(|published_adjustments| published_adjustments(route).ok())
            .unwrap_or_default()
    }

    /// The route this chart may ask what it publishes, if there is one to ask.
    fn asked_route(&self) -> Option<&VenueRoute> {
        self.config
            .selection
            .as_ref()
            .filter(|_| !self.awaiting_market)
            .map(|selection| &selection.listing.route)
    }

    /// Keeps the header controls' copy, choices and choices current.
    fn sync_header(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let state = self.header_state(cx);
        let instrument_stale = self.instrument.as_ref().is_some_and(|instrument| {
            instrument.built != state || cx.tree().get(instrument.parent).is_err()
        });
        if instrument_stale {
            let Some(parent) = self.instrument.as_ref().map(|instrument| instrument.parent) else {
                return Ok(());
            };
            self.release_instrument(cx, false);
            if cx.tree().get(parent).is_ok() {
                self.build_instrument(cx, parent)?;
            }
        }
        let controls_stale = self.controls.as_ref().is_some_and(|controls| {
            controls.built != state || cx.tree().get(controls.parent).is_err()
        });
        if controls_stale {
            let Some(parent) = self.controls.as_ref().map(|controls| controls.parent) else {
                return Ok(());
            };
            self.release_controls(cx, false);
            if cx.tree().get(parent).is_ok() {
                self.build_controls(cx, parent)?;
            }
            return Ok(());
        }
        let Some(controls) = self.controls.as_ref() else {
            return Ok(());
        };
        // The selected state is dynamic: the panel may be toggled between two frames
        // without the copy changing.
        controls
            .settings
            .set_selected(cx.app_mut(), self.panel_open);
        let available = self.indicator_settings_available(cx);
        controls.settings.set_disabled(cx.app_mut(), !available);
        if let Some(adjustment) = controls.adjustment.as_ref() {
            adjustment.set_chosen(cx.app_mut(), [self.config.adjustment.label()], false);
        }
        controls
            .timeframe
            .set_chosen(cx.app_mut(), [self.config.timeframe.label()], false);
        Ok(())
    }

    /// Takes the instrument control down. `in_row` says the pane is rebuilding its row,
    /// which already took the node with it.
    fn release_instrument(&mut self, cx: &mut WidgetCx<'_>, in_row: bool) {
        let Some(instrument) = self.instrument.take() else {
            return;
        };
        if !in_row && cx.tree().get(instrument.control.node()).is_ok() {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, instrument.control.node());
        }
        instrument.control.release(cx.app_mut());
    }

    /// Takes the right group's controls down.
    fn release_controls(&mut self, cx: &mut WidgetCx<'_>, in_row: bool) {
        let Some(controls) = self.controls.take() else {
            return;
        };
        if !in_row && cx.tree().get(controls.root).is_ok() {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, controls.root);
        }
        controls.settings.release(cx.app_mut());
        if let Some(adjustment) = controls.adjustment.as_ref() {
            adjustment.release(cx.app_mut());
        }
        controls.timeframe.release(cx.app_mut());
    }
}

/// The selector of the body root, which tests locate the pane by.
const BODY_SELECTOR: &str = "CandlestickBody";
/// The selector of the overlay that says there is nothing to draw.
const EMPTY_SELECTOR: &str = "CandlestickEmptyOverlay";
/// The selector of the box the settings panel is built into.
const PANEL_PARENT_SELECTOR: &str = "CandlestickPanelParent";
/// The key suffixes the header controls are built under. The instance's scope is appended
/// by `WidgetCx::unique_key`, so two panes never share an action identity.
const INSTRUMENT_KEY_SUFFIX: &str = "candlestick.header.instrument";
const SETTINGS_KEY_SUFFIX: &str = "candlestick.header.settings";
const ADJUSTMENT_KEY_SUFFIX: &str = "candlestick.header.adjustment";
const TIMEFRAME_KEY_SUFFIX: &str = "candlestick.header.timeframe";

/// The venue a chart's countdown belongs to.
fn venue_of(config: &CandlestickConfig) -> Option<&VenueId> {
    config
        .selection
        .as_ref()
        .map(|selection| &selection.listing.route.venue)
}

/// The pointer position an input event carries, for the canvas hit test.
fn pointer_position(event: &InputEvent) -> Option<Point> {
    match event {
        InputEvent::PointerEnter { position, .. }
        | InputEvent::PointerMove { position, .. }
        | InputEvent::PointerDown { position, .. }
        | InputEvent::PointerUp { position, .. }
        | InputEvent::PointerLeave { position, .. }
        | InputEvent::Scroll { position, .. } => Some(*position),
        _ => None,
    }
}

/// The adjustment convention a picker entry names, from the canonical label it states.
fn adjustment_of_label(label: &str) -> Option<Adjustment> {
    match label {
        "none" => Some(Adjustment::None),
        "forward" => Some(Adjustment::Forward),
        _ => None,
    }
}

#[cfg(test)]
mod tests;

//! How every perpetual-statistics pane behaves.
//!
//! The five widgets differ only in which series they plot, so everything else is here:
//! the scope, the instrument picker, the interval, the countdown, the manual refresh, and
//! the body.
//!
//! # What this pane is
//!
//! These series are a bounded history rather than a stream, so "live" means poll: the
//! service fetches the most recent window once a minute, and the pane counts down to the
//! next fetch so a number that has not moved can be told apart from a pane that has
//! stopped. A manual refresh resets that schedule rather than running alongside it.
//!
//! # What it does with a failure
//!
//! A failed fetch is a failure the user sees: the pane reports it through the framework,
//! which puts a banner over the chart that is still on screen. The service stops its
//! schedule at that point, so the banner's recovery is what starts it again — a venue that
//! is briefly unreachable does not turn into a request every minute until someone notices.
//!
//! # Where the market comes from
//!
//! The loaded venues, the published catalogue and the names a chart is labelled in are all
//! the directory's answers: the connector the composition root injects in the application,
//! and a fixture in a test (see [`StatisticsDirectory`]). A directory generation written
//! into `services.catalog_generation` is the one signal that says the directory moved, so
//! the pane compares it in `sync` and re-reads the directory when it did (AGENTS §8.3). A
//! publication keeps the window the user is reading: it re-runs the gate and the directory's
//! answers, and the stream that is polling the window stays as it is, because that is what
//! keeps a published catalogue from counting the window on screen as one the pane has
//! already overtaken (see [`StatisticsWidget::reload`]). The pane never loads the directory,
//! never polls the catalogue and never subscribes to a venue event.
//!
//! # Why replies are matched by request identity
//!
//! Two commands share one inbox — a load/retry and a manual refresh. The framework hands
//! each `call` a [`RequestId`], which the reply carries back; the pane keeps the command
//! table ([`Pending`]) and applies a reply only to the identity it belongs to. An identity
//! it does not know is a reply the replacement already overtook: it is dropped and counted
//! ([`Pending::dropped`]) rather than applied to the command now in flight (AGENTS §8.2).
//!
//! # The body is a chart, and the chart is a canvas
//!
//! The engine's only self-drawing entry point is a `Canvas` node, and the chart is the one
//! thing in this pane that needs it. [`WidgetView::body`] therefore builds the chart's
//! canvas node (and the empty-window overlay); after that, `sync` rebuilds the canvas when
//! what it would draw moved ([`ChartDispatch`]). A chart that is dropped and re-created is
//! rebuilt into the same host node, because the body is built only once.

use std::{marker::PhantomData, rc::Rc, sync::Arc};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    kit::{
        button::Button,
        empty_state::EmptyState,
        icon::IconKind,
        select::{Select, SelectItem, SelectOptions},
        size::{UnifiedSizable, UnifiedSize},
    },
    style_builder::StyleBuilder,
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use instrument_selector_ui::{
    host::SelectorHost,
    instrument_button::{InstrumentButton, InstrumentButtonHandle},
    instrument_selector::{
        InstrumentFilters, InstrumentKindFilter, InstrumentStatusFilter, LoadedScope, offers_route,
    },
};
use market_participants_connector::market::timeframe::StatisticsTimeFrame;
use market_participants_model::market::selection::InstrumentSelection;
use smol_str::SmolStr;
use ui_core::{
    a11y::A11yPatch,
    app::{App, TimerId},
    error::EngineError,
    event::{Event, InputEvent},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::style::StylePatch,
    text::TextStyle,
    window::WindowId,
};
use widget::{
    binding::{Inbound, Pending, WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    error::TranslatedError,
    identity::{WidgetId, WidgetInstanceId},
    protocol::RequestId,
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    util::{format_countdown, unix_millis},
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

use super::directory::{
    ChartUnits, ConnectorDirectory, ListingSupport, StatisticsDirectory, market_venues,
};
use crate::{
    chart::ChartDispatch,
    model::MetricKind,
    service::{
        StatisticsFetchError, StatisticsProtocol, StatisticsRequest, StatisticsResponse,
        StatisticsService, StatisticsServiceError, StatisticsSource, StatisticsStatus,
        StatisticsStreamKey, StatisticsStreamMessage,
    },
};

/// How often the pane re-reads the window it is showing.
///
/// It is not a fetch: it only re-checks the clock behind the countdown. A quarter of a
/// second is faster than the label's own resolution and cheaper than a frame.
pub(crate) const COUNTDOWN_TICK: std::time::Duration = std::time::Duration::from_millis(250);

/// The intervals the picker offers, and the only ones a pane can poll.
///
/// A stored interval outside this list is one this build cannot fetch: the picker cannot
/// draw it and the request would name a window the service does not serve, so the pane puts
/// it back to the default instead.
pub(crate) const TIMEFRAMES: [StatisticsTimeFrame; 9] = [
    StatisticsTimeFrame::Minute5,
    StatisticsTimeFrame::Minute15,
    StatisticsTimeFrame::Minute30,
    StatisticsTimeFrame::Hour1,
    StatisticsTimeFrame::Hour2,
    StatisticsTimeFrame::Hour4,
    StatisticsTimeFrame::Hour6,
    StatisticsTimeFrame::Hour12,
    StatisticsTimeFrame::Day1,
];

/// The kit key suffixes the header controls are minted from.
const INSTRUMENT_KEY_SUFFIX: &str = "perpetual_statistics.header.instrument";
const REFRESH_KEY_SUFFIX: &str = "perpetual_statistics.header.refresh";
const TIMEFRAME_KEY_SUFFIX: &str = "perpetual_statistics.header.timeframe";

/// The series one widget plots, and the wording that is its own.
///
/// This trait is the whole of what separates the five widgets: an id that is persisted, a
/// name, and the series that answers. Both are constants, which is why a widget is a type
/// rather than a value.
pub trait StatisticsMetric: Sized + 'static {
    /// The persistent identity of this pane. It is stored, so it cannot change.
    const ID: WidgetId;
    /// The message key of the widget's name.
    const DISPLAY_NAME: &'static str;
    /// The message key of the widget's description.
    const DESCRIPTION: Option<&'static str>;
    /// The asset path of the picture the widget gallery draws for this pane.
    const ICON: &'static str;
    /// Which series this pane plots.
    const METRIC: MetricKind;
}

/// Which venue, instrument and interval a pane follows.
///
/// The scope is optional because a pane exists before the user has chosen anything: it then
/// asks for an instrument rather than plotting a default one nobody asked for. Another
/// series, another venue or another interval is another chart, so all three are part of the
/// scope and all three replace what is on screen.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatisticsConfig {
    pub selection: Option<InstrumentSelection>,
    pub timeframe: StatisticsTimeFrame,
}

impl Default for StatisticsConfig {
    fn default() -> Self {
        Self {
            selection: None,
            timeframe: StatisticsTimeFrame::Minute5,
        }
    }
}

impl WidgetConfig for StatisticsConfig {
    /// The first persisted schema version, and the only one: the scope is an optional
    /// listing and the interval is one of [`TIMEFRAMES`].
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// A failure a pane reports that is not already a stream error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StatisticsFailure {
    Fetch(StatisticsFetchError),
    /// The application installed no market-participants directory, so this pane has
    /// nothing it could ever read.
    Unavailable,
    Rpc(RpcCallError<StatisticsServiceError>),
}

impl TranslatedError for StatisticsFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Fetch(error) => error.translated_message(translate),
            Self::Unavailable => t_smol!(translate, "error.statistics.stream_unavailable"),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
}

impl TranslatedError for StatisticsFetchError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::MarketParticipant(error) => error.translated_message(translate),
            Self::Timeout(timeout) => {
                t_smol!(
                    translate,
                    "error.statistics.timeout",
                    seconds = timeout.as_secs()
                )
            }
        }
    }
}

impl TranslatedError for StatisticsServiceError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::StreamClosed | Self::StreamBusy => {
                t_smol!(translate, "error.statistics.stream_unavailable")
            }
        }
    }
}

/// Whether this failure is the directory answering "this route's catalog is not in memory
/// yet" rather than reporting something wrong.
///
/// The pane's own gate asks that question before it reads anything, so this is the answer
/// changing underneath the request — a venue re-discovering its scopes is what does it. It
/// is a wait, not a failure: loading the market again is what puts the catalog back. The
/// pane's own service errors are refusals about the stream it is running, so only the
/// market read can carry the directory's answer.
fn is_waiting_for_a_catalog(failure: &StatisticsFailure) -> bool {
    matches!(
        failure,
        StatisticsFailure::Fetch(StatisticsFetchError::MarketParticipant(error))
            if widget::error::market_is_catalog_not_loaded(error)
    )
}

/// Which command an in-flight reply belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    /// A load or a retry of the configured scope.
    Load,
    /// A manual refresh of the scope already loaded.
    Refresh,
}

/// What a header control reports to the next `sync`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum HeaderRequest {
    Refresh,
    Selection(InstrumentSelection),
    Timeframe(StatisticsTimeFrame),
}

/// The countdown the header draws, and the one-shot timer that keeps it moving.
///
/// It is shared with the timer callback (`Rc`), which has only `&mut App`: the deadline the
/// label counts to lives in a `Cell`, the label itself is a signal, and the callback re-arms
/// the timer. Every deadline change goes through `update`, which cancels the pending tick
/// before it writes the label and arms the next chain — a deadline that flickers cannot
/// leave two chains ticking (I3: the pane is woken once per tick, not twice), and `release`
/// cancels through the same path.
struct Countdown {
    /// The window the timer belongs to.
    window: WindowId,
    /// The wall-clock millisecond the next read is due at, or nothing worth counting to (a
    /// read in flight, a failure, a schedule that stopped).
    deadline: std::cell::Cell<Option<i64>>,
    /// The time left, as the header's text node draws it.
    label: Signal<Option<SmolStr>>,
    /// The wall clock, which is what a deadline published in Unix milliseconds is
    /// comparable against; a test hands in a clock it can move.
    now: Rc<dyn Fn() -> i64>,
    /// The tick this pane is waiting for, if any. Holding the id is what makes a deadline
    /// change and `release` able to cancel it instead of leaving it to fire.
    timer: std::cell::RefCell<Option<TimerId>>,
}

/// The collaborators a pane is built over that are the same however its answer is sourced.
pub(crate) struct PaneScaffold {
    pub status: WidgetStatus,
    pub config_change: WidgetConfigChange,
    pub recovery: WidgetAction,
    /// The wall clock, which a test hands in so it can move it.
    pub now: Rc<dyn Fn() -> i64>,
}

impl Countdown {
    fn new(cx: &mut WidgetCx<'_>, now: Rc<dyn Fn() -> i64>) -> Self {
        Self {
            window: cx.window(),
            deadline: std::cell::Cell::new(None),
            label: cx.signal(None),
            now,
            timer: std::cell::RefCell::new(None),
        }
    }

    /// The deadline the label is counting to.
    fn deadline(&self) -> Option<i64> {
        self.deadline.get()
    }

    /// Moves the countdown to a new deadline (or stops it), and re-arms the timer.
    fn update(self: &Rc<Self>, app: &mut App, deadline: Option<i64>) {
        self.deadline.set(deadline);
        if let Some(id) = self.timer.borrow_mut().take() {
            app.cancel_timer(&id);
        }
        self.write_label(app);
        if deadline.is_some() {
            self.arm(app);
        }
    }

    /// Recomputes the label from the clock; returns whether it changed.
    fn write_label(&self, app: &mut App) -> bool {
        app.write(
            &self.label,
            countdown_label(self.deadline.get(), (self.now)()),
        )
    }

    /// Arms the one-shot tick for the next interval, if there is anything to count to.
    fn arm(self: &Rc<Self>, app: &mut App) {
        if self.deadline.get().is_none() {
            return;
        }
        let this = Rc::clone(self);
        match app.after(self.window, COUNTDOWN_TICK, move |app| this.tick(app)) {
            Ok(id) => *self.timer.borrow_mut() = Some(id),
            Err(error) => {
                log::error!("the statistics countdown could not arm its next tick: {error}")
            }
        }
    }

    /// One tick: write the label and re-arm the next one.
    fn tick(self: &Rc<Self>, app: &mut App) {
        *self.timer.borrow_mut() = None;
        self.write_label(app);
        self.arm(app);
    }
}

/// The header controls one group holds, and the copy they were built with.
///
/// A kit button's label and a picker's items are chosen when the control is built, so a
/// language switch — or a selection or interval that the stored copy no longer states —
/// rebuilds the group (AGENTS §15.1). Its node is the widget's own row inside the group the
/// pane made.
struct HeaderEntry {
    root: NodeId,
    parent: NodeId,
    language: LanguageCode,
    selection: Option<InstrumentSelection>,
    timeframe: StatisticsTimeFrame,
    generation: u64,
    instrument: Option<InstrumentButtonHandle>,
    refresh: Option<Button>,
    select: Option<Select>,
}

/// One pane: the scope it follows, the chart it draws, and its header controls.
pub struct StatisticsWidget<M: StatisticsMetric> {
    pub config: StatisticsConfig,
    binding: Option<WidgetBinding<StatisticsProtocol>>,
    pub status: WidgetStatus,
    recovery: WidgetAction,
    config_change: WidgetConfigChange,
    chart: Option<ChartDispatch>,
    /// Whether a window has been accepted since this scope began, which is what makes the
    /// chart content rather than a place waiting for it.
    accepted: bool,
    /// The revision of the window the pane has reported as ready, so readiness is published
    /// at the protocol's own boundary rather than per snapshot.
    ready: Option<u64>,
    /// When the next automatic fetch is due, as the service's own deadline.
    next_refresh_at: Option<i64>,
    countdown: Rc<Countdown>,
    /// Whether a refresh command this pane sent has been answered *and its round reported*.
    refresh_pending: bool,
    /// The commands this pane is waiting for, by the identity their replies carry.
    pending: Pending<PendingCommand>,
    /// The assets the chart's series are measured in, read from the listing's own rules once
    /// the program has answered what it lists.
    units: ChartUnits,
    /// The loaded market directory, or nothing when the host installed no directory root
    /// (the framework's headless host is one).
    directory: Option<Rc<dyn StatisticsDirectory>>,
    /// The directory generation this pane last acted on.
    seen_generation: u64,
    /// Whether the chart is waiting for its market rather than reading it.
    ///
    /// The pane keeps its stream while it waits — the alternative is cancelling the binding,
    /// which is terminal, and this pane has to be able to read the market again once the
    /// user loads it — so a window that was in flight when the route went away can still
    /// arrive. Accepting it would put a snapshot back and make the pane report content over
    /// the notice the framework is drawing, so every message entry refuses while this is
    /// set. The stream key cannot do this job: it says which scope a window belongs to, and
    /// this one belongs to the scope the notice is about.
    awaiting_market: bool,
    /// The node the chart's canvas lives in, once the body exists.
    chart_host: Option<NodeId>,
    /// Whether the window on screen is an empty one, which the body's overlay draws.
    empty: Signal<bool>,
    /// The sentence the empty-window overlay draws, bound so a language switch is a write.
    empty_text: Signal<SmolStr>,
    /// Where a header control hands over what the user did.
    requests: Signal<Option<HeaderRequest>>,
    left_header: Option<HeaderEntry>,
    right_header: Option<HeaderEntry>,
    _metric: PhantomData<fn() -> M>,
}

impl<M> WidgetView for StatisticsWidget<M>
where
    M: StatisticsMetric,
{
    type Config = StatisticsConfig;

    const ID: WidgetId = M::ID;
    const DISPLAY_NAME: &'static str = M::DISPLAY_NAME;
    const CATEGORY: WidgetCategory = WidgetCategory::Statistics;
    const DESCRIPTION: Option<&'static str> = M::DESCRIPTION;
    const ICON: Option<&'static str> = Some(M::ICON);

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: WidgetStatus,
        _reload: ReloadHandle,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError> {
        let directory: Option<Rc<dyn StatisticsDirectory>> =
            cx.services().market_participants().map(|connector| {
                Rc::new(ConnectorDirectory::new(
                    connector.clone(),
                    M::METRIC.capability(),
                )) as Rc<dyn StatisticsDirectory>
            });
        // The answer is read through the directory the composition root injects; a host
        // without one leaves the source absent and `load` reports that.
        let source = cx
            .services()
            .market_participants()
            .map(market_participants_connector::connector::MarketParticipantsConnector::market);
        Ok(Self::assemble(
            config,
            source,
            directory,
            PaneScaffold {
                status,
                config_change,
                recovery,
                now: Rc::new(unix_millis),
            },
            cx,
        ))
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        // Nothing a host without a directory installs can ever be loaded, so that is a
        // failure rather than a wait.
        if self.directory.is_none() {
            self.report_unavailable(cx);
            return;
        }
        // The market this pane follows has to be loaded before anything else happens here,
        // and the check is first on purpose: a route the user never loaded, or one a
        // shrinking commit has since taken away, leaves the pane waiting for it instead of
        // reaching the branches below, which settle the selection the user stored.
        if !self.market_ready(cx) {
            self.wait_for_market(cx);
            return;
        }
        // A pane that was waiting — nothing selected, or a scope that is in memory again —
        // carries no reading over, so what it starts is a load rather than a retry. What a
        // recovery carries over is the window the pane has accepted, and only that: a pane
        // that failed before a window arrived has nothing to keep, so starting over is what
        // stands the connecting placeholder in front of an empty chart instead of reporting
        // the window it never received as "no data".
        let retry = self.status.is_failed(cx.app()) && self.accepted;
        self.start(retry, cx);
    }

    /// The options this widget persists, asked for by the framework to write the pane back.
    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    /// A pane is content once it has accepted a window.
    ///
    /// Having a chart is not the same thing: a chart is built as soon as an instrument is
    /// chosen, and an empty chart is not content — it is a pane that has asked for something
    /// and is waiting for it. An *empty window* is the different thing, and it is content:
    /// it is what "this instrument has published nothing yet" looks like, and the chart says
    /// so itself.
    fn has_content(&self) -> bool {
        self.chart.is_some() && self.accepted
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let root = NodeBuilder::boxed()
            .id("TEST_PERPETUAL_STATISTICS")
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .size_full()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        let host = NodeBuilder::boxed()
            .id("TEST_PERPETUAL_STATISTICS_CHART_HOST")
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .min_width_0()
                    .min_height_0()
                    .relative()
                    .build(),
            )
            .build(cx.tree_mut(), root)?;

        // The canvas is the host's first child on purpose: the empty-window overlay below
        // is built after it, so the sentence is drawn over the axes rather than under them
        // (the old body appended its overlay last for the same reason).
        self.chart_host = Some(host);
        if let Some(chart) = self.chart.as_mut() {
            chart.build(cx, host)?;
        }

        // A window the venue published with no interval in it draws axes and nothing else,
        // which reads exactly like a chart that has not arrived: the overlay says which of
        // the two it is, over the plot.
        let empty = self.empty;
        let empty_hidden = self.empty;
        let overlay = NodeBuilder::boxed()
            .id("TEST_PERPETUAL_STATISTICS_EMPTY")
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
            .bind_style(move |read| StylePatch {
                visible: Some(*read.read(&empty)),
                ..StylePatch::default()
            })
            .bind_a11y(move |read| A11yPatch {
                hidden: Some(!*read.read(&empty_hidden)),
                ..A11yPatch::default()
            })
            .build(cx.tree_mut(), host)?;
        let theme = cx.theme().clone();
        EmptyState::new(theme, "StatisticsEmpty")
            .bind_text(self.empty_text)
            .build(cx.tree_mut(), overlay)?;
        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        // What the service delivered is drained before anything in this frame can replace a
        // command: an answer that arrived before the frame belongs to the command that was
        // in flight when it arrived.
        self.drain_inbox(cx);

        // The published directory generation is the one signal that says the directory
        // moved. A change can matter in either direction — the market this pane waits for
        // has been loaded, or the one it was reading has gone — and `reload` is what decides
        // which, because it re-runs the precondition; a pane that is already showing a
        // window keeps it.
        let generation = cx.services().catalog_generation;
        let generation = *cx.read(&generation);
        if generation != self.seen_generation {
            self.seen_generation = generation;
            // A failure is terminal: a directory publication does not revive a failed
            // pane — only the user's own recovery does.
            if !self.status.is_failed(cx.app()) {
                self.reload(cx);
            }
        }

        // A window's labels were written when it arrived, so a display zone or a language
        // that moved while the pane was open rewrites them here, on the frame that reads the
        // new setting — before anything is drawn from the chart.
        let zone = cx.data_time_zone();
        if let Some(chart) = self.chart.as_mut() {
            chart.set_time_zone(zone);
        }
        let translate = cx.translate().clone();
        if let Some(chart) = self.chart.as_mut() {
            chart.set_locale(&translate);
        }

        // A header control owns no `&mut self`; it asks for the transition it was used for,
        // and this is where the request becomes one. The read is taken out of the signal
        // before the write: an engine signal is not copied on read, so a write while a read
        // guard is alive would alias the value it is reading.
        let request = cx.read(&self.requests).clone();
        if let Some(request) = request {
            cx.write(&self.requests, None);
            self.apply_header_request(request, cx);
        }

        // The body is built by the framework from the answer `has_content` gives, and it
        // asks that question before this hook runs: content that first appeared here is one
        // frame away, so ask for that frame.
        if self.has_content() && self.chart_host.is_none() {
            cx.request_frame()?;
        }

        self.sync_chart(cx)?;

        // The countdown follows the schedule the state now states. Comparing first is what
        // keeps a steady frame from cancelling and re-arming the same tick.
        let deadline = self.countdown_deadline(cx.app());
        if deadline != self.countdown.deadline() {
            Countdown::update(&self.countdown, cx.app_mut(), deadline);
        }
        self.sync_header(cx)?;
        Ok(())
    }

    fn header(
        &mut self,
        group: HeaderGroup,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        // The pane may rebuild the row it calls this for: the controls this widget built
        // into the previous one are the widget's, so they go before the new ones are built.
        self.release_header(group, true, cx);
        match group {
            HeaderGroup::Left => self.build_instrument_header(parent, cx),
            HeaderGroup::Right => self.build_drawing_header(parent, cx),
        }
    }

    fn dispatch(&mut self, event: &Event, cx: &mut WidgetCx<'_>) -> bool {
        let Event::Input(input) = event else {
            return false;
        };
        let Some(chart) = self.chart.as_mut() else {
            return false;
        };
        // A pointer that is over the plot follows the reading under it; one that left says
        // so. Either way only a *reading* that moved is a change worth a frame: a pointer
        // that moved a pixel inside one interval is the same drawing.
        let moved = match input {
            InputEvent::PointerMove { position, .. }
            | InputEvent::PointerEnter { position, .. } => chart.point_at(*position),
            InputEvent::PointerLeave { .. } => chart.clear_hover(),
            _ => return false,
        };
        if moved {
            let _ = cx.request_frame();
        }
        true
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        // Dropping the bridge aborts its forwarding tasks and cancels the service.
        self.binding = None;
        // The header rows belong to the pane, which removes them: only the components'
        // signal scopes and action handlers are this widget's to release.
        self.release_header(HeaderGroup::Left, false, cx);
        self.release_header(HeaderGroup::Right, false, cx);
        // The chart's canvas node lives in the body, which the framework removes after this
        // hook; the geometry it registered is the chart's own, so it goes here.
        if let Some(mut chart) = self.chart.take() {
            chart.release(cx);
        }
        // The countdown tick is this widget's: the framework's subtree teardown does not own
        // it, so release is where it is cancelled.
        Countdown::update(&self.countdown, cx.app_mut(), None);
        self.pending.clear();
        self.chart_host = None;
        self.accepted = false;
        self.ready = None;
        self.next_refresh_at = None;
    }
}

impl<M> StatisticsWidget<M>
where
    M: StatisticsMetric,
{
    /// Assembles a pane over the collaborators a caller hands it.
    ///
    /// The source and the directory are arguments because the pane has no opinion about
    /// where its answers come from: the production constructor answers them with the
    /// injected connector, and a test with a fixture.
    fn assemble<S>(
        config: StatisticsConfig,
        source: Option<S>,
        directory: Option<Rc<dyn StatisticsDirectory>>,
        scaffold: PaneScaffold,
        cx: &mut WidgetCx<'_>,
    ) -> Self
    where
        S: StatisticsSource,
    {
        let binding = source.map(|source| WidgetBinding::spawn(cx, StatisticsService::new(source)));
        let countdown = Rc::new(Countdown::new(cx, scaffold.now));
        let generation = cx.services().catalog_generation;
        let seen_generation = *cx.read(&generation);
        Self {
            config,
            binding,
            status: scaffold.status,
            recovery: scaffold.recovery,
            config_change: scaffold.config_change,
            chart: None,
            accepted: false,
            ready: None,
            next_refresh_at: None,
            countdown,
            refresh_pending: false,
            pending: Pending::new(),
            units: ChartUnits::default(),
            directory,
            seen_generation,
            awaiting_market: false,
            chart_host: None,
            empty: cx.signal(false),
            empty_text: cx.signal(SmolStr::default()),
            requests: cx.signal(None),
            left_header: None,
            right_header: None,
            _metric: PhantomData,
        }
    }

    /// Builds a pane over a source and a directory a test drives.
    ///
    /// The framework's headless host cannot build a market-participants connector, so the
    /// seam a test answers through is here: the same assembly the production constructor
    /// runs, with the directory and the source handed in.
    #[cfg(test)]
    pub(crate) fn with_source<S>(
        config: StatisticsConfig,
        source: S,
        directory: Option<Rc<dyn StatisticsDirectory>>,
        scaffold: PaneScaffold,
        cx: &mut WidgetCx<'_>,
    ) -> Self
    where
        S: StatisticsSource,
    {
        Self::assemble(config, Some(source), directory, scaffold, cx)
    }

    /// The scope this pane would accept a message from.
    ///
    /// Absent until an instrument is chosen: nothing is being polled, so nothing is in
    /// scope.
    pub(crate) fn current_key(&self) -> Option<StatisticsStreamKey> {
        self.config.selection.as_ref().map(|selection| {
            StatisticsStreamKey::new(M::METRIC, selection.listing.clone(), self.config.timeframe)
        })
    }

    /// Re-reads the directory after it published a new generation (AGENTS §8.3).
    ///
    /// A publication says the catalogue behind a window moved, not that the readings in it
    /// did: the window on screen is the same listing over the same interval, and the stream
    /// that is filling it resolves the listing against the catalogue on every fetch, so its
    /// next fetch is the published generation's. Replacing that stream is what a reload
    /// here must *not* do: the service numbers the windows of one stream, so a replacement
    /// starts again at its first revision, and the window on screen would be counted as one
    /// the pane had already overtaken — the chart would freeze on the old window until the
    /// new stream's numbering caught up.
    ///
    /// What a publication does re-run is the pane's gate and the answers it takes from the
    /// directory: the route's question about the stored listing, and the units its labels
    /// are written in. Content is dropped here only when the directory says the pane can no
    /// longer read it — the market it follows is not loaded (the neutral notice), or the
    /// route no longer publishes the listing (nothing chosen). A fetch that fails after the
    /// publication is the ordinary failure path, which keeps the chart under the banner.
    fn reload(&mut self, cx: &mut WidgetCx<'_>) {
        if self.directory.is_none() {
            self.report_unavailable(cx);
            return;
        }
        if !self.market_ready(cx) {
            self.wait_for_market(cx);
            return;
        }
        // A pane with no window of its own has nothing to keep: what it was waiting for is
        // what the publication may have settled, so it starts over the way a load does.
        if !self.has_content() {
            self.start(false, cx);
            return;
        }
        let Some(selection) = self.config.selection.clone() else {
            return;
        };
        let support = self
            .directory
            .as_ref()
            .map(|directory| directory.listing_support(&selection.listing))
            .unwrap_or(ListingSupport::Unknown);
        self.settle_selection(&selection, support, cx);
    }

    /// Whether this pane has something to fetch.
    fn has_scope(&self) -> bool {
        !self.awaiting_market && self.current_key().is_some()
    }

    /// Whether a pane would enable its drawing controls, which is what a test asserts
    /// instead of reaching into a control it cannot read back.
    #[cfg(test)]
    pub(crate) fn has_scope_for_test(&self) -> bool {
        self.has_scope()
    }

    /// The deadline this pane counts down to, if it has one to count down to.
    ///
    /// A countdown belongs to a pane that is *ready*: it counts down the time left until the
    /// next automatic fetch of the window that is on screen. While a fetch is in flight
    /// there is nothing to count down to, and a pane with no scope has no deadline either.
    fn countdown_deadline(&self, app: &App) -> Option<i64> {
        let status = self.status.state(app);
        if status.is_busy() || status.is_failed() {
            return None;
        }
        self.next_refresh_at
    }

    /// Whether the refresh control has nothing to do.
    ///
    /// It is live exactly when the countdown is: a pane that is connecting, fetching or
    /// failed has nothing to bring forward, and so has one that is waiting for its market.
    /// The pending half is not decoration: the service refuses a second refresh while one is
    /// in flight and the framework shows a refusal as a failure.
    pub(crate) fn refresh_is_disabled(&self, app: &App) -> bool {
        self.awaiting_market || self.refresh_pending || self.countdown_deadline(app).is_none()
    }

    /// Reports that the host installed no directory root, which this pane cannot read
    /// without.
    fn report_unavailable(&mut self, cx: &mut WidgetCx<'_>) {
        let recovery = self.recovery.clone();
        self.status.report(
            cx.app_mut(),
            WidgetState::failed(Arc::new(StatisticsFailure::Unavailable), recovery),
        );
    }

    /// Whether what this pane needs is in memory: at least one market that answers with
    /// these statistics, and the route of the stored selection among them.
    fn market_ready(&self, _cx: &WidgetCx<'_>) -> bool {
        let Some(directory) = self.directory.as_ref() else {
            return false;
        };
        if !directory.any_market_loaded() {
            return false;
        }
        let Some(selection) = self.config.selection.as_ref() else {
            return true;
        };
        directory.is_loaded(&selection.listing.route)
    }

    /// Builds the chart of the configured listing, or drops the one that was there.
    ///
    /// The labels come from the listing the program answered with, and a listing it could
    /// not answer for is drawn with none: the series are still worth plotting, and the axes
    /// fall back to naming nothing rather than naming the wrong asset.
    fn rebuild_chart(&mut self, cx: &mut WidgetCx<'_>) {
        let zone = cx.data_time_zone();
        let translate = cx.translate().clone();
        let chart = self.config.selection.as_ref().and_then(|selection| {
            let directory = self.directory.as_ref()?;
            let scopes = directory.loaded_scopes();
            if !offers_route(&scopes, &selection.listing.route, M::METRIC.capability()) {
                return None;
            }
            ChartDispatch::for_metric(
                &translate,
                self.units.base.as_str(),
                self.units.quote.as_str(),
                zone,
                M::METRIC,
            )
        });
        if let Some(mut previous) = self.chart.take() {
            previous.release(cx);
        }
        self.chart = chart;
    }

    /// Settles the stored scope against the directory, and starts polling it.
    ///
    /// The question is **advisory**: it only ever takes the scope away, and only when the
    /// program answered that it does not serve the listing. A question that could not be put
    /// leaves the scope alone, and the poll reports the failure that is really there.
    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) {
        // The pane reached here only past its own gate, so the market it was waiting for is
        // in memory again and messages belong to it once more.
        self.awaiting_market = false;
        self.ready = None;
        self.next_refresh_at = None;
        // The commands that were in flight belong to the generation this call is replacing,
        // so their answers are not something to wait for any more.
        self.pending.clear();
        self.refresh_pending = false;
        // An interval this build does not offer is settled before anything is asked for it.
        if !TIMEFRAMES.contains(&self.config.timeframe) {
            self.config.timeframe = StatisticsConfig::default().timeframe;
            log::warn!(
                "the interval stored for this pane is not one this build offers; it fell back \
                 to {:?}",
                self.config.timeframe
            );
            self.config_change.publish(cx.app_mut());
        }
        let Some(selection) = self.config.selection.clone() else {
            self.nothing_chosen(cx);
            return;
        };
        // A retry keeps the window the user is still reading; an explicit load rebuilds the
        // chart for the scope it now follows.
        if !retry {
            self.rebuild_chart(cx);
        }
        self.accepted = retry && self.chart.is_some();
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        let support = self
            .directory
            .as_ref()
            .map(|directory| directory.listing_support(&selection.listing))
            .unwrap_or(ListingSupport::Unknown);
        if !self.settle_selection(&selection, support, cx) {
            return;
        }
        self.poll(retry, cx);
        let _ = cx.request_frame();
    }

    /// Applies the directory's answer about the stored listing.
    ///
    /// Returns whether this answer still describes the scope the pane follows, which is what
    /// decides whether the poll for it starts.
    fn settle_selection(
        &mut self,
        asked: &InstrumentSelection,
        support: ListingSupport,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        if self.config.selection.as_ref() != Some(asked) {
            return false;
        }
        match support {
            ListingSupport::Supported(rules) => {
                // The labels name the assets of the listing the program answered with, so a
                // chart that has not drawn a window yet is rebuilt with them. A chart that
                // is already showing one keeps it: a retry is not a reason to blank a screen
                // of data away.
                let units = self
                    .directory
                    .as_ref()
                    .map(|directory| directory.units(&rules, cx.translate()))
                    .unwrap_or_default();
                if self.units != units {
                    self.units = units;
                    if !self.accepted {
                        self.rebuild_chart(cx);
                    }
                }
                true
            }
            ListingSupport::Unsupported => {
                log::warn!(
                    "`{}` is no longer published by {}; the pane went back to having nothing \
                     chosen",
                    asked.listing.listing_id.as_str(),
                    asked.listing.route
                );
                self.config.selection = None;
                self.config_change.publish(cx.app_mut());
                self.nothing_chosen(cx);
                true
            }
            ListingSupport::Unknown => true,
        }
    }

    /// Asks the service for the window of the scope the config now holds.
    fn poll(&mut self, retry: bool, cx: &mut WidgetCx<'_>) {
        let Some(selection) = self.config.selection.clone() else {
            self.nothing_chosen(cx);
            return;
        };
        let request = StatisticsRequest::from_scope(
            M::METRIC,
            selection.listing,
            self.config.timeframe,
            retry,
        );
        let Some(binding) = self.binding.as_mut() else {
            self.report_unavailable(cx);
            return;
        };
        let request_id = binding.call(cx, request);
        self.pending.track(request_id, PendingCommand::Load);
        let _ = cx.request_frame();
    }

    /// Says what a pane with nothing to follow is waiting for.
    fn nothing_chosen(&mut self, cx: &mut WidgetCx<'_>) {
        if let Some(mut chart) = self.chart.take() {
            chart.release(cx);
        }
        self.accepted = false;
        self.ready = None;
        self.next_refresh_at = None;
        self.pending.clear();
        self.refresh_pending = false;
        self.report_notice(cx);
        let _ = cx.request_frame();
    }

    /// Consumes what the binding delivered, in arrival order.
    fn drain_inbox(&mut self, cx: &mut WidgetCx<'_>) {
        let messages = self
            .binding
            .as_ref()
            .map(WidgetBinding::take)
            .unwrap_or_default();
        for message in messages {
            match message {
                Inbound::Reliable { key, message } => self.apply_reliable(key, message, cx),
                Inbound::Latest { key, message } => self.apply_latest(key, message, cx),
                Inbound::Rpc { request_id, result } => self.apply_rpc(request_id, result, cx),
            }
        }
    }

    /// Applies one RPC reply to the command whose identity it carries.
    ///
    /// An identity this pane does not hold is a reply the replacement overtook: it is
    /// dropped and counted rather than applied to the command now in flight.
    pub(crate) fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<StatisticsProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        // A failure is terminal, and every entry point has to say so: the reply to a command
        // the recovery replaced would otherwise report the same failure again over the
        // banner the user is reading.
        if self.status.is_failed(cx.app()) {
            return;
        }
        let Some(command) = self.pending.take(request_id) else {
            log::debug!(
                "the statistics pane ignored reply {request_id}: it is not a command this pane \
                 is waiting for"
            );
            return;
        };
        match command {
            PendingCommand::Load => match result {
                Ok(StatisticsResponse::StreamStarted | StatisticsResponse::RefreshQueued) => {}
                Err(error) => {
                    let recovery = self.recovery.clone();
                    self.status.report(
                        cx.app_mut(),
                        WidgetState::failed(Arc::new(StatisticsFailure::Rpc(error)), recovery),
                    );
                }
            },
            PendingCommand::Refresh => {
                // An accepted command stays pending until the round it started is reported:
                // the acknowledgement and the round are two different messages, and the gate
                // the service holds spans both.
                if let Err(error) = result {
                    self.refresh_pending = false;
                    let recovery = self.recovery.clone();
                    self.status.report(
                        cx.app_mut(),
                        WidgetState::failed(Arc::new(StatisticsFailure::Rpc(error)), recovery),
                    );
                }
            }
        }
    }

    /// Applies a status the stream reported.
    pub(crate) fn apply_reliable(
        &mut self,
        key: StatisticsStreamKey,
        message: Arc<StatisticsStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx.app())
            || self.awaiting_market
        {
            return;
        }
        let StatisticsStreamMessage::Status(status) = message.as_ref() else {
            return;
        };
        self.next_refresh_at = status.next_refresh_at();
        match status {
            StatisticsStatus::Connecting => {
                self.status.report(cx.app_mut(), WidgetState::Connecting);
            }
            StatisticsStatus::Refreshing { .. } => {
                self.refresh_pending = false;
                self.status.report(cx.app_mut(), WidgetState::Loading);
            }
            StatisticsStatus::Ready { revision, .. } => {
                self.refresh_pending = false;
                self.ready = Some(*revision);
                // The reliable channel and the latest one have no order between them, so
                // this can arrive before the window it describes: claiming readiness now
                // would be claiming a chart the pane does not have.
                if self.chart_revision().is_some_and(|held| held >= *revision) {
                    self.status.report(cx.app_mut(), WidgetState::Ready);
                } else {
                    self.status.report(cx.app_mut(), WidgetState::Loading);
                }
            }
            StatisticsStatus::Failed { error } => {
                self.refresh_pending = false;
                let failure = StatisticsFailure::Fetch(error.clone());
                // A read the directory answered "not loaded" to is a market that is no longer
                // loaded rather than a failure: the pane goes back to its neutral notice, and
                // the directory's next publication is what brings it out of it.
                if is_waiting_for_a_catalog(&failure) {
                    self.wait_for_market(cx);
                    return;
                }
                let recovery = self.recovery.clone();
                self.status.report(
                    cx.app_mut(),
                    WidgetState::failed(Arc::new(failure), recovery),
                );
            }
        }
    }

    /// Applies a window the stream published.
    pub(crate) fn apply_latest(
        &mut self,
        key: StatisticsStreamKey,
        message: Arc<StatisticsStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx.app())
            || self.awaiting_market
        {
            return;
        }
        let StatisticsStreamMessage::Snapshot(snapshot) = message.as_ref() else {
            return;
        };
        // A window the pane has already overtaken would only make the chart jump backwards:
        // within one generation the service's revision only moves forwards, so a smaller one
        // can only be a window a status has replaced.
        if self
            .chart_revision()
            .is_some_and(|held| snapshot.revision < held)
        {
            return;
        }
        let Some(chart) = self.chart.as_mut() else {
            return;
        };
        chart.set_snapshot(message.as_ref());
        self.accepted = true;
        // Readiness is published at the protocol's own boundary: a complete window is not the
        // same thing as the window a status said was complete, and this is where the two
        // meet.
        if self
            .ready
            .is_some_and(|ready| snapshot.revision >= ready && chart.revision() >= ready)
        {
            self.status.report(cx.app_mut(), WidgetState::Ready);
        }
        let _ = cx.request_frame();
    }

    /// The revision of the window the chart is showing.
    pub(crate) fn chart_revision(&self) -> Option<u64> {
        self.chart.as_ref().map(ChartDispatch::revision)
    }

    /// Reports what this pane is waiting for: the market it follows, or the instrument it has
    /// nothing chosen for.
    fn report_notice(&self, cx: &mut WidgetCx<'_>) {
        if self.awaiting_market {
            let venues = self
                .config
                .selection
                .as_ref()
                .map(|selection| vec![selection.listing.route.venue])
                .unwrap_or_else(Self::listed_venues);
            self.status.report(
                cx.app_mut(),
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues,
                    supported: Self::listed_venues(),
                }),
            );
        } else if self.config.selection.is_none() {
            self.status.report(
                cx.app_mut(),
                WidgetState::waiting(Notice::message(
                    "widget.perpetual_statistics.select_instrument_prompt",
                )),
            );
        }
    }

    /// The venues the waiting notice lists: every venue whose loader can answer with these
    /// statistics.
    pub(crate) fn listed_venues() -> Vec<market_participants_model::identifier::VenueId> {
        market_venues(M::METRIC.capability())
    }

    /// Reports the neutral notice for the market this pane follows, and drops the chart it
    /// was drawing.
    fn wait_for_market(&mut self, cx: &mut WidgetCx<'_>) {
        self.awaiting_market = true;
        if let Some(mut chart) = self.chart.take() {
            chart.release(cx);
        }
        self.accepted = false;
        self.ready = None;
        self.next_refresh_at = None;
        self.pending.clear();
        self.refresh_pending = false;
        self.report_notice(cx);
        let _ = cx.request_frame();
    }

    /// Rebuilds the chart's canvas from what the pane now shows.
    fn sync_chart(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if let (Some(host), Some(chart)) = (self.chart_host, self.chart.as_mut()) {
            if chart.node().is_none() {
                chart.build(cx, host)?;
                // The node has no box until the frame that lays it out, so this frame cannot
                // draw it. Ask for that frame here rather than relying on another write in
                // this hook to schedule one.
                cx.request_frame()?;
            }
            chart.sync(cx)?;
        }
        let empty = self.accepted
            && self
                .chart
                .as_ref()
                .is_some_and(|chart| chart.point_count() == 0);
        cx.write(&self.empty, empty);
        // The sentence is the catalogue's, so the bound slot is rewritten in the language
        // showing now — a language switch is one write to this signal, not a rebuild.
        let text = t_smol!(cx.translate(), "common.no_data");
        cx.write(&self.empty_text, text);
        Ok(())
    }

    /// Applies what a header control reported.
    fn apply_header_request(&mut self, request: HeaderRequest, cx: &mut WidgetCx<'_>) {
        match request {
            HeaderRequest::Refresh => self.refresh(cx),
            HeaderRequest::Selection(selection) => {
                self.select_selection(selection, cx);
            }
            HeaderRequest::Timeframe(timeframe) => self.select_timeframe(timeframe, cx),
        }
    }

    /// The filters the picker is built with.
    ///
    /// The loaded venues are read here, where the control is built, rather than kept: a
    /// scope loaded after this pane started reading is a scope the user may select, and one
    /// a shrinking commit withdrew is not.
    fn instrument_filters(&self) -> InstrumentFilters {
        InstrumentFilters {
            // These series exist for perpetual markets only, so the picker offers nothing
            // else: a spot listing would be a scope no endpoint here can answer.
            kind_filter: InstrumentKindFilter::Perpetual,
            status_filter: InstrumentStatusFilter::Any,
            required_capability: M::METRIC.capability(),
            entry_predicate: None,
            routes: self.directory.as_ref().map_or_else(
                || Arc::from(Vec::<LoadedScope>::new()),
                |directory| directory.loaded_scopes(),
            ),
        }
    }

    /// The selector host the instrument control opens its dialog through.
    ///
    /// The host owns the services rather than borrowing them, so it takes the shared handle
    /// the composition root handed over — the same aggregate the widget works over, not a
    /// second one assembled from its fields.
    fn selector_host(&self, cx: &WidgetCx<'_>) -> SelectorHost {
        SelectorHost::new(Rc::clone(cx.services_rc()))
    }

    /// Builds the instrument control into the header's left group.
    fn build_instrument_header(
        &mut self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let language = *cx.read(&cx.language());
        let row = NodeBuilder::boxed()
            .id("TEST_PERPETUAL_STATISTICS_HEADER_INSTRUMENT")
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .gap(4.0)
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        let host = self.selector_host(cx);
        let filters = self.instrument_filters();
        let requests = self.requests;
        let button = InstrumentButton::new(
            cx.unique_key(INSTRUMENT_KEY_SUFFIX),
            self.config.selection.clone(),
            filters,
            "widget.perpetual_statistics.select_instrument",
            move |selection: InstrumentSelection, app: &mut App, window: WindowId| {
                app.write(&requests, Some(HeaderRequest::Selection(selection)));
                let _ = app.request_repaint(window);
            },
        );
        let window = cx.window();
        let control = button.build(&host, cx.app_mut(), window, row)?;
        self.left_header = Some(HeaderEntry {
            root: row,
            parent,
            language,
            selection: self.config.selection.clone(),
            timeframe: self.config.timeframe,
            generation: self.seen_generation,
            instrument: Some(control),
            refresh: None,
            select: None,
        });
        Ok(())
    }

    /// Builds the interval picker and the refresh into the header's right group.
    fn build_drawing_header(
        &mut self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let language = *cx.read(&cx.language());
        let theme = cx.theme().clone();
        let row = NodeBuilder::boxed()
            .id("TEST_PERPETUAL_STATISTICS_HEADER_DRAWING")
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .gap(4.0)
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        let refresh = self.build_refresh_button(cx, row)?;
        let countdown = self.countdown.label;
        NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(UnifiedSize::Small.text_size()),
        )
        .id("TEST_PERPETUAL_STATISTICS_COUNTDOWN")
        .bind_text(move |read| {
            let label: Option<SmolStr> = read.read(&countdown).clone();
            label.unwrap_or_default()
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(theme.role(read, base_ui::theme::role::Role::FgDefault)),
            ..StylePatch::default()
        })
        .build(cx.tree_mut(), row)?;
        let select = self.build_timeframe_select(cx, row)?;
        self.right_header = Some(HeaderEntry {
            root: row,
            parent,
            language,
            selection: self.config.selection.clone(),
            timeframe: self.config.timeframe,
            generation: self.seen_generation,
            instrument: None,
            refresh: Some(refresh),
            select: Some(select),
        });
        Ok(())
    }

    /// Builds the refresh control, in the state the pane is in now.
    fn build_refresh_button(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<Button, EngineError> {
        let requests = self.requests;
        let label = t_smol!(cx.translate(), "widget.perpetual_statistics.refresh_now");
        let disabled = self.refresh_is_disabled(cx.app());
        let key = cx.unique_key(REFRESH_KEY_SUFFIX);
        let theme = cx.theme().clone();
        let window = cx.window();
        let button = Button::new(cx.app_mut(), theme, key)
            .ghost()
            .small()
            .icon(IconKind::ArrowsClockwise)
            .disabled(disabled)
            .a11y_label(label.clone())
            .tooltip_text(label)
            .on_click(move |ctx| {
                // A button handler owns no widget: it asks the next sync for the refresh.
                ctx.update(&requests, |request| *request = Some(HeaderRequest::Refresh));
                let window = ctx.window();
                let _ = ctx.app_mut().request_repaint(window);
            });
        button.build(cx.app_mut(), window, parent)?;
        Ok(button)
    }

    /// Builds the interval picker, in the state the pane is in now.
    fn build_timeframe_select(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<Select, EngineError> {
        let requests = self.requests;
        let label = t_smol!(cx.translate(), "widget.perpetual_statistics.interval");
        let selected = self.config.timeframe;
        let options = SelectOptions::default()
            .size(UnifiedSize::Small)
            .label(label)
            .placeholder(SmolStr::new_static(selected.label()));
        let items = TIMEFRAMES
            .iter()
            .map(|timeframe| {
                let label = SmolStr::new_static(timeframe.label());
                SelectItem::new(label.clone(), label)
            })
            .collect::<Vec<_>>();
        let theme = cx.theme().clone();
        let layer = base_ui::kit::layer::Layer::clone(cx.layer());
        let router = base_ui::kit::overlay::AnimationRouter::clone(cx.animation());
        let window = cx.window();
        let key = cx.unique_key(TIMEFRAME_KEY_SUFFIX);
        let select = Select::new(cx.app_mut(), theme, layer, router, key, options).on_change(
            move |app, chosen| {
                // The picker identifies an interval by the label it shows, which is the only
                // name the menu entries carry.
                let Some(timeframe) = chosen.first().and_then(|id| {
                    TIMEFRAMES
                        .iter()
                        .copied()
                        .find(|it| it.label() == id.as_str())
                }) else {
                    return;
                };
                app.write(&requests, Some(HeaderRequest::Timeframe(timeframe)));
                let _ = app.request_repaint(window);
            },
        );
        select
            .set_items(cx.app_mut(), items)
            .unwrap_or_else(|error| {
                log::warn!("the statistics interval picker could not set its items: {error}");
            });
        select.set_chosen(cx.app_mut(), [selected.label()], false);
        let _ = select.set_disabled(cx.app_mut(), !self.has_scope());
        select.build(cx.app_mut(), window, parent)?;
        Ok(select)
    }

    /// Keeps the header controls in step with what they should state.
    fn sync_header(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.sync_instrument_header(cx)?;
        self.sync_drawing_header(cx)
    }

    /// Rebuilds the instrument control when its copy moved, and drops it when its row is gone.
    fn sync_instrument_header(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(header) = self.left_header.as_ref() else {
            return Ok(());
        };
        // The pane rebuilds the row it calls `header` for, so the row this widget built into
        // may already be gone — and it took the control with it. Touching a stale node would
        // fail the whole synchronization, so a row that is missing is a release and nothing
        // else.
        if cx.tree().get(header.root).is_err() {
            self.release_header(HeaderGroup::Left, false, cx);
            return Ok(());
        }
        let moved = header.language != *cx.read(&cx.language())
            || header.selection.as_ref() != self.config.selection.as_ref()
            || header.generation != self.seen_generation;
        if !moved {
            return Ok(());
        }
        let parent = header.parent;
        self.release_header(HeaderGroup::Left, true, cx);
        self.build_instrument_header(parent, cx)
    }

    /// Rebuilds the drawing controls when their copy moved, and keeps their state current.
    fn sync_drawing_header(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(header) = self.right_header.as_ref() else {
            return Ok(());
        };
        if cx.tree().get(header.root).is_err() {
            self.release_header(HeaderGroup::Right, false, cx);
            return Ok(());
        }
        let moved = header.language != *cx.read(&cx.language())
            || header.timeframe != self.config.timeframe
            || header.generation != self.seen_generation;
        if moved {
            let parent = header.parent;
            self.release_header(HeaderGroup::Right, true, cx);
            self.build_drawing_header(parent, cx)?;
        }
        let disabled = !self.has_scope();
        let refresh_disabled = self.refresh_is_disabled(cx.app());
        let timeframe = SmolStr::new_static(self.config.timeframe.label());
        let Some(header) = self.right_header.as_ref() else {
            return Ok(());
        };
        if let Some(button) = header.refresh.as_ref() {
            button.sync(cx.app_mut())?;
            let _ = button.set_disabled(cx.app_mut(), refresh_disabled);
        }
        if let Some(select) = header.select.as_ref() {
            let stated = select.chosen(cx.app());
            if stated.len() != 1 || stated.first() != Some(&timeframe) {
                select.set_chosen(cx.app_mut(), [timeframe.as_str()], false);
            }
            let _ = select.set_disabled(cx.app_mut(), disabled);
            select.sync(cx.app_mut())?;
        }
        Ok(())
    }

    /// Takes a header group's controls down: the widget's own row, and the components' scopes
    /// and handlers.
    ///
    /// `remove_node` is true when the pane's group node is still there and the widget is
    /// replacing its own row in it; a row the pane already took down needs no node removal
    /// (`StaleNode`). The components outlive their nodes, so they are released in either case.
    fn release_header(&mut self, group: HeaderGroup, remove_node: bool, cx: &mut WidgetCx<'_>) {
        let entry = match group {
            HeaderGroup::Left => self.left_header.take(),
            HeaderGroup::Right => self.right_header.take(),
        };
        let Some(header) = entry else {
            return;
        };
        if remove_node && cx.tree().get(header.root).is_ok() {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, header.root);
        }
        if let Some(control) = header.instrument {
            let _ = control.release(cx.app_mut());
        }
        if let Some(button) = header.refresh {
            let _ = button.release(cx.app_mut());
        }
        if let Some(select) = header.select {
            let _ = select.release(cx.app_mut());
        }
    }

    /// Follows another listing.
    ///
    /// Returns whether the selection was accepted. A selection names the exact listing, so
    /// there is nothing to check about which market it belongs to: the picker offers
    /// perpetual listings of routes that publish these statistics, and a stored or linked
    /// selection that is not one is settled in `load`.
    pub(crate) fn select_selection(
        &mut self,
        selection: InstrumentSelection,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        if self.config.selection.as_ref() == Some(&selection) && !self.status.is_failed(cx.app()) {
            return false;
        }
        self.config.selection = Some(selection);
        self.config_change.publish(cx.app_mut());
        self.start(false, cx);
        true
    }

    pub(crate) fn select_timeframe(
        &mut self,
        timeframe: StatisticsTimeFrame,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.config.timeframe == timeframe {
            return;
        }
        self.config.timeframe = timeframe;
        self.config_change.publish(cx.app_mut());
        self.start(false, cx);
    }

    /// Fetches the current scope without waiting for the next interval.
    ///
    /// Only a ready pane gets here, so the command it sends is one the loop answers: a
    /// failure is recovered through the framework's own control, above the chart.
    pub(crate) fn refresh(&mut self, cx: &mut WidgetCx<'_>) {
        if self.refresh_is_disabled(cx.app()) {
            return;
        }
        self.refresh_pending = true;
        let Some(binding) = self.binding.as_mut() else {
            self.refresh_pending = false;
            self.report_unavailable(cx);
            return;
        };
        let request_id = binding.call(cx, StatisticsRequest::Refresh);
        self.pending.track(request_id, PendingCommand::Refresh);
        let _ = cx.request_frame();
    }

    /// The countdown label the header draws, as a test reads it back.
    #[cfg(test)]
    pub(crate) fn countdown_label(&self, app: &App) -> Option<SmolStr> {
        self.countdown.label(app)
    }

    /// Whether the countdown is waiting for a tick to fire, as a test reads the cancellation
    /// of one back: a cancelled countdown is a pane that is woken by nothing.
    #[cfg(test)]
    pub(crate) fn countdown_is_armed_for_test(&self) -> bool {
        self.countdown.is_armed()
    }

    /// Whether the pane reports a pending refresh, as a test reads it back.
    #[cfg(test)]
    pub(crate) fn refresh_pending_for_test(&self) -> bool {
        self.refresh_pending
    }

    /// How many replies this pane discarded as answers it was no longer waiting for.
    #[cfg(test)]
    pub(crate) fn dropped_answers_for_test(&self) -> usize {
        self.pending.dropped()
    }

    /// Whether the pane is waiting for no reply at all.
    #[cfg(test)]
    pub(crate) fn pending_is_empty_for_test(&self) -> bool {
        self.pending.is_empty()
    }

    /// The action identities the header's right group registered, as a test reads the
    /// per-instance keys back.
    #[cfg(test)]
    pub(crate) fn header_actions_for_test(&self) -> Vec<ui_core::event::Action> {
        let mut actions = Vec::new();
        if let Some(header) = self.right_header.as_ref()
            && let Some(button) = header.refresh.as_ref()
            && let Some(action) = button.action_identity()
        {
            actions.push(action);
        }
        actions
    }

    /// The chart, as a test drives its hover and canvas.
    #[cfg(test)]
    pub(crate) fn chart_for_test(&self) -> Option<&ChartDispatch> {
        self.chart.as_ref()
    }

    /// Whether the pane is waiting for its market, as a test reads it back.
    #[cfg(test)]
    pub(crate) fn awaiting_market_for_test(&self) -> bool {
        self.awaiting_market
    }

    /// The interval this pane follows, as a test reads it back.
    #[cfg(test)]
    pub(crate) fn timeframe_for_test(&self) -> StatisticsTimeFrame {
        self.config.timeframe
    }
}

/// The label beside the refresh button for `deadline` at `now`.
///
/// A deadline that has already passed counts as `00:00` rather than as a negative wait,
/// which is the same rule [`format_countdown`] applies: the pane never shows a countdown
/// that has run out as anything but zero.
fn countdown_label(deadline: Option<i64>, now: i64) -> Option<SmolStr> {
    deadline.map(|deadline| format_countdown(deadline.saturating_sub(now)))
}

#[cfg(test)]
impl Countdown {
    /// The label this countdown's node is drawing.
    fn label(&self, app: &App) -> Option<SmolStr> {
        app.read(&self.label).clone()
    }

    /// Whether a tick is waiting to fire.
    ///
    /// The id is what a deadline change and `release` cancel, so holding one is exactly the
    /// "a tick will wake this pane" the cancellation tests assert against.
    fn is_armed(&self) -> bool {
        self.timer.borrow().is_some()
    }
}

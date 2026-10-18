//! How every perpetual-statistics pane behaves.
//!
//! The five widgets differ only in which series they plot, so everything else is
//! here: the scope, the instrument picker, the interval, the countdown, the manual
//! refresh, and the body.
//!
//! # What this pane is
//!
//! These series are a bounded history rather than a stream, so "live" means poll:
//! the service fetches the most recent window once a minute, and the pane counts
//! down to the next fetch so a number that has not moved can be told apart from a
//! pane that has stopped. A manual refresh resets that schedule rather than
//! running alongside it.
//!
//! # What it does with a failure
//!
//! A failed fetch is a failure the user sees: the pane reports it through the
//! framework, which puts a banner over the chart that is still on screen. The
//! service stops its schedule at that point, so the banner's recovery is what
//! starts it again — a venue that is briefly unreachable does not turn into a
//! request every minute until someone notices.

use std::{marker::PhantomData, sync::Arc, time::Duration};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    button::Button,
    empty_state::EmptyState,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    menu::{MenuItem, MenuItemAction},
    select::Select,
    size::UnifiedSizable,
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, ElementId, Entity, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Task, Window, base::Disableable, div,
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
    market::timeframe::StatisticsTimeFrame,
    venue::loaded::VenuesUpdated,
};
use market_participants_model::{
    asset::AssetId,
    identifier::VenueId,
    instrument::InstrumentKind,
    listing::{route::VenueListingKey, rules::TradingRules},
    market::{selection::InstrumentSelection, units::QuantityUnit},
    routing::CapabilityId,
};
use smol_str::SmolStr;
use widget::{
    binding::{WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    error::{LocalizedError, market_is_catalog_not_loaded},
    identity::{WidgetId, WidgetInstanceId},
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    util::{format_countdown, unix_millis},
    view::{WidgetHeader, WidgetView},
};
use widget_tokio_runtime::global;

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
/// It is not a fetch: it only re-checks the clock behind the countdown. A quarter
/// of a second is faster than the label's own resolution and cheaper than a frame.
pub(crate) const COUNTDOWN_TICK: Duration = Duration::from_millis(250);

/// The intervals the picker offers, and the only ones a pane can poll.
///
/// A stored interval outside this list is one this build cannot fetch: the picker cannot
/// draw it and the request would name a window the service does not serve, so the pane
/// puts it back to the default instead.
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

/// The series one widget plots, and the wording that is its own.
///
/// This trait is the whole of what separates the five widgets: an id that is
/// persisted, a name, and the series that answers. Both are constants, which is
/// why a widget is a type rather than a value.
pub trait StatisticsMetric: Sized + 'static {
    /// The persistent identity of this pane. It is stored, so it cannot change.
    const ID: WidgetId;
    /// The message key of the widget's name.
    const DISPLAY_NAME: &'static str;
    const DESCRIPTION: &'static str;
    /// The asset path of the picture the widget gallery draws for this pane.
    const ICON: &'static str;
    /// Which series this pane plots.
    const METRIC: MetricKind;
}

/// Which venue, instrument and interval a pane follows.
///
/// The scope is optional because a pane exists before the user has chosen
/// anything: it then asks for an instrument rather than plotting a default one
/// nobody asked for. Another series, another venue or another interval is another
/// chart, so all three are part of the scope and all three replace what is on
/// screen.
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
    /// The first persisted schema version, and the only one: the scope is an
    /// optional listing and the interval is one of [`TIMEFRAMES`].
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// What the program knows about a listing this pane is configured to follow.
///
/// The three are kept apart because only one of them is an answer: a question
/// that could not be put proves nothing about the listing.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ListingSupport {
    /// The catalog holds the listing, the route still serves these statistics,
    /// and the listing is a perpetual — with the rules its units are read from.
    Supported(Box<TradingRules>),
    /// The program answered that it does not serve this listing: the route left
    /// the registry, the route publishes no perpetual statistics, the catalog
    /// does not hold the listing, or the listing is not a perpetual contract.
    Unsupported,
    /// The question could not be answered.
    Unknown,
}

/// Asks the program whether it still serves `listing`, and in which units.
///
/// The route registry answers first because it is the only source of what a route
/// publishes, and the catalog answers for the listing itself. These endpoints
/// exist for perpetual contracts only, so a listing of any other product is an
/// unsupported one here — the provider refuses it at the capability boundary, and
/// this is the same refusal made before a window is asked for. A catalog failure
/// is reported as the unknown answer it is: a venue that did not reply is not a
/// venue that delisted the contract.
fn listing_support(
    catalog: &Catalog,
    routes: &[LoadedScope],
    listing: &VenueListingKey,
    capability: CapabilityId,
) -> ListingSupport {
    if !offers_route(routes, &listing.route, capability) {
        return ListingSupport::Unsupported;
    }
    match catalog.resolve_listing(listing) {
        Ok(resolved)
            if resolved.instrument.kind() != InstrumentKind::Perpetual
                || resolved.status.is_terminal() =>
        {
            ListingSupport::Unsupported
        }
        Ok(resolved) => ListingSupport::Supported(Box::new(resolved.rules)),
        Err(CatalogError::ListingNotFound { .. } | CatalogError::RouteNotRegistered { .. }) => {
            ListingSupport::Unsupported
        }
        Err(error) => {
            log::warn!(
                "asking the catalog whether it still lists `{}` failed; the pane asks for the \
                 window anyway: {error}",
                listing.listing_id.as_str()
            );
            ListingSupport::Unknown
        }
    }
}

/// A failure a pane reports that is not already a stream error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StatisticsFailure {
    Fetch(StatisticsFetchError),
    Rpc(RpcCallError<StatisticsServiceError>),
}

impl LocalizedError for StatisticsFailure {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Fetch(error) => error.localized_message(cx),
            Self::Rpc(error) => error.localized_message(cx),
        }
    }
}

/// Whether this failure is the directory answering "this route's catalog is not in
/// memory yet" rather than reporting something wrong.
///
/// The pane's own gate asks that question before it reads anything, so this is the
/// answer changing underneath the request — a venue re-discovering its scopes is what
/// does it. It is a wait, not a failure: loading the market again is what
/// puts the catalog back. The pane's own service errors are refusals about the stream
/// it is running, so only the market read can carry the directory's answer.
fn is_waiting_for_a_catalog(failure: &StatisticsFailure) -> bool {
    matches!(
        failure,
        StatisticsFailure::Fetch(StatisticsFetchError::MarketParticipant(error))
            if market_is_catalog_not_loaded(error)
    )
}

impl LocalizedError for StatisticsFetchError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::MarketParticipant(error) => error.localized_message(cx),
            Self::Timeout(timeout) => {
                t!(cx, "error.statistics.timeout", seconds = timeout.as_secs())
            }
        }
    }
}

impl LocalizedError for StatisticsServiceError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::StreamClosed | Self::StreamBusy => {
                t!(cx, "error.statistics.stream_unavailable")
            }
        }
    }
}

/// One pane: the scope it follows, the chart it draws, and its header controls.
pub struct StatisticsWidget<M: StatisticsMetric> {
    pub config: StatisticsConfig,
    binding: WidgetBinding<StatisticsProtocol>,
    pub status: Entity<WidgetStatus>,
    pub recovery: WidgetAction,
    config_change: WidgetConfigChange,
    chart: Option<ChartDispatch>,
    /// Whether a window has been accepted since this scope began, which is what
    /// makes the chart content rather than a place waiting for it.
    pub accepted: bool,
    /// The revision of the window the pane has reported as ready, so readiness is
    /// published at the protocol's own boundary rather than per snapshot.
    pub ready: Option<u64>,
    /// When the next automatic fetch is due, as the service's own deadline.
    pub next_refresh_at: Option<i64>,
    pub countdown: Option<SharedString>,
    /// Whether a refresh command this pane sent has been answered *and its round
    /// reported*.
    ///
    /// It is set by the click and cleared when the service says what it did with the
    /// command, which is what closes the window between the two: the service holds
    /// its gate for the whole round, and the round is announced after the command is
    /// acknowledged — so a control that trusted the acknowledgement alone would be
    /// live again for the moment in between.
    refresh_pending: bool,
    /// The assets the chart's series are measured in, read from the listing's own
    /// rules once the program has answered what it lists.
    units: ChartUnits,
    directory_generation: u64,
    /// Whether the chart is waiting for its market rather than reading it.
    ///
    /// The pane keeps its stream while it waits — the alternative is cancelling the
    /// binding, which is terminal, and this pane has to be able to read the market
    /// again once the user loads it — so a window that was in flight when the route
    /// went away can still arrive. Accepting it would put a snapshot back and make the
    /// pane report content over the notice the framework is drawing, so every message
    /// entry refuses while this is set. The stream key cannot do this job: it says
    /// which scope a window belongs to, and this one belongs to the scope the notice is
    /// about.
    ///
    /// Crate-visible because the pane's tests live in `pane_tests.rs` beside this module, and
    /// what the header controls refuse input on is this flag.
    pub awaiting_market: bool,
    _countdown_task: Task<()>,
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
    const DESCRIPTION: &'static str = M::DESCRIPTION;
    const ICON: Option<&'static str> = Some(M::ICON);

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: Entity<WidgetStatus>,
        _reload: impl Fn(&mut App) + 'static,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = cx
            .global::<market_participants_connector::connector::MarketParticipantsConnector>()
            .market();
        Self::new_with_source(config, source, status, config_change, recovery, cx)
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        // The market this pane follows has to be loaded before anything else
        // happens here, and the check is first on purpose: a route the user never
        // loaded, or one a shrinking commit has since taken away, leaves the
        // pane waiting for it instead of reaching the branches below, which settle
        // the selection the user stored.
        if !self.market_ready(cx) {
            self.wait_for_market(cx);
            return;
        }
        // A pane that was waiting — nothing selected, or a scope that is in memory again —
        // carries no reading over, so what it starts is a load rather than a retry. What a
        // recovery carries over is the window the pane has accepted, and only that: a pane that
        // failed before a window arrived — the first read of a scope it has just followed — has
        // nothing to keep, so starting over is what stands the connecting placeholder in front of
        // an empty chart instead of reporting the window it never received as "no data".
        let retry = self.status.is_failed(cx) && self.accepted;
        self.start(retry, cx);
    }

    /// The options this widget persists.
    ///
    /// The framework asks for these to write the pane back, so this answers with the
    /// field itself rather than computing anything.
    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    /// A pane is content once it has accepted a window.
    ///
    /// Having a chart is not the same thing: a chart is built as soon as an
    /// instrument is chosen, and an empty chart is not content — it is a pane that
    /// has asked for something and is waiting for it. An *empty window* is the
    /// different thing, and it is content: it is what "this instrument has
    /// published nothing yet" looks like, and the chart says so itself.
    fn has_content(&self) -> bool {
        self.chart.is_some() && self.accepted
    }

    /// The scope on the left, and the interval and refresh on the right.
    ///
    /// Neither group is usable before the market participant's metadata is: the two right-hand controls
    /// refuse input while the pane waits for its market, and the instrument picker disables itself
    /// whenever the loaded venues offer it nothing to select from.
    fn header(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> WidgetHeader {
        WidgetHeader::empty()
            .left(self.instrument_button(cx))
            .right(self.drawing_controls(cx))
    }
}

impl<M> StatisticsWidget<M>
where
    M: StatisticsMetric,
{
    /// Messages are reported through the binding, which a test drives by hand: it
    /// is cancelled first so the service behind it cannot answer at the same time.
    #[cfg(test)]
    pub(crate) fn cancel_binding(&self) {
        self.binding.cancel();
    }

    /// Whether a chart exists at all, which is not the same as having content: a
    /// chart is built as soon as an instrument is chosen.
    #[cfg(test)]
    pub(crate) const fn has_chart(&self) -> bool {
        self.chart.is_some()
    }

    /// The token [`Self::start`] mints once per start, which a test reads to tell a pane
    /// that started over from one that only re-checked its precondition.
    #[cfg(test)]
    pub(crate) const fn generation_for_test(&self) -> u64 {
        self.directory_generation
    }

    /// The assets the chart is labelled in, which a test reads to tell the rules of one
    /// generation from another's.
    #[cfg(test)]
    pub(crate) const fn units_for_test(&self) -> &ChartUnits {
        &self.units
    }

    /// Puts the selection back to "nothing chosen" when the program no longer
    /// serves the listing it names.
    ///
    /// Returns whether this answer still describes the scope the pane follows, which is what
    /// decides whether the poll for it starts. The question is **advisory**, and it is only
    /// ever a reset: only an answer that *names* the listing as unsupported takes the scope
    /// away. Everything else it cannot answer leaves the scope alone — the venue may simply
    /// be quiet — and the poll that follows reports its own failure, which the user can
    /// recover from. The poll is the authoritative request: the connector resolves the listing
    /// inside it, so a catalog that could not be read is reported as the failure it is, and
    /// never as a listing the venue does not carry. A selection the user has changed since is
    /// theirs, and it started a poll of its own.
    fn settle_selection(
        &mut self,
        asked: &InstrumentSelection,
        support: ListingSupport,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.config.selection.as_ref() != Some(asked) {
            return false;
        }
        match support {
            ListingSupport::Supported(rules) => {
                // The labels name the assets of the listing the program answered
                // with, so a chart that has not drawn a window yet is rebuilt with
                // them. A chart that is already showing one keeps it: a retry is not
                // a reason to blank a screen of data away.
                let units = ChartUnits::of(&rules, cx);
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
                self.config_change.publish(cx);
                true
            }
            ListingSupport::Unknown => true,
        }
    }

    /// The box and offset the chart's own horizontal viewport has, which is what a
    /// test reads to see how a pane narrower than the chart behaves.
    #[cfg(test)]
    pub(crate) fn chart_horizontal_scroll(&self) -> Option<gpui_kit::ScrollHandle> {
        self.chart.as_ref().map(ChartDispatch::horizontal_scroll)
    }

    /// The seam a test can answer through: `new` only picks the production source.
    pub fn new_with_source<S>(
        config: StatisticsConfig,
        source: S,
        status: Entity<WidgetStatus>,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: StatisticsSource,
    {
        let service = StatisticsService::new(source);
        let binding = WidgetBinding::spawn_conditionally(
            global::handle(cx).handle(),
            service,
            cx,
            Self::apply_reliable,
            Self::apply_latest,
        );
        let countdown_task = cx.spawn(async move |this, cx| {
            loop {
                if this
                    .update(cx, |this, cx| {
                        this.tick_countdown(cx);
                    })
                    .is_err()
                {
                    return;
                }
                cx.background_executor().timer(COUNTDOWN_TICK).await;
            }
        });
        // The pane recovers from the market it waits for without polling: a venue commit is
        // the one event that can change what may be read, and a change that arrives while the
        // pane has nothing to show is a reason to load again.
        let events = cx.global::<MarketParticipantsConnector>().venues().events();
        cx.subscribe(&events, |this, _, event: &VenuesUpdated, cx| {
            this.on_venues_updated(event, cx);
        })
        .detach();
        let mut widget = Self {
            config,
            binding,
            status,
            recovery,
            config_change,
            chart: None,
            accepted: false,
            ready: None,
            next_refresh_at: None,
            countdown: None,
            refresh_pending: false,
            units: ChartUnits::default(),
            directory_generation: 0,
            awaiting_market: false,
            _countdown_task: countdown_task,
            _metric: PhantomData,
        };
        widget.rebuild_chart(cx);
        widget
    }

    /// The scope this pane would accept a message from.
    ///
    /// Absent until an instrument is chosen: nothing is being polled, so nothing
    /// is in scope.
    pub(crate) fn current_key(&self) -> Option<StatisticsStreamKey> {
        self.config.selection.as_ref().map(|selection| {
            StatisticsStreamKey::new(M::METRIC, selection.listing.clone(), self.config.timeframe)
        })
    }

    /// Whether this pane has something to fetch.
    ///
    /// Both header controls read this rather than deciding for themselves: an
    /// interval is only worth choosing when there is a window to fetch over it, so
    /// the picker and the refresh are enabled and disabled together.
    ///
    /// A pane that is waiting for its market has nothing to fetch either, however it was
    /// scoped before the market went away: the intervals it would offer are the route's, and
    /// a route whose market is not in memory cannot be asked. The wait is therefore part of
    /// having a scope rather than a second condition beside it.
    fn has_scope(&self) -> bool {
        !self.awaiting_market && self.current_key().is_some()
    }

    /// Whether a pane would enable its drawing controls, which is what a test
    /// asserts instead of reaching into a control it cannot read back.
    #[cfg(test)]
    pub(crate) fn has_scope_for_test(&self) -> bool {
        self.has_scope()
    }

    /// The deadline this pane counts down to, if it has one to count down to.
    ///
    /// A countdown belongs to a pane that is *ready*: it counts down the time left
    /// until the next automatic fetch of the window that is on screen. While a fetch
    /// is in flight there is nothing to count down to — the deadline the service
    /// announced belongs to the fetch after the one being waited for — so a number
    /// that started moving then would be counting down to a window the pane has not
    /// received. A pane with no scope has no deadline either: it is not polling.
    fn countdown_deadline(&self, cx: &impl AppContext) -> Option<i64> {
        let status = self.status.state(cx);
        if status.is_busy() || status.is_failed() {
            return None;
        }
        self.next_refresh_at
    }

    /// Whether the refresh control has nothing to do.
    ///
    /// It is live exactly when the countdown is: a pane that is connecting, fetching
    /// or failed has nothing to bring forward — the first two are already waiting on
    /// data, and a failure is a state the framework offers its own recovery over,
    /// above the chart — and so has one that is waiting for its market, which is not
    /// polling anything and says so with a placeholder of its own. So the control is an
    /// icon alone and refuses input the rest of the time, and carries the countdown only
    /// while there is one.
    ///
    /// The pending half is not decoration: the service refuses a second refresh while
    /// one is in flight and the framework shows a refusal as a failure, so a control
    /// that forwarded the click while its own command was still unanswered would
    /// answer a fast double click with a banner.
    ///
    /// It is crate-visible rather than private to this module because the pane's
    /// tests live in `pane_tests.rs` beside it, which is a module of its own.
    pub(crate) fn refresh_is_disabled(&self, cx: &impl AppContext) -> bool {
        self.awaiting_market || self.refresh_pending || self.countdown_deadline(cx).is_none()
    }

    /// Builds the chart of the configured listing, or drops the one that was
    /// there.
    ///
    /// The labels come from the listing the program answered with, and a listing it
    /// could not answer for is drawn with none: the series are still worth plotting,
    /// and the axes fall back to naming nothing rather than naming the wrong asset.
    fn rebuild_chart(&mut self, cx: &mut App) {
        self.chart = self.config.selection.as_ref().and_then(|selection| {
            if !route_serves_statistics(cx, &selection.listing.route, M::METRIC.capability()) {
                return None;
            }
            ChartDispatch::for_metric(
                cx,
                self.units.base.as_str(),
                self.units.quote.as_str(),
                M::METRIC,
            )
        });
    }

    /// Settles the stored scope against the program, and starts polling it.
    ///
    /// A retry keeps the chart the user is still reading — blanking it to reconnect would
    /// flash a screen of data away — while an explicit load starts over, because the pane
    /// cannot vouch for what it was showing.
    ///
    /// The question this starts with is **advisory**: it only ever takes the scope away, and
    /// only when the program answered that it does not serve the listing — the route left the
    /// registry, the route publishes no perpetual statistics, the catalog does not hold the
    /// listing, or the listing is not a perpetual contract. The poll below is the
    /// authoritative request, and it resolves the listing inside the connector, so a catalog
    /// that could not be read is reported as the failure it is rather than as a listing that
    /// cannot be named.
    fn start(&mut self, retry: bool, cx: &mut Context<Self>) {
        // The pane reached here only past its own gate, so the market it was waiting
        // for is in memory again and messages belong to it once more.
        self.awaiting_market = false;
        self.directory_generation = self.directory_generation.wrapping_add(1);
        let generation = self.directory_generation;
        self.ready = None;
        self.next_refresh_at = None;
        self.countdown = None;
        // The command that was in flight belongs to the generation this call is
        // replacing, so its answer is not something to wait for any more.
        self.refresh_pending = false;
        // An interval this build does not offer is settled before anything is asked for
        // it: the picker could not draw it, and the request would name a window the
        // service does not serve.
        if !TIMEFRAMES.contains(&self.config.timeframe) {
            self.config.timeframe = StatisticsConfig::default().timeframe;
            log::warn!(
                "the interval stored for this pane is not one this build offers; \
                 it fell back to {:?}",
                self.config.timeframe
            );
            self.config_change.publish(cx);
        }
        let Some(selection) = self.config.selection.clone() else {
            self.nothing_chosen(cx);
            return;
        };
        // A retry keeps the window the user is still reading; an explicit load
        // rebuilds the chart for the scope it now follows.
        if !retry {
            self.rebuild_chart(cx);
        }
        self.accepted = retry && self.chart.is_some();
        // A listing the program no longer serves goes back to "nothing chosen" before
        // anything is asked for it: a window for a listing nobody publishes fails about a
        // scope the user cannot return to anyway. Only that answer does — a question that
        // could not be put leaves the scope alone and the poll reports the failure that is
        // really there. The question is work this pane is doing, so it reports connecting
        // while it is in flight — the request that follows is what asks for a window.
        self.status.report(WidgetState::Connecting, cx);
        cx.notify();
        let catalog = cx.global::<MarketParticipantsConnector>().catalog();
        let routes = LoadedScope::all(cx.global::<MarketParticipantsConnector>().venues());
        let support = listing_support(catalog, &routes, &selection.listing, M::METRIC.capability());
        cx.spawn(async move |this, cx| {
            let _ = this.update(cx, |this, cx| {
                // An answer about a scope the user has left is not this pane's to apply:
                // their own choice started a poll of its own.
                if this.directory_generation != generation
                    || !this.settle_selection(&selection, support, cx)
                {
                    return;
                }
                this.poll(retry, cx);
            });
        })
        .detach();
    }

    /// Asks the service for the window of the scope the config now holds.
    fn poll(&mut self, retry: bool, cx: &mut Context<Self>) {
        let Some(selection) = self.config.selection.clone() else {
            self.nothing_chosen(cx);
            return;
        };
        self.binding.call(
            StatisticsRequest::from_scope(
                M::METRIC,
                selection.listing,
                self.config.timeframe,
                retry,
            ),
            cx,
            Self::apply_load_result,
        );
        cx.notify();
    }

    /// Says what a pane with nothing to follow is waiting for.
    fn nothing_chosen(&mut self, cx: &mut Context<Self>) {
        // There is nothing to draw yet, so there is nothing to poll. The pane has no
        // content of its own, so it says what it is waiting for.
        self.chart = None;
        self.accepted = false;
        self.report_notice(cx);
        cx.notify();
    }

    fn apply_load_result(
        &mut self,
        result: WidgetRpcResult<StatisticsProtocol>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(StatisticsResponse::StreamStarted | StatisticsResponse::RefreshQueued) => {}
            Err(error) => {
                let recovery = self.recovery.clone();
                self.status.report(
                    WidgetState::failed(Arc::new(StatisticsFailure::Rpc(error)), recovery),
                    cx,
                );
            }
        }
    }

    fn apply_refresh_result(
        &mut self,
        result: WidgetRpcResult<StatisticsProtocol>,
        cx: &mut Context<Self>,
    ) {
        // An accepted command stays pending until the round it started is reported:
        // the acknowledgement and the round are two different messages, and the gate
        // the service holds spans both.
        if let Err(error) = result {
            self.refresh_pending = false;
            let recovery = self.recovery.clone();
            self.status.report(
                WidgetState::failed(Arc::new(StatisticsFailure::Rpc(error)), recovery),
                cx,
            );
        }
    }

    /// Applies a status the stream reported.
    ///
    /// Returns whether anything a frame draws changed; a status is drawn by the
    /// framework around the body, so publishing it is enough.
    pub(crate) fn apply_reliable(
        &mut self,
        key: StatisticsStreamKey,
        message: Arc<StatisticsStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx)
            || self.awaiting_market
        {
            return false;
        }
        let StatisticsStreamMessage::Status(status) = message.as_ref() else {
            return false;
        };
        self.next_refresh_at = status.next_refresh_at();
        match status {
            StatisticsStatus::Connecting => self.status.report(WidgetState::Connecting, cx),
            StatisticsStatus::Refreshing { .. } => {
                self.refresh_pending = false;
                self.status.report(WidgetState::Loading, cx);
            }
            StatisticsStatus::Ready { revision, .. } => {
                self.refresh_pending = false;
                self.ready = Some(*revision);
                // The reliable channel and the latest one have no order between
                // them, so this can arrive before the window it describes:
                // claiming readiness now would be claiming a chart the pane does
                // not have.
                if self.chart_revision().is_some_and(|held| held >= *revision) {
                    self.status.report(WidgetState::Ready, cx);
                } else {
                    self.status.report(WidgetState::Loading, cx);
                }
            }
            StatisticsStatus::Failed { error } => {
                self.refresh_pending = false;
                let failure = StatisticsFailure::Fetch(error.clone());
                // A read the directory answered "not loaded" to is a market that is no
                // longer loaded rather than a failure: the pane goes back to its
                // neutral notice, and the directory's next publication is what brings it
                // out of it.
                if is_waiting_for_a_catalog(&failure) {
                    self.wait_for_market(cx);
                    return false;
                }
                let recovery = self.recovery.clone();
                self.status
                    .report(WidgetState::failed(Arc::new(failure), recovery), cx);
            }
        }
        false
    }

    /// Applies a window the stream published.
    ///
    /// Returns whether anything a frame draws changed, which is what the framework
    /// turns into a repaint: the body is cached, and a window that folds into the
    /// same pixels is not worth a frame.
    pub(crate) fn apply_latest(
        &mut self,
        key: StatisticsStreamKey,
        message: Arc<StatisticsStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx)
            || self.awaiting_market
        {
            return false;
        }
        let StatisticsStreamMessage::Snapshot(snapshot) = message.as_ref() else {
            return false;
        };
        // A window the pane has already overtaken would only make the chart jump
        // backwards: within one generation the service's revision only moves
        // forwards, so a smaller one can only be a window a status has replaced.
        if self
            .chart_revision()
            .is_some_and(|held| snapshot.revision < held)
        {
            return false;
        }
        let Some(chart) = self.chart.as_mut() else {
            return false;
        };
        let changed = chart.set_snapshot(message.as_ref());

        self.accepted = true;
        // Readiness is published at the protocol's own boundary: a complete window
        // is not the same thing as the window a status said was complete, and this
        // is where the two meet.
        if self
            .ready
            .is_some_and(|ready| snapshot.revision >= ready && chart.revision() >= ready)
        {
            self.status.report(WidgetState::Ready, cx);
        }
        changed
    }

    /// The revision of the window the chart is showing.
    pub(crate) fn chart_revision(&self) -> Option<u64> {
        self.chart.as_ref().map(ChartDispatch::revision)
    }

    /// How many intervals the window on screen holds, which is what a test reads to
    /// tell an empty window from one that never arrived.
    #[cfg(test)]
    pub(crate) fn chart_point_count(&self) -> Option<usize> {
        self.chart.as_ref().map(ChartDispatch::point_count)
    }

    /// Whether what this pane needs is in memory: at least one market that answers with
    /// these statistics, and the route of the stored selection among them.
    ///
    /// A pane that follows nothing still needs a market: the picker is drawn from the
    /// loaded venues, so with none of them loaded there is nothing to choose from — the
    /// pane waits for the market participant's metadata and says so, rather than offering a choice
    /// that is not there. Once one is loaded, a pane that still follows nothing is the
    /// "select an instrument" notice's business and this answers `true` for it.
    ///
    /// Reading a market is only possible once the user has loaded it, and "not
    /// loaded" is a wait rather than a failure. One question answers each route: a
    /// scope in the loaded venues is a scope whose catalog committed with it.
    fn market_ready(&self, cx: &App) -> bool {
        if !self.any_market_loaded(cx) {
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

    /// Whether any market this pane could read is loaded, whatever the user selected.
    ///
    /// The venues are the ones this pane's metric exists on, read off the definition table
    /// rather than off what happens to be loaded — the same answer
    /// [`Self::listed_venues`] gives the notice.
    fn any_market_loaded(&self, cx: &App) -> bool {
        let loaded = cx.global::<MarketParticipantsConnector>().venues();
        Self::listed_venues()
            .iter()
            .any(|venue| loaded.loaded(venue).is_some())
    }

    /// Reports what this pane is waiting for: the market it follows, or the instrument
    /// it has nothing chosen for.
    ///
    /// The list the sentence ends with is every venue that answers with these statistics —
    /// the capability this pane is defined by, read off the definition table rather than off
    /// what happens to be loaded — and the venue it waits for is the one behind the
    /// selection, or the whole list while nothing is chosen and no single venue can be named.
    ///
    /// This is the one place that reference is produced — [`Self::wait_for_market`] and
    /// [`Self::nothing_chosen`] call this rather than holding one of their own. Waiting for
    /// the market is what the flag says — the path that drops the chart for it sets it, the
    /// one that starts reading again clears it. The sentence is the framework's to resolve,
    /// in the language showing on the frame that draws it.
    fn report_notice(&self, cx: &mut Context<Self>) {
        if self.awaiting_market {
            let venues = self
                .config
                .selection
                .as_ref()
                .map(|selection| vec![selection.listing.route.venue])
                .unwrap_or_else(Self::listed_venues);
            self.status.report(
                WidgetState::waiting(Notice::MarketNotLoaded {
                    venues,
                    supported: Self::listed_venues(),
                }),
                cx,
            );
        } else if self.config.selection.is_none() {
            self.status.report(
                WidgetState::waiting(Notice::message(
                    "widget.perpetual_statistics.select_instrument_prompt",
                )),
                cx,
            );
        }
    }

    /// The venues the waiting notice lists: every venue whose loader can answer with these
    /// statistics.
    ///
    /// A pane reads one venue — the one behind the instrument the user followed — so the
    /// market it waits for follows the selection. What it can read at all does not: every
    /// metric this pane can draw is served by one venue set, which is the open-interest
    /// capability's off the definition table, and that is the same answer whether or not
    /// anything is loaded and whichever metric is picked.
    pub(crate) fn listed_venues() -> Vec<VenueId> {
        market_participants_connector::venue::definition::venues_supporting(
            CapabilityId::OpenInterest,
        )
    }

    /// Reports the neutral notice for the market this pane follows, and drops the
    /// chart it was drawing.
    ///
    /// The route behind that chart is gone, so nothing on screen is backed by a
    /// market any more, and the framework only draws the notice while the pane
    /// reports no content — which is why the pane also stops accepting messages: the
    /// stream it was reading is still alive, and a window that was already in flight
    /// would otherwise put content back over the notice. The stored selection is left
    /// alone: it is the user's, and loading the market again is what brings it back.
    fn wait_for_market(&mut self, cx: &mut Context<Self>) {
        self.awaiting_market = true;
        self.accepted = false;
        self.chart = None;
        self.ready = None;
        self.next_refresh_at = None;
        self.countdown = None;
        self.refresh_pending = false;
        self.report_notice(cx);
        cx.notify();
    }

    /// Runs the load a venue commit may have unblocked.
    ///
    /// A commit can matter to this pane for either of two reasons. The first is that it changes
    /// whether the market it needs is in memory, and the pane already holds that answer in the two
    /// things it maintains anyway: `awaiting_market` says it is not reading because a market it
    /// needs is missing, and `market_ready` says everything it needs is there now. A pane that is
    /// waiting for one and can now read starts over rather than re-checking; a pane that was
    /// reading one that is gone goes back to waiting.
    ///
    /// The second is that it landed on the venue behind the stored selection, which is the user
    /// loading or refreshing the market this pane reads: the catalog generation the live stream
    /// resolved its units and precision against is replaced, so the pane starts over. Nothing is
    /// remembered for it the way the other panes remember a reload — this protocol has one request
    /// per scope, and the service answers a `Load` by replacing whatever stream that scope is on
    /// (`StatisticsService::run`), because a poll that ended rather than failed would otherwise
    /// answer a stream that is gone. So the plain `Load` [`Self::load`] sends is already the
    /// request that opens a new stream. A commit of any other venue is neither reason, and leaves
    /// the pane and its poll exactly as they are. A failure is terminal: the event does not revive
    /// a failed pane, only the user's own recovery does.
    fn on_venues_updated(&mut self, event: &VenuesUpdated, cx: &mut Context<Self>) {
        if self.status.is_failed(cx) {
            return;
        }
        // A commit on the venue the stored selection is on is this pane's own market being loaded
        // again, so the pane cannot keep polling the stream it holds.
        if self
            .config
            .selection
            .as_ref()
            .is_some_and(|selection| selection.listing.route.venue == event.venue)
        {
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

    /// The control that opens the instrument picker.
    ///
    /// Before anything is chosen it offers the choice rather than reporting a
    /// scope, because there is no scope yet. What it states once a listing is
    /// followed — the published name and the route it is on — is the control's own
    /// text, so the pane and the dialog cannot disagree about which listing is
    /// followed.
    fn instrument_button(&self, cx: &Context<Self>) -> AnyElement {
        let widget = cx.entity().downgrade();

        InstrumentButton::new(
            ElementId::Name(SharedString::new_static("StatisticsInstrument")),
            self.config.selection.clone(),
            // These series exist for perpetual markets only, so the picker offers
            // nothing else: a spot listing would be a scope no endpoint here can
            // answer.
            InstrumentFilters {
                kind_filter: InstrumentKindFilter::Perpetual,
                status_filter: InstrumentStatusFilter::Any,
                required_capability: M::METRIC.capability(),
                entry_predicate: None,
                routes: LoadedScope::all(cx.global::<MarketParticipantsConnector>().venues()),
            },
            "widget.perpetual_statistics.select_instrument",
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

    /// The interval and the refresh, in the header's right group.
    ///
    /// There is no separate countdown any more: it is the refresh button's own
    /// label, because the two are one control's worth of information — when the
    /// next fetch is due, and the way to bring it forward.
    fn drawing_controls(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .child(self.refresh_button(cx))
            .child(self.timeframe_select(cx))
            .into_any_element()
    }

    /// The interval picker.
    ///
    /// It is disabled without a scope, exactly like the refresh: the interval only
    /// says which window to fetch, so with nothing to fetch it changes nothing.
    /// A pane waiting for its market is one of those: `has_scope` answers `false` for it,
    /// so a pane that cannot refresh cannot present itself as able to re-scope either.
    fn timeframe_select(&self, cx: &Context<Self>) -> Select {
        let disabled = !self.has_scope();
        let selected = self.config.timeframe;
        let select = TIMEFRAMES.iter().copied().fold(
            Select::new(ElementId::Name(SharedString::new_static(
                "StatisticsTimeframe",
            )))
            .placeholder(SharedString::new(selected.label()))
            .ghost()
            .small()
            .disabled(disabled)
            .selected_item(Some(SharedString::new(selected.label())))
            .aria_label(t!(cx, "widget.perpetual_statistics.interval")),
            |select, timeframe| {
                select.menu_item(MenuItem::Action(MenuItemAction::new(
                    SharedString::new(timeframe.label()),
                    SharedString::new(timeframe.label()),
                )))
            },
        );

        select.on_change({
            let widget = cx.entity();
            move |timeframe, _, app| {
                // The picker identifies an interval by the label it shows, which is
                // the only name the menu entries carry.
                let Some(timeframe) = timeframe
                    .as_deref()
                    .and_then(|label| TIMEFRAMES.iter().copied().find(|it| it.label() == label))
                else {
                    return;
                };
                widget.update(app, |this, cx| this.select_timeframe(timeframe, cx));
            }
        })
    }

    /// The control that fetches the current scope again, and the time left until
    /// the next automatic fetch.
    ///
    /// The countdown is the button's own label rather than a word beside it: it is
    /// the number that says the pane is still alive, and the control that resets
    /// it is the one place a reader would look for it. A pane with no scope has no
    /// schedule to count down to, so the button is bare and disabled.
    fn refresh_button(&self, cx: &Context<Self>) -> Button {
        let disabled = self.refresh_is_disabled(cx);
        Button::new(ElementId::Name(SharedString::new_static(
            "StatisticsRefresh",
        )))
        .ghost()
        .small()
        .disabled(disabled)
        .aria_label(t!(cx, "widget.perpetual_statistics.refresh_now"))
        .child(Icon::new(IconKind::ArrowsClockwise).size_3())
        .children(self.countdown.clone())
        .on_click({
            let widget = cx.entity();
            move |_, _, app| {
                widget.update(app, |this, cx| this.refresh(cx));
            }
        })
    }

    /// Follows another listing.
    ///
    /// Returns whether the selection was accepted. A selection names the exact
    /// listing, so there is nothing to check about which market it belongs to: the
    /// picker offers perpetual listings of routes that publish these statistics,
    /// and a stored or linked selection that is not one is settled in `load`.
    pub(crate) fn select_selection(
        &mut self,
        selection: InstrumentSelection,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.config.selection.as_ref() == Some(&selection) && !self.status.is_failed(cx) {
            return false;
        }
        self.config.selection = Some(selection);
        self.config_change.publish(cx);
        self.start(false, cx);
        true
    }

    pub(crate) fn select_timeframe(
        &mut self,
        timeframe: StatisticsTimeFrame,
        cx: &mut Context<Self>,
    ) {
        if self.config.timeframe == timeframe {
            return;
        }
        self.config.timeframe = timeframe;
        self.config_change.publish(cx);
        self.start(false, cx);
    }

    /// Fetches the current scope without waiting for the next interval.
    ///
    /// Only a ready pane gets here, so the command it sends is one the loop answers:
    /// a failure is recovered through the framework's own control, above the chart.
    ///
    /// It is crate-visible for the same reason the predicate above is: the tests
    /// that press it live in `pane_tests.rs`.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refresh_is_disabled(cx) {
            return;
        }
        self.refresh_pending = true;
        self.binding
            .call(StatisticsRequest::Refresh, cx, Self::apply_refresh_result);
        cx.notify();
    }

    /// Rebuilds the countdown, asking for the frame that draws it.
    ///
    /// The countdown is the only thing this pane draws that moves on its own, and
    /// it moves once a second: the task wakes four times a second so a second is
    /// never missed, and only the write that changed it asks for a frame. The
    /// header is not cached, so nothing else here has to announce itself.
    ///
    /// The label is the deadline of a ready pane and nothing else, so leaving the
    /// ready state — a fetch starting, a failure, a reload — takes the countdown off
    /// the button in the same tick that greys it.
    fn tick_countdown(&mut self, cx: &mut Context<Self>) {
        let label = self.countdown_deadline(cx).map(|due| {
            SharedString::new(format_countdown(due.saturating_sub(unix_millis())).as_str())
        });
        if self.countdown == label {
            return;
        }
        self.countdown = label;
        cx.notify();
    }

    /// Runs one countdown tick, which a test drives instead of the clock.
    #[cfg(test)]
    pub(crate) fn tick_countdown_for_test(&mut self, cx: &mut Context<Self>) {
        self.tick_countdown(cx);
    }
}

impl<M> Render for StatisticsWidget<M>
where
    M: StatisticsMetric,
{
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The body is only embedded while the pane has content, so this is the
        // ready state and nothing else. Everything the pane is *doing* is drawn by
        // the framework around this.
        let Some(chart) = self.chart.as_ref() else {
            return div().size_full().min_h_0().into_any_element();
        };
        // A window the venue published with no interval in it draws axes and nothing
        // else, which reads exactly like a chart that has not arrived: the pane says
        // which of the two it is over the plot.
        let empty = (self.accepted && chart.point_count() == 0).then(|| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .child(EmptyState::new("StatisticsEmpty", t!(cx, "common.no_data")))
        });
        // The chart owns everything below this line — its minimum size, its two
        // scroll containers and the room its axes need — so the pane imposes no
        // width or height of its own here: a pane narrower than the axes need
        // scrolls inside the chart rather than clipping an axis nobody can reach.
        div()
            .relative()
            .size_full()
            .min_h_0()
            .child(chart.entity())
            .children(empty)
            .into_any_element()
    }
}

/// Whether a route is one these panes can read.
///
/// These endpoints exist for perpetual markets only, and a route publishes one
/// series per capability, so the loaded venues are asked rather than the selection
/// trusted.
pub(crate) fn route_serves_statistics(
    cx: &App,
    route: &market_participants_model::listing::route::VenueRoute,
    capability: CapabilityId,
) -> bool {
    cx.global::<MarketParticipantsConnector>()
        .venues()
        .scope(route)
        .is_some_and(|scope| {
            scope
                .capabilities
                .iter()
                .any(|support| support.capability == capability)
        })
}

/// The assets one chart's series are measured in, named the way the catalog names
/// them.
///
/// The size of a series is the listing's own traded quantity and its worth is a
/// monetary amount, so the two names come from the listing's quantity rule and
/// from its price rule: no field name is read as a unit, and a quantity the
/// listing counts in contracts is named as contracts rather than as an asset.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct ChartUnits {
    pub base: SmolStr,
    pub quote: SmolStr,
}

impl ChartUnits {
    /// Reads the two names from one listing's own rules.
    pub(crate) fn of(rules: &TradingRules, cx: &App) -> Self {
        // One capture names both axes, so a chart never mixes two publications even
        // if the catalog is republished while it is labelled.
        let generation = cx
            .global::<MarketParticipantsConnector>()
            .catalog()
            .cached_display();
        Self {
            base: quantity_unit_label(&rules.common.quantity.unit, &generation, cx),
            quote: asset_display(&generation, &rules.common.price.quote_asset, cx),
        }
    }
}

/// The display code of one asset, as the captured generation published it.
///
/// Only the display catalog knows what an asset is called: the canonical identity
/// of a token is a digest, and the code it publishes is the whole name of the
/// asset. An asset the captured generation does not name is
/// the localized unknown rather than an identity digest, because a reader cannot
/// use a hash and a name invented here would disagree with every other pane.
fn asset_display(generation: &DisplaySnapshot, asset: &AssetId, cx: &App) -> SmolStr {
    generation.resolver().asset_code(asset).map_or_else(
        |_| SmolStr::new(t!(cx, "error.display.unknown_asset").as_ref()),
        |code| SmolStr::new(code.as_str()),
    )
}

/// The label of what one quantity counts.
fn quantity_unit_label(unit: &QuantityUnit, generation: &DisplaySnapshot, cx: &App) -> SmolStr {
    match unit {
        QuantityUnit::AssetUnits { asset } | QuantityUnit::Notional { asset } => {
            asset_display(generation, asset, cx)
        }
        QuantityUnit::Contracts => {
            SmolStr::new(t!(cx, "widget.perpetual_statistics.contracts_unit").as_ref())
        }
        QuantityUnit::OutcomeTokens { .. } => {
            SmolStr::new(t!(cx, "widget.perpetual_statistics.outcome_tokens_unit").as_ref())
        }
    }
}

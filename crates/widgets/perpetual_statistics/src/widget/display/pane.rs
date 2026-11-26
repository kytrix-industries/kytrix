use std::{marker::PhantomData, rc::Rc, sync::Arc};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
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
    math::px,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::{style::StylePatch, style_builder::StyleBuilder},
    text::TextStyle,
    window::WindowId,
};
use ui_kit::kit::{
    button::Button,
    empty_state::EmptyState,
    icon::IconKind,
    select::{Select, SelectItem, SelectOptions},
    size::{UnifiedSizable, UnifiedSize},
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

pub(crate) const COUNTDOWN_TICK: std::time::Duration = std::time::Duration::from_millis(250);

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

const INSTRUMENT_KEY_SUFFIX: &str = "perpetual_statistics.header.instrument";
const REFRESH_KEY_SUFFIX: &str = "perpetual_statistics.header.refresh";
const TIMEFRAME_KEY_SUFFIX: &str = "perpetual_statistics.header.timeframe";

pub trait StatisticsMetric: Sized + 'static {
    const ID: WidgetId;
    const DISPLAY_NAME: &'static str;
    const DESCRIPTION: Option<&'static str>;
    const ICON: &'static str;
    const METRIC: MetricKind;
}

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
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StatisticsFailure {
    Fetch(StatisticsFetchError),
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

fn is_waiting_for_a_catalog(failure: &StatisticsFailure) -> bool {
    matches!(
        failure,
        StatisticsFailure::Fetch(StatisticsFetchError::MarketParticipant(error))
            if widget::error::market_is_catalog_not_loaded(error)
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    Load,
    Refresh,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum HeaderRequest {
    Refresh,
    Selection(InstrumentSelection),
    Timeframe(StatisticsTimeFrame),
}

struct Countdown {
    window: WindowId,
    deadline: std::cell::Cell<Option<i64>>,
    label: Signal<Option<SmolStr>>,
    now: Rc<dyn Fn() -> i64>,
    timer: std::cell::RefCell<Option<TimerId>>,
}

pub(crate) struct PaneScaffold {
    pub status: WidgetStatus,
    pub config_change: WidgetConfigChange,
    pub recovery: WidgetAction,
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

    fn deadline(&self) -> Option<i64> {
        self.deadline.get()
    }

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

    fn write_label(&self, app: &mut App) -> bool {
        app.write(
            &self.label,
            countdown_label(self.deadline.get(), (self.now)()),
        )
    }

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

    fn tick(self: &Rc<Self>, app: &mut App) {
        *self.timer.borrow_mut() = None;
        self.write_label(app);
        self.arm(app);
    }
}

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

pub struct StatisticsWidget<M: StatisticsMetric> {
    pub config: StatisticsConfig,
    binding: Option<WidgetBinding<StatisticsProtocol>>,
    pub status: WidgetStatus,
    recovery: WidgetAction,
    config_change: WidgetConfigChange,
    chart: Option<ChartDispatch>,
    accepted: bool,
    ready: Option<u64>,
    next_refresh_at: Option<i64>,
    countdown: Rc<Countdown>,
    refresh_pending: bool,
    pending: Pending<PendingCommand>,
    units: ChartUnits,
    directory: Option<Rc<dyn StatisticsDirectory>>,
    seen_generation: u64,
    awaiting_market: bool,
    chart_host: Option<NodeId>,
    empty: Signal<bool>,
    empty_text: Signal<SmolStr>,
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
        if self.directory.is_none() {
            self.report_unavailable(cx);
            return;
        }
        if !self.market_ready(cx) {
            self.wait_for_market(cx);
            return;
        }
        let retry = self.status.is_failed(cx.app()) && self.accepted;
        self.start(retry, cx);
    }

    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

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

        self.chart_host = Some(host);
        if let Some(chart) = self.chart.as_mut() {
            chart.build(cx, host)?;
        }

        let empty = self.empty;
        let empty_hidden = self.empty;
        let overlay = NodeBuilder::boxed()
            .id("TEST_PERPETUAL_STATISTICS_EMPTY")
            .style(
                StyleBuilder::new()
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
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
        self.drain_inbox(cx);

        let generation = cx.services().catalog_generation;
        let generation = *cx.read(&generation);
        if generation != self.seen_generation {
            self.seen_generation = generation;
            if !self.status.is_failed(cx.app()) {
                self.reload(cx);
            }
        }

        let zone = cx.data_time_zone();
        if let Some(chart) = self.chart.as_mut() {
            chart.set_time_zone(zone);
        }
        let translate = cx.translate().clone();
        if let Some(chart) = self.chart.as_mut() {
            chart.set_locale(&translate);
        }

        let request = cx.read(&self.requests).clone();
        if let Some(request) = request {
            cx.write(&self.requests, None);
            self.apply_header_request(request, cx);
        }

        if self.has_content() && self.chart_host.is_none() {
            cx.request_frame()?;
        }

        self.sync_chart(cx)?;

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
        let moved = match input {
            InputEvent::PointerMove { position, .. }
            | InputEvent::PointerEnter { position, .. } => {
                let node = chart.node();
                let local = cx
                    .tree()
                    .hit_test(*position)
                    .filter(|hit| Some(hit.node) == node)
                    .map(|hit| hit.local_point);
                match local {
                    Some(local) => chart.point_at(local),
                    None => chart.clear_hover(),
                }
            }
            InputEvent::PointerLeave { .. } => chart.clear_hover(),
            _ => return false,
        };
        if moved {
            let _ = cx.request_frame();
        }
        true
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        self.binding = None;
        self.release_header(HeaderGroup::Left, false, cx);
        self.release_header(HeaderGroup::Right, false, cx);
        if let Some(mut chart) = self.chart.take() {
            chart.release(cx);
        }
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

    pub(crate) fn current_key(&self) -> Option<StatisticsStreamKey> {
        self.config.selection.as_ref().map(|selection| {
            StatisticsStreamKey::new(M::METRIC, selection.listing.clone(), self.config.timeframe)
        })
    }

    fn reload(&mut self, cx: &mut WidgetCx<'_>) {
        if self.directory.is_none() {
            self.report_unavailable(cx);
            return;
        }
        if !self.market_ready(cx) {
            self.wait_for_market(cx);
            return;
        }
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

    fn has_scope(&self) -> bool {
        !self.awaiting_market && self.current_key().is_some()
    }

    #[cfg(test)]
    pub(crate) fn has_scope_for_test(&self) -> bool {
        self.has_scope()
    }

    fn countdown_deadline(&self, app: &App) -> Option<i64> {
        let status = self.status.state(app);
        if status.is_busy() || status.is_failed() {
            return None;
        }
        self.next_refresh_at
    }

    pub(crate) fn refresh_is_disabled(&self, app: &App) -> bool {
        self.awaiting_market || self.refresh_pending || self.countdown_deadline(app).is_none()
    }

    fn report_unavailable(&mut self, cx: &mut WidgetCx<'_>) {
        let recovery = self.recovery.clone();
        self.status.report(
            cx.app_mut(),
            WidgetState::failed(Arc::new(StatisticsFailure::Unavailable), recovery),
        );
    }

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

    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) {
        self.awaiting_market = false;
        self.ready = None;
        self.next_refresh_at = None;
        self.pending.clear();
        self.refresh_pending = false;
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

    pub(crate) fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<StatisticsProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
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
                if self.chart_revision().is_some_and(|held| held >= *revision) {
                    self.status.report(cx.app_mut(), WidgetState::Ready);
                } else {
                    self.status.report(cx.app_mut(), WidgetState::Loading);
                }
            }
            StatisticsStatus::Failed { error } => {
                self.refresh_pending = false;
                let failure = StatisticsFailure::Fetch(error.clone());
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
        if self
            .ready
            .is_some_and(|ready| snapshot.revision >= ready && chart.revision() >= ready)
        {
            self.status.report(cx.app_mut(), WidgetState::Ready);
        }
        let _ = cx.request_frame();
    }

    pub(crate) fn chart_revision(&self) -> Option<u64> {
        self.chart.as_ref().map(ChartDispatch::revision)
    }

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

    pub(crate) fn listed_venues() -> Vec<market_participants_model::identifier::VenueId> {
        market_venues(M::METRIC.capability())
    }

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

    fn sync_chart(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if let (Some(host), Some(chart)) = (self.chart_host, self.chart.as_mut()) {
            if chart.node().is_none() {
                chart.build(cx, host)?;
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
        let text = t_smol!(cx.translate(), "common.no_data");
        cx.write(&self.empty_text, text);
        Ok(())
    }

    fn apply_header_request(&mut self, request: HeaderRequest, cx: &mut WidgetCx<'_>) {
        match request {
            HeaderRequest::Refresh => self.refresh(cx),
            HeaderRequest::Selection(selection) => {
                self.select_selection(selection, cx);
            }
            HeaderRequest::Timeframe(timeframe) => self.select_timeframe(timeframe, cx),
        }
    }

    fn instrument_filters(&self) -> InstrumentFilters {
        InstrumentFilters {
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

    fn selector_host(&self, cx: &WidgetCx<'_>) -> SelectorHost {
        SelectorHost::new(Rc::clone(cx.services_rc()))
    }

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
                    .gap(px(4.0))
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
                    .gap(px(4.0))
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
            foreground: Some(theme.role(read, ui_kit::theme::role::Role::FgDefault)),
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
                ctx.update(&requests, |request| *request = Some(HeaderRequest::Refresh));
                let window = ctx.window();
                let _ = ctx.app_mut().request_repaint(window);
            });
        button.build(cx.app_mut(), window, parent)?;
        Ok(button)
    }

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
        let layer = ui_kit::kit::layer::Layer::clone(cx.layer());
        let router = ui_kit::kit::overlay::AnimationRouter::clone(cx.animation());
        let window = cx.window();
        let key = cx.unique_key(TIMEFRAME_KEY_SUFFIX);
        let select = Select::new(cx.app_mut(), theme, layer, router, key, options).on_change(
            move |app, chosen| {
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

    fn sync_header(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.sync_instrument_header(cx)?;
        self.sync_drawing_header(cx)
    }

    fn sync_instrument_header(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(header) = self.left_header.as_ref() else {
            return Ok(());
        };
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

    #[cfg(test)]
    pub(crate) fn countdown_label(&self, app: &App) -> Option<SmolStr> {
        self.countdown.label(app)
    }

    #[cfg(test)]
    pub(crate) fn countdown_is_armed_for_test(&self) -> bool {
        self.countdown.is_armed()
    }

    #[cfg(test)]
    pub(crate) fn refresh_pending_for_test(&self) -> bool {
        self.refresh_pending
    }

    #[cfg(test)]
    pub(crate) fn dropped_answers_for_test(&self) -> usize {
        self.pending.dropped()
    }

    #[cfg(test)]
    pub(crate) fn pending_is_empty_for_test(&self) -> bool {
        self.pending.is_empty()
    }

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

    #[cfg(test)]
    pub(crate) fn chart_for_test(&self) -> Option<&ChartDispatch> {
        self.chart.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn awaiting_market_for_test(&self) -> bool {
        self.awaiting_market
    }

    #[cfg(test)]
    pub(crate) fn timeframe_for_test(&self) -> StatisticsTimeFrame {
        self.config.timeframe
    }
}

fn countdown_label(deadline: Option<i64>, now: i64) -> Option<SmolStr> {
    deadline.map(|deadline| format_countdown(deadline.saturating_sub(now)))
}

#[cfg(test)]
impl Countdown {
    fn label(&self, app: &App) -> Option<SmolStr> {
        app.read(&self.label).clone()
    }

    fn is_armed(&self) -> bool {
        self.timer.borrow().is_some()
    }
}

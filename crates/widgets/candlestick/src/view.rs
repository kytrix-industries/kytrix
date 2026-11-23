use std::{rc::Rc, sync::Arc, time::Duration};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use i18n::{t_smol, translate::Translate};
use instrument_selector_ui::{
    host::SelectorHost,
    instrument_button::{InstrumentButton, InstrumentButtonHandle},
    instrument_selector::{
        InstrumentFilters, InstrumentKindFilter, InstrumentStatusFilter, LoadedScope,
    },
};
use market_participants_connector::{
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
use ui_kit::{
    kit::{
        button::Button,
        empty_state::EmptyState,
        select::{Select, SelectItem, SelectOptions},
        separator::Separator,
        size::{UnifiedSizable, UnifiedSize},
    },
    style_builder::StyleBuilder,
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
    directory::{ConnectorDirectory, ListingAnswer, MarketDirectory, market_venues},
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

const MAXIMUM_CANDLES: usize = 1_500;
const HISTORY_PAGE_SIZE: u16 = 300;
const PREFETCH_MARGIN: usize = 120;
const MIN_CANDLE_WIDTH: f32 = 5.0;
const MAX_CANDLE_WIDTH: f32 = 25.0;

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

#[derive(Clone, Debug, PartialEq)]
enum HeaderRequest {
    Timeframe(TimeFrame),
    Adjustment(Adjustment),
    ToggleSettings,
}

fn is_unsupported_timeframe(error: &MarketError) -> bool {
    matches!(
        error,
        MarketError::Network(network)
            if matches!(network.error(), MarketParticipantError::UnsupportedTimeFrame { .. })
    )
}

fn is_unsupported_adjustment(error: &MarketError) -> bool {
    matches!(
        error,
        MarketError::Network(network)
            if matches!(network.error(), MarketParticipantError::UnsupportedAdjustment { .. })
    )
}

fn is_unsupported_capability(error: &MarketError) -> bool {
    matches!(
        error,
        MarketError::UnsupportedCapability {
            capability: CapabilityId::Candlesticks,
            ..
        }
    )
}

const fn adjustment_key(adjustment: Adjustment) -> &'static str {
    match adjustment {
        Adjustment::None => "widget.candlestick.adjust_none",
        Adjustment::Forward => "widget.candlestick.adjust_forward",
    }
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    Load,
    Retry,
    FetchHistory,
    FetchNewer,
}

#[derive(Clone, Debug, PartialEq)]
struct HeaderState {
    language: i18n::language::codes::LanguageCode,
    selection: Option<InstrumentSelection>,
    generation: u64,
}

struct HeaderControls {
    root: NodeId,
    parent: NodeId,
    settings: Button,
    adjustment: Option<Select>,
    timeframe: Select,
    built: HeaderState,
}

struct HeaderInstrument {
    parent: NodeId,
    control: InstrumentButtonHandle,
    built: HeaderState,
}

struct Service {
    binding: Option<WidgetBinding<CandlestickProtocol>>,
    published_timeframes: Option<Rc<dyn Fn(&VenueRoute) -> Result<Vec<TimeFrame>, MarketError>>>,
    published_adjustments: Option<Rc<dyn Fn(&VenueRoute) -> Result<Vec<Adjustment>, MarketError>>>,
}

impl Service {
    const fn none() -> Self {
        Self {
            binding: None,
            published_timeframes: None,
            published_adjustments: None,
        }
    }
}

pub struct CandlestickWidget {
    config: CandlestickConfig,
    binding: Option<WidgetBinding<CandlestickProtocol>>,
    directory: Option<Rc<dyn MarketDirectory>>,
    status: WidgetStatus,
    recovery: WidgetAction,
    config_change: WidgetConfigChange,
    pending: Pending<PendingCommand>,
    generation: u64,
    seen_generation: u64,
    snapshot: Option<CandlestickSeriesSnapshot>,
    countdown: CountdownState,
    countdown_timer: Option<TimerId>,
    countdown_wake: Signal<Option<i64>>,
    viewport: ChartViewport,
    chart: ChartCanvas,
    body_root: Option<NodeId>,
    panel_parent: Option<NodeId>,
    panel_separator: Option<NodeId>,
    panel_open: bool,
    panel: Option<IndicatorPanel>,
    empty_container: Option<NodeId>,
    empty_visible: Signal<bool>,
    empty_text: Signal<SmolStr>,
    empty_language: Option<i18n::language::codes::LanguageCode>,
    requests: Signal<Option<HeaderRequest>>,
    panel_requests: Signal<Option<PanelRequest>>,
    selection_requests: Signal<Option<InstrumentSelection>>,
    instrument: Option<HeaderInstrument>,
    controls: Option<HeaderControls>,
    dragging: bool,
    drag_last_x: Option<f32>,
    drag_remainder: f32,
    fetching_history: bool,
    fetching_newer: bool,
    history_requested_before: Option<i64>,
    newer_requested_after: Option<i64>,
    awaiting_market: bool,
    market_reloaded: bool,
    published_timeframes: Option<Rc<dyn Fn(&VenueRoute) -> Result<Vec<TimeFrame>, MarketError>>>,
    published_adjustments: Option<Rc<dyn Fn(&VenueRoute) -> Result<Vec<Adjustment>, MarketError>>>,
    #[cfg(test)]
    prefetched_history_newest: Option<NewestCandle>,
    #[cfg(test)]
    history_requests: usize,
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
        if !self.market_ready() {
            self.wait_for_market(cx);
            return;
        }
        let market_reloaded = std::mem::take(&mut self.market_reloaded);
        let retry = self.status.is_failed(cx.app());
        self.resolve_timeframe(cx);
        self.resolve_adjustment(cx);
        self.start_current(retry, market_reloaded, cx);
    }

    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    fn has_content(&self) -> bool {
        self.snapshot.is_some()
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if self.body_root.is_some() {
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
        cx.request_frame()?;
        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.drain_inbox(cx);

        let generation = *cx.read(&cx.services().catalog_generation);
        if generation != self.seen_generation {
            self.seen_generation = generation;
            if self.status.is_failed(cx.app()) {
            } else if self.awaiting_market {
                if self.market_ready() {
                    self.load(cx);
                }
            } else if !self.market_ready() {
                self.wait_for_market(cx);
            } else if self.config.selection.is_some() {
                self.market_reloaded = true;
                self.load(cx);
            }
        }

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
        if let Some(panel) = self.panel.as_mut()
            && panel.dispatch(cx, event)
        {
            return true;
        }
        let Event::Input(input) = event else {
            return false;
        };
        if matches!(
            input,
            InputEvent::PointerCancel { .. } | InputEvent::PointerLeave { .. }
        ) {
            if self.end_drag(None) {
                let _ = cx.request_frame();
            }
            return true;
        }
        if self.dragging
            && let InputEvent::PointerUp { .. } = input
        {
            let inside = pointer_position(input).filter(|position| self.chart_contains(*position));
            if self.end_drag(inside) {
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
                true
            }
            _ => false,
        }
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        if let Some(mut binding) = self.binding.take() {
            binding.cancel();
        }
        self.release_instrument(cx, false);
        self.release_controls(cx, false);
        self.release_panel(cx);
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
    fn assemble(
        config: CandlestickConfig,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let connector = cx.services().market_participants.clone();
        let service = connector.as_ref().map_or_else(Service::none, |connector| {
            Self::follow_source(connector.market(), cx)
        });
        let directory: Option<Rc<dyn MarketDirectory>> = connector.map(|connector| {
            Rc::new(ConnectorDirectory::new(connector)) as Rc<dyn MarketDirectory>
        });
        Self::assemble_with(
            config,
            service,
            directory,
            status,
            config_change,
            recovery,
            cx,
        )
    }

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

    fn assemble_with(
        config: CandlestickConfig,
        service: Service,
        directory: Option<Rc<dyn MarketDirectory>>,
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
            directory,
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

    #[cfg(test)]
    fn with_source<S: CandlestickSource>(
        config: CandlestickConfig,
        source: S,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        Self::with_source_and_directory(config, source, None, status, config_change, recovery, cx)
    }

    #[cfg(test)]
    fn with_source_and_directory<S: CandlestickSource>(
        config: CandlestickConfig,
        source: S,
        directory: Option<Rc<dyn MarketDirectory>>,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let service = Self::follow_source(source, cx);
        Self::assemble_with(
            config,
            service,
            directory,
            status,
            config_change,
            recovery,
            cx,
        )
    }

    fn market_ready(&self) -> bool {
        let Some(directory) = self.directory.as_deref() else {
            return false;
        };
        if !directory.any_market_loaded() {
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
        directory.is_loaded(route)
    }

    fn wait_for_market(&mut self, cx: &mut WidgetCx<'_>) {
        self.awaiting_market = true;
        self.clear_loaded_state(cx);
        self.close_indicator_panel(cx);
        self.report_notice(cx);
        let _ = cx.request_frame();
    }

    fn report_notice(&self, cx: &mut WidgetCx<'_>) {
        let notice = if self.awaiting_market {
            let venues = self
                .config
                .selection
                .as_ref()
                .map(|selection| vec![selection.listing.route.venue])
                .unwrap_or_else(market_venues);
            Notice::MarketNotLoaded {
                venues,
                supported: market_venues(),
            }
        } else {
            Notice::message("widget.candlestick.select_instrument_prompt")
        };
        self.status
            .report(cx.app_mut(), WidgetState::waiting(notice));
    }

    fn current_key(&self) -> Option<CandlestickStreamKey> {
        self.config.selection.as_ref().map(|selection| {
            CandlestickStreamKey::new(
                selection.listing.clone(),
                self.config.timeframe,
                self.config.adjustment,
            )
        })
    }

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

    fn start_current(&mut self, retry: bool, market_reloaded: bool, cx: &mut WidgetCx<'_>) {
        self.awaiting_market = false;
        self.generation = self.generation.wrapping_add(1);
        let Some(selection) = self.config.selection.clone() else {
            self.clear_loaded_state(cx);
            self.report_notice(cx);
            let _ = cx.request_frame();
            return;
        };
        if retry {
            self.fetching_history = false;
            self.fetching_newer = false;
            self.history_requested_before = None;
            self.newer_requested_after = None;
            self.pending.clear();
        } else {
            self.clear_loaded_state(cx);
        }
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        if let Some(directory) = self.directory.clone() {
            match directory.listing(&selection.listing) {
                ListingAnswer::Terminal => {
                    self.clear_selection("the listing no longer trades", cx);
                    return;
                }
                ListingAnswer::Missing => {
                    self.clear_selection("the route does not publish it", cx);
                    return;
                }
                ListingAnswer::Unserved => {
                    self.clear_selection("the directory does not serve it", cx);
                    return;
                }
                ListingAnswer::Unknown(error) => {
                    log::warn!(
                        "asking whether `{:?}` is published failed; the chart asks for the \
                         stream anyway: {error}",
                        selection.listing
                    );
                }
                ListingAnswer::Listed => {}
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
            return;
        };
        let request_id = binding.call(cx, request);
        self.pending.track(request_id, command);
        let _ = cx.request_frame();
    }

    fn apply_header_request(&mut self, request: HeaderRequest, cx: &mut WidgetCx<'_>) {
        match request {
            HeaderRequest::Timeframe(timeframe) => self.select_timeframe(timeframe, cx),
            HeaderRequest::Adjustment(adjustment) => self.select_adjustment(adjustment, cx),
            HeaderRequest::ToggleSettings => {
                if self.indicator_settings_available() {
                    self.toggle_indicator_panel(cx);
                }
            }
        }
    }

    fn apply_panel_request(&mut self, request: PanelRequest, cx: &mut WidgetCx<'_>) {
        match request {
            PanelRequest::SetIndicator(indicator) => self.set_indicator_config(indicator, cx),
        }
    }

    fn select_selection(&mut self, selection: InstrumentSelection, cx: &mut WidgetCx<'_>) {
        if let Some(directory) = self.directory.as_deref()
            && !directory.offers_candlesticks(&selection.listing.route)
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

    fn select_adjustment(&mut self, adjustment: Adjustment, cx: &mut WidgetCx<'_>) {
        if self.config.adjustment == adjustment && !self.status.is_failed(cx.app()) {
            return;
        }
        self.config.adjustment = adjustment;
        self.config_change.publish(cx.app_mut());
        self.start_current(false, false, cx);
    }

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

    fn indicator_settings_available(&self) -> bool {
        self.market_ready() && self.config.selection.is_some()
    }

    fn toggle_indicator_panel(&mut self, cx: &mut WidgetCx<'_>) {
        if self.panel_open {
            self.close_indicator_panel(cx);
        } else {
            self.panel_open = true;
        }
        let _ = cx.request_frame();
    }

    fn close_indicator_panel(&mut self, cx: &mut WidgetCx<'_>) {
        self.panel_open = false;
        let _ = cx.request_frame();
    }

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

    fn cancel_countdown(&mut self, cx: &mut WidgetCx<'_>) {
        if let Some(id) = self.countdown_timer.take() {
            cx.app_mut().cancel_timer(&id);
        }
    }

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

    fn chart_contains(&self, position: Point) -> bool {
        self.chart
            .bounds()
            .is_some_and(|bounds| bounds.contains(position))
    }

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
        let display = self.directory.as_ref().map(|directory| directory.display());
        if self.chart.sync(cx, &frame, display.as_ref())? {
            let _ = cx.request_frame();
        }
        Ok(())
    }

    fn apply_snapshot(&mut self, venue: &VenueId, snapshot: &CandlestickSeriesSnapshot) {
        self.snapshot = Some(snapshot.clone());
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
        if self.viewport.right_timestamp.is_some_and(|timestamp| {
            snapshot
                .latest_timestamp()
                .is_some_and(|latest| timestamp >= latest)
        }) {
            self.viewport.right_timestamp = None;
        }
    }

    fn prefetch_if_needed(&mut self, cx: &mut WidgetCx<'_>) {
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
        let margin = PREFETCH_MARGIN.max(self.config.indicator.warmup_depth());
        let following_the_live_edge =
            self.viewport.right_timestamp.is_none() && snapshot.at_live_edge();
        let room_for_a_page = MAXIMUM_CANDLES.saturating_sub(snapshot.candles().len())
            >= usize::from(HISTORY_PAGE_SIZE);
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
        match command {
            PendingCommand::Load
            | PendingCommand::Retry
            | PendingCommand::FetchHistory
            | PendingCommand::FetchNewer => self.fail_rpc(error, cx),
        }
    }

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
                CandlestickStatus::Ready => {
                    self.status.report(cx.app_mut(), WidgetState::Ready);
                }
                CandlestickStatus::Error(error) => {
                    if error.is_venue_not_loaded() {
                        self.wait_for_market(cx);
                        return;
                    }
                    if self.recover_unsupported_scope(error, cx) {
                        return;
                    }
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
            CandlestickStreamMessage::PageSettled { direction } => match direction {
                PageDirection::History => self.fetching_history = false,
                PageDirection::Newer => self.fetching_newer = false,
            },
            CandlestickStreamMessage::Snapshot(_) => {}
        }
    }

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

    fn apply_pointer(&mut self, position: Point) -> bool {
        if self.dragging {
            let Some(last_x) = self.drag_last_x.replace(position.x) else {
                return false;
            };
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

    fn zoom_at(&mut self, position: Point, delta_y: f32) -> bool {
        if delta_y == 0.0 {
            return false;
        }
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
            routes: self.loaded_scopes(),
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

    fn loaded_scopes(&self) -> Arc<[LoadedScope]> {
        self.directory
            .as_ref()
            .map_or_else(Arc::default, |directory| directory.loaded_scopes())
    }

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
        let available = self.indicator_settings_available();
        let label = t_smol!(translate, "widget.candlestick.indicator.settings");
        let key = cx.unique_key(SETTINGS_KEY_SUFFIX);
        let window = cx.window();
        let mut settings = Button::new(cx.app_mut(), theme.clone(), key)
            .ghost()
            .with_unified_size(UnifiedSize::Small)
            .icon(ui_kit::kit::icon::IconKind::Faders)
            .a11y_label(label.clone())
            .tooltip_text(label)
            .selected(self.panel_open)
            .disabled(!available);
        if available {
            settings = settings.on_click(move |action| {
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
        theme: &ui_kit::theme::handle::ThemeHandle,
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
        let layer = ui_kit::kit::layer::Layer::clone(cx.layer());
        let router = ui_kit::kit::overlay::AnimationRouter::clone(cx.animation());
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
        theme: &ui_kit::theme::handle::ThemeHandle,
    ) -> Result<Select, EngineError> {
        let translate = cx.translate().clone();
        let requests = self.requests;
        let window = cx.window();
        let key = cx.unique_key(TIMEFRAME_KEY_SUFFIX);
        let offered = self.offered_intervals();
        let disabled = offered.is_empty();
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
        let layer = ui_kit::kit::layer::Layer::clone(cx.layer());
        let router = ui_kit::kit::overlay::AnimationRouter::clone(cx.animation());
        let select = Select::new(cx.app_mut(), theme.clone(), layer, router, key, options)
            .on_change(move |app, chosen| {
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

    fn offered_adjustments(&self) -> Vec<Adjustment> {
        let Some(route) = self.asked_route() else {
            return Vec::new();
        };
        self.published_adjustments
            .as_ref()
            .and_then(|published_adjustments| published_adjustments(route).ok())
            .unwrap_or_default()
    }

    fn asked_route(&self) -> Option<&VenueRoute> {
        self.config
            .selection
            .as_ref()
            .filter(|_| !self.awaiting_market)
            .map(|selection| &selection.listing.route)
    }

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
        controls
            .settings
            .set_selected(cx.app_mut(), self.panel_open);
        let available = self.indicator_settings_available();
        controls.settings.set_disabled(cx.app_mut(), !available);
        if let Some(adjustment) = controls.adjustment.as_ref() {
            adjustment.set_chosen(cx.app_mut(), [self.config.adjustment.label()], false);
        }
        controls
            .timeframe
            .set_chosen(cx.app_mut(), [self.config.timeframe.label()], false);
        Ok(())
    }

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

const BODY_SELECTOR: &str = "CandlestickBody";
const EMPTY_SELECTOR: &str = "CandlestickEmptyOverlay";
const PANEL_PARENT_SELECTOR: &str = "CandlestickPanelParent";
const INSTRUMENT_KEY_SUFFIX: &str = "candlestick.header.instrument";
const SETTINGS_KEY_SUFFIX: &str = "candlestick.header.settings";
const ADJUSTMENT_KEY_SUFFIX: &str = "candlestick.header.adjustment";
const TIMEFRAME_KEY_SUFFIX: &str = "candlestick.header.timeframe";

fn venue_of(config: &CandlestickConfig) -> Option<&VenueId> {
    config
        .selection
        .as_ref()
        .map(|selection| &selection.listing.route.venue)
}

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

fn adjustment_of_label(label: &str) -> Option<Adjustment> {
    match label {
        "none" => Some(Adjustment::None),
        "forward" => Some(Adjustment::Forward),
        _ => None,
    }
}

#[cfg(test)]
mod tests;

use std::{mem, rc::Rc, sync::Arc};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use instrument_selector_ui::{host::SelectorHost, prediction_selector::PredictionEventSelector};
use market_participants_model::prediction::{
    PredictionEventRef, PredictionMarketRef, PredictionOrderBook, PredictionRange,
};
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    event::{Event, InputEvent, PointerButton},
    math::{LogicalPoint, LogicalPx, px},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::{
        style::StylePatch,
        style_builder::{PaintBuilder, StyleBuilder},
    },
    text::TextStyle,
    window::WindowId,
};
use ui_kit::{
    kit::{
        button::Button,
        empty_state::EmptyState,
        select::{Select, SelectItem, SelectOptions},
        size::{UnifiedSizable, UnifiedSize},
    },
    theme::{handle::ThemeHandle, role::Role},
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

use crate::{
    book::{
        DEFAULT_BOOK_RATIO, GROUPING_MULTIPLIERS, MAX_BOOK_RATIO, MIN_BOOK_RATIO,
        PredictionBookPanel, PredictionOrderBookError, PredictionOrderBookProtocol,
        PredictionOrderBookRequest, PredictionOrderBookService, PredictionOrderBookStatus,
        PredictionOrderBookStreamKey, PredictionOrderBookStreamMessage, clamp_ratio,
        grouping_steps, settle_grouping,
    },
    chart::canvas::ChartCanvas,
    directory::{MarketDirectory, book_venues, event_venues},
    model::{
        MAX_LINES, PredictionMarketSeries, PredictionMarketSnapshot, PredictionOrderBookTarget,
    },
    service::{
        PredictionMarketFetchError, PredictionMarketProtocol, PredictionMarketRequest,
        PredictionMarketService, PredictionMarketServiceError, PredictionMarketStatus,
        PredictionMarketStreamKey, PredictionMarketStreamMessage,
    },
};

const PULSE_DURATION_MS: u32 = 700;

const FRAME_TICK_MS: u32 = 16;

const FRAME_TICK: std::time::Duration = std::time::Duration::from_millis(16);

const BOOK_SPLITTER_HEIGHT: LogicalPx = LogicalPx::new(7.0);

const MIN_BOOK_HEIGHT: LogicalPx = LogicalPx::new(56.0);

const EVENT_KEY_SUFFIX: &str = "prediction_market.header.event";
const GROUP_KEY_SUFFIX: &str = "prediction_market.header.market_group";
const MARKET_KEY_SUFFIX: &str = "prediction_market.header.market";
const RANGE_KEY_SUFFIX: &str = "prediction_market.header.range";
const BOOK_TOGGLE_KEY_SUFFIX: &str = "prediction_market.header.book_toggle";
const BOOK_GROUPING_KEY_SUFFIX: &str = "prediction_market.header.book_grouping";
const BOOK_RETRY_STRIP_SUFFIX: &str = "prediction_market.book.retry.strip";
const BOOK_RETRY_PANEL_SUFFIX: &str = "prediction_market.book.retry.panel";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PredictionMarketError {
    Fetch(PredictionMarketFetchError),
    Rpc(widget::rpc::RpcCallError<PredictionMarketServiceError>),
}

impl TranslatedError for PredictionMarketError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Fetch(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
}

impl TranslatedError for PredictionMarketFetchError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Market(error) => error.translated_message(translate),
            Self::NoMarkets => t_smol!(translate, "error.prediction_market.no_markets"),
            Self::Timeout(timeout) => t_smol!(
                translate,
                "error.prediction_market.timeout",
                seconds = timeout.as_secs()
            ),
        }
    }
}

impl TranslatedError for PredictionMarketServiceError {
    fn translated_message(&self, _translate: &Translate) -> SmolStr {
        match *self {}
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct PredictionMarketConfig {
    #[serde(rename = "range")]
    pub range: PredictionRange,
    #[serde(rename = "order_book_visible")]
    pub order_book_visible: bool,
    #[serde(rename = "order_book_ratio")]
    pub order_book_ratio: f32,
    #[serde(rename = "order_book_grouping")]
    pub order_book_grouping: u32,
}

impl Default for PredictionMarketConfig {
    fn default() -> Self {
        Self {
            range: PredictionRange::default(),
            order_book_visible: true,
            order_book_ratio: DEFAULT_BOOK_RATIO,
            order_book_grouping: GROUPING_MULTIPLIERS[0],
        }
    }
}

impl PredictionMarketConfig {
    fn settle(&mut self) -> bool {
        let defaults = Self::default();
        let mut settled = false;
        if clamp_ratio(self.order_book_ratio) != self.order_book_ratio {
            self.order_book_ratio = defaults.order_book_ratio;
            settled = true;
        }
        if !GROUPING_MULTIPLIERS.contains(&self.order_book_grouping) {
            self.order_book_grouping = defaults.order_book_grouping;
            settled = true;
        }
        settled
    }
}

impl WidgetConfig for PredictionMarketConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

#[derive(Clone, Copy, Debug)]
struct Pulse {
    elapsed_ms: u32,
}

impl Pulse {
    fn phase(&self) -> Option<f32> {
        (self.elapsed_ms < PULSE_DURATION_MS)
            .then(|| self.elapsed_ms as f32 / PULSE_DURATION_MS as f32)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SplitDragStart {
    start_ratio: f32,
    start_y: LogicalPx,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    Load,
    Retry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BookPendingCommand {
    Load,
    Stop,
    Retry,
}

#[derive(Clone, Debug, PartialEq)]
enum HeaderRequest {
    Event(PredictionEventRef, SmolStr),
    Market(PredictionMarketRef),
    Range(PredictionRange),
    ToggleBook,
    Grouping(u32),
    RetryBook,
}

#[derive(Clone, Debug, PartialEq)]
enum BookRegion {
    Loading,
    Empty,
    Failed(Arc<PredictionOrderBookError>),
    Reading {
        failed: Option<Arc<PredictionOrderBookError>>,
    },
}

impl BookRegion {
    fn is_reading(&self) -> bool {
        matches!(self, Self::Reading { .. })
    }
}

#[derive(Clone, Debug, PartialEq)]
struct HeaderState {
    selection: Option<PredictionEventRef>,
    event_label: Option<SmolStr>,
    language: LanguageCode,
    generation: u64,
    market: Option<PredictionMarketRef>,
    markets: Vec<(SmolStr, SmolStr, SmolStr)>,
    groups: Vec<(SmolStr, SmolStr)>,
    book_supported: bool,
    book_visible: bool,
    grouping: Option<(u32, Vec<(u32, SmolStr)>)>,
}

enum HeaderComponent {
    Button(Button),
    Select(Select),
}

impl HeaderComponent {
    fn node(&self) -> Option<NodeId> {
        match self {
            Self::Button(button) => button.node(),
            Self::Select(select) => select.trigger_node(),
        }
    }

    fn release(self, app: &mut App) -> usize {
        match self {
            Self::Button(button) => button.release(app),
            Self::Select(select) => select.release(app),
        }
    }
}

struct HeaderGroupControls {
    parent: NodeId,
    components: Vec<HeaderComponent>,
}

struct HeaderControls {
    left: Option<HeaderGroupControls>,
    right: Option<HeaderGroupControls>,
    built: HeaderState,
}

struct BookChrome {
    area: NodeId,
    splitter: NodeId,
    strip: NodeId,
    status: NodeId,
    ladder: NodeId,
    strip_retry_parent: NodeId,
    status_retry_parent: NodeId,
    strip_retry: Button,
    strip_retry_node: NodeId,
    status_retry: Button,
    status_retry_node: NodeId,
    retry_language: LanguageCode,
}

pub struct PredictionMarketWidget {
    config: PredictionMarketConfig,
    config_change: WidgetConfigChange,
    status: WidgetStatus,
    recovery: WidgetAction,
    binding: Option<WidgetBinding<PredictionMarketProtocol>>,
    book_binding: Option<WidgetBinding<PredictionOrderBookProtocol>>,
    pending: Pending<PendingCommand>,
    book_pending: Pending<BookPendingCommand>,
    directory: Option<Rc<dyn MarketDirectory>>,
    selection: Option<PredictionEventRef>,
    selection_title: Option<SmolStr>,
    market: Option<PredictionMarketRef>,
    snapshot: Option<Arc<PredictionMarketSnapshot>>,
    snapshots: Signal<Option<Arc<PredictionMarketSnapshot>>>,
    book_region: Signal<BookRegion>,
    empty_text: Signal<SmolStr>,
    requests: Signal<Option<HeaderRequest>>,
    pulse_ticks: Signal<u64>,
    pulse_seen_ticks: u64,
    pulse: Option<Pulse>,
    pulse_timer: Option<ui_core::app::TimerId>,
    pointer: Option<LogicalPoint>,
    chart: ChartCanvas,
    book_panel: PredictionBookPanel,
    book_snapshot: Option<Arc<PredictionOrderBook>>,
    book_error: Option<Arc<PredictionOrderBookError>>,
    book_key: Option<PredictionOrderBookStreamKey>,
    book_epoch: u64,
    language: Signal<LanguageCode>,
    translate: Translate,
    theme: ThemeHandle,
    awaiting_market: bool,
    seen_generation: u64,
    generation: Signal<u64>,
    market_reloaded: bool,
    epoch: u64,
    body_root: Option<NodeId>,
    split_area: Option<NodeId>,
    book_chrome: Option<BookChrome>,
    book_style_ratio: Option<f32>,
    header: Option<HeaderControls>,
    last_published_snapshot: Option<Arc<PredictionMarketSnapshot>>,
    last_published_region: Option<BookRegion>,
    empty_text_language: Option<LanguageCode>,
    split_drag: Option<SplitDragStart>,
    split_dragging: bool,
    book_status_shown: Option<bool>,
    book_strip_shown: Option<bool>,
    book_ladder_shown: Option<bool>,
}

impl WidgetView for PredictionMarketWidget {
    type Config = PredictionMarketConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.prediction_market");
    const DISPLAY_NAME: &'static str = "widget.names.prediction_market";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.prediction_market");
    const ICON: Option<&'static str> = Some("widgets/prediction_market.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: WidgetStatus,
        _reload: ReloadHandle,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError> {
        let generation = cx.services().catalog_generation;
        let seen_generation = *cx.read(&generation);
        let language = cx.services().language;
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let snapshots = cx.signal(None::<Arc<PredictionMarketSnapshot>>);
        let book_region = cx.signal(BookRegion::Empty);
        let empty_text = cx.signal(SmolStr::new_static(""));
        let requests = cx.signal(None::<HeaderRequest>);
        let pulse_ticks = cx.signal(0_u64);

        let Some(connector) = cx.services().market_participants.clone() else {
            return Ok(Self::assemble(
                config,
                config_change,
                status,
                recovery,
                None,
                None,
                None,
                generation,
                seen_generation,
                language,
                translate,
                theme,
                snapshots,
                book_region,
                empty_text,
                requests,
                pulse_ticks,
            ));
        };
        let service = PredictionMarketService::new(connector.market());
        let binding = WidgetBinding::spawn(cx, service);
        let book_service = PredictionOrderBookService::new(connector.market());
        let book_binding = WidgetBinding::spawn(cx, book_service);
        let directory: Rc<dyn MarketDirectory> =
            Rc::new(crate::directory::ConnectorDirectory::new(connector));
        Ok(Self::assemble(
            config,
            config_change,
            status,
            recovery,
            Some(binding),
            Some(book_binding),
            Some(directory),
            generation,
            seen_generation,
            language,
            translate,
            theme,
            snapshots,
            book_region,
            empty_text,
            requests,
            pulse_ticks,
        ))
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        if self.config.settle() {
            log::warn!(
                "the prediction market's stored book ratio or grouping is not offered here; \
                 it was reset to the default"
            );
            self.config_change.publish(cx.app_mut());
        }
        let Some(directory) = self.directory.clone() else {
            self.wait_for_market(cx);
            return;
        };
        if !self.market_ready(directory.as_ref()) {
            self.wait_for_market(cx);
            return;
        }
        self.awaiting_market = false;
        let Some(selection) = self.selection.clone() else {
            self.status.report(
                cx.app_mut(),
                WidgetState::waiting(Notice::message(
                    "widget.prediction_market.select_event_prompt",
                )),
            );
            return;
        };
        let market_reloaded = mem::take(&mut self.market_reloaded);
        let retry = self.status.is_failed(cx.app());
        if !retry && !market_reloaded {
            self.snapshot = None;
            self.forget_chart_state(cx);
            self.clear_book(cx);
        } else if market_reloaded {
            self.stop_book(cx);
        }
        self.epoch = self.epoch.wrapping_add(1);
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        let request = if retry || market_reloaded {
            PredictionMarketRequest::Retry {
                event: selection,
                market: self.market.clone(),
                range: self.config.range,
                epoch: self.epoch,
            }
        } else {
            PredictionMarketRequest::Load {
                event: selection,
                market: self.market.clone(),
                range: self.config.range,
                epoch: self.epoch,
            }
        };
        self.pending.clear();
        if let Some(binding) = self.binding.as_mut() {
            let request_id = binding.call(cx, request);
            let command = if retry || market_reloaded {
                PendingCommand::Retry
            } else {
                PendingCommand::Load
            };
            self.pending.track(request_id, command);
        }
    }

    fn has_content(&self) -> bool {
        self.snapshot.is_some() && !self.awaiting_market
    }

    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let translate = self.translate.clone();
        let language = self.language;
        let snapshots = self.snapshots;
        let font_size = self.theme.tokens().small.font_size;

        let root = NodeBuilder::boxed()
            .id("PredictionMarketBody")
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .flex_col()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        self.body_root = Some(root);

        let legend = NodeBuilder::boxed()
            .id("TEST_PREDICTION_MARKET_LEGEND")
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .width_full()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .padding_x(px(8.0))
                    .padding_y(px(4.0))
                    .build(),
            )
            .build(cx.tree_mut(), root)?;
        for index in 0..MAX_LINES {
            let slot = NodeBuilder::boxed()
                .id(format_smolstr!("TEST_PREDICTION_MARKET_LEGEND_{index}"))
                .style(
                    StyleBuilder::new()
                        .flex_none()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .build(),
                )
                .bind_style(move |read| {
                    let snapshot = read.read(&snapshots);
                    let snapshot = &*snapshot;
                    let visible = snapshot
                        .as_ref()
                        .is_some_and(|snapshot| index < snapshot.series.len());
                    StylePatch {
                        visible: Some(visible),
                        ..StylePatch::default()
                    }
                })
                .build(cx.tree_mut(), legend)?;
            let dot_theme = self.theme.clone();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_none()
                        .size(px(8.0), px(8.0))
                        .build(),
                )
                .paint(PaintBuilder::new().rounded(px(4.0)).build())
                .bind_style(move |read| {
                    let snapshot = read.read(&snapshots);
                    let snapshot = &*snapshot;
                    let color_index = snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.series.get(index))
                        .map_or(0, |line| line.color);
                    StylePatch {
                        background: Some(dot_theme.role(read, series_role(color_index))),
                        ..StylePatch::default()
                    }
                })
                .build(cx.tree_mut(), slot)?;
            let text_theme = self.theme.clone();
            let text_language = language;
            NodeBuilder::text(SmolStr::default(), TextStyle::new(font_size))
                .bind_text(move |read| {
                    let _language = read.read(&text_language);
                    let snapshot = read.read(&snapshots);
                    let snapshot = &*snapshot;
                    snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.series.get(index))
                        .map(series_label)
                        .unwrap_or_default()
                })
                .bind_style(move |read| StylePatch {
                    foreground: Some(text_theme.role(read, Role::FgMuted)),
                    ..StylePatch::default()
                })
                .build(cx.tree_mut(), slot)?;
        }
        let omitted_language = language;
        let omitted_translate = translate;
        NodeBuilder::text(SmolStr::default(), TextStyle::new(font_size))
            .bind_text(move |read| {
                let _language = read.read(&omitted_language);
                let snapshot = read.read(&snapshots);
                let snapshot = &*snapshot;
                snapshot.as_ref().map_or_else(
                    || SmolStr::new_static(""),
                    |snapshot| {
                        if snapshot.omitted > 0 {
                            t_smol!(
                                omitted_translate,
                                "widget.prediction_market.more_lines",
                                count = snapshot.omitted
                            )
                        } else {
                            SmolStr::new_static("")
                        }
                    },
                )
            })
            .bind_style(move |read| {
                let snapshot = read.read(&snapshots);
                let snapshot = &*snapshot;
                let visible = snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.omitted > 0);
                StylePatch {
                    visible: Some(visible),
                    ..StylePatch::default()
                }
            })
            .build(cx.tree_mut(), legend)?;

        let split_area = NodeBuilder::boxed()
            .id("PredictionMarketChartArea")
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .flex_col()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .build(cx.tree_mut(), root)?;
        self.split_area = Some(split_area);

        let chart_wrap = NodeBuilder::boxed()
            .id("TEST_PREDICTION_MARKET_CHART_FRAME")
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .relative()
                    .min_width_0()
                    .min_height_0()
                    .overflow_hidden()
                    .build(),
            )
            .build(cx.tree_mut(), split_area)?;
        self.chart.build(cx, chart_wrap)?;

        let empty = self.empty_text;
        let empty_theme = self.theme.clone();
        let overlay = NodeBuilder::boxed()
            .id("TEST_PREDICTION_MARKET_EMPTY")
            .style(
                StyleBuilder::new()
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .size_full()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .build(),
            )
            .bind_style(move |read| {
                let snapshot = read.read(&snapshots);
                let snapshot = &*snapshot;
                let visible = snapshot
                    .as_ref()
                    .is_none_or(|snapshot| !has_points(snapshot.as_ref()));
                StylePatch {
                    visible: Some(visible),
                    ..StylePatch::default()
                }
            })
            .build(cx.tree_mut(), chart_wrap)?;
        EmptyState::new(empty_theme, "PredictionMarketEmpty")
            .bind_text(empty)
            .build(cx.tree_mut(), overlay)?;

        self.ensure_book_region(cx)?;
        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let generation = *cx.read(&self.generation);
        if generation != self.seen_generation {
            self.seen_generation = generation;
            if !self.status.is_failed(cx.app()) {
                self.market_reloaded = true;
                self.load(cx);
            }
        }

        if let Some(binding) = self.binding.as_ref() {
            for inbound in binding.take() {
                self.apply_market(inbound, cx);
            }
        }
        if let Some(binding) = self.book_binding.as_ref() {
            for inbound in binding.take() {
                self.apply_book(inbound, cx);
            }
        }

        let requested = (*cx.read(&self.requests)).clone();
        if let Some(request) = requested {
            cx.write(&self.requests, None);
            self.apply_request(request, cx);
        }

        self.advance_pulse(cx);

        let snapshot_moved = match (&self.last_published_snapshot, &self.snapshot) {
            (None, None) => false,
            (Some(published), Some(current)) => !Arc::ptr_eq(published, current),
            _ => true,
        };
        if snapshot_moved {
            self.last_published_snapshot = self.snapshot.clone();
            cx.write(&self.snapshots, self.snapshot.clone());
        }
        self.publish_empty_text(cx);

        if let Some(snapshot) = self.snapshot.clone() {
            let pulse_phase = self.pulse_phase(cx);
            self.chart.sync(cx, &snapshot, self.pointer, pulse_phase)?;
        }

        self.ensure_book_region(cx)?;
        self.sync_book_region(cx)?;
        self.sync_header(cx)?;

        if self.pulse.is_some() {
            self.schedule_pulse_tick(cx)?;
        }
        Ok(())
    }

    fn header(
        &mut self,
        group: HeaderGroup,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        if self.awaiting_market || !self.market_ready_now() {
            return Ok(());
        }
        self.release_header_group(group, cx, false);
        let state = self.header_state(cx);
        self.build_header_group(group, parent, &state, cx)?;
        if let Some(header) = self.header.as_mut() {
            header.built = state;
        }
        Ok(())
    }

    fn dispatch(&mut self, event: &Event, cx: &mut WidgetCx<'_>) -> bool {
        let Event::Input(input) = event else {
            return false;
        };
        match input {
            InputEvent::PointerMove { position, .. }
            | InputEvent::PointerEnter { position, .. } => {
                if self.split_drag.is_some() {
                    self.drag_book_splitter(*position, cx);
                    return true;
                }
                if self.pointer != Some(*position) {
                    self.pointer = Some(*position);
                    let _ = cx.request_frame();
                }
                true
            }
            InputEvent::PointerLeave { .. } => {
                if self.split_drag.is_some() {
                    return true;
                }
                if self.pointer.take().is_some() {
                    let _ = cx.request_frame();
                }
                true
            }
            InputEvent::PointerDown {
                position,
                button: PointerButton::Primary,
                ..
            } => {
                let Some(splitter) = self.book_chrome.as_ref().map(|chrome| chrome.splitter) else {
                    return false;
                };
                if !pointer_over(cx, splitter, *position) {
                    return false;
                }
                self.begin_book_split(*position);
                true
            }
            InputEvent::PointerUp {
                button: PointerButton::Primary,
                ..
            } => {
                if self.split_drag.is_none() {
                    return false;
                }
                self.finish_book_split(cx);
                true
            }
            _ => false,
        }
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        if let Some(mut binding) = self.binding.take() {
            binding.cancel();
        }
        if let Some(mut binding) = self.book_binding.take() {
            binding.cancel();
        }
        self.release_header_group(HeaderGroup::Left, cx, false);
        self.release_header_group(HeaderGroup::Right, cx, false);
        self.header = None;
        self.chart.release(cx);
        self.book_panel.release(cx);
        if let Some(timer) = self.pulse_timer.take() {
            cx.app_mut().cancel_timer(&timer);
        }
        if let Some(chrome) = self.book_chrome.take() {
            let _ = chrome.strip_retry.release(cx.app_mut());
            let _ = chrome.status_retry.release(cx.app_mut());
        }
        self.body_root = None;
        self.split_area = None;
        self.split_drag = None;
        self.split_dragging = false;
        self.snapshot = None;
        self.last_published_snapshot = None;
    }
}

impl PredictionMarketWidget {
    fn assemble(
        config: PredictionMarketConfig,
        config_change: WidgetConfigChange,
        status: WidgetStatus,
        recovery: WidgetAction,
        binding: Option<WidgetBinding<PredictionMarketProtocol>>,
        book_binding: Option<WidgetBinding<PredictionOrderBookProtocol>>,
        directory: Option<Rc<dyn MarketDirectory>>,
        generation: Signal<u64>,
        seen_generation: u64,
        language: Signal<LanguageCode>,
        translate: Translate,
        theme: ThemeHandle,
        snapshots: Signal<Option<Arc<PredictionMarketSnapshot>>>,
        book_region: Signal<BookRegion>,
        empty_text: Signal<SmolStr>,
        requests: Signal<Option<HeaderRequest>>,
        pulse_ticks: Signal<u64>,
    ) -> Self {
        Self {
            config,
            config_change,
            status,
            recovery,
            binding,
            book_binding,
            pending: Pending::new(),
            book_pending: Pending::new(),
            directory,
            selection: None,
            selection_title: None,
            market: None,
            snapshot: None,
            snapshots,
            book_region,
            empty_text,
            requests,
            pulse_ticks,
            pulse_seen_ticks: 0,
            pulse: None,
            pulse_timer: None,
            pointer: None,
            chart: ChartCanvas::new(),
            book_panel: PredictionBookPanel::new(),
            book_snapshot: None,
            book_error: None,
            book_key: None,
            book_epoch: 0,
            language,
            translate,
            theme,
            awaiting_market: false,
            seen_generation,
            generation,
            market_reloaded: false,
            epoch: 0,
            body_root: None,
            split_area: None,
            book_chrome: None,
            book_style_ratio: None,
            header: None,
            last_published_snapshot: None,
            last_published_region: None,
            empty_text_language: None,
            split_drag: None,
            split_dragging: false,
            book_status_shown: None,
            book_strip_shown: None,
            book_ladder_shown: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_source<S>(
        config: PredictionMarketConfig,
        source: S,
        directory: Rc<dyn MarketDirectory>,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self
    where
        S: crate::service::PredictionMarketSource + crate::book::PredictionOrderBookSource,
    {
        let generation = cx.services().catalog_generation;
        let seen_generation = *cx.read(&generation);
        let language = cx.services().language;
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let snapshots = cx.signal(None::<Arc<PredictionMarketSnapshot>>);
        let book_region = cx.signal(BookRegion::Empty);
        let empty_text = cx.signal(SmolStr::new_static(""));
        let requests = cx.signal(None::<HeaderRequest>);
        let pulse_ticks = cx.signal(0_u64);
        let service = PredictionMarketService::new(source.clone());
        let binding = WidgetBinding::spawn(cx, service);
        let book_service = PredictionOrderBookService::new(source);
        let book_binding = WidgetBinding::spawn(cx, book_service);
        Self::assemble(
            config,
            config_change,
            status,
            recovery,
            Some(binding),
            Some(book_binding),
            Some(directory),
            generation,
            seen_generation,
            language,
            translate,
            theme,
            snapshots,
            book_region,
            empty_text,
            requests,
            pulse_ticks,
        )
    }

    fn book_supported(event: &PredictionEventRef) -> bool {
        book_venues().contains(&event.route.venue)
    }

    fn supported_book_target(&self) -> Option<PredictionOrderBookTarget> {
        let target = self.snapshot.as_ref()?.book_target()?;
        Self::book_supported(&target.event).then_some(target)
    }

    fn shows_book(&self) -> bool {
        self.config.order_book_visible && self.supported_book_target().is_some()
    }

    fn market_ready(&self, directory: &dyn MarketDirectory) -> bool {
        if !directory.any_event_market_loaded() {
            return false;
        }
        match self.selection.as_ref() {
            Some(event) => directory.is_loaded(&event.route),
            None => true,
        }
    }

    fn market_ready_now(&self) -> bool {
        self.directory
            .as_ref()
            .is_some_and(|directory| self.market_ready(directory.as_ref()))
    }

    fn wait_for_market(&mut self, cx: &mut WidgetCx<'_>) {
        self.awaiting_market = true;
        self.snapshot = None;
        self.forget_chart_state(cx);
        self.clear_book(cx);
        self.pending.clear();
        self.book_pending.clear();
        self.report_markets_not_loaded(cx);
    }

    fn report_markets_not_loaded(&self, cx: &mut WidgetCx<'_>) {
        let supported = event_venues();
        let venues = self
            .selection
            .as_ref()
            .map(|event| vec![event.route.venue])
            .unwrap_or_else(|| supported.clone());
        self.status.report(
            cx.app_mut(),
            WidgetState::waiting(Notice::MarketNotLoaded { venues, supported }),
        );
    }

    fn current_key(&self) -> Option<PredictionMarketStreamKey> {
        self.selection
            .as_ref()
            .map(|event| PredictionMarketStreamKey {
                event: event.clone(),
                market: self.market.clone(),
                range: self.config.range,
                epoch: self.epoch,
            })
    }

    fn apply_request(&mut self, request: HeaderRequest, cx: &mut WidgetCx<'_>) {
        match request {
            HeaderRequest::Event(event, title) => self.select_event(event, title, cx),
            HeaderRequest::Market(market) => self.select_market(market, cx),
            HeaderRequest::Range(range) => self.select_range(range, cx),
            HeaderRequest::ToggleBook => self.toggle_book(cx),
            HeaderRequest::Grouping(multiplier) => self.select_book_grouping(multiplier, cx),
            HeaderRequest::RetryBook => self.retry_book(cx),
        }
    }

    fn select_event(&mut self, event: PredictionEventRef, title: SmolStr, cx: &mut WidgetCx<'_>) {
        self.selection = Some(event);
        self.selection_title = Some(title);
        self.market = None;
        self.snapshot = None;
        self.forget_chart_state(cx);
        self.clear_book(cx);
        self.load(cx);
        let _ = cx.request_frame();
    }

    fn select_market(&mut self, market: PredictionMarketRef, cx: &mut WidgetCx<'_>) {
        if self.market.as_ref() == Some(&market) && !self.status.is_failed(cx.app()) {
            return;
        }
        self.market = Some(market);
        self.snapshot = None;
        self.forget_chart_state(cx);
        self.clear_book(cx);
        self.load(cx);
        let _ = cx.request_frame();
    }

    fn select_range(&mut self, range: PredictionRange, cx: &mut WidgetCx<'_>) {
        if self.config.range == range {
            return;
        }
        self.config.range = range;
        self.config_change.publish(cx.app_mut());
        self.snapshot = None;
        self.forget_chart_state(cx);
        self.load(cx);
        let _ = cx.request_frame();
    }

    fn toggle_book(&mut self, cx: &mut WidgetCx<'_>) {
        self.config.order_book_visible = !self.config.order_book_visible;
        if self.config.order_book_visible {
            self.sync_book_target(cx);
        } else {
            self.stop_book(cx);
        }
        self.config_change.publish(cx.app_mut());
        let _ = cx.request_frame();
    }

    fn select_book_grouping(&mut self, multiplier: u32, cx: &mut WidgetCx<'_>) {
        let Some(book) = self.book_snapshot.as_ref() else {
            return;
        };
        let settled = settle_grouping(book.tick_size, multiplier);
        if settled == self.config.order_book_grouping {
            return;
        }
        self.config.order_book_grouping = settled;
        self.config_change.publish(cx.app_mut());
        self.book_panel.set_multiplier(settled);
        let _ = cx.request_frame();
    }

    fn stop_book(&mut self, cx: &mut WidgetCx<'_>) {
        self.split_drag = None;
        self.split_dragging = false;
        if let Some(binding) = self.book_binding.as_mut() {
            let request_id = binding.call(cx, PredictionOrderBookRequest::Stop);
            self.book_pending
                .track(request_id, BookPendingCommand::Stop);
        }
        self.book_key = None;
    }

    fn clear_book(&mut self, cx: &mut WidgetCx<'_>) {
        self.stop_book(cx);
        self.book_snapshot = None;
        self.book_error = None;
        self.book_epoch = self.book_epoch.wrapping_add(1);
        self.book_panel.clear();
    }

    fn sync_book_target(&mut self, cx: &mut WidgetCx<'_>) {
        if !self.config.order_book_visible {
            return;
        }
        let Some(target) = self.supported_book_target() else {
            if self.book_key.is_some() || self.book_snapshot.is_some() || self.book_error.is_some()
            {
                self.clear_book(cx);
                let _ = cx.request_frame();
            }
            return;
        };
        if self
            .book_key
            .as_ref()
            .is_some_and(|key| key.event == target.event && key.leg == target.leg)
        {
            return;
        }
        let same_ladder = self
            .book_snapshot
            .as_ref()
            .is_some_and(|book| book.leg == target.leg);
        self.book_epoch = self.book_epoch.wrapping_add(1);
        if !same_ladder {
            self.book_snapshot = None;
            self.book_error = None;
            self.book_panel.clear();
        }
        self.book_key = Some(PredictionOrderBookStreamKey {
            event: target.event.clone(),
            leg: target.leg.clone(),
            epoch: self.book_epoch,
        });
        if let Some(binding) = self.book_binding.as_mut() {
            let request_id = binding.call(
                cx,
                PredictionOrderBookRequest::Load {
                    target,
                    epoch: self.book_epoch,
                },
            );
            self.book_pending
                .track(request_id, BookPendingCommand::Load);
        }
        let _ = cx.request_frame();
    }

    fn retry_book(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(target) = self.supported_book_target() else {
            return;
        };
        self.book_epoch = self.book_epoch.wrapping_add(1);
        self.book_error = None;
        self.book_key = Some(PredictionOrderBookStreamKey {
            event: target.event.clone(),
            leg: target.leg.clone(),
            epoch: self.book_epoch,
        });
        if let Some(binding) = self.book_binding.as_mut() {
            let request_id = binding.call(
                cx,
                PredictionOrderBookRequest::Retry {
                    target,
                    epoch: self.book_epoch,
                },
            );
            self.book_pending
                .track(request_id, BookPendingCommand::Retry);
        }
        let _ = cx.request_frame();
    }

    fn book_loading(&self) -> bool {
        self.book_key.is_some() && self.book_snapshot.is_none() && self.book_error.is_none()
    }

    fn book_leg_label(&self) -> Option<SmolStr> {
        let (Some(key), Some(snapshot)) = (self.book_key.as_ref(), self.snapshot.as_ref()) else {
            return None;
        };
        snapshot
            .legs
            .iter()
            .find(|leg| leg.leg == key.leg)
            .map(|leg| leg.label.clone())
    }

    fn book_region(&self) -> BookRegion {
        if self.book_snapshot.is_some() {
            BookRegion::Reading {
                failed: self.book_error.clone(),
            }
        } else if let Some(error) = self.book_error.clone() {
            BookRegion::Failed(error)
        } else if self.book_loading() {
            BookRegion::Loading
        } else {
            BookRegion::Empty
        }
    }

    fn apply_market(&mut self, inbound: Inbound<PredictionMarketProtocol>, cx: &mut WidgetCx<'_>) {
        match inbound {
            Inbound::Reliable { key, message } => self.apply_market_reliable(key, &message, cx),
            Inbound::Latest { key, message } => self.apply_market_latest(key, &message, cx),
            Inbound::Rpc { request_id, result } => self.apply_market_rpc(request_id, result, cx),
        }
    }

    fn apply_market_reliable(
        &mut self,
        key: PredictionMarketStreamKey,
        message: &PredictionMarketStreamMessage,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx.app())
            || self.awaiting_market
        {
            return;
        }
        let PredictionMarketStreamMessage::Status(status) = message else {
            return;
        };
        match status {
            PredictionMarketStatus::Connecting => {
                self.status.report(cx.app_mut(), WidgetState::Connecting);
            }
            PredictionMarketStatus::Ready { .. } => {
                if self.snapshot.is_some() {
                    self.status.report(cx.app_mut(), WidgetState::Ready);
                }
            }
            PredictionMarketStatus::Failed { error } => {
                if let PredictionMarketFetchError::Market(market) = error
                    && (market_is_catalog_not_loaded(market) || market.is_venue_not_loaded())
                {
                    self.awaiting_market = true;
                    self.snapshot = None;
                    self.forget_chart_state(cx);
                    self.clear_book(cx);
                    self.report_markets_not_loaded(cx);
                    let _ = cx.request_frame();
                    return;
                }
                self.stop_book(cx);
                let recovery = self.recovery.clone();
                self.status.report(
                    cx.app_mut(),
                    WidgetState::failed(
                        Arc::new(PredictionMarketError::Fetch(error.clone())),
                        recovery,
                    ),
                );
            }
        }
    }

    fn apply_market_latest(
        &mut self,
        key: PredictionMarketStreamKey,
        message: &PredictionMarketStreamMessage,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx.app())
            || self.awaiting_market
        {
            return;
        }
        let PredictionMarketStreamMessage::Snapshot(snapshot) = message else {
            return;
        };
        if self
            .snapshot
            .as_ref()
            .is_some_and(|held| snapshot.revision < held.revision)
        {
            return;
        }
        let newest_before = self
            .snapshot
            .as_ref()
            .and_then(|held| held.latest_timestamp());
        let newest = snapshot.latest_timestamp();
        self.snapshot = Some(snapshot.clone());
        self.selection_title = Some(snapshot.title.clone());
        self.sync_book_target(cx);
        self.status.report(cx.app_mut(), WidgetState::Ready);
        let breathing = self.pulse.and_then(|pulse| pulse.phase()).is_some();
        let reduce_motion = *cx.read(&cx.services().reduce_motion.clone());
        if !reduce_motion
            && !breathing
            && newest_before
                .is_some_and(|previous| newest.is_some_and(|current| current > previous))
        {
            self.pulse = Some(Pulse { elapsed_ms: 0 });
        }
    }

    fn apply_market_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<PredictionMarketProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        match self.pending.take(request_id) {
            Some(PendingCommand::Load | PendingCommand::Retry) => {
                if self.status.is_failed(cx.app()) || self.awaiting_market {
                    return;
                }
                if let Err(error) = result {
                    let recovery = self.recovery.clone();
                    self.status.report(
                        cx.app_mut(),
                        WidgetState::failed(Arc::new(PredictionMarketError::Rpc(error)), recovery),
                    );
                }
            }
            None => {}
        }
    }

    fn apply_book(&mut self, inbound: Inbound<PredictionOrderBookProtocol>, cx: &mut WidgetCx<'_>) {
        match inbound {
            Inbound::Reliable { key, message } => self.apply_book_reliable(key, &message, cx),
            Inbound::Latest { key, message } => self.apply_book_latest(key, &message, cx),
            Inbound::Rpc { request_id, result } => self.apply_book_rpc(request_id, result, cx),
        }
    }

    fn apply_book_reliable(
        &mut self,
        key: PredictionOrderBookStreamKey,
        message: &PredictionOrderBookStreamMessage,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.book_key.as_ref() != Some(&key)
            || self.status.is_failed(cx.app())
            || self.awaiting_market
        {
            return;
        }
        let PredictionOrderBookStreamMessage::Status(status) = message else {
            return;
        };
        let changed = match status {
            PredictionOrderBookStatus::Connecting => {
                let changed = self.book_error.is_some();
                self.book_error = None;
                changed
            }
            PredictionOrderBookStatus::Ready => false,
            PredictionOrderBookStatus::Failed { error } => {
                let changed = self.book_error.as_deref() != Some(error);
                self.book_error = Some(Arc::new(error.clone()));
                changed
            }
        };
        if changed {
            let _ = cx.request_frame();
        }
    }

    fn apply_book_latest(
        &mut self,
        key: PredictionOrderBookStreamKey,
        message: &PredictionOrderBookStreamMessage,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.book_key.as_ref() != Some(&key)
            || self.status.is_failed(cx.app())
            || self.awaiting_market
        {
            return;
        }
        let PredictionOrderBookStreamMessage::Snapshot(book) = message else {
            return;
        };
        if self.book_error.is_some() {
            return;
        }
        let has_levels = !book.bids.is_empty() || !book.asks.is_empty();
        let multiplier = if has_levels {
            let settled = settle_grouping(book.tick_size, self.config.order_book_grouping);
            if settled != self.config.order_book_grouping {
                self.config.order_book_grouping = settled;
                self.config_change.publish(cx.app_mut());
            }
            settled
        } else {
            self.config.order_book_grouping
        };
        self.book_snapshot = Some(book.clone());
        self.book_panel.set_leg_label(self.book_leg_label());
        self.book_panel.set_book(book.clone(), multiplier);
        if self.config.order_book_visible {
            let _ = cx.request_frame();
        }
    }

    fn apply_book_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<PredictionOrderBookProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        match self.book_pending.take(request_id) {
            Some(BookPendingCommand::Load | BookPendingCommand::Retry) => {
                if let Err(error) = result {
                    log::warn!("the prediction order book request failed: {error}");
                    self.book_error = Some(Arc::new(PredictionOrderBookError::Rpc(error)));
                    let _ = cx.request_frame();
                }
            }
            Some(BookPendingCommand::Stop) | None => {}
        }
    }

    fn ensure_book_region(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let shows = self.shows_book();
        match (shows, self.book_chrome.is_some()) {
            (true, false) => self.build_book_region(cx),
            (false, true) => {
                let chrome = self.book_chrome.take();
                self.release_book_region(cx, chrome);
                self.book_style_ratio = None;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn build_book_region(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(area_parent) = self.split_area else {
            return Ok(());
        };
        let theme = self.theme.clone();
        let language = *cx.read(&self.language);
        let language_signal = self.language;
        let requests = self.requests;
        let window = cx.window();
        let ratio = clamp_ratio(self.config.order_book_ratio);
        let region_signal = self.book_region;
        let translate = self.translate.clone();

        let splitter = NodeBuilder::boxed()
            .id("TEST_PREDICTION_MARKET_SPLITTER")
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .width_full()
                    .height(BOOK_SPLITTER_HEIGHT)
                    .flex_row()
                    .items_center()
                    .build(),
            )
            .build(cx.tree_mut(), area_parent)?;
        let line_theme = theme.clone();
        NodeBuilder::boxed()
            .id("PredictionMarketSplitterLine")
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .width_full()
                    .height(px(1.0))
                    .build(),
            )
            .bind_style(move |read| StylePatch {
                background: Some(line_theme.role(read, Role::LineBorder)),
                ..StylePatch::default()
            })
            .build(cx.tree_mut(), splitter)?;

        let mut area_style = StyleBuilder::new()
            .flex_none()
            .width_full()
            .flex_col()
            .build();
        area_style
            .height_percent(ratio)
            .min_height_length(MIN_BOOK_HEIGHT);
        let area = NodeBuilder::boxed()
            .id("TEST_PREDICTION_MARKET_BOOK_AREA")
            .style(area_style)
            .build(cx.tree_mut(), area_parent)?;

        let strip = NodeBuilder::boxed()
            .id("TEST_PREDICTION_MARKET_BOOK_ERROR")
            .style(book_strip_style())
            .build(cx.tree_mut(), area)?;
        let strip_text_theme = theme.clone();
        let strip_text_language = language_signal;
        let strip_text_translate = translate.clone();
        NodeBuilder::text(SmolStr::default(), TextStyle::new(px(11.0)))
            .bind_text(move |read| {
                let _language = read.read(&strip_text_language);
                let region = read.read(&region_signal);
                match &*region {
                    BookRegion::Reading {
                        failed: Some(error),
                    } => error.translated_message(&strip_text_translate),
                    _ => SmolStr::new_static(""),
                }
            })
            .bind_style(move |read| StylePatch {
                foreground: Some(strip_text_theme.role(read, Role::FgMuted)),
                ..StylePatch::default()
            })
            .build(cx.tree_mut(), strip)?;
        let strip_retry_parent = NodeBuilder::boxed()
            .style(StyleBuilder::new().flex_none().build())
            .build(cx.tree_mut(), strip)?;

        let status = NodeBuilder::boxed()
            .id("TEST_PREDICTION_MARKET_BOOK_STATUS")
            .style(book_status_style())
            .build(cx.tree_mut(), area)?;
        let empty_wrap = NodeBuilder::boxed()
            .style(overlay_center_style())
            .bind_style(move |read| {
                let region = read.read(&region_signal);
                StylePatch {
                    visible: Some(matches!(&*region, BookRegion::Empty)),
                    ..StylePatch::default()
                }
            })
            .build(cx.tree_mut(), status)?;
        let empty_theme = theme.clone();
        EmptyState::new(empty_theme, "PredictionMarketBookEmpty")
            .bind_text(self.empty_text)
            .build(cx.tree_mut(), empty_wrap)?;
        let status_text_theme = theme.clone();
        let status_text_language = language_signal;
        let status_text_translate = translate.clone();
        let status_text_wrap = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .size_full()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .build(),
            )
            .bind_style(move |read| {
                let _language = read.read(&status_text_language);
                let region = read.read(&region_signal);
                StylePatch {
                    visible: Some(matches!(
                        &*region,
                        BookRegion::Loading | BookRegion::Failed(_)
                    )),
                    ..StylePatch::default()
                }
            })
            .build(cx.tree_mut(), status)?;
        NodeBuilder::text(SmolStr::default(), TextStyle::new(px(12.0)))
            .bind_text(move |read| {
                let _language = read.read(&status_text_language);
                let region = read.read(&region_signal);
                book_status_text(&region, &status_text_translate)
            })
            .bind_style(move |read| StylePatch {
                foreground: Some(status_text_theme.role(read, Role::FgMuted)),
                ..StylePatch::default()
            })
            .build(cx.tree_mut(), status_text_wrap)?;
        let status_retry_parent = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .size_full()
                    .flex_col()
                    .items_center()
                    .justify_end()
                    .padding_y(px(4.0))
                    .build(),
            )
            .bind_style(move |read| {
                let region = read.read(&region_signal);
                StylePatch {
                    visible: Some(matches!(&*region, BookRegion::Failed(_))),
                    ..StylePatch::default()
                }
            })
            .build(cx.tree_mut(), status)?;

        let ladder = NodeBuilder::boxed()
            .id("PredictionMarketBookLadder")
            .style(book_ladder_style())
            .build(cx.tree_mut(), area)?;
        self.book_panel.build(cx, ladder)?;

        let strip_retry = build_retry_button(
            cx,
            strip_retry_parent,
            BOOK_RETRY_STRIP_SUFFIX,
            requests,
            window,
            theme.clone(),
            &translate,
        )?;
        let status_retry = build_retry_button(
            cx,
            status_retry_parent,
            BOOK_RETRY_PANEL_SUFFIX,
            requests,
            window,
            theme,
            &translate,
        )?;

        let strip_retry_node = strip_retry.node().unwrap_or(strip_retry_parent);
        let status_retry_node = status_retry.node().unwrap_or(status_retry_parent);

        self.book_chrome = Some(BookChrome {
            area,
            splitter,
            strip,
            status,
            ladder,
            strip_retry_parent,
            status_retry_parent,
            strip_retry,
            strip_retry_node,
            status_retry,
            status_retry_node,
            retry_language: language,
        });
        self.book_style_ratio = Some(ratio);
        self.book_status_shown = None;
        self.book_strip_shown = None;
        self.book_ladder_shown = None;
        Ok(())
    }

    fn release_book_region(&mut self, cx: &mut WidgetCx<'_>, chrome: Option<BookChrome>) {
        let Some(chrome) = chrome else {
            return;
        };
        let window = cx.window();
        self.book_panel.release(cx);
        for node in [chrome.area, chrome.splitter] {
            if cx.tree().get(node).is_ok() {
                let _ = cx.app_mut().remove_node(window, node);
            }
        }
        let _ = chrome.strip_retry.release(cx.app_mut());
        let _ = chrome.status_retry.release(cx.app_mut());
    }

    fn sync_book_region(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let region = self.book_region();
        if self.last_published_region.as_ref() != Some(&region) {
            self.last_published_region = Some(region.clone());
            cx.write(&self.book_region, region.clone());
        }
        if self.book_chrome.is_none() {
            return Ok(());
        }
        self.sync_book_region_layout(cx, &region)?;
        let ratio = clamp_ratio(self.config.order_book_ratio);
        let area = self.book_chrome.as_ref().map(|chrome| chrome.area);
        if self.book_style_ratio != Some(ratio)
            && let Some(area) = area
        {
            let mut style = StyleBuilder::new()
                .flex_none()
                .width_full()
                .flex_col()
                .build();
            style
                .height_percent(ratio)
                .min_height_length(MIN_BOOK_HEIGHT);
            cx.tree_mut().set_style(area, style)?;
            self.book_style_ratio = Some(ratio);
        }
        let language = *cx.read(&self.language);
        let retry_language = self
            .book_chrome
            .as_ref()
            .map(|chrome| chrome.retry_language);
        if retry_language != Some(language) {
            self.rebuild_book_retries(cx)?;
        }
        if region.is_reading() {
            self.book_panel.sync(cx)?;
        }
        Ok(())
    }

    fn sync_book_region_layout(
        &mut self,
        cx: &mut WidgetCx<'_>,
        region: &BookRegion,
    ) -> Result<(), EngineError> {
        let Some(chrome) = self.book_chrome.as_ref() else {
            return Ok(());
        };
        let (status_shown, strip_shown, ladder_shown) = (
            !region.is_reading(),
            matches!(region, BookRegion::Reading { failed: Some(_) }),
            region.is_reading(),
        );
        if self.book_status_shown != Some(status_shown) {
            let style = if status_shown {
                book_status_style()
            } else {
                collapsed_style()
            };
            cx.tree_mut().set_style(chrome.status, style)?;
            self.book_status_shown = Some(status_shown);
        }
        if self.book_strip_shown != Some(strip_shown) {
            let style = if strip_shown {
                book_strip_style()
            } else {
                collapsed_style()
            };
            cx.tree_mut().set_style(chrome.strip, style)?;
            self.book_strip_shown = Some(strip_shown);
        }
        if self.book_ladder_shown != Some(ladder_shown) {
            let style = if ladder_shown {
                book_ladder_style()
            } else {
                collapsed_style()
            };
            cx.tree_mut().set_style(chrome.ladder, style)?;
            self.book_ladder_shown = Some(ladder_shown);
        }
        Ok(())
    }

    fn rebuild_book_retries(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(chrome) = self.book_chrome.as_mut() else {
            return Ok(());
        };
        let window = cx.window();
        let theme = self.theme.clone();
        let translate = self.translate.clone();
        let requests = self.requests;
        let language = *cx.read(&self.language);
        for node in [chrome.strip_retry_node, chrome.status_retry_node] {
            if cx.tree().get(node).is_ok() {
                let _ = cx.app_mut().remove_node(window, node);
            }
        }
        let strip_retry = build_retry_button(
            cx,
            chrome.strip_retry_parent,
            BOOK_RETRY_STRIP_SUFFIX,
            requests,
            window,
            theme.clone(),
            &translate,
        )?;
        let status_retry = build_retry_button(
            cx,
            chrome.status_retry_parent,
            BOOK_RETRY_PANEL_SUFFIX,
            requests,
            window,
            theme,
            &translate,
        )?;
        chrome.strip_retry_node = strip_retry.node().unwrap_or(chrome.strip_retry_parent);
        chrome.status_retry_node = status_retry.node().unwrap_or(chrome.status_retry_parent);
        chrome.strip_retry = strip_retry;
        chrome.status_retry = status_retry;
        chrome.retry_language = language;
        Ok(())
    }

    fn publish_empty_text(&mut self, cx: &mut WidgetCx<'_>) {
        let language = *cx.read(&self.language);
        if self.empty_text_language == Some(language) {
            return;
        }
        self.empty_text_language = Some(language);
        let text = t_smol!(self.translate, "common.no_data");
        cx.write(&self.empty_text, text);
    }

    fn selector_host(&self, cx: &WidgetCx<'_>) -> SelectorHost {
        let host = SelectorHost::new(Rc::clone(cx.services_rc()));
        match &self.directory {
            Some(directory) => host.with_catalog(directory.catalog()),
            None => host,
        }
    }

    fn header_state(&self, cx: &WidgetCx<'_>) -> HeaderState {
        let snapshot = self.snapshot.as_ref();
        let mut markets = Vec::new();
        let mut groups = Vec::new();
        if let Some(snapshot) = snapshot {
            for choice in snapshot.markets.iter() {
                markets.push((
                    SmolStr::new(choice.market.native_market.as_str()),
                    choice.label.clone(),
                    choice.group.key.clone(),
                ));
                if !groups.iter().any(|(key, _)| key == &choice.group.key) {
                    groups.push((choice.group.key.clone(), choice.group.label.clone()));
                }
            }
        }
        let selected = snapshot.and_then(|snapshot| snapshot.selected_choice(self.market.as_ref()));
        let grouping = self.book_snapshot.as_ref().and_then(|book| {
            self.book_panel.has_grouping_choice().then(|| {
                (
                    settle_grouping(book.tick_size, self.config.order_book_grouping),
                    grouping_steps(book.tick_size),
                )
            })
        });
        HeaderState {
            selection: self.selection.clone(),
            event_label: self
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.title.clone())
                .or_else(|| self.selection_title.clone()),
            language: *cx.read(&self.language),
            generation: self.seen_generation,
            market: selected.map(|choice| choice.market.clone()),
            markets,
            groups,
            book_supported: self.supported_book_target().is_some(),
            book_visible: self.config.order_book_visible,
            grouping,
        }
    }

    fn build_header_group(
        &mut self,
        group: HeaderGroup,
        parent: NodeId,
        state: &HeaderState,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let mut components = Vec::new();
        match group {
            HeaderGroup::Left => {
                components.push(HeaderComponent::Button(
                    self.build_event_button(parent, cx)?,
                ));
                if let Some(snapshot) = self.snapshot.clone()
                    && let Some(selected) = snapshot.selected_choice(self.market.as_ref()).cloned()
                {
                    if state.groups.len() > 1 {
                        components.push(HeaderComponent::Select(
                            self.build_group_select(parent, &snapshot, &selected, cx)?,
                        ));
                    }
                    components.push(HeaderComponent::Select(
                        self.build_market_select(parent, &snapshot, &selected, cx)?,
                    ));
                }
            }
            HeaderGroup::Right => {
                if state.book_supported {
                    components.push(HeaderComponent::Button(self.build_book_toggle(parent, cx)?));
                    if state.book_visible
                        && let Some(select) = self.build_grouping_select(parent, state, cx)?
                    {
                        components.push(HeaderComponent::Select(select));
                    }
                }
                components.push(HeaderComponent::Select(
                    self.build_range_select(parent, cx)?,
                ));
            }
        }
        let controls = HeaderGroupControls { parent, components };
        let header = self.header.get_or_insert_with(|| HeaderControls {
            left: None,
            right: None,
            built: state.clone(),
        });
        match group {
            HeaderGroup::Left => header.left = Some(controls),
            HeaderGroup::Right => header.right = Some(controls),
        }
        Ok(())
    }

    fn release_header_group(
        &mut self,
        group: HeaderGroup,
        cx: &mut WidgetCx<'_>,
        remove_nodes: bool,
    ) {
        let controls = self.header.as_mut().and_then(|header| match group {
            HeaderGroup::Left => header.left.take(),
            HeaderGroup::Right => header.right.take(),
        });
        let Some(controls) = controls else {
            return;
        };
        let window = cx.window();
        for component in controls.components {
            let node = component.node();
            if remove_nodes
                && let Some(node) = node
                && cx.tree().get(node).is_ok()
            {
                let _ = cx.app_mut().remove_node(window, node);
            }
            let _ = component.release(cx.app_mut());
        }
    }

    fn sync_header(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(built) = self.header.as_ref().map(|header| header.built.clone()) else {
            return Ok(());
        };
        if self.awaiting_market || !self.market_ready_now() {
            self.release_header_group(HeaderGroup::Left, cx, true);
            self.release_header_group(HeaderGroup::Right, cx, true);
            self.header = None;
            return Ok(());
        }
        let state = self.header_state(cx);
        if built == state {
            return Ok(());
        }
        for group in [HeaderGroup::Left, HeaderGroup::Right] {
            let parent = self.header.as_ref().and_then(|header| match group {
                HeaderGroup::Left => header.left.as_ref(),
                HeaderGroup::Right => header.right.as_ref(),
            });
            let Some(parent) = parent.map(|controls| controls.parent) else {
                continue;
            };
            if cx.tree().get(parent).is_err() {
                self.release_header_group(group, cx, false);
                continue;
            }
            self.release_header_group(group, cx, true);
            self.build_header_group(group, parent, &state, cx)?;
        }
        if let Some(header) = self.header.as_mut() {
            header.built = state;
        }
        Ok(())
    }

    fn build_event_button(
        &self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Button, EngineError> {
        let host = self.selector_host(cx);
        let key = cx.unique_key(EVENT_KEY_SUFFIX);
        let translate = self.translate.clone();
        let label = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.title.clone())
            .or_else(|| self.selection_title.clone())
            .unwrap_or_else(|| t_smol!(translate, "widget.prediction_market.select_event"));
        let label_for_a11y = label.clone();
        let selection = self.selection.clone();
        let title = self.selection_title.clone();
        let requests = self.requests;
        let window = cx.window();
        let button = Button::new(cx.app_mut(), self.theme.clone(), key)
            .primary()
            .with_unified_size(UnifiedSize::Small)
            .label(label)
            .a11y_label(label_for_a11y)
            .on_click(move |ctx| {
                let app = ctx.app_mut();
                let host = host.clone();
                let requests = requests;
                PredictionEventSelector::show(
                    selection.clone(),
                    title.clone(),
                    Rc::new(move |event, title, app, window| {
                        hand_over_request(
                            requests,
                            HeaderRequest::Event(event, title),
                            app,
                            window,
                        );
                    }),
                    &host,
                    app,
                    window,
                );
            });
        button.build(cx.app_mut(), window, parent)?;
        Ok(button)
    }

    fn build_group_select(
        &self,
        parent: NodeId,
        snapshot: &PredictionMarketSnapshot,
        selected: &crate::model::PredictionMarketChoice,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Select, EngineError> {
        let mut items = Vec::with_capacity(snapshot.market_groups().len());
        for group in snapshot.market_groups() {
            items.push(SelectItem::new(group.key.clone(), group.label.clone()));
        }
        let chosen = selected.group.key.clone();
        let markets = snapshot.markets.clone();
        let requests = self.requests;
        let choices = Arc::new(markets);
        let options = SelectOptions::default()
            .size(UnifiedSize::Small)
            .label(t_smol!(
                self.translate,
                "widget.prediction_market.select_market_group"
            ))
            .placeholder(selected.group.label.clone());
        self.build_select(
            parent,
            cx,
            GROUP_KEY_SUFFIX,
            options,
            items,
            chosen,
            move |app, window, id| {
                let Some(market) = choices
                    .iter()
                    .find(|choice| choice.group.key.as_str() == id)
                    .map(|choice| choice.market.clone())
                else {
                    return;
                };
                hand_over_request(requests, HeaderRequest::Market(market), app, window);
            },
        )
    }

    fn build_market_select(
        &self,
        parent: NodeId,
        snapshot: &PredictionMarketSnapshot,
        selected: &crate::model::PredictionMarketChoice,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Select, EngineError> {
        let mut items = Vec::new();
        for choice in snapshot.markets_in_group(&selected.group) {
            items.push(SelectItem::new(
                SmolStr::new(choice.market.native_market.as_str()),
                choice.label.clone(),
            ));
        }
        let chosen = SmolStr::new(selected.market.native_market.as_str());
        let markets = Arc::new(snapshot.markets.clone());
        let group_key = selected.group.key.clone();
        let requests = self.requests;
        let options = SelectOptions::default()
            .size(UnifiedSize::Small)
            .label(t_smol!(
                self.translate,
                "widget.prediction_market.select_market"
            ))
            .placeholder(selected.label.clone());
        self.build_select(
            parent,
            cx,
            MARKET_KEY_SUFFIX,
            options,
            items,
            chosen,
            move |app, window, id| {
                let Some(market) = markets
                    .iter()
                    .find(|choice| {
                        choice.group.key == group_key && choice.market.native_market.as_str() == id
                    })
                    .map(|choice| choice.market.clone())
                else {
                    return;
                };
                hand_over_request(requests, HeaderRequest::Market(market), app, window);
            },
        )
    }

    fn build_range_select(
        &self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Select, EngineError> {
        let mut items = Vec::with_capacity(PredictionRange::ALL.len());
        for range in PredictionRange::ALL {
            items.push(SelectItem::new(
                range_id(range),
                range_label(range, &self.translate),
            ));
        }
        let requests = self.requests;
        let options = SelectOptions::default()
            .size(UnifiedSize::Small)
            .label(t_smol!(
                self.translate,
                "widget.prediction_market.timeframe"
            ))
            .placeholder(range_label(self.config.range, &self.translate));
        self.build_select(
            parent,
            cx,
            RANGE_KEY_SUFFIX,
            options,
            items,
            SmolStr::new_static(range_id(self.config.range)),
            move |app, window, id| {
                let Some(range) = range_of_id(id) else {
                    return;
                };
                hand_over_request(requests, HeaderRequest::Range(range), app, window);
            },
        )
    }

    fn build_grouping_select(
        &self,
        parent: NodeId,
        state: &HeaderState,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Option<Select>, EngineError> {
        let Some((chosen, steps)) = state.grouping.as_ref() else {
            return Ok(None);
        };
        let items: Vec<SelectItem> = steps
            .iter()
            .map(|(multiplier, label)| {
                SelectItem::new(format_smolstr!("{multiplier}"), label.clone())
            })
            .collect();
        let requests = self.requests;
        let options = SelectOptions::default()
            .size(UnifiedSize::Small)
            .label(t_smol!(
                self.translate,
                "widget.prediction_market.book.price_grouping"
            ))
            .placeholder(
                self.book_panel
                    .grouping_label()
                    .unwrap_or_else(|| format_smolstr!("{chosen}")),
            );
        self.build_select(
            parent,
            cx,
            BOOK_GROUPING_KEY_SUFFIX,
            options,
            items,
            format_smolstr!("{chosen}"),
            move |app, window, id| {
                let Some(multiplier) = id.parse::<u32>().ok() else {
                    return;
                };
                hand_over_request(requests, HeaderRequest::Grouping(multiplier), app, window);
            },
        )
        .map(Some)
    }

    fn build_select(
        &self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
        suffix: &str,
        options: SelectOptions,
        items: Vec<SelectItem>,
        chosen: SmolStr,
        on_change: impl Fn(&mut App, WindowId, &str) + 'static,
    ) -> Result<Select, EngineError> {
        let key = cx.unique_key(suffix);
        let window = cx.window();
        let select = Select::new(cx.app_mut(), self.theme.clone(), key, options).on_change(
            move |app, chosen| {
                let Some(id) = chosen.first() else {
                    return;
                };
                on_change(app, window, id.as_str());
            },
        );
        select
            .set_items(cx.app_mut(), items)
            .unwrap_or_else(|error| {
                log::warn!(
                    "the prediction market's picker `{suffix}` could not set its items: {error}"
                );
            });
        select.set_chosen(cx.app_mut(), [chosen.as_str()], false);
        select.build(cx.app_mut(), window, parent)?;
        Ok(select)
    }

    fn build_book_toggle(
        &self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Button, EngineError> {
        let key = cx.unique_key(BOOK_TOGGLE_KEY_SUFFIX);
        let requests = self.requests;
        let window = cx.window();
        let label = t_smol!(self.translate, "widget.prediction_market.book.title");
        let button = Button::new(cx.app_mut(), self.theme.clone(), key)
            .ghost()
            .with_unified_size(UnifiedSize::Small)
            .selected(self.config.order_book_visible)
            .label(label.clone())
            .a11y_label(label)
            .on_click(move |ctx| {
                hand_over_request(requests, HeaderRequest::ToggleBook, ctx.app_mut(), window);
            });
        button.build(cx.app_mut(), window, parent)?;
        Ok(button)
    }

    fn pulse_phase(&mut self, cx: &WidgetCx<'_>) -> Option<f32> {
        if *cx.read(&cx.services().reduce_motion) {
            self.pulse = None;
            return None;
        }
        let pulse = self.pulse?;
        if pulse.phase().is_none() {
            self.pulse = None;
            return None;
        }
        pulse.phase()
    }

    fn advance_pulse(&mut self, cx: &mut WidgetCx<'_>) {
        let ticks = *cx.read(&self.pulse_ticks);
        if ticks == self.pulse_seen_ticks {
            return;
        }
        self.pulse_seen_ticks = ticks;
        self.pulse_timer = None;
        let Some(pulse) = self.pulse.as_mut() else {
            return;
        };
        pulse.elapsed_ms = pulse.elapsed_ms.saturating_add(FRAME_TICK_MS);
        if pulse.phase().is_none() {
            self.pulse = None;
        }
    }

    fn schedule_pulse_tick(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if self.pulse_timer.is_some() {
            return Ok(());
        }
        let Some(root) = self.body_root else {
            return Ok(());
        };
        let window = cx.window();
        let ticks = self.pulse_ticks;
        let timer = cx
            .app_mut()
            .after_scoped(window, root, FRAME_TICK, move |app| {
                app.update(&ticks, |ticks| *ticks += 1);
                let _ = app.request_repaint(window);
            })?;
        self.pulse_timer = Some(timer);
        Ok(())
    }

    fn forget_chart_state(&mut self, cx: &mut WidgetCx<'_>) {
        self.pointer = None;
        self.pulse = None;
        if let Some(timer) = self.pulse_timer.take() {
            cx.app_mut().cancel_timer(&timer);
        }
        self.chart.clear();
    }

    fn begin_book_split(&mut self, position: LogicalPoint) {
        self.split_drag = Some(SplitDragStart {
            start_ratio: clamp_ratio(self.config.order_book_ratio),
            start_y: position.y,
        });
        self.split_dragging = false;
    }

    fn drag_book_splitter(&mut self, position: LogicalPoint, cx: &mut WidgetCx<'_>) {
        let Some(start) = self.split_drag else {
            return;
        };
        let Some(area) = self.split_area else {
            return;
        };
        let Ok(node) = cx.tree().get(area) else {
            return;
        };
        let height = node.layout().bounds.size.height;
        let ratio = crate::book::split_ratio(start.start_ratio, position.y - start.start_y, height);
        let ratio = if height > px(0.0) {
            ratio
                .max(MIN_BOOK_RATIO.max(MIN_BOOK_HEIGHT / height))
                .min(MAX_BOOK_RATIO)
        } else {
            ratio
        };
        if ratio == self.config.order_book_ratio {
            return;
        }
        self.config.order_book_ratio = ratio;
        self.split_dragging = true;
        let _ = cx.request_frame();
    }

    fn finish_book_split(&mut self, cx: &mut WidgetCx<'_>) {
        let start = self.split_drag.take();
        let dragged = mem::take(&mut self.split_dragging);
        let changed =
            dragged && start.is_some_and(|start| self.config.order_book_ratio != start.start_ratio);
        if changed {
            self.config_change.publish(cx.app_mut());
            let _ = cx.request_frame();
        }
    }
}

fn pointer_over(cx: &WidgetCx<'_>, node: NodeId, position: LogicalPoint) -> bool {
    cx.tree()
        .get(node)
        .is_ok_and(|node| node.layout().bounds.contains(position))
}

fn book_strip_style() -> ui_core::paint::style::Style {
    StyleBuilder::new()
        .flex_none()
        .width_full()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(4.0))
        .padding_x(px(8.0))
        .padding_y(px(2.0))
        .build()
}

fn book_status_style() -> ui_core::paint::style::Style {
    StyleBuilder::new()
        .flex_1()
        .min_height_0()
        .relative()
        .build()
}

fn book_ladder_style() -> ui_core::paint::style::Style {
    StyleBuilder::new()
        .flex_1()
        .min_height_0()
        .flex_col()
        .build()
}

fn collapsed_style() -> ui_core::paint::style::Style {
    StyleBuilder::new()
        .flex_none()
        .width_full()
        .height(px(0.0))
        .overflow_hidden()
        .build()
}

fn overlay_center_style() -> ui_core::paint::style::Style {
    StyleBuilder::new()
        .absolute()
        .top(px(0.0))
        .left(px(0.0))
        .size_full()
        .flex_row()
        .items_center()
        .justify_center()
        .build()
}

fn has_points(snapshot: &PredictionMarketSnapshot) -> bool {
    snapshot.series.iter().any(|line| !line.points.is_empty())
}

fn series_role(index: usize) -> Role {
    const ROLES: [Role; 6] = [
        Role::ChartSeries0,
        Role::ChartSeries1,
        Role::ChartSeries2,
        Role::ChartSeries3,
        Role::ChartSeries4,
        Role::ChartSeries5,
    ];
    ROLES[index % ROLES.len()]
}

fn series_label(line: &PredictionMarketSeries) -> SmolStr {
    match line.latest {
        Some(latest) => format_smolstr!(
            "{} {:.0}%",
            line.label,
            latest.ratio().inner() as f64 / 1e16
        ),
        None => line.label.clone(),
    }
}

fn book_status_text(region: &BookRegion, translate: &Translate) -> SmolStr {
    match region {
        BookRegion::Loading => t_smol!(translate, "common.loading"),
        BookRegion::Failed(error) => error.translated_message(translate),
        BookRegion::Empty | BookRegion::Reading { .. } => SmolStr::new_static(""),
    }
}

fn build_retry_button(
    cx: &mut WidgetCx<'_>,
    parent: NodeId,
    suffix: &str,
    requests: Signal<Option<HeaderRequest>>,
    window: WindowId,
    theme: ThemeHandle,
    translate: &Translate,
) -> Result<Button, EngineError> {
    let key = cx.unique_key(suffix);
    let button = Button::new(cx.app_mut(), theme, key)
        .ghost()
        .with_unified_size(UnifiedSize::Small)
        .label(t_smol!(translate, "common.retry"))
        .on_click(move |ctx| {
            hand_over_request(requests, HeaderRequest::RetryBook, ctx.app_mut(), window);
        });
    button.build(cx.app_mut(), window, parent)?;
    Ok(button)
}

fn hand_over_request(
    requests: Signal<Option<HeaderRequest>>,
    request: HeaderRequest,
    app: &mut App,
    window: WindowId,
) {
    app.write(&requests, Some(request));
    let _ = app.request_repaint(window);
}

fn range_label(range: PredictionRange, translate: &Translate) -> SmolStr {
    match range {
        PredictionRange::Hour1 => t_smol!(translate, "widget.prediction_market.range.hour_1"),
        PredictionRange::Hour6 => t_smol!(translate, "widget.prediction_market.range.hour_6"),
        PredictionRange::Day1 => t_smol!(translate, "widget.prediction_market.range.day_1"),
        PredictionRange::Week1 => t_smol!(translate, "widget.prediction_market.range.week_1"),
        PredictionRange::Month1 => t_smol!(translate, "widget.prediction_market.range.month_1"),
        PredictionRange::All => t_smol!(translate, "widget.prediction_market.range.all"),
    }
}

fn range_id(range: PredictionRange) -> &'static str {
    match range {
        PredictionRange::Hour1 => "hour_1",
        PredictionRange::Hour6 => "hour_6",
        PredictionRange::Day1 => "day_1",
        PredictionRange::Week1 => "week_1",
        PredictionRange::Month1 => "month_1",
        PredictionRange::All => "all",
    }
}

fn range_of_id(id: &str) -> Option<PredictionRange> {
    PredictionRange::ALL
        .into_iter()
        .find(|range| range_id(*range) == id)
}

#[cfg(test)]
mod tests;

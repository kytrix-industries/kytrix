use std::{
    cell::Cell,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use ahash::{HashMap, HashMapExt};
use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    kit::{
        button::Button,
        icon::IconKind,
        list::{List, ListOptions, SelectionMode},
        size::UnifiedSizable,
        table::{Column, ColumnAlign, Table, TableOptions},
    },
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_service_provider::service::{
    connector::KytrixServiceConnector,
    engine::{FinancialIndexTopic, KytrixServiceError},
};
use serde::{Deserialize, Deserializer, Serialize};
use smol_str::SmolStr;
use ui_core::{
    a11y::A11yPatch,
    app::{App, TimerId},
    error::EngineError,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::{primitive::Border, style::StylePatch},
    text::{TextAlign, TextOverflow, TextStyle, TextWrap},
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
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

#[cfg(test)]
use super::service::FinancialIndexSource;
use super::{
    dialog::IndexSelector,
    model::{
        FinancialIndexDataError, FinancialIndexDirection, FinancialIndexQuote,
        SelectedFinancialIndex, format_percent, format_price, format_signed_cents,
        normalize_selection,
    },
    service::{
        FinancialIndexProtocol, FinancialIndexRequest, FinancialIndexResponse, FinancialIndexScope,
        FinancialIndexService, FinancialIndexServiceError, FinancialIndexStatus,
        FinancialIndexStreamError, FinancialIndexStreamMessage,
    },
};

const FLASH_DURATION: Duration = Duration::from_millis(350);
const FLASH_PEAK: Duration = Duration::from_millis(100);
const FLASH_TICK: Duration = Duration::from_millis(16);
const GRID_FLASH_HEIGHT: f32 = 6.0;
const GRID_CELL_WIDTH: f32 = 160.0;
const GRID_CELL_HEIGHT: f32 = 88.0;
const COLUMN_COUNT: usize = 4;
const COLUMN_WEIGHTS: [f32; COLUMN_COUNT] = [0.4, 0.2, 0.2, 0.2];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum FinancialIndexDisplayMode {
    #[default]
    Grid,
    Table,
}

impl FinancialIndexDisplayMode {
    const fn icon(self) -> IconKind {
        match self {
            Self::Grid => IconKind::SquaresFour,
            Self::Table => IconKind::Table,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FinancialIndexConfig {
    selection: Vec<SelectedFinancialIndex>,
    mode: FinancialIndexDisplayMode,
}

impl Default for FinancialIndexConfig {
    fn default() -> Self {
        Self {
            selection: Vec::new(),
            mode: FinancialIndexDisplayMode::Grid,
        }
    }
}

impl WidgetConfig for FinancialIndexConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

impl<'de> Deserialize<'de> for FinancialIndexConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            selection: Vec<SelectedFinancialIndex>,
            mode: FinancialIndexDisplayMode,
        }

        let wire = Wire::deserialize(deserializer)?;

        Ok(Self {
            selection: normalize_selection(&wire.selection).map_err(serde::de::Error::custom)?,
            mode: wire.mode,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FinancialIndexFailure {
    Stream(FinancialIndexStreamError),
    Rpc(RpcCallError<FinancialIndexServiceError>),
    Selection(FinancialIndexDataError),
}

impl TranslatedError for FinancialIndexFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Stream(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
            Self::Selection(error) => error.translated_message(translate),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FinancialIndexRow {
    index: SelectedFinancialIndex,
    quote: Option<FinancialIndexQuote>,
}

#[derive(Clone, Copy, Debug)]
struct QuoteFlash {
    direction: FinancialIndexDirection,
    started_at: Instant,
}

impl QuoteFlash {
    fn is_active(self, now: Instant) -> bool {
        now.saturating_duration_since(self.started_at) < FLASH_DURATION
    }

    fn opacity(self, now: Instant) -> f32 {
        flash_opacity(now.saturating_duration_since(self.started_at))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct FlashVisual {
    direction: FinancialIndexDirection,
    opacity: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GridLayout {
    pub columns: usize,
    pub lines: usize,
    pub cell_height: f32,
}

pub(crate) fn grid_layout(viewport_width: f32, item_count: usize) -> GridLayout {
    if item_count == 0 {
        return GridLayout {
            columns: 0,
            lines: 0,
            cell_height: GRID_CELL_HEIGHT,
        };
    }
    let columns = ((viewport_width / GRID_CELL_WIDTH).floor() as usize)
        .max(1)
        .min(item_count);
    GridLayout {
        columns,
        lines: item_count.div_ceil(columns),
        cell_height: GRID_CELL_HEIGHT,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HeaderRequest {
    Choose,
    Mode(FinancialIndexDisplayMode),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StreamCommand {
    Load,
    Retry,
    Stop,
}

#[derive(Clone, Debug)]
struct ManifestSettlement {
    asked: Vec<SelectedFinancialIndex>,
    retry: bool,
}

struct HeaderLeft {
    parent: NodeId,
    wrapper: NodeId,
    choose: Button,
    built: (LanguageCode, bool),
}

struct ModeControl {
    wrapper: NodeId,
    button: Button,
}

struct HeaderRight {
    parent: NodeId,
    grid: ModeControl,
    table: ModeControl,
    built: LanguageCode,
}

struct GridBody {
    wrapper: NodeId,
    list: List,
    columns: Rc<Cell<usize>>,
    count: Rc<Cell<usize>>,
    built_columns: usize,
    built_lines: usize,
}

struct TableBody {
    wrapper: NodeId,
    table: Table,
}

enum BodyContent {
    Grid(GridBody),
    Table(TableBody),
}

impl BodyContent {
    fn wrapper(&self) -> NodeId {
        match self {
            Self::Grid(grid) => grid.wrapper,
            Self::Table(table) => table.wrapper,
        }
    }
}

pub struct FinancialIndexWidget {
    config: FinancialIndexConfig,
    stream: Option<WidgetBinding<FinancialIndexProtocol>>,
    manifest: Option<WidgetBinding<FinancialIndexProtocol>>,
    stream_pending: Pending<StreamCommand>,
    manifest_pending: Pending<ManifestSettlement>,
    status: WidgetStatus,
    recovery: WidgetAction,
    config_change: WidgetConfigChange,
    translate: Translate,
    theme: ThemeHandle,
    language: Signal<LanguageCode>,
    account_id: Option<SmolStr>,
    quotes: Option<Arc<HashMap<FinancialIndexTopic, FinancialIndexQuote>>>,
    accepted: Option<FinancialIndexScope>,
    ready: bool,
    rows: Arc<[FinancialIndexRow]>,
    rows_signal: Signal<Arc<[FinancialIndexRow]>>,
    busy: bool,
    busy_signal: Signal<bool>,
    rows_dirty: bool,
    flashes: HashMap<FinancialIndexTopic, QuoteFlash>,
    flash_signal: Signal<Arc<HashMap<FinancialIndexTopic, FlashVisual>>>,
    flash_timer: Option<TimerId>,
    flash_timer_fired: Rc<Cell<bool>>,
    content_slot: Option<NodeId>,
    content: Option<BodyContent>,
    built_mode: Option<FinancialIndexDisplayMode>,
    built_language: Option<LanguageCode>,
    selector: Option<IndexSelector>,
    requests: Signal<Option<HeaderRequest>>,
    confirmed: Signal<Option<Vec<SelectedFinancialIndex>>>,
    header_left: Option<HeaderLeft>,
    header_right: Option<HeaderRight>,
}

impl WidgetView for FinancialIndexWidget {
    type Config = FinancialIndexConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.financial_index");
    const DISPLAY_NAME: &'static str = "widget.names.financial_index";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.financial_index");
    const ICON: Option<&'static str> = Some("widgets/indices.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: WidgetStatus,
        _reload: ReloadHandle,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError> {
        let source = cx.services().kytrix_service().cloned();
        Ok(Self::assemble(
            config,
            source,
            status,
            config_change,
            recovery,
            cx,
        ))
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        let retry = self.status.is_failed(cx.app());
        self.account_id = cx.account();
        self.start(retry, cx);
    }

    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    fn has_content(&self) -> bool {
        self.accepted.is_some() && self.accepted == self.current_scope()
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let translate = self.translate.clone();
        let language = self.language;
        let frame = NodeBuilder::boxed()
            .id("FinancialIndexFrame")
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .flex_col()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .bind_a11y(move |read| {
                let _language = *read.read(&language);
                A11yPatch {
                    label: Some(t_smol!(translate, "widget.names.financial_index")),
                    ..A11yPatch::default()
                }
            })
            .build(cx.tree_mut(), parent)?;
        self.content_slot = Some(frame);
        self.build_content(frame, cx)?;
        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let account = cx.account();
        if account != self.account_id {
            self.account_id = account;
            self.start(false, cx);
        }

        if let Some(binding) = self.stream.as_ref() {
            for inbound in binding.take() {
                self.apply_stream(inbound, cx);
            }
        }
        if let Some(binding) = self.manifest.as_ref() {
            for inbound in binding.take() {
                self.apply_manifest(inbound, cx);
            }
        }

        let request = *cx.read(&self.requests);
        if let Some(request) = request {
            cx.write(&self.requests, None);
            self.apply_request(request, cx)?;
        }

        let confirmed = cx.read(&self.confirmed).clone();
        if let Some(selection) = confirmed {
            cx.write(&self.confirmed, None);
            self.select_indices(selection, cx);
        }

        if let Some(selector) = self.selector.as_mut() {
            selector.sync(cx)?;
            if !selector.is_open(cx.app()) {
                selector.release(cx);
                self.selector = None;
            }
        }

        let language = *cx.read(&self.language);
        let stale = self.built_mode != Some(self.config.mode)
            || (matches!(self.config.mode, FinancialIndexDisplayMode::Table)
                && self.built_language != Some(language));
        if self.content.is_some() && stale {
            self.rebuild_content(cx)?;
        }

        if self.rows_dirty {
            self.rows_dirty = false;
            self.publish_rows(cx);
            self.publish_rows_to_content(cx)?;
        }
        self.sync_grid_columns(cx)?;
        self.settle_flashes(cx);
        self.sync_header(cx)?;

        if self.has_content() && self.content.is_none() {
            let _ = cx.request_frame();
        }

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
                self.release_header_left(cx, true)?;
                self.build_header_left(parent, cx)
            }
            HeaderGroup::Right => {
                self.release_header_right(cx, true)?;
                self.build_header_right(parent, cx)
            }
        }
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        self.stop_flash_timer(cx.app_mut());
        if let Some(mut selector) = self.selector.take() {
            selector.release(cx);
        }
        self.release_content(cx);
        if let Err(error) = self.release_header(cx, false) {
            log::error!("the financial index header could not be released: {error}");
        }
        self.stream = None;
        self.manifest = None;
    }
}

impl FinancialIndexWidget {
    fn assemble(
        config: FinancialIndexConfig,
        source: Option<KytrixServiceConnector>,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let stream = source
            .clone()
            .map(|source| WidgetBinding::spawn(cx, FinancialIndexService::new(source)));
        let manifest =
            source.map(|source| WidgetBinding::spawn(cx, FinancialIndexService::new(source)));
        Self::with_bindings(
            config,
            stream,
            manifest,
            status,
            config_change,
            recovery,
            cx,
        )
    }

    #[cfg(test)]
    fn with_source<S: FinancialIndexSource>(
        config: FinancialIndexConfig,
        source: S,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let stream = WidgetBinding::spawn(cx, FinancialIndexService::new(source.clone()));
        let manifest = WidgetBinding::spawn(cx, FinancialIndexService::new(source));
        Self::with_bindings(
            config,
            Some(stream),
            Some(manifest),
            status,
            config_change,
            recovery,
            cx,
        )
    }

    fn with_bindings(
        config: FinancialIndexConfig,
        stream: Option<WidgetBinding<FinancialIndexProtocol>>,
        manifest: Option<WidgetBinding<FinancialIndexProtocol>>,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let rows_signal: Signal<Arc<[FinancialIndexRow]>> = cx.signal(Arc::from([]));
        let busy_signal: Signal<bool> = cx.signal(false);
        let flash_signal: Signal<Arc<HashMap<FinancialIndexTopic, FlashVisual>>> =
            cx.signal(Arc::new(HashMap::new()));
        let requests: Signal<Option<HeaderRequest>> = cx.signal(None);
        let confirmed: Signal<Option<Vec<SelectedFinancialIndex>>> = cx.signal(None);
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let language = cx.language();
        let account_id = cx.account();

        Self {
            config,
            stream,
            manifest,
            stream_pending: Pending::new(),
            manifest_pending: Pending::new(),
            status,
            recovery,
            config_change,
            translate,
            theme,
            language,
            account_id,
            quotes: None,
            accepted: None,
            ready: false,
            rows: Arc::from([]),
            rows_signal,
            busy: false,
            busy_signal,
            rows_dirty: false,
            flashes: HashMap::new(),
            flash_signal,
            flash_timer: None,
            flash_timer_fired: Rc::new(Cell::new(false)),
            content_slot: None,
            content: None,
            built_mode: None,
            built_language: None,
            selector: None,
            requests,
            confirmed,
            header_left: None,
            header_right: None,
        }
    }

    fn current_scope(&self) -> Option<FinancialIndexScope> {
        FinancialIndexScope::new(self.account_id.as_ref(), &self.config.selection)
    }

    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) {
        self.ready = false;
        self.flashes.clear();
        if self.current_scope().is_none() {
            self.manifest_pending.clear();
            self.follow(retry, cx);
            return;
        }
        if !retry {
            self.clear_quotes(cx);
        }
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        let _ = cx.request_frame();
        let asked = self.config.selection.clone();
        let Some(binding) = self.manifest.as_mut() else {
            self.follow(retry, cx);
            return;
        };
        let request_id = binding.call(cx, FinancialIndexRequest::FetchManifest);
        self.manifest_pending
            .track(request_id, ManifestSettlement { asked, retry });
    }

    fn follow(&mut self, retry: bool, cx: &mut WidgetCx<'_>) {
        let Some(scope) = self.current_scope() else {
            self.clear_quotes(cx);
            let notice = if self.account_id.is_none() {
                Notice::message("widget.financial_index.logged_out")
            } else {
                Notice::message("widget.financial_index.select_at_least_one")
            };
            self.status
                .report(cx.app_mut(), WidgetState::waiting(notice));
            if let Some(binding) = self.stream.as_mut() {
                let request_id = binding.call(cx, FinancialIndexRequest::Stop);
                self.stream_pending.track(request_id, StreamCommand::Stop);
            }
            let _ = cx.request_frame();
            return;
        };
        let Some(binding) = self.stream.as_mut() else {
            self.clear_quotes(cx);
            self.status.report(
                cx.app_mut(),
                WidgetState::failed(
                    Arc::new(FinancialIndexFailure::Stream(
                        FinancialIndexStreamError::Service(KytrixServiceError::Closed),
                    )),
                    self.recovery.clone(),
                ),
            );
            let _ = cx.request_frame();
            return;
        };
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        let (command, request) = if retry {
            (StreamCommand::Retry, FinancialIndexRequest::Retry(scope))
        } else {
            (StreamCommand::Load, FinancialIndexRequest::Load(scope))
        };
        let request_id = binding.call(cx, request);
        self.stream_pending.track(request_id, command);
        let _ = cx.request_frame();
    }

    fn clear_quotes(&mut self, cx: &mut WidgetCx<'_>) {
        self.quotes = None;
        self.accepted = None;
        self.ready = false;
        self.flashes.clear();
        self.publish_rows(cx);
    }

    fn apply_stream(&mut self, inbound: Inbound<FinancialIndexProtocol>, cx: &mut WidgetCx<'_>) {
        match inbound {
            Inbound::Reliable { key, message } => self.apply_reliable(key, &message, cx),
            Inbound::Latest { key, message } => self.apply_latest(key, &message, cx),
            Inbound::Rpc { request_id, result } => {
                self.apply_stream_rpc(request_id, result, cx);
            }
        }
    }

    fn apply_stream_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<FinancialIndexProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        let Some(command) = self.stream_pending.take(request_id) else {
            log::debug!(
                "the financial index pane ignored reply {request_id}: no command is waiting for it"
            );
            return;
        };
        let Err(error) = result else {
            return;
        };
        if !matches!(command, StreamCommand::Load | StreamCommand::Retry) {
            return;
        }
        if self.status.is_failed(cx.app()) || self.current_scope().is_none() {
            return;
        }
        let recovery = self.recovery.clone();
        self.status.report(
            cx.app_mut(),
            WidgetState::failed(Arc::new(FinancialIndexFailure::Rpc(error)), recovery),
        );
        let _ = cx.request_frame();
    }

    fn apply_manifest(&mut self, inbound: Inbound<FinancialIndexProtocol>, cx: &mut WidgetCx<'_>) {
        let Inbound::Rpc { request_id, result } = inbound else {
            return;
        };
        let Some(pending) = self.manifest_pending.take(request_id) else {
            log::debug!(
                "the financial index pane ignored manifest reply {request_id}: no read is \
                 waiting for it"
            );
            return;
        };
        if self.current_scope().is_none() || self.config.selection != pending.asked {
            return;
        }
        let outcome = match result {
            Ok(FinancialIndexResponse::Manifest { catalog, .. }) => Ok(catalog),
            Ok(_) => Err(KytrixServiceError::Protocol(SmolStr::new_static(
                "the service answered a catalogue read with something else",
            ))),
            Err(_) => Err(KytrixServiceError::Closed),
        };
        if !self.settle_selection(pending.asked, outcome, cx) {
            return;
        }
        self.follow(pending.retry, cx);
    }

    fn settle_selection(
        &mut self,
        asked: Vec<SelectedFinancialIndex>,
        manifest: Result<super::model::FinancialIndexCatalog, KytrixServiceError>,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        if self.account_id.is_none() || self.config.selection != asked {
            return false;
        }
        let Ok(catalog) = manifest else {
            return true;
        };
        let kept = catalog
            .indices
            .iter()
            .map(|descriptor| descriptor.index.topic.code())
            .collect::<ahash::AHashSet<_>>();
        let settled = asked
            .iter()
            .filter(|index| kept.contains(index.topic.code()))
            .cloned()
            .collect::<Vec<_>>();
        if settled.len() == asked.len() {
            return true;
        }
        log::warn!(
            "{} selected index(es) are no longer in the manifest; the pane went back to \
             following {}",
            asked.len() - settled.len(),
            settled.len()
        );
        self.config.selection = settled;
        self.config_change.publish(cx.app_mut());
        true
    }

    fn apply_reliable(
        &mut self,
        key: FinancialIndexScope,
        message: &FinancialIndexStreamMessage,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_scope().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
            return;
        }
        let FinancialIndexStreamMessage::Status(status) = message else {
            return;
        };

        match status {
            FinancialIndexStatus::Connecting => {
                self.ready = false;
                self.status.report(cx.app_mut(), WidgetState::Connecting);
            }
            FinancialIndexStatus::Ready => {
                self.ready = true;
                if self.has_content() {
                    self.status.report(cx.app_mut(), WidgetState::Ready);
                } else {
                    self.status.report(cx.app_mut(), WidgetState::Loading);
                }
            }
            FinancialIndexStatus::Failed(error) => {
                self.ready = false;
                let recovery = self.recovery.clone();
                self.status.report(
                    cx.app_mut(),
                    WidgetState::failed(
                        Arc::new(FinancialIndexFailure::Stream(error.clone())),
                        recovery,
                    ),
                );
            }
        }

        self.publish_rows(cx);
        let _ = cx.request_frame();
    }

    fn apply_latest(
        &mut self,
        key: FinancialIndexScope,
        message: &FinancialIndexStreamMessage,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_scope().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
            return;
        }
        let FinancialIndexStreamMessage::Quotes(quotes) = message else {
            return;
        };

        self.mark_flashes(quotes, cx);
        self.quotes = Some(Arc::clone(quotes));
        self.accepted = Some(key);
        if self.ready {
            self.status.report(cx.app_mut(), WidgetState::Ready);
        }
        self.publish_rows(cx);
        let _ = cx.request_frame();
    }

    fn mark_flashes(
        &mut self,
        quotes: &HashMap<FinancialIndexTopic, FinancialIndexQuote>,
        cx: &mut WidgetCx<'_>,
    ) {
        let Some(previous) = self.quotes.as_deref() else {
            return;
        };
        let reduce_motion = cx.services().reduce_motion;
        if *cx.read(&reduce_motion) || self.config.mode != FinancialIndexDisplayMode::Grid {
            return;
        }
        let now = Instant::now();
        for (topic, quote) in quotes {
            let Some(previous) = previous.get(topic) else {
                continue;
            };
            let direction = match quote.latest.cmp(&previous.latest) {
                std::cmp::Ordering::Less => FinancialIndexDirection::Down,
                std::cmp::Ordering::Equal => continue,
                std::cmp::Ordering::Greater => FinancialIndexDirection::Up,
            };
            self.flashes.insert(
                topic.clone(),
                QuoteFlash {
                    direction,
                    started_at: now,
                },
            );
        }
    }

    fn publish_rows(&mut self, cx: &mut WidgetCx<'_>) {
        let busy = self.status.is_busy(cx.app());
        let rows = rows_of(&self.config.selection, self.quotes.as_deref());
        let changed = busy != self.busy || self.rows.as_ref() != rows.as_ref();
        self.busy = busy;
        self.rows = Arc::clone(&rows);
        if changed {
            self.rows_dirty = true;
        }
        cx.write(&self.rows_signal, rows);
        cx.write(&self.busy_signal, busy);
    }

    fn publish_rows_to_content(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let rows = self.rows.len();
        let width = self.viewport_width(cx);
        match self.content.as_mut() {
            Some(BodyContent::Grid(grid)) => {
                let layout = grid_layout(width, rows);
                if grid.built_columns == layout.columns
                    && grid.count.get() == rows
                    && grid.built_lines == layout.lines
                {
                    return Ok(());
                }
                grid.count.set(rows);
                grid.columns.set(layout.columns.max(1));
                grid.built_columns = layout.columns;
                grid.built_lines = layout.lines;
                grid.list.set_count(cx.app_mut(), layout.lines)?;
                grid.list.sync(cx.app_mut())?;
            }
            Some(BodyContent::Table(table)) => {
                if table.table.rows() == rows {
                    return Ok(());
                }
                table.table.set_rows(cx.app_mut(), rows)?;
                table.table.sync(cx.app_mut())?;
            }
            None => {}
        }
        Ok(())
    }

    fn sync_grid_columns(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let rows = self.rows.len();
        let width = self.viewport_width(cx);
        let Some(BodyContent::Grid(grid)) = self.content.as_mut() else {
            return Ok(());
        };
        let layout = grid_layout(width, rows);
        if grid.built_columns == layout.columns
            && grid.built_lines == layout.lines
            && grid.count.get() == rows
            && grid.columns.get() == layout.columns.max(1)
        {
            return Ok(());
        }
        grid.built_columns = layout.columns;
        grid.built_lines = layout.lines;
        grid.count.set(rows);
        grid.columns.set(layout.columns.max(1));
        grid.list.set_count(cx.app_mut(), layout.lines)?;
        grid.list.sync(cx.app_mut())?;
        let _ = cx.request_frame();
        Ok(())
    }

    fn viewport_width(&self, cx: &WidgetCx<'_>) -> f32 {
        self.content_slot
            .and_then(|slot| cx.tree().get(slot).ok())
            .map_or(0.0, |node| node.layout().bounds.size.width)
    }

    fn settle_flashes(&mut self, cx: &mut WidgetCx<'_>) {
        let now = Instant::now();
        self.flashes.retain(|_, flash| flash.is_active(now));
        let mut visuals = HashMap::with_capacity(self.flashes.len());
        for (topic, flash) in &self.flashes {
            visuals.insert(
                topic.clone(),
                FlashVisual {
                    direction: flash.direction,
                    opacity: flash.opacity(now),
                },
            );
        }
        cx.write(&self.flash_signal, Arc::new(visuals));
        if self.flashes.is_empty() {
            self.stop_flash_timer(cx.app_mut());
        } else {
            self.arm_flash_timer(cx);
        }
    }

    fn arm_flash_timer(&mut self, cx: &mut WidgetCx<'_>) {
        if self.flash_timer.is_some() && !self.flash_timer_fired.get() {
            return;
        }
        self.flash_timer_fired.set(false);
        self.flash_timer = None;
        let fired = Rc::clone(&self.flash_timer_fired);
        let window = cx.window();
        let app = cx.app_mut();
        match app.after(window, FLASH_TICK, move |app| {
            fired.set(true);
            let _ = app.request_repaint(window);
        }) {
            Ok(timer) => self.flash_timer = Some(timer),
            Err(error) => log::warn!("the financial index flash could not be timed: {error}"),
        }
    }

    fn stop_flash_timer(&mut self, app: &mut App) {
        if let Some(timer) = self.flash_timer.take() {
            app.cancel_timer(&timer);
        }
    }

    fn apply_request(
        &mut self,
        request: HeaderRequest,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        match request {
            HeaderRequest::Choose => {
                if self.account_id.is_none() || self.selector.is_some() {
                    return Ok(());
                }
                let confirmed = self.confirmed;
                let window = cx.window();
                let on_confirm = Rc::new(
                    move |app: &mut App, selection: Vec<SelectedFinancialIndex>| {
                        app.write(&confirmed, Some(selection));
                        let _ = app.request_repaint(window);
                    },
                );
                match IndexSelector::open(cx, &self.config.selection, on_confirm) {
                    Ok(selector) => self.selector = Some(selector),
                    Err(error) => log::error!("the index selector could not be opened: {error}"),
                }
            }
            HeaderRequest::Mode(mode) => self.set_mode(mode, cx),
        }
        Ok(())
    }

    fn select_indices(&mut self, indices: Vec<SelectedFinancialIndex>, cx: &mut WidgetCx<'_>) {
        let selection = match normalize_selection(&indices) {
            Ok(selection) => selection,
            Err(error) => {
                let recovery = self.recovery.clone();
                self.status.report(
                    cx.app_mut(),
                    WidgetState::failed(
                        Arc::new(FinancialIndexFailure::Selection(error)),
                        recovery,
                    ),
                );
                let _ = cx.request_frame();
                return;
            }
        };
        if selection == self.config.selection {
            return;
        }
        self.config.selection = selection;
        self.config_change.publish(cx.app_mut());
        self.start(false, cx);
    }

    fn set_mode(&mut self, mode: FinancialIndexDisplayMode, cx: &mut WidgetCx<'_>) {
        if self.config.mode == mode {
            return;
        }
        self.config.mode = mode;
        self.config_change.publish(cx.app_mut());
        let _ = cx.request_frame();
    }

    fn build_content(&mut self, slot: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        let content = match self.config.mode {
            FinancialIndexDisplayMode::Grid => BodyContent::Grid(self.build_grid(slot, cx)?),
            FinancialIndexDisplayMode::Table => BodyContent::Table(self.build_table(slot, cx)?),
        };
        self.content = Some(content);
        self.built_mode = Some(self.config.mode);
        self.built_language = Some(language);
        self.rows_dirty = false;
        self.publish_rows_to_content(cx)?;
        Ok(())
    }

    fn rebuild_content(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(slot) = self.content_slot else {
            return Ok(());
        };
        self.release_content(cx);
        self.build_content(slot, cx)?;
        let _ = cx.request_frame();
        Ok(())
    }

    fn release_content(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(content) = self.content.take() else {
            return;
        };
        let wrapper = content.wrapper();
        if cx.tree().get(wrapper).is_ok() {
            let window = cx.window();
            if let Err(error) = cx.app_mut().remove_node(window, wrapper) {
                log::error!("the financial index content could not be removed: {error}");
            }
        }
        match content {
            BodyContent::Grid(grid) => {
                grid.list.release(cx.app_mut());
            }
            BodyContent::Table(table) => {
                table.table.release(cx.app_mut());
            }
        }
        self.built_mode = None;
        self.built_language = None;
    }

    fn build_grid(
        &mut self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<GridBody, EngineError> {
        let wrapper = NodeBuilder::boxed()
            .id("TEST_INDICES_GRID")
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;

        let layout = grid_layout(self.viewport_width(cx), self.rows.len());
        let columns = Rc::new(Cell::new(layout.columns.max(1)));
        let columns_for_rows = Rc::clone(&columns);
        let count = Rc::new(Cell::new(0));
        let count_for_rows = Rc::clone(&count);
        let rows = self.rows_signal;
        let flash = self.flash_signal;
        let language = self.language;
        let translate = self.translate.clone();
        let cell_theme = self.theme.clone();
        let font_size = self.theme.tokens().small.font_size;
        let large_font_size = self.theme.tokens().large.font_size;
        let key = cx.unique_key("financial_index.grid");
        let options = ListOptions {
            padding_x: 0.0,
            padding_y: 0.0,
            ..ListOptions::fixed(GRID_CELL_HEIGHT).selection(SelectionMode::None)
        };
        let list = List::new(
            cx.app_mut(),
            self.theme.clone(),
            key,
            options,
            move |tree, parent, line| {
                let sink = GridSink {
                    rows,
                    count: Rc::clone(&count_for_rows),
                    columns: Rc::clone(&columns_for_rows),
                    flash,
                    language,
                    translate: translate.clone(),
                    theme: cell_theme.clone(),
                    font_size,
                    large_font_size,
                };
                sink.build(tree, parent, line)
            },
        );
        list.build(cx.tree_mut(), wrapper)?;
        let window = cx.window();
        list.attach(cx.app_mut(), window)?;
        Ok(GridBody {
            wrapper,
            list,
            columns,
            count,
            built_columns: layout.columns,
            built_lines: 0,
        })
    }

    fn build_table(
        &mut self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<TableBody, EngineError> {
        let wrapper = NodeBuilder::boxed()
            .id("TEST_INDICES_TABLE")
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .flex_col()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;

        let theme = self.theme.clone();
        let cell_theme = self.theme.clone();
        let font_size = theme.tokens().medium.font_size;
        let small_font_size = theme.tokens().small.font_size;
        let row_height = theme.tokens().medium.height;
        let translate = self.translate.clone();
        let language = self.language;
        let rows_signal = self.rows_signal;
        let busy_signal = self.busy_signal;
        let columns: Vec<Column> = table_columns(&translate).collect();
        let cells = move |tree: &mut NodeTree, cell_parent: NodeId, row: usize, column: usize| {
            let sink = TableCellSink {
                rows: rows_signal,
                busy: busy_signal,
                language,
                translate: translate.clone(),
                theme: cell_theme.clone(),
                font_size,
                small_font_size,
            };
            if let Err(error) = sink.build(tree, cell_parent, row, column) {
                log::error!("a financial index cell could not be built: {error}");
            }
        };
        let key = cx.unique_key("financial_index.table");
        let table = Table::new(
            cx.app_mut(),
            theme,
            key,
            columns,
            TableOptions::fixed(row_height).selection(SelectionMode::None),
            cells,
            t_smol!(self.translate, "common.no_data"),
        );
        table.set_initial_rows(cx.app_mut(), self.rows.len());
        let window = cx.window();
        table.mount(cx.app_mut(), window, wrapper)?;
        Ok(TableBody { wrapper, table })
    }

    fn build_header_left(
        &mut self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        let disabled = self.account_id.is_none();
        let wrapper = NodeBuilder::boxed()
            .id("TEST_INDICES_CHOOSE")
            .style(StyleBuilder::new().flex_none().build())
            .build(cx.tree_mut(), parent)?;
        let label = t_smol!(self.translate, "widget.financial_index.choose");
        let requests = self.requests;
        let window = cx.window();
        let key = cx.unique_key("financial_index.header.choose");
        let mut choose = Button::new(cx.app_mut(), self.theme.clone(), key)
            .primary()
            .small()
            .icon(IconKind::Plus)
            .a11y_label(label.clone())
            .tooltip_text(label)
            .on_click(move |ctx| {
                let _ = ctx.write(&requests, Some(HeaderRequest::Choose));
                let window = ctx.window();
                let _ = ctx.app_mut().request_repaint(window);
            });
        choose = choose.disabled(disabled);
        choose.build(cx.app_mut(), window, wrapper)?;
        self.header_left = Some(HeaderLeft {
            parent,
            wrapper,
            choose,
            built: (language, disabled),
        });
        Ok(())
    }

    fn build_header_right(
        &mut self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        let grid = self.build_mode_control(parent, FinancialIndexDisplayMode::Grid, cx)?;
        let table = self.build_mode_control(parent, FinancialIndexDisplayMode::Table, cx)?;
        self.header_right = Some(HeaderRight {
            parent,
            grid,
            table,
            built: language,
        });
        Ok(())
    }

    fn build_mode_control(
        &self,
        parent: NodeId,
        mode: FinancialIndexDisplayMode,
        cx: &mut WidgetCx<'_>,
    ) -> Result<ModeControl, EngineError> {
        let suffix = match mode {
            FinancialIndexDisplayMode::Grid => "financial_index.header.grid",
            FinancialIndexDisplayMode::Table => "financial_index.header.table",
        };
        let selector = match mode {
            FinancialIndexDisplayMode::Grid => "TEST_INDICES_MODE_GRID",
            FinancialIndexDisplayMode::Table => "TEST_INDICES_MODE_TABLE",
        };
        let wrapper = NodeBuilder::boxed()
            .id(selector)
            .style(StyleBuilder::new().flex_none().build())
            .build(cx.tree_mut(), parent)?;
        let label = mode_label(mode, &self.translate);
        let requests = self.requests;
        let window = cx.window();
        let key = cx.unique_key(suffix);
        let button = Button::new(cx.app_mut(), self.theme.clone(), key)
            .ghost()
            .small()
            .icon(mode.icon())
            .a11y_label(label.clone())
            .tooltip_text(label)
            .selected(self.config.mode == mode)
            .on_click(move |ctx| {
                let _ = ctx.write(&requests, Some(HeaderRequest::Mode(mode)));
                let window = ctx.window();
                let _ = ctx.app_mut().request_repaint(window);
            });
        button.build(cx.app_mut(), window, wrapper)?;
        Ok(ModeControl { wrapper, button })
    }

    fn sync_header(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        let disabled = self.account_id.is_none();

        let stale_left = self
            .header_left
            .as_ref()
            .is_some_and(|left| left.built != (language, disabled));
        if stale_left {
            let parent = self.header_left.as_ref().map(|left| left.parent);
            match parent {
                Some(parent) if cx.tree().get(parent).is_ok() => {
                    self.release_header_left(cx, true)?;
                    self.build_header_left(parent, cx)?;
                }
                Some(_) => self.release_header_left(cx, false)?,
                None => {}
            }
        } else if let Some(left) = self.header_left.as_ref() {
            left.choose.sync(cx.app_mut())?;
            left.choose.set_disabled(cx.app_mut(), disabled);
        }

        let stale_right = self
            .header_right
            .as_ref()
            .is_some_and(|right| right.built != language);
        if stale_right {
            let parent = self.header_right.as_ref().map(|right| right.parent);
            match parent {
                Some(parent) if cx.tree().get(parent).is_ok() => {
                    self.release_header_right(cx, true)?;
                    self.build_header_right(parent, cx)?;
                }
                Some(_) => self.release_header_right(cx, false)?,
                None => {}
            }
        } else if let Some(right) = self.header_right.as_ref() {
            right.grid.button.sync(cx.app_mut())?;
            right.table.button.sync(cx.app_mut())?;
            let mode = self.config.mode;
            right
                .grid
                .button
                .set_selected(cx.app_mut(), mode == FinancialIndexDisplayMode::Grid);
            right
                .table
                .button
                .set_selected(cx.app_mut(), mode == FinancialIndexDisplayMode::Table);
        }

        Ok(())
    }

    fn release_header_left(
        &mut self,
        cx: &mut WidgetCx<'_>,
        remove_nodes: bool,
    ) -> Result<(), EngineError> {
        let Some(left) = self.header_left.take() else {
            return Ok(());
        };
        if remove_nodes {
            remove_own_nodes(cx, [left.wrapper])?;
        }
        left.choose.release(cx.app_mut());
        Ok(())
    }

    fn release_header_right(
        &mut self,
        cx: &mut WidgetCx<'_>,
        remove_nodes: bool,
    ) -> Result<(), EngineError> {
        let Some(right) = self.header_right.take() else {
            return Ok(());
        };
        if remove_nodes {
            remove_own_nodes(cx, [right.grid.wrapper, right.table.wrapper])?;
        }
        right.grid.button.release(cx.app_mut());
        right.table.button.release(cx.app_mut());
        Ok(())
    }

    fn release_header(
        &mut self,
        cx: &mut WidgetCx<'_>,
        remove_nodes: bool,
    ) -> Result<(), EngineError> {
        self.release_header_left(cx, remove_nodes)?;
        self.release_header_right(cx, remove_nodes)
    }
}

fn remove_own_nodes(
    cx: &mut WidgetCx<'_>,
    nodes: impl IntoIterator<Item = NodeId>,
) -> Result<(), EngineError> {
    for node in nodes {
        if cx.tree().get(node).is_err() {
            continue;
        }
        let window = cx.window();
        cx.app_mut().remove_node(window, node)?;
    }
    Ok(())
}

fn table_columns(translate: &Translate) -> impl Iterator<Item = Column> + '_ {
    [
        t_smol!(translate, "widget.financial_index.index"),
        t_smol!(translate, "widget.financial_index.latest"),
        t_smol!(translate, "widget.financial_index.change"),
        t_smol!(translate, "widget.financial_index.change_percent"),
    ]
    .into_iter()
    .zip(COLUMN_WEIGHTS)
    .enumerate()
    .map(|(index, (header, weight))| {
        let align = if index == 0 {
            ColumnAlign::Start
        } else {
            ColumnAlign::End
        };
        Column::flex(header, weight).align(align)
    })
}

fn rows_of(
    selection: &[SelectedFinancialIndex],
    quotes: Option<&HashMap<FinancialIndexTopic, FinancialIndexQuote>>,
) -> Arc<[FinancialIndexRow]> {
    let mut rows = Vec::with_capacity(selection.len());
    rows.extend(selection.iter().map(|index| FinancialIndexRow {
        quote: quotes.and_then(|quotes| quotes.get(&index.topic)).copied(),
        index: index.clone(),
    }));
    rows.into()
}

fn flash_opacity(elapsed: Duration) -> f32 {
    if elapsed >= FLASH_DURATION {
        return 0.0;
    }
    if elapsed <= FLASH_PEAK {
        elapsed.as_secs_f32() / FLASH_PEAK.as_secs_f32()
    } else {
        (FLASH_DURATION - elapsed).as_secs_f32() / (FLASH_DURATION - FLASH_PEAK).as_secs_f32()
    }
}

fn mode_label(mode: FinancialIndexDisplayMode, translate: &Translate) -> SmolStr {
    match mode {
        FinancialIndexDisplayMode::Grid => t_smol!(translate, "widget.financial_index.grid"),
        FinancialIndexDisplayMode::Table => t_smol!(translate, "widget.financial_index.table"),
    }
}

fn direction_role(direction: FinancialIndexDirection) -> Role {
    match direction {
        FinancialIndexDirection::Up => Role::MarketUp,
        FinancialIndexDirection::Down => Role::MarketDown,
        FinancialIndexDirection::Flat => Role::MarketFlat,
    }
}

fn direction_surface(direction: FinancialIndexDirection) -> Role {
    match direction {
        FinancialIndexDirection::Up => Role::MarketUpSurface,
        FinancialIndexDirection::Down => Role::MarketDownSurface,
        FinancialIndexDirection::Flat => Role::SurfaceRaised,
    }
}

fn index_name(index: &SelectedFinancialIndex, language: LanguageCode) -> SmolStr {
    SmolStr::new(index.name.get(language))
}

fn pending_text(busy: bool, translate: &Translate) -> SmolStr {
    if busy {
        t_smol!(translate, "common.connecting")
    } else {
        SmolStr::new_static("--")
    }
}

struct TableCellSink {
    rows: Signal<Arc<[FinancialIndexRow]>>,
    busy: Signal<bool>,
    language: Signal<LanguageCode>,
    translate: Translate,
    theme: ThemeHandle,
    font_size: f32,
    small_font_size: f32,
}

impl TableCellSink {
    fn build(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
        column: usize,
    ) -> Result<(), EngineError> {
        match column {
            0 => self.build_name(tree, parent, row),
            1 => self.build_price(tree, parent, row),
            2 => self.build_change(tree, parent, row),
            3 => self.build_change_percent(tree, parent, row),
            _ => Ok(()),
        }
    }

    fn build_name(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
    ) -> Result<(), EngineError> {
        let rows = self.rows;
        let busy = self.busy;
        let language = self.language;
        let translate = self.translate.clone();
        let theme = self.theme.clone();
        let cell = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .min_width_0()
                    .build(),
            )
            .build(tree, parent)?;

        let name_rows = rows;
        let name_language = language;
        let name_theme = theme.clone();
        let mut name_style = TextStyle::new(self.font_size);
        name_style.wrap = TextWrap::NoWrap;
        name_style.overflow = TextOverflow::Ellipsis;
        NodeBuilder::text(SmolStr::default(), name_style)
            .style(StyleBuilder::new().flex_1().min_width_0().build())
            .bind_text(move |read| {
                let language = *read.read(&name_language);
                let rows = read.read(&name_rows);
                rows.get(row)
                    .map_or_else(SmolStr::default, |row| index_name(&row.index, language))
            })
            .bind_style(move |read| StylePatch {
                foreground: Some(name_theme.role(read, Role::FgDefault)),
                ..StylePatch::default()
            })
            .build(tree, cell)?;

        let note_theme = theme;
        NodeBuilder::text(SmolStr::default(), TextStyle::new(self.small_font_size))
            .style(StyleBuilder::new().flex_none().min_width_0().build())
            .bind_text(move |read| {
                let rows = read.read(&rows);
                let busy = *read.read(&busy);
                match rows.get(row).and_then(|row| row.quote) {
                    Some(quote) if !quote.is_trading => {
                        t_smol!(translate, "widget.financial_index.market_closed")
                    }
                    Some(_) => SmolStr::default(),
                    None => pending_text(busy, &translate),
                }
            })
            .bind_style(move |read| StylePatch {
                foreground: Some(note_theme.role(read, Role::FgMuted)),
                ..StylePatch::default()
            })
            .build(tree, cell)?;
        Ok(())
    }

    fn build_price(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
    ) -> Result<(), EngineError> {
        let rows = self.rows;
        let busy = self.busy;
        let translate = self.translate.clone();
        let theme = self.theme.clone();
        let mut style = TextStyle::new(self.font_size);
        style.wrap = TextWrap::NoWrap;
        style.overflow = TextOverflow::Ellipsis;
        NodeBuilder::text(SmolStr::default(), style)
            .bind_text(move |read| {
                let rows = read.read(&rows);
                let busy = *read.read(&busy);
                match rows.get(row).and_then(|row| row.quote) {
                    Some(quote) => format_price(quote.latest),
                    None => pending_text(busy, &translate),
                }
            })
            .bind_style(move |read| {
                let rows = read.read(&rows);
                let quote = rows.get(row).and_then(|row| row.quote);
                StylePatch {
                    foreground: Some(theme.role(read, price_role(quote))),
                    ..StylePatch::default()
                }
            })
            .build(tree, parent)?;
        Ok(())
    }

    fn build_change(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
    ) -> Result<(), EngineError> {
        self.build_number(tree, parent, row, NumberColumn::Change)
    }

    fn build_change_percent(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
    ) -> Result<(), EngineError> {
        self.build_number(tree, parent, row, NumberColumn::ChangePercent)
    }

    fn build_number(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
        column: NumberColumn,
    ) -> Result<(), EngineError> {
        let rows = self.rows;
        let theme = self.theme.clone();
        let mut style = TextStyle::new(self.font_size);
        style.wrap = TextWrap::NoWrap;
        style.overflow = TextOverflow::Ellipsis;
        NodeBuilder::text(SmolStr::default(), style)
            .bind_text(move |read| {
                let rows = read.read(&rows);
                rows.get(row).and_then(|row| row.quote).map_or_else(
                    || SmolStr::new_static("--"),
                    |quote| match column {
                        NumberColumn::Change => format_signed_cents(quote.change()),
                        NumberColumn::ChangePercent => format_percent(quote.change_percent()),
                    },
                )
            })
            .bind_style(move |read| {
                let rows = read.read(&rows);
                let quote = rows.get(row).and_then(|row| row.quote);
                StylePatch {
                    foreground: Some(theme.role(read, price_role(quote))),
                    ..StylePatch::default()
                }
            })
            .build(tree, parent)?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum NumberColumn {
    Change,
    ChangePercent,
}

fn price_role(quote: Option<FinancialIndexQuote>) -> Role {
    quote.map_or(Role::FgMuted, |quote| direction_role(quote.direction()))
}

struct GridSink {
    rows: Signal<Arc<[FinancialIndexRow]>>,
    count: Rc<Cell<usize>>,
    columns: Rc<Cell<usize>>,
    flash: Signal<Arc<HashMap<FinancialIndexTopic, FlashVisual>>>,
    language: Signal<LanguageCode>,
    translate: Translate,
    theme: ThemeHandle,
    font_size: f32,
    large_font_size: f32,
}

impl GridSink {
    fn build(&self, tree: &mut NodeTree, parent: NodeId, line: usize) -> Result<(), EngineError> {
        let columns = self.columns.get().max(1);
        let count = self.count.get();
        let frame = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .justify_center()
                    .width_full()
                    .height(GRID_CELL_HEIGHT)
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .build(tree, parent)?;
        for column in 0..columns {
            let index = line * columns + column;
            if index < count {
                self.build_cell(tree, frame, index)?;
            } else {
                NodeBuilder::boxed()
                    .style(
                        StyleBuilder::new()
                            .width(GRID_CELL_WIDTH)
                            .height(GRID_CELL_HEIGHT)
                            .flex_none()
                            .min_width_0()
                            .build(),
                    )
                    .build(tree, frame)?;
            }
        }
        Ok(())
    }

    fn build_cell(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        index: usize,
    ) -> Result<(), EngineError> {
        let card = self.build_card(tree, parent, index)?;
        self.build_name(tree, card, index)?;
        self.build_price(tree, card, index)?;
        self.build_details(tree, card, index)?;
        self.build_flash(tree, card, index)
    }

    fn build_card(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        index: usize,
    ) -> Result<NodeId, EngineError> {
        let rows = self.rows;
        let theme = self.theme.clone();
        NodeBuilder::boxed()
            .id("TEST_INDICES_GRID_CELL")
            .style(
                StyleBuilder::new()
                    .width(GRID_CELL_WIDTH)
                    .height(GRID_CELL_HEIGHT)
                    .flex_none()
                    .relative()
                    .overflow_hidden()
                    .min_width_0()
                    .flex_col()
                    .gap(4.0)
                    .padding(8.0)
                    .border(1.0)
                    .build(),
            )
            .bind_style(move |read| {
                let rows = read.read(&rows);
                let direction = rows.get(index).and_then(|row| row.quote).map_or(
                    FinancialIndexDirection::Flat,
                    FinancialIndexQuote::direction,
                );
                StylePatch {
                    background: Some(theme.role(read, direction_surface(direction))),
                    border: Some(Border {
                        width: 1.0,
                        color: theme.role(read, Role::LineDivider),
                    }),
                    ..StylePatch::default()
                }
            })
            .build(tree, parent)
    }

    fn build_name(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        index: usize,
    ) -> Result<(), EngineError> {
        let rows = self.rows;
        let language = self.language;
        let theme = self.theme.clone();
        let mut style = TextStyle::new(self.font_size);
        style.alignment = TextAlign::Center;
        style.wrap = TextWrap::NoWrap;
        style.overflow = TextOverflow::Ellipsis;
        NodeBuilder::text(SmolStr::default(), style)
            .style(StyleBuilder::new().width_full().min_width_0().build())
            .bind_text(move |read| {
                let language = *read.read(&language);
                let rows = read.read(&rows);
                rows.get(index)
                    .map_or_else(SmolStr::default, |row| index_name(&row.index, language))
            })
            .bind_style(move |read| StylePatch {
                foreground: Some(theme.role(read, Role::FgMuted)),
                ..StylePatch::default()
            })
            .build(tree, parent)?;
        Ok(())
    }

    fn build_price(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        index: usize,
    ) -> Result<(), EngineError> {
        let rows = self.rows;
        let theme = self.theme.clone();
        let mut style = TextStyle::new(self.large_font_size);
        style.alignment = TextAlign::Center;
        style.weight = 700;
        style.wrap = TextWrap::NoWrap;
        style.overflow = TextOverflow::Ellipsis;
        NodeBuilder::text(SmolStr::default(), style)
            .style(
                StyleBuilder::new()
                    .width_full()
                    .flex_1()
                    .min_width_0()
                    .build(),
            )
            .bind_text(move |read| {
                let rows = read.read(&rows);
                rows.get(index).and_then(|row| row.quote).map_or_else(
                    || SmolStr::new_static("--"),
                    |quote| format_price(quote.latest),
                )
            })
            .bind_style(move |read| {
                let rows = read.read(&rows);
                let quote = rows.get(index).and_then(|row| row.quote);
                StylePatch {
                    foreground: Some(theme.role(read, price_role(quote))),
                    ..StylePatch::default()
                }
            })
            .build(tree, parent)?;
        Ok(())
    }

    fn build_details(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        index: usize,
    ) -> Result<NodeId, EngineError> {
        let details = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .width_full()
                    .min_width_0()
                    .gap(4.0)
                    .build(),
            )
            .build(tree, parent)?;

        let percent_rows = self.rows;
        let percent_theme = self.theme.clone();
        let mut percent_style = TextStyle::new(self.font_size);
        percent_style.alignment = TextAlign::Center;
        percent_style.wrap = TextWrap::NoWrap;
        percent_style.overflow = TextOverflow::Ellipsis;
        NodeBuilder::text(SmolStr::default(), percent_style)
            .style(StyleBuilder::new().flex_1().min_width_0().build())
            .bind_text(move |read| {
                let rows = read.read(&percent_rows);
                rows.get(index).and_then(|row| row.quote).map_or_else(
                    || SmolStr::new_static("--"),
                    |quote| format_percent(quote.change_percent()),
                )
            })
            .bind_style(move |read| {
                let rows = read.read(&percent_rows);
                let quote = rows.get(index).and_then(|row| row.quote);
                StylePatch {
                    foreground: Some(percent_theme.role(read, price_role(quote))),
                    ..StylePatch::default()
                }
            })
            .build(tree, details)?;

        let closed_rows = self.rows;
        let closed_theme = self.theme.clone();
        let translate = self.translate.clone();
        let mut closed_style = TextStyle::new(self.font_size);
        closed_style.alignment = TextAlign::Center;
        closed_style.wrap = TextWrap::NoWrap;
        closed_style.overflow = TextOverflow::Ellipsis;
        NodeBuilder::text(SmolStr::default(), closed_style)
            .style(StyleBuilder::new().flex_none().min_width_0().build())
            .bind_text(move |read| {
                let rows = read.read(&closed_rows);
                match rows.get(index).and_then(|row| row.quote) {
                    Some(quote) if !quote.is_trading => {
                        t_smol!(translate, "widget.financial_index.market_closed")
                    }
                    _ => SmolStr::default(),
                }
            })
            .bind_style(move |read| StylePatch {
                foreground: Some(closed_theme.role(read, Role::FgMuted)),
                ..StylePatch::default()
            })
            .build(tree, details)?;

        let change_rows = self.rows;
        let change_theme = self.theme.clone();
        let mut change_style = TextStyle::new(self.font_size);
        change_style.alignment = TextAlign::Center;
        change_style.wrap = TextWrap::NoWrap;
        change_style.overflow = TextOverflow::Ellipsis;
        NodeBuilder::text(SmolStr::default(), change_style)
            .style(StyleBuilder::new().flex_1().min_width_0().build())
            .bind_text(move |read| {
                let rows = read.read(&change_rows);
                rows.get(index).and_then(|row| row.quote).map_or_else(
                    || SmolStr::new_static("--"),
                    |quote| format_signed_cents(quote.change()),
                )
            })
            .bind_style(move |read| {
                let rows = read.read(&change_rows);
                let quote = rows.get(index).and_then(|row| row.quote);
                StylePatch {
                    foreground: Some(change_theme.role(read, price_role(quote))),
                    ..StylePatch::default()
                }
            })
            .build(tree, details)?;
        Ok(details)
    }

    fn build_flash(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        index: usize,
    ) -> Result<(), EngineError> {
        let rows = self.rows;
        let theme = self.theme.clone();
        let flash = self.flash;
        NodeBuilder::boxed()
            .id("TEST_INDICES_GRID_FLASH")
            .style(
                StyleBuilder::new()
                    .absolute()
                    .top(-1.0)
                    .left(-1.0)
                    .right(-1.0)
                    .height(GRID_FLASH_HEIGHT)
                    .build(),
            )
            .bind_style(move |read| {
                let flashes = read.read(&flash);
                let rows = read.read(&rows);
                let topic = rows.get(index).map(|row| row.index.topic.clone());
                let visual = topic.and_then(|topic| flashes.get(&topic).copied());
                let direction =
                    visual.map_or(FinancialIndexDirection::Flat, |visual| visual.direction);
                StylePatch {
                    background: Some(theme.role(read, direction_role(direction))),
                    opacity: Some(visual.map_or(0.0, |visual| visual.opacity)),
                    ..StylePatch::default()
                }
            })
            .build(tree, parent)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use app_config::{
        settings::{AccountConfig, AccountWalletKind, Config},
        widget::{VersionedWidgetConfig, decode_or_default},
    };
    use kytrix_proto::types::v1 as proto_types;
    use kytrix_service_provider::service::engine::{ErrorCode, KytrixServiceError};
    use ui_core::node::signal::SignalScope;
    use widget::test_support::WidgetTestApp;

    use super::*;
    use crate::{
        model::{
            FinancialIndexCatalog, LocalizedFinancialIndexName, MAX_NAME_CHARS,
            MAX_SELECTED_INDICES, prepare_catalog,
        },
        service::FinancialIndexStreamError,
        test_support::{self, ACCOUNT, FakeSource, event, selected, topic},
    };

    fn settings(account: Option<&str>) -> Config {
        let mut config = Config::default();
        config.account = account.map(|user_id| AccountConfig {
            user_id: user_id.to_owned(),
            wallet_address: "0x0000000000000000000000000000000000000001".to_owned(),
            wallet_kind: AccountWalletKind::Ethereum,
        });
        config
    }

    fn set_account(harness: &mut WidgetTestApp, account: Option<&str>) {
        let config = harness.services().config;
        harness.app_mut().write(&config, settings(account));
    }

    fn config(selection: &[SelectedFinancialIndex]) -> FinancialIndexConfig {
        FinancialIndexConfig {
            selection: normalize_selection(selection)
                .expect("the fixture selection should be valid"),
            ..FinancialIndexConfig::default()
        }
    }

    fn table_config(selection: &[SelectedFinancialIndex]) -> FinancialIndexConfig {
        FinancialIndexConfig {
            mode: FinancialIndexDisplayMode::Table,
            ..config(selection)
        }
    }

    fn quote(code: &str, latest: u32) -> FinancialIndexQuote {
        FinancialIndexQuote::from_event(&event(topic(code), latest, i64::from(latest)))
    }

    fn manifest_carrying(selection: &[SelectedFinancialIndex]) -> proto_types::FinancialIndices {
        proto_types::FinancialIndices {
            categories: vec![proto_types::FinancialIndexCategory {
                code: "test".to_owned(),
                name: Some(proto_types::FinancialIndexLocalizedName {
                    en: "Test indices".to_owned(),
                    zh: "测试指数".to_owned(),
                }),
            }],
            indices: selection
                .iter()
                .map(|index| proto_types::FinancialIndexDefinition {
                    code: index.topic.code().to_owned(),
                    category: "test".to_owned(),
                    name: Some(proto_types::FinancialIndexLocalizedName {
                        en: index.name.en.to_string(),
                        zh: index.name.zh.to_string(),
                    }),
                    request_code: format!(".{}", index.topic.code()),
                })
                .collect(),
        }
    }

    struct Pane {
        widget: FinancialIndexWidget,
        status: WidgetStatus,
        parent: NodeId,
        scope: SignalScope,
        publishes: Rc<Cell<usize>>,
    }

    fn drive<R>(
        harness: &mut WidgetTestApp,
        pane: &mut Pane,
        body: impl FnOnce(&mut FinancialIndexWidget, &mut WidgetCx<'_>) -> R,
    ) -> R {
        let services = std::rc::Rc::clone(harness.services_rc());
        let window = harness.window();
        let scope = pane.scope;
        let mut cx = WidgetCx::new(harness.app_mut(), window, &services, scope);
        body(&mut pane.widget, &mut cx)
    }

    const BODY_WIDTH: f32 = 640.0;
    const BODY_HEIGHT: f32 = 480.0;

    fn change_tracker() -> (WidgetConfigChange, Rc<Cell<usize>>) {
        let publishes: Rc<Cell<usize>> = Rc::new(Cell::new(0));
        let counter = Rc::clone(&publishes);
        (
            WidgetConfigChange::new(move |_| counter.set(counter.get() + 1)),
            publishes,
        )
    }

    fn peer_pane(
        harness: &mut WidgetTestApp,
        source: FakeSource,
        config: FinancialIndexConfig,
        parent: NodeId,
    ) -> Pane {
        let scope = harness.app_mut().signal_scope();
        let status = WidgetStatus::new(harness.app_mut(), scope);
        let services = std::rc::Rc::clone(harness.services_rc());
        let window = harness.window();
        let (config_change, publishes) = change_tracker();
        let widget = {
            let mut cx = WidgetCx::new(harness.app_mut(), window, &services, scope);
            FinancialIndexWidget::with_source(
                config,
                source,
                status,
                config_change,
                WidgetAction::new("common.retry", |_| {}),
                &mut cx,
            )
        };
        Pane {
            widget,
            status,
            parent,
            scope,
            publishes,
        }
    }

    fn pane_with(
        harness: &mut WidgetTestApp,
        source: FakeSource,
        config: FinancialIndexConfig,
    ) -> Pane {
        source.set_manifest(manifest_carrying(&config.selection));
        let scope = harness.scope();
        let status = WidgetStatus::new(harness.app_mut(), scope);
        let parent = harness.with_cx(|cx| {
            let root = cx.tree().root();
            NodeBuilder::boxed()
                .id("TEST_INDICES_BODY")
                .style(
                    StyleBuilder::new()
                        .width(BODY_WIDTH)
                        .height(BODY_HEIGHT)
                        .flex_col()
                        .build(),
                )
                .build(cx.tree_mut(), root)
                .expect("the body parent builds")
        });
        let (config_change, publishes) = change_tracker();
        let widget = harness.with_cx(|cx| {
            FinancialIndexWidget::with_source(
                config,
                source,
                status,
                config_change,
                WidgetAction::new("common.retry", |_| {}),
                cx,
            )
        });
        Pane {
            widget,
            status,
            parent,
            scope,
            publishes,
        }
    }

    fn pane(harness: &mut WidgetTestApp, source: FakeSource, config: FinancialIndexConfig) -> Pane {
        set_account(harness, Some(ACCOUNT));
        let mut pane = pane_with(harness, source, config);
        load(harness, &mut pane);
        pane
    }

    fn pane_over(
        harness: &mut WidgetTestApp,
        source: FakeSource,
        config: FinancialIndexConfig,
        prepare: impl FnOnce(&FakeSource),
    ) -> Pane {
        set_account(harness, Some(ACCOUNT));
        let mut pane = pane_with(harness, source.clone(), config);
        prepare(&source);
        load(harness, &mut pane);
        pane
    }

    fn signed_out_pane(harness: &mut WidgetTestApp, source: FakeSource) -> Pane {
        set_account(harness, None);
        let mut pane = pane_with(harness, source, config(&[selected("HSI")]));
        load(harness, &mut pane);
        pane
    }

    fn load(harness: &mut WidgetTestApp, pane: &mut Pane) {
        drive(harness, pane, |widget, cx| widget.load(cx));
    }

    fn sync(harness: &mut WidgetTestApp, pane: &mut Pane) {
        drive(harness, pane, |widget, cx| widget.sync(cx)).expect("the widget syncs");
    }

    fn build_body(harness: &mut WidgetTestApp, pane: &mut Pane) {
        let parent = pane.parent;
        drive(harness, pane, |widget, cx| widget.body(parent, cx)).expect("the body builds");
        settle_frames(harness, pane);
    }

    fn settle_frames(harness: &mut WidgetTestApp, pane: &mut Pane) {
        for _ in 0..4 {
            sync(harness, pane);
            harness.drain();
        }
    }

    fn settle(
        harness: &mut WidgetTestApp,
        pane: &mut Pane,
        what: &str,
        ready: impl Fn(&WidgetTestApp, &Pane) -> bool,
    ) {
        for _ in 0..64 {
            harness.drain_runtime();
            sync(harness, pane);
            harness.drain();
            if ready(harness, pane) {
                return;
            }
        }
        panic!("{what} should happen");
    }

    fn report_quotes(harness: &mut WidgetTestApp, pane: &mut Pane, quotes: &[(&str, u32)]) {
        if let Some(binding) = pane.widget.stream.as_mut() {
            binding.cancel();
        }
        report_quotes_keeping_the_bridge(harness, pane, quotes);
    }

    fn report_quotes_keeping_the_bridge(
        harness: &mut WidgetTestApp,
        pane: &mut Pane,
        quotes: &[(&str, u32)],
    ) {
        let scope = pane
            .widget
            .current_scope()
            .expect("a loaded pane follows a scope");
        let mut published = HashMap::with_capacity(quotes.len());
        published.extend(
            quotes
                .iter()
                .map(|(code, latest)| (topic(code), quote(code, *latest))),
        );
        drive(harness, pane, |widget, cx| {
            widget.apply_latest(
                scope,
                &FinancialIndexStreamMessage::Quotes(Arc::new(published)),
                cx,
            );
        });
    }

    fn report_status(harness: &mut WidgetTestApp, pane: &mut Pane, status: FinancialIndexStatus) {
        let scope = pane
            .widget
            .current_scope()
            .expect("a loaded pane follows a scope");
        drive(harness, pane, |widget, cx| {
            widget.apply_reliable(scope, &FinancialIndexStreamMessage::Status(status), cx);
        });
    }

    fn report_failure(harness: &mut WidgetTestApp, pane: &mut Pane) {
        report_status(
            harness,
            pane,
            FinancialIndexStatus::Failed(FinancialIndexStreamError::Service(
                KytrixServiceError::Connect("the test session refused".into()),
            )),
        );
    }

    fn failure_of(harness: &WidgetTestApp, pane: &Pane) -> SmolStr {
        pane.status
            .state(harness.app())
            .failure()
            .expect("the pane should have reported a failure")
            .text(&harness.services().translate)
    }

    fn notice_of(harness: &WidgetTestApp, pane: &Pane) -> SmolStr {
        pane.status
            .state(harness.app())
            .notice()
            .expect("the pane should be waiting")
            .resolve(&harness.services().translate)
    }

    fn latest_of(pane: &Pane) -> Vec<Option<u32>> {
        pane.widget
            .rows
            .iter()
            .map(|row| row.quote.map(|quote| quote.latest))
            .collect()
    }

    fn table(pane: &Pane) -> &Table {
        match pane.widget.content.as_ref() {
            Some(BodyContent::Table(table)) => &table.table,
            _ => panic!("the pane should be drawing a table"),
        }
    }

    fn grid(pane: &Pane) -> &List {
        match pane.widget.content.as_ref() {
            Some(BodyContent::Grid(grid)) => &grid.list,
            _ => panic!("the pane should be drawing a grid"),
        }
    }

    fn cell_text(harness: &WidgetTestApp, pane: &Pane, row: usize, column: usize) -> String {
        let node = table(pane)
            .cell_node(harness.app(), row, column)
            .unwrap_or_else(|| panic!("row {row} column {column} should be mounted"));
        harness.text_of(node)
    }

    fn nodes_with_id(harness: &WidgetTestApp, root: NodeId, id: &str) -> Vec<NodeId> {
        let tree = harness.tree();
        let mut found = Vec::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if tree.debug_id(node).is_some_and(|debug| debug == id) {
                found.push(node);
            }
            if let Ok(target) = tree.get(node) {
                stack.extend(target.children().iter().rev().copied());
            }
        }
        found
    }

    fn find(harness: &WidgetTestApp, parent: NodeId, prefix: &str) -> Option<NodeId> {
        harness.find_by_debug_prefix(parent, prefix)
    }

    fn texts_of(harness: &WidgetTestApp, root: NodeId) -> Vec<String> {
        let tree = harness.tree();
        let mut texts = Vec::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            let Ok(target) = tree.get(node) else {
                continue;
            };
            if matches!(target.kind(), ui_core::node::tree::NodeKind::Text(_)) {
                texts.push(harness.text_of(node));
            }
            stack.extend(target.children().iter().copied());
        }
        texts
    }

    fn place(harness: &WidgetTestApp, node: NodeId) -> (f32, f32, f32, f32) {
        let bounds = harness
            .tree()
            .get(node)
            .expect("the node is in the tree")
            .layout()
            .bounds;
        (
            bounds.origin.x,
            bounds.origin.y,
            bounds.size.width,
            bounds.size.height,
        )
    }

    fn catalog_carrying(selection: &[SelectedFinancialIndex]) -> FinancialIndexCatalog {
        prepare_catalog(manifest_carrying(selection)).expect("the fixture catalogue is valid")
    }

    fn manifest_answer(
        selection: &[SelectedFinancialIndex],
    ) -> WidgetRpcResult<FinancialIndexProtocol> {
        Ok(FinancialIndexResponse::Manifest {
            catalog: catalog_carrying(selection),
            limit: None,
        })
    }

    fn deliver_manifest(
        harness: &mut WidgetTestApp,
        pane: &mut Pane,
        request_id: RequestId,
        result: WidgetRpcResult<FinancialIndexProtocol>,
    ) {
        drive(harness, pane, |widget, cx| {
            widget.apply_manifest(Inbound::Rpc { request_id, result }, cx)
        });
    }

    fn deliver_stream(
        harness: &mut WidgetTestApp,
        pane: &mut Pane,
        request_id: RequestId,
        result: WidgetRpcResult<FinancialIndexProtocol>,
    ) {
        drive(harness, pane, |widget, cx| {
            widget.apply_stream_rpc(request_id, result, cx)
        });
    }

    fn header_parent(harness: &mut WidgetTestApp, id: &'static str) -> NodeId {
        harness.with_cx(|cx| {
            let root = cx.tree().root();
            NodeBuilder::boxed()
                .id(id)
                .build(cx.tree_mut(), root)
                .expect("the header parent builds")
        })
    }

    fn build_header(harness: &mut WidgetTestApp, pane: &mut Pane) -> (NodeId, NodeId) {
        let left = header_parent(harness, "TEST_HEADER_LEFT");
        let right = header_parent(harness, "TEST_HEADER_RIGHT");
        drive(harness, pane, |widget, cx| {
            widget.header(HeaderGroup::Left, left, cx)?;
            widget.header(HeaderGroup::Right, right, cx)
        })
        .expect("the header builds");
        harness.drain();
        (left, right)
    }

    #[test]
    fn the_widget_should_have_a_stable_identity() {
        assert_eq!(FinancialIndexWidget::ID.as_str(), "kytrix.financial_index");
        assert_eq!(
            FinancialIndexWidget::DISPLAY_NAME,
            "widget.names.financial_index"
        );
        assert_eq!(
            FinancialIndexConfig::SCHEMA_VERSION,
            app_config::widget::WidgetConfigSchemaVersion::V1
        );
    }

    #[test]
    fn the_default_config_should_be_a_grid_with_nothing_selected() {
        let config = FinancialIndexConfig::default();

        assert_eq!(config.mode, FinancialIndexDisplayMode::Grid);
        assert!(config.selection.is_empty());
    }

    #[test]
    fn a_config_should_reject_missing_fields() {
        let error = serde_json::from_str::<FinancialIndexConfig>(r#"{"selection": []}"#)
            .expect_err("a config without its mode is not a config");

        assert!(error.to_string().contains("mode"), "got {error}");
    }

    #[test]
    fn a_config_should_normalize_its_selection_and_reject_the_next_index() {
        let duplicate = format!(
            r#"{{"selection": [{}, {}], "mode": "Grid"}}"#,
            serde_json::to_string(&selected("HSI")).expect("a selected index serializes"),
            serde_json::to_string(&selected("HSI")).expect("a selected index serializes"),
        );
        let decoded: FinancialIndexConfig =
            serde_json::from_str(&duplicate).expect("a duplicate selection is normalized");
        assert_eq!(decoded.selection.len(), 1);

        let selection = (0..=MAX_SELECTED_INDICES)
            .map(|index| selected(&format!("IX{index}")))
            .collect::<Vec<_>>();
        let error = serde_json::to_string(&FinancialIndexConfig {
            selection,
            mode: FinancialIndexDisplayMode::Grid,
        })
        .map(|encoded| serde_json::from_str::<FinancialIndexConfig>(&encoded))
        .expect("the config serializes")
        .expect_err("a selection past the hard limit is not a config");

        assert!(
            error
                .to_string()
                .contains(MAX_SELECTED_INDICES.to_string().as_str())
        );
    }

    #[test]
    fn a_config_should_reject_names_the_manifest_could_not_carry() {
        let mut index = selected("HSI");
        index.name = LocalizedFinancialIndexName {
            en: SmolStr::new("a".repeat(MAX_NAME_CHARS + 1)),
            zh: SmolStr::new_static("恒生指数"),
        };
        let encoded = serde_json::to_string(&FinancialIndexConfig {
            selection: vec![index],
            mode: FinancialIndexDisplayMode::Grid,
        })
        .expect("the config serializes");

        let error = serde_json::from_str::<FinancialIndexConfig>(&encoded)
            .expect_err("a name the manifest could not carry is not a config");

        assert!(
            error.to_string().contains("name"),
            "the reason should name the name: {error}"
        );
    }

    #[test]
    fn a_missing_config_should_use_the_default_and_a_present_one_should_decode() {
        let missing = decode_or_default::<FinancialIndexConfig>(None);

        assert_eq!(missing, FinancialIndexConfig::default());

        let encoded =
            VersionedWidgetConfig::encode(&config(&[selected("HSI")])).expect("the config encodes");
        let decoded = decode_or_default::<FinancialIndexConfig>(Some(encoded));

        assert_eq!(decoded.selection.len(), 1);
    }

    #[test]
    fn a_flash_should_rise_to_its_peak_and_fall_back_to_nothing() {
        assert_eq!(flash_opacity(Duration::ZERO), 0.0);
        assert_eq!(flash_opacity(FLASH_PEAK), 1.0);
        assert_eq!(flash_opacity(FLASH_DURATION), 0.0);
        assert!(flash_opacity(FLASH_PEAK / 2) > 0.0);
        assert!(flash_opacity(FLASH_PEAK + (FLASH_DURATION - FLASH_PEAK) / 2) > 0.0);
        assert_eq!(
            flash_opacity(FLASH_DURATION + Duration::from_millis(1)),
            0.0
        );
    }

    #[test]
    fn a_flash_should_be_active_until_it_has_faded() {
        let flash = QuoteFlash {
            direction: FinancialIndexDirection::Up,
            started_at: Instant::now(),
        };
        let now = flash.started_at;

        assert!(flash.is_active(now));
        assert!(flash.is_active(now + FLASH_PEAK));
        assert!(!flash.is_active(now + FLASH_DURATION));
        assert_eq!(flash.opacity(now + FLASH_PEAK), 1.0);
    }

    #[test]
    fn a_grid_layout_should_use_every_column_the_whole_width_fits() {
        let narrow = grid_layout(640.0, 12);
        assert_eq!(narrow.columns, 4, "640 px fits four 160 px cells");
        assert_eq!(narrow.lines, 3);

        let wide = grid_layout(800.0, 12);
        assert_eq!(wide.columns, 5, "800 px fits five 160 px cells");
        assert_eq!(wide.lines, 3);
    }

    #[test]
    fn a_grid_layout_should_ignore_the_height_when_counting_columns() {
        let wide = grid_layout(10_000.0, 12);

        assert_eq!(wide.columns, 12, "every item fits on one line");
        assert_eq!(wide.lines, 1);
    }

    #[test]
    fn a_grid_layout_should_floor_the_width_without_resizing_cells() {
        let layout = grid_layout(648.0, 12);

        assert_eq!(
            layout.columns, 4,
            "the 8 px a fourth cell would need are not a column"
        );
        assert_eq!(layout.cell_height, GRID_CELL_HEIGHT);
    }

    #[test]
    fn an_empty_grid_should_have_no_content() {
        let layout = grid_layout(640.0, 0);

        assert_eq!(layout.columns, 0);
        assert_eq!(layout.lines, 0);
    }

    #[test]
    fn a_grid_should_keep_one_cell_in_no_space_at_all() {
        for width in [0.0, 1.0, 100.0, f32::MIN_POSITIVE] {
            let layout = grid_layout(width, 3);
            assert_eq!(layout.columns, 1, "width {width} keeps one column");
            assert_eq!(layout.lines, 3);
        }
    }

    #[test]
    fn a_name_should_be_read_in_the_language_the_interface_is_reading() {
        let index = selected("HSI");

        assert_eq!(index_name(&index, LanguageCode::En), "HSI English");
        assert_eq!(index_name(&index, LanguageCode::ZhCn), "HSI Chinese");
    }

    fn follow(harness: &mut WidgetTestApp, pane: &mut Pane, source: &FakeSource) {
        let _control = source.subscribe();
        settle(
            harness,
            pane,
            "the pane should follow its scope",
            |_harness, _pane| !source.subscribed().is_empty(),
        );
    }

    #[test]
    fn a_signed_out_pane_should_say_what_it_is_waiting_for() {
        let mut harness = test_support::harness();
        let mut pane = signed_out_pane(&mut harness, FakeSource::default());
        settle_frames(&mut harness, &mut pane);

        assert_eq!(
            notice_of(&harness, &pane),
            harness
                .services()
                .translate
                .translate("widget.financial_index.logged_out")
        );
        assert!(pane.widget.config.selection.len() == 1);
        assert!(!pane.widget.has_content());
    }

    #[test]
    fn a_waiting_pane_should_say_what_it_waits_for_in_the_language_showing_now() {
        let mut harness = test_support::harness();
        let mut pane = signed_out_pane(&mut harness, FakeSource::default());
        settle_frames(&mut harness, &mut pane);
        let english = notice_of(&harness, &pane);

        harness.switch_language(LanguageCode::ZhCn);

        assert_eq!(harness.services().translate.language(), LanguageCode::ZhCn);
        assert_ne!(notice_of(&harness, &pane), english);
        assert_eq!(
            harness
                .services()
                .translate
                .translate("widget.financial_index.logged_out"),
            notice_of(&harness, &pane)
        );
    }

    #[test]
    fn a_pane_with_nothing_selected_should_ask_for_a_selection() {
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, FakeSource::default(), config(&[]));
        settle_frames(&mut harness, &mut pane);

        assert_eq!(
            notice_of(&harness, &pane),
            harness
                .services()
                .translate
                .translate("widget.financial_index.select_at_least_one")
        );
        assert!(!pane.widget.has_content());
    }

    #[test]
    fn a_pane_should_follow_the_selection_it_was_given() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);

        assert_eq!(
            source.subscribed(),
            vec![vec![topic("HSI")]],
            "the pane subscribes to the indices it was given"
        );
    }

    #[test]
    fn a_pane_should_show_the_quotes_its_session_publishes() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(
            &mut harness,
            source.clone(),
            config(&[selected("HSI"), selected("SPX")]),
        );
        follow(&mut harness, &mut pane, &source);

        report_quotes(&mut harness, &mut pane, &[("HSI", 101), ("SPX", 202)]);
        report_status(&mut harness, &mut pane, FinancialIndexStatus::Ready);

        assert_eq!(latest_of(&pane), vec![Some(101), Some(202)]);
        assert!(pane.widget.has_content());
        assert!(!pane.status.is_failed(harness.app()));
    }

    #[test]
    fn quotes_should_become_content_and_readiness_should_follow_them() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);

        report_status(&mut harness, &mut pane, FinancialIndexStatus::Ready);
        assert!(
            !pane.widget.has_content(),
            "a ready subscription with no quotes is not content"
        );

        report_quotes(&mut harness, &mut pane, &[("HSI", 101)]);
        assert!(
            pane.widget.has_content(),
            "quotes are what make a pane content"
        );

        build_body(&mut harness, &mut pane);
        assert!(pane.widget.content.is_some());
    }

    #[test]
    fn quotes_for_a_scope_the_pane_has_left_should_not_be_content() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        report_quotes(&mut harness, &mut pane, &[("HSI", 101)]);
        assert!(pane.widget.has_content());

        pane.widget.config.selection =
            normalize_selection(&[selected("SPX")]).expect("the fixture selection is valid");

        assert!(!pane.widget.has_content());
    }

    #[test]
    fn a_failed_scope_should_offer_the_recovery_it_was_given() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);

        report_failure(&mut harness, &mut pane);

        assert!(pane.status.is_failed(harness.app()));
        let state = pane.status.state(harness.app());
        let failure = state.failure().expect("the failure is reported");
        assert_eq!(failure.action().label_key(), "common.retry");
        assert!(failure_of(&harness, &pane).contains("Kytrix"));
    }

    #[test]
    fn messages_after_a_failure_should_be_ignored() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        report_failure(&mut harness, &mut pane);
        let failed = failure_of(&harness, &pane);

        report_quotes(&mut harness, &mut pane, &[("HSI", 101)]);
        report_status(&mut harness, &mut pane, FinancialIndexStatus::Ready);

        assert!(!pane.widget.has_content());
        assert_eq!(failure_of(&harness, &pane), failed);
    }

    #[test]
    fn a_refused_selection_should_say_how_many_indices_the_account_may_follow() {
        let source = FakeSource::with_session(2);
        source.refuse_selection();
        let mut harness = test_support::harness();
        let mut pane = pane(
            &mut harness,
            source,
            config(&[selected("HSI"), selected("SPX"), selected("IX3")]),
        );
        settle(
            &mut harness,
            &mut pane,
            "the refusal should be reported",
            |harness, pane| pane.status.is_failed(harness.app()),
        );

        assert_eq!(
            failure_of(&harness, &pane),
            "Your current limit is 2 indices"
        );
    }

    #[test]
    fn a_refusal_without_a_published_limit_should_still_say_what_happened() {
        let source = FakeSource::default();
        source.refuse(KytrixServiceError::Server(
            ErrorCode::FinancialIndexTopicLimitExceeded,
        ));
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source, config(&[selected("HSI")]));
        settle(
            &mut harness,
            &mut pane,
            "the refusal should be reported",
            |harness, pane| pane.status.is_failed(harness.app()),
        );

        assert_eq!(
            failure_of(&harness, &pane),
            "The selection exceeds your current index limit"
        );
    }

    #[test]
    fn changing_the_selection_should_follow_the_new_scope_and_drop_the_old_quotes() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        report_quotes(&mut harness, &mut pane, &[("HSI", 101)]);
        build_body(&mut harness, &mut pane);

        source.set_manifest(manifest_carrying(&[selected("SPX")]));
        let _next = source.subscribe();
        drive(&mut harness, &mut pane, |widget, cx| {
            widget.select_indices(vec![selected("SPX")], cx)
        });
        settle(
            &mut harness,
            &mut pane,
            "the new scope should be followed",
            |_, pane| {
                pane.widget.rows.len() == 1 && pane.widget.rows[0].index.topic == topic("SPX")
            },
        );

        assert_eq!(latest_of(&pane), vec![None], "the old quotes are gone");
        assert!(!pane.widget.has_content(), "nothing has quoted yet");
    }

    #[test]
    fn confirming_the_selection_a_pane_already_follows_should_change_nothing() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        report_quotes(&mut harness, &mut pane, &[("HSI", 101)]);
        build_body(&mut harness, &mut pane);
        let subscribed = source.subscribed();

        drive(&mut harness, &mut pane, |widget, cx| {
            widget.select_indices(vec![selected("HSI")], cx)
        });
        settle_frames(&mut harness, &mut pane);

        assert_eq!(source.subscribed(), subscribed, "nothing is re-subscribed");
        assert_eq!(latest_of(&pane), vec![Some(101)]);
    }

    #[test]
    fn signing_out_should_take_the_indices_with_it() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        report_quotes(&mut harness, &mut pane, &[("HSI", 101)]);
        build_body(&mut harness, &mut pane);
        assert!(pane.widget.has_content());

        set_account(&mut harness, None);
        settle(
            &mut harness,
            &mut pane,
            "the pane should sign out",
            |_, pane| !pane.widget.has_content(),
        );

        assert!(pane.widget.quotes.is_none());
        assert_eq!(
            notice_of(&harness, &pane),
            harness
                .services()
                .translate
                .translate("widget.financial_index.logged_out")
        );
    }

    #[test]
    fn an_account_change_should_subscribe_again_for_the_new_account() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        let _next = source.subscribe();
        let other = "11111111-2222-4333-8444-555555555555";

        set_account(&mut harness, Some(other));
        settle(
            &mut harness,
            &mut pane,
            "the new account should be followed",
            |_, pane| pane.widget.account_id.as_deref() == Some(other),
        );
        settle(
            &mut harness,
            &mut pane,
            "the stream should be reopened",
            |_harness, _pane| source.subscribed().len() >= 2,
        );

        assert_eq!(pane.widget.account_id.as_deref(), Some(other));
    }

    #[test]
    fn the_recovery_should_read_the_scope_again() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        report_failure(&mut harness, &mut pane);
        assert!(pane.status.is_failed(harness.app()));

        let _next = source.subscribe();
        drive(&mut harness, &mut pane, |widget, cx| widget.load(cx));
        settle(
            &mut harness,
            &mut pane,
            "the scope should be read again",
            |_, _| source.subscribed().len() == 2,
        );

        assert_eq!(source.subscribed().len(), 2);
    }

    #[test]
    fn the_recovery_should_keep_the_quotes_the_reader_was_reading() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        report_quotes_keeping_the_bridge(&mut harness, &mut pane, &[("HSI", 101)]);
        report_status(&mut harness, &mut pane, FinancialIndexStatus::Ready);
        report_failure(&mut harness, &mut pane);
        assert!(pane.status.is_failed(harness.app()));

        let _next = source.subscribe();
        drive(&mut harness, &mut pane, |widget, cx| widget.load(cx));
        settle(
            &mut harness,
            &mut pane,
            "the recovery should reopen the scope's stream",
            |_, _| source.subscribed().len() == 2,
        );

        assert_eq!(
            latest_of(&pane),
            vec![Some(101)],
            "a recovery keeps the quotes the reader was reading"
        );
        assert!(pane.widget.quotes.is_some());
        assert_eq!(
            pane.widget.accepted,
            pane.widget.current_scope(),
            "the kept quotes still belong to the scope the pane follows"
        );
        assert!(
            pane.widget.has_content(),
            "a recovery does not blank the pane while it reconnects"
        );
    }

    #[test]
    fn a_price_that_moved_should_flash_and_the_first_quotes_should_not() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);

        report_quotes(&mut harness, &mut pane, &[("HSI", 101)]);
        assert!(
            pane.widget.flashes.is_empty(),
            "the first quotes of a scope are where it starts, not a move"
        );

        report_quotes(&mut harness, &mut pane, &[("HSI", 102)]);
        let flash = *pane
            .widget
            .flashes
            .get(&topic("HSI"))
            .expect("a move should flash");
        assert_eq!(flash.direction, FinancialIndexDirection::Up);
        assert!(flash.is_active(Instant::now()));
    }

    #[test]
    fn a_flash_should_be_forgotten_once_it_has_faded() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        report_quotes(&mut harness, &mut pane, &[("HSI", 101)]);
        report_quotes(&mut harness, &mut pane, &[("HSI", 102)]);
        build_body(&mut harness, &mut pane);
        assert_eq!(pane.widget.flashes.len(), 1, "a visible flash is kept");

        pane.widget.flashes.insert(
            topic("HSI"),
            QuoteFlash {
                direction: FinancialIndexDirection::Up,
                started_at: Instant::now() - FLASH_DURATION - Duration::from_millis(1),
            },
        );
        settle_frames(&mut harness, &mut pane);

        assert!(
            pane.widget.flashes.is_empty(),
            "a flash that has run its course asks for no more frames"
        );
        assert!(
            harness.with_cx(|cx| cx.read(&pane.widget.flash_signal).is_empty()),
            "the drawn flash map is emptied with it"
        );
    }

    #[test]
    fn reduce_motion_should_leave_no_flash() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        let reduce_motion = harness.services().reduce_motion;
        harness.app_mut().write(&reduce_motion, true);

        report_quotes(&mut harness, &mut pane, &[("HSI", 101)]);
        report_quotes(&mut harness, &mut pane, &[("HSI", 102)]);

        assert!(
            pane.widget.flashes.is_empty(),
            "a reader who asked for less movement is not shown a market's worth of it"
        );
    }

    #[test]
    fn a_flash_should_cover_the_top_of_its_cell() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        report_quotes(&mut harness, &mut pane, &[("HSI", 101)]);
        report_quotes(&mut harness, &mut pane, &[("HSI", 102)]);
        pane.widget.flashes.insert(
            topic("HSI"),
            QuoteFlash {
                direction: FinancialIndexDirection::Up,
                started_at: Instant::now() - FLASH_PEAK,
            },
        );
        build_body(&mut harness, &mut pane);
        settle_frames(&mut harness, &mut pane);
        harness.drain();

        let grid_node =
            find(&harness, pane.parent, "TEST_INDICES_GRID").expect("the grid is drawn");
        let cell = find(&harness, grid_node, "TEST_INDICES_GRID_CELL").expect("a cell is drawn");
        let bar =
            find(&harness, cell, "TEST_INDICES_GRID_FLASH").expect("the moving cell draws its bar");
        let tree = harness.tree();
        let opacity = tree
            .get(bar)
            .expect("the bar is in the tree")
            .style_patch()
            .opacity
            .expect("the bar binds its opacity");

        assert!(opacity > 0.0, "a visible flash is drawn");
        let (cell_x, cell_y, cell_width, cell_height) = place(&harness, cell);
        let (bar_x, bar_y, bar_width, bar_height) = place(&harness, bar);
        assert_eq!(cell_width, GRID_CELL_WIDTH);
        assert_eq!(cell_height, GRID_CELL_HEIGHT);
        assert_eq!(bar_height, GRID_FLASH_HEIGHT);
        assert!(
            (bar_y - cell_y).abs() <= 1.5,
            "the bar is drawn over the top of its cell, not under its text: bar {bar_y}, \
             cell {cell_y}"
        );
        assert!(
            bar_x <= cell_x + 0.5 && bar_width >= cell_width - 1.0,
            "the bar spans the cell: bar {bar_x}..{}, cell {cell_x}..{}",
            bar_x + bar_width,
            cell_x + cell_width
        );
    }

    #[test]
    fn a_visible_flash_should_ask_for_frames_only_until_it_fades() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        report_quotes(&mut harness, &mut pane, &[("HSI", 101)]);
        report_quotes(&mut harness, &mut pane, &[("HSI", 102)]);
        build_body(&mut harness, &mut pane);
        let first = pane.widget.flash_timer.expect("a visible flash is timed");

        harness.advance(FLASH_TICK);
        sync(&mut harness, &mut pane);
        let next = pane
            .widget
            .flash_timer
            .expect("a flash still visible keeps asking for frames");
        assert_ne!(
            next, first,
            "the pane armed the next tick instead of leaving the dispatched one behind"
        );

        harness.advance(FLASH_TICK);
        sync(&mut harness, &mut pane);
        let third = pane
            .widget
            .flash_timer
            .expect("every visible tick is followed by the next one");
        assert_ne!(
            third, next,
            "a flash that is still moving keeps the pane timed"
        );

        pane.widget.flashes.insert(
            topic("HSI"),
            QuoteFlash {
                direction: FinancialIndexDirection::Up,
                started_at: Instant::now() - FLASH_DURATION - Duration::from_millis(1),
            },
        );
        sync(&mut harness, &mut pane);

        assert!(pane.widget.flashes.is_empty());
        assert!(
            pane.widget.flash_timer.is_none(),
            "a pane with nothing moving leaves no wakeup behind (I3)"
        );
    }

    #[test]
    fn releasing_a_flashing_pane_should_cancel_its_timer() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        report_quotes(&mut harness, &mut pane, &[("HSI", 101)]);
        report_quotes(&mut harness, &mut pane, &[("HSI", 102)]);
        build_body(&mut harness, &mut pane);
        let timer = pane
            .widget
            .flash_timer
            .expect("the pane is timed while the flash is visible");

        drive(&mut harness, &mut pane, |widget, cx| widget.release(cx));

        let cancelled = harness.app_mut().cancel_timer(&timer);
        assert!(pane.widget.flash_timer.is_none());
        assert!(
            !cancelled,
            "the released pane cancelled the timer it had armed"
        );
    }

    #[test]
    fn the_table_should_hold_the_rows_the_pane_draws() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(
            &mut harness,
            source.clone(),
            table_config(&[selected("HSI"), selected("SPX")]),
        );
        follow(&mut harness, &mut pane, &source);
        report_quotes(&mut harness, &mut pane, &[("HSI", 101)]);
        report_status(&mut harness, &mut pane, FinancialIndexStatus::Ready);
        build_body(&mut harness, &mut pane);

        assert_eq!(table(&pane).rows(), 2);
        assert_eq!(cell_text(&harness, &pane, 0, 0).trim(), "HSI English");
        assert_eq!(cell_text(&harness, &pane, 0, 1), "1.01");
        assert_eq!(cell_text(&harness, &pane, 0, 2), "+0.01");
        assert_eq!(cell_text(&harness, &pane, 0, 3), "+1.00%");
    }

    #[test]
    fn the_table_should_grow_and_shrink_with_the_selection() {
        let source = FakeSource::with_session(8);
        let mut harness = test_support::harness();
        let two = [selected("HSI"), selected("SPX")];
        let mut pane = pane(&mut harness, source.clone(), table_config(&two));
        follow(&mut harness, &mut pane, &source);
        report_quotes(&mut harness, &mut pane, &[("HSI", 101), ("SPX", 202)]);
        report_status(&mut harness, &mut pane, FinancialIndexStatus::Ready);
        build_body(&mut harness, &mut pane);
        assert_eq!(table(&pane).rows(), 2);

        let three = [selected("HSI"), selected("SPX"), selected("IX3")];
        source.set_manifest(manifest_carrying(&three));
        let _next_row = source.subscribe();
        drive(&mut harness, &mut pane, |widget, cx| {
            widget.select_indices(three.to_vec(), cx)
        });
        settle(
            &mut harness,
            &mut pane,
            "the table should grow a row",
            |_harness, pane| pane.widget.rows.len() == 3 && table(pane).rows() == 3,
        );
        assert_eq!(
            table(&pane).rows(),
            3,
            "a newly selected index is a row of the table"
        );

        source.set_manifest(manifest_carrying(&two));
        let _next_row = source.subscribe();
        drive(&mut harness, &mut pane, |widget, cx| {
            widget.select_indices(two.to_vec(), cx)
        });
        settle(
            &mut harness,
            &mut pane,
            "the table should drop the row",
            |_harness, pane| pane.widget.rows.len() == 2 && table(pane).rows() == 2,
        );
        assert_eq!(
            table(&pane).rows(),
            2,
            "the row of a dropped index is not left behind"
        );
    }

    #[test]
    fn a_row_without_a_quote_should_say_whether_the_pane_is_working() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(
            &mut harness,
            source.clone(),
            table_config(&[selected("HSI"), selected("SPX")]),
        );
        follow(&mut harness, &mut pane, &source);
        report_quotes(&mut harness, &mut pane, &[("HSI", 101)]);
        report_status(&mut harness, &mut pane, FinancialIndexStatus::Ready);
        build_body(&mut harness, &mut pane);

        assert_eq!(cell_text(&harness, &pane, 1, 1), "--");
        assert_eq!(
            table(&pane)
                .cell_node(harness.app(), 1, 1)
                .map(|node| harness.text_of(node)),
            Some("--".to_owned())
        );
    }

    #[test]
    fn a_failure_should_tell_the_table_the_pane_stopped_working() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(
            &mut harness,
            source.clone(),
            table_config(&[selected("HSI"), selected("SPX")]),
        );
        follow(&mut harness, &mut pane, &source);
        report_quotes(&mut harness, &mut pane, &[("HSI", 101)]);
        build_body(&mut harness, &mut pane);
        report_failure(&mut harness, &mut pane);
        settle_frames(&mut harness, &mut pane);

        assert!(!pane.widget.busy);
        assert_eq!(
            table(&pane)
                .cell_node(harness.app(), 1, 1)
                .map(|node| harness.text_of(node)),
            Some("--".to_owned()),
            "a failed pane is not still connecting"
        );
    }

    #[test]
    fn unchanged_rows_should_not_be_rebuilt_for_every_frame() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(
            &mut harness,
            source.clone(),
            table_config(&[selected("HSI")]),
        );
        follow(&mut harness, &mut pane, &source);
        report_quotes(&mut harness, &mut pane, &[("HSI", 101)]);
        build_body(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        harness.drain();
        let requests = harness.frame_requests();

        sync(&mut harness, &mut pane);
        harness.drain();

        assert_eq!(
            harness.frame_requests(),
            requests,
            "a steady pane writes nothing and asks for nothing (I3)"
        );
    }

    #[test]
    fn the_grid_should_hold_the_rows_the_pane_draws() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(
            &mut harness,
            source.clone(),
            config(&[selected("HSI"), selected("SPX")]),
        );
        follow(&mut harness, &mut pane, &source);
        report_quotes(&mut harness, &mut pane, &[("HSI", 101), ("SPX", 202)]);
        report_status(&mut harness, &mut pane, FinancialIndexStatus::Ready);
        build_body(&mut harness, &mut pane);

        assert_eq!(
            grid(&pane).count(harness.app()),
            1,
            "both cells fit on one line"
        );
        let grid_node =
            find(&harness, pane.parent, "TEST_INDICES_GRID").expect("the grid is drawn");
        let line = grid(&pane)
            .mounted_rows(harness.app())
            .first()
            .map(|(_, node)| *node)
            .expect("a line is mounted");
        let texts = texts_of(&harness, line);

        assert_eq!(
            texts
                .iter()
                .filter(|text| text.as_str() == "HSI English")
                .count(),
            1,
            "the first cell draws its index: {texts:?}"
        );
        assert_eq!(
            texts
                .iter()
                .filter(|text| text.as_str() == "SPX English")
                .count(),
            1,
            "the second cell draws its index: {texts:?}"
        );
        assert!(texts.iter().any(|text| text == "1.01"), "{texts:?}");
        assert!(texts.iter().any(|text| text == "2.02"), "{texts:?}");
        assert!(find(&harness, grid_node, "TEST_INDICES_GRID_CELL").is_some());
    }

    #[test]
    fn the_grid_should_draw_fixed_width_cells_in_a_centered_block() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(
            &mut harness,
            source.clone(),
            config(&[selected("HSI"), selected("SPX")]),
        );
        follow(&mut harness, &mut pane, &source);
        report_quotes(&mut harness, &mut pane, &[("HSI", 101), ("SPX", 202)]);
        report_status(&mut harness, &mut pane, FinancialIndexStatus::Ready);
        build_body(&mut harness, &mut pane);
        harness.drain();

        let grid_node =
            find(&harness, pane.parent, "TEST_INDICES_GRID").expect("the grid is drawn");
        let (grid_x, _, grid_width, _) = place(&harness, grid_node);
        assert_eq!(grid_width, BODY_WIDTH, "the grid is as wide as its body");
        let line = grid(&pane)
            .mounted_rows(harness.app())
            .first()
            .map(|(_, node)| *node)
            .expect("a line is mounted");
        let cells = nodes_with_id(&harness, line, "TEST_INDICES_GRID_CELL");
        assert_eq!(cells.len(), 2, "the line holds one cell per selected index");

        let (first_x, _, first_width, first_height) = place(&harness, cells[0]);
        let (second_x, _, second_width, _) = place(&harness, cells[1]);
        assert_eq!(
            (first_width, first_height),
            (GRID_CELL_WIDTH, GRID_CELL_HEIGHT),
            "a cell keeps the old 160x88 size"
        );
        assert_eq!(second_width, GRID_CELL_WIDTH);
        assert!(
            (first_x - (grid_x + 160.0)).abs() <= 0.5,
            "two 160 px cells in a 640 px line are centred: first cell at {first_x}"
        );
        assert!(
            (second_x - (first_x + GRID_CELL_WIDTH)).abs() <= 0.5,
            "cells sit against each other, as the old grid placed them"
        );
    }

    #[test]
    fn the_grid_should_wrap_new_lines_when_the_selection_grows() {
        let source = FakeSource::with_session(8);
        let mut harness = test_support::harness();
        let four = [
            selected("HSI"),
            selected("SPX"),
            selected("IX3"),
            selected("IX4"),
        ];
        let mut pane = pane(&mut harness, source.clone(), config(&four));
        follow(&mut harness, &mut pane, &source);
        build_body(&mut harness, &mut pane);
        assert_eq!(pane.widget.rows.len(), 4);
        assert_eq!(
            grid(&pane).count(harness.app()),
            1,
            "four cells fit on the 640 px line"
        );

        let five = [
            selected("HSI"),
            selected("SPX"),
            selected("IX3"),
            selected("IX4"),
            selected("IX5"),
        ];
        source.set_manifest(manifest_carrying(&five));
        let _next_line = source.subscribe();
        drive(&mut harness, &mut pane, |widget, cx| {
            widget.select_indices(five.to_vec(), cx)
        });
        settle(
            &mut harness,
            &mut pane,
            "the fifth cell should wrap to a second line",
            |harness, pane| pane.widget.rows.len() == 5 && grid(pane).count(harness.app()) == 2,
        );
        assert_eq!(
            grid(&pane).count(harness.app()),
            2,
            "a fifth cell is a second line, not an invisible row"
        );

        source.set_manifest(manifest_carrying(&four));
        let _next_line = source.subscribe();
        drive(&mut harness, &mut pane, |widget, cx| {
            widget.select_indices(four.to_vec(), cx)
        });
        settle(
            &mut harness,
            &mut pane,
            "the dropped cell should leave the second line empty",
            |harness, pane| pane.widget.rows.len() == 4 && grid(pane).count(harness.app()) == 1,
        );
        assert_eq!(
            grid(&pane).count(harness.app()),
            1,
            "the line the dropped index left behind is gone"
        );
    }

    #[test]
    fn an_empty_answer_should_say_there_is_no_data() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(
            &mut harness,
            source.clone(),
            table_config(&[selected("HSI")]),
        );
        follow(&mut harness, &mut pane, &source);
        report_status(&mut harness, &mut pane, FinancialIndexStatus::Ready);
        drive(&mut harness, &mut pane, |widget, cx| {
            widget.config.selection.clear();
            widget.publish_rows(cx);
        });
        build_body(&mut harness, &mut pane);
        settle_frames(&mut harness, &mut pane);

        assert_eq!(table(&pane).rows(), 0);
        let empty = table(&pane)
            .empty_node()
            .expect("an empty table draws its sentence");
        assert_eq!(harness.text_of(empty), "No data");
    }

    #[test]
    fn switching_the_display_mode_should_redraw_the_rows_the_pane_holds() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        report_quotes(&mut harness, &mut pane, &[("HSI", 101)]);
        report_status(&mut harness, &mut pane, FinancialIndexStatus::Ready);
        build_body(&mut harness, &mut pane);
        assert!(matches!(pane.widget.content, Some(BodyContent::Grid(_))));

        drive(&mut harness, &mut pane, |widget, cx| {
            widget.set_mode(FinancialIndexDisplayMode::Table, cx)
        });
        settle_frames(&mut harness, &mut pane);

        assert!(matches!(pane.widget.content, Some(BodyContent::Table(_))));
        assert_eq!(
            cell_text(&harness, &pane, 0, 1),
            "1.01",
            "the table draws the rows the pane already holds"
        );
        assert!(pane.widget.has_content(), "no scope was re-read");
    }

    #[test]
    fn the_mode_controls_should_be_exclusive() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        build_body(&mut harness, &mut pane);
        build_header(&mut harness, &mut pane);
        let (grid_action, table_action) = {
            let right = pane
                .widget
                .header_right
                .as_ref()
                .expect("the mode switch is built");
            (
                right
                    .grid
                    .button
                    .action_identity()
                    .expect("the grid half registers an action"),
                right
                    .table
                    .button
                    .action_identity()
                    .expect("the table half registers an action"),
            )
        };
        let node = pane
            .widget
            .header_right
            .as_ref()
            .and_then(|right| right.table.button.node())
            .expect("the table half is a node");

        let window = harness.window();
        let action = table_action;
        let source = ui_core::event::ActionSource::Click {
            click_count: 1,
            button: ui_core::event::PointerButton::Primary,
        };
        harness
            .app_mut()
            .dispatch_action(window, node, action, source);
        settle_frames(&mut harness, &mut pane);

        assert_eq!(pane.widget.config.mode, FinancialIndexDisplayMode::Table);
        assert!(matches!(pane.widget.content, Some(BodyContent::Table(_))));

        let window = harness.window();
        let node = pane
            .widget
            .header_right
            .as_ref()
            .and_then(|right| right.grid.button.node())
            .expect("the grid half is a node");
        harness
            .app_mut()
            .dispatch_action(window, node, grid_action, source);
        settle_frames(&mut harness, &mut pane);

        assert_eq!(pane.widget.config.mode, FinancialIndexDisplayMode::Grid);
        assert!(matches!(pane.widget.content, Some(BodyContent::Grid(_))));
    }

    #[test]
    fn a_selected_index_the_manifest_dropped_should_go_away() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane_over(
            &mut harness,
            source,
            config(&[selected("HSI"), selected("IX3")]),
            |source| source.set_manifest(test_support::manifest()),
        );
        settle(
            &mut harness,
            &mut pane,
            "the selection should be settled",
            |_harness, pane| pane.widget.config.selection.len() == 1,
        );

        assert_eq!(pane.widget.config.selection[0].topic, topic("HSI"));
    }

    #[test]
    fn a_manifest_read_that_failed_should_leave_the_selection_alone() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane_over(&mut harness, source, config(&[selected("HSI")]), |source| {
            source.set_manifest(proto_types::FinancialIndices {
                categories: Vec::new(),
                indices: vec![proto_types::FinancialIndexDefinition {
                    code: "HSI".to_owned(),
                    category: "missing".to_owned(),
                    name: None,
                    request_code: ".HSI".to_owned(),
                }],
            });
        });
        settle_frames(&mut harness, &mut pane);

        assert_eq!(
            pane.widget.config.selection.len(),
            1,
            "a read that proved nothing leaves the selection alone"
        );
    }

    #[test]
    fn a_selected_index_the_manifest_dropped_should_never_be_subscribed_to() {
        let source = FakeSource::with_session(4);
        let _control = source.subscribe();
        let mut harness = test_support::harness();
        let mut pane = pane_over(
            &mut harness,
            source.clone(),
            config(&[selected("IX3"), selected("HSI")]),
            |source| source.set_manifest(test_support::manifest()),
        );
        settle(
            &mut harness,
            &mut pane,
            "the settled scope should be followed",
            |_harness, _pane| !source.subscribed().is_empty(),
        );

        assert_eq!(source.subscribed(), vec![vec![topic("HSI")]]);
    }

    #[test]
    fn a_selection_the_manifest_dropped_entirely_should_subscribe_to_nothing() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane_over(
            &mut harness,
            source.clone(),
            config(&[selected("IX3")]),
            |source| source.set_manifest(test_support::manifest()),
        );
        settle(
            &mut harness,
            &mut pane,
            "the pane should go back to waiting",
            |harness, pane| pane.status.state(harness.app()).notice().is_some(),
        );

        assert!(source.subscribed().is_empty());
        assert!(pane.widget.config.selection.is_empty());
        assert_eq!(
            notice_of(&harness, &pane),
            harness
                .services()
                .translate
                .translate("widget.financial_index.select_at_least_one")
        );
    }

    #[test]
    fn a_late_catalogue_answer_should_not_settle_the_new_selection() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source, config(&[selected("HSI")]));
        drive(&mut harness, &mut pane, |widget, cx| {
            widget.select_indices(vec![selected("SPX")], cx)
        });
        assert_eq!(
            pane.publishes.get(),
            1,
            "the new selection was written once"
        );

        deliver_manifest(
            &mut harness,
            &mut pane,
            RequestId::new(1),
            manifest_answer(&[selected("HSI")]),
        );

        assert_eq!(
            pane.widget.config.selection,
            vec![selected("SPX")],
            "an answer to the replaced read is not the new selection's to settle"
        );
        assert_eq!(
            pane.publishes.get(),
            1,
            "the late answer persisted a selection the user did not make"
        );
        assert_eq!(
            pane.widget.manifest_pending.dropped(),
            1,
            "the late answer was dropped and counted"
        );

        deliver_manifest(
            &mut harness,
            &mut pane,
            RequestId::new(2),
            manifest_answer(&[selected("SPX")]),
        );
        assert_eq!(pane.widget.config.selection, vec![selected("SPX")]);
        assert_eq!(
            pane.widget.manifest_pending.dropped(),
            1,
            "an answer to the command in flight is applied, not dropped"
        );
    }

    #[test]
    fn a_replaced_commands_failure_should_not_fail_the_pane() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source, config(&[selected("HSI")]));
        deliver_manifest(
            &mut harness,
            &mut pane,
            RequestId::new(1),
            manifest_answer(&[selected("HSI")]),
        );
        drive(&mut harness, &mut pane, |widget, cx| {
            widget.select_indices(vec![selected("SPX")], cx)
        });
        deliver_manifest(
            &mut harness,
            &mut pane,
            RequestId::new(2),
            manifest_answer(&[selected("SPX")]),
        );

        deliver_stream(
            &mut harness,
            &mut pane,
            RequestId::new(1),
            Err(RpcCallError::ServiceClosed {
                request_id: RequestId::new(1),
            }),
        );

        assert!(
            !pane.status.is_failed(harness.app()),
            "a replaced command's refusal is not the current command's"
        );
        assert_eq!(
            pane.widget.stream_pending.dropped(),
            1,
            "the late refusal was dropped and counted"
        );

        deliver_stream(
            &mut harness,
            &mut pane,
            RequestId::new(2),
            Err(RpcCallError::ServiceClosed {
                request_id: RequestId::new(2),
            }),
        );

        assert!(
            pane.status.is_failed(harness.app()),
            "the command in flight reports its own failure"
        );
    }

    #[test]
    fn the_header_should_offer_the_selector_a_signed_in_pane_can_open() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        build_body(&mut harness, &mut pane);
        let (left, _right) = build_header(&mut harness, &mut pane);

        let wrapper =
            find(&harness, left, "TEST_INDICES_CHOOSE").expect("the choose control is built");
        let button = pane
            .widget
            .header_left
            .as_ref()
            .expect("the left group is kept");
        assert_eq!(button.wrapper, wrapper);
        assert!(!button.built.1, "an account has something to choose from");
    }

    #[test]
    fn a_signed_out_pane_should_not_offer_the_selector() {
        let mut harness = test_support::harness();
        let mut pane = signed_out_pane(&mut harness, FakeSource::default());
        settle_frames(&mut harness, &mut pane);
        build_body(&mut harness, &mut pane);
        build_header(&mut harness, &mut pane);

        let button = pane
            .widget
            .header_left
            .as_ref()
            .expect("the left group is built");
        assert!(
            button.built.1,
            "a catalogue is served to an account, so there is nothing to choose from"
        );
    }

    #[test]
    fn a_signed_out_pane_should_not_open_the_selector_from_its_header() {
        let mut harness = test_support::harness();
        let mut pane = signed_out_pane(&mut harness, FakeSource::default());
        settle_frames(&mut harness, &mut pane);
        build_body(&mut harness, &mut pane);
        build_header(&mut harness, &mut pane);
        let (node, action) = {
            let control = pane
                .widget
                .header_left
                .as_ref()
                .expect("the left group is built");
            (
                control.choose.node().expect("the control is a node"),
                control
                    .choose
                    .action_identity()
                    .expect("the control registers an action"),
            )
        };

        let window = harness.window();
        harness.app_mut().dispatch_action(
            window,
            node,
            action,
            ui_core::event::ActionSource::Click {
                click_count: 1,
                button: ui_core::event::PointerButton::Primary,
            },
        );
        settle_frames(&mut harness, &mut pane);

        assert!(
            pane.widget.selector.is_none(),
            "a pane with no account has nothing to choose from, so the request is refused"
        );
    }

    #[test]
    fn choosing_should_open_the_selector() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        build_body(&mut harness, &mut pane);
        build_header(&mut harness, &mut pane);

        drive(&mut harness, &mut pane, |widget, cx| {
            widget
                .apply_request(HeaderRequest::Choose, cx)
                .expect("the selector opens")
        });

        assert!(
            pane.widget.selector.is_some(),
            "the pane holds the selector it opened"
        );
        harness.drain();
        sync(&mut harness, &mut pane);
        assert!(pane.widget.selector.is_some(), "an open selector stays");
    }

    #[test]
    fn a_selector_that_confirms_should_replace_the_selection() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        source.set_manifest(manifest_carrying(&[selected("SPX")]));
        let _next = source.subscribe();

        let confirmed = pane.widget.confirmed;
        harness.with_cx(|cx| {
            cx.write(&confirmed, Some(vec![selected("SPX")]));
        });
        settle(
            &mut harness,
            &mut pane,
            "the confirmation should be applied",
            |_harness, pane| {
                pane.widget.config.selection.len() == 1
                    && pane.widget.config.selection[0].topic == topic("SPX")
            },
        );

        assert!(pane.widget.current_scope().is_some());
    }

    #[test]
    fn the_header_should_be_rebuilt_when_the_language_moves() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        build_body(&mut harness, &mut pane);
        let (left, _right) = build_header(&mut harness, &mut pane);
        let english = pane
            .widget
            .header_left
            .as_ref()
            .map(|control| control.built)
            .expect("the left group records what it was built with");

        harness.switch_language(LanguageCode::ZhCn);
        settle_frames(&mut harness, &mut pane);

        let rebuilt = pane
            .widget
            .header_left
            .as_ref()
            .map(|control| control.built)
            .expect("the left group is rebuilt");
        assert_ne!(rebuilt.0, english.0, "the language moved");
        assert!(
            find(&harness, left, "TEST_INDICES_CHOOSE").is_some(),
            "the control is rebuilt in the same place"
        );
    }

    #[test]
    fn the_table_should_be_rebuilt_when_the_language_moves() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(
            &mut harness,
            source.clone(),
            table_config(&[selected("HSI")]),
        );
        follow(&mut harness, &mut pane, &source);
        report_quotes(&mut harness, &mut pane, &[("HSI", 101)]);
        build_body(&mut harness, &mut pane);
        let english = cell_text(&harness, &pane, 0, 0);
        let header = table(&pane)
            .header_node(harness.app(), 0)
            .map(|node| harness.text_of(node));

        harness.switch_language(LanguageCode::ZhCn);
        settle_frames(&mut harness, &mut pane);

        assert_ne!(cell_text(&harness, &pane, 0, 0), english);
        assert_ne!(
            table(&pane)
                .header_node(harness.app(), 0)
                .map(|node| harness.text_of(node)),
            header
        );
    }

    #[test]
    fn two_panes_should_keep_their_own_header_handlers() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut first = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        let parent = first.parent;
        let mut second = peer_pane(
            &mut harness,
            source.clone(),
            config(&[selected("SPX")]),
            parent,
        );
        follow(&mut harness, &mut first, &source);
        follow(&mut harness, &mut second, &source);
        build_header(&mut harness, &mut first);
        build_header(&mut harness, &mut second);

        let first_button = first
            .widget
            .header_left
            .as_ref()
            .expect("the first pane has a control");
        let second_button = second
            .widget
            .header_left
            .as_ref()
            .expect("the second pane has a control");
        let first_action = first_button
            .choose
            .action_identity()
            .expect("the control registers an action");
        let second_action = second_button
            .choose
            .action_identity()
            .expect("the control registers an action");

        assert_ne!(first_action, second_action, "two panes choose two keys");
        assert!(harness.app().has_action_handler(&first_action));
        assert!(harness.app().has_action_handler(&second_action));

        drive(&mut harness, &mut first, |widget, cx| widget.release(cx));
        assert!(!harness.app().has_action_handler(&first_action));
        assert!(
            harness.app().has_action_handler(&second_action),
            "releasing one pane removed another pane's handler"
        );
    }

    #[test]
    fn release_should_leave_nothing_of_the_pane_behind() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);
        report_quotes(&mut harness, &mut pane, &[("HSI", 101)]);
        build_body(&mut harness, &mut pane);
        build_header(&mut harness, &mut pane);
        let live_before = harness.node_count();
        let choose_action = pane
            .widget
            .header_left
            .as_ref()
            .and_then(|control| control.choose.action_identity())
            .expect("the choose control registers an action");
        let mode_action = pane
            .widget
            .header_right
            .as_ref()
            .and_then(|right| right.table.button.action_identity())
            .expect("the table half registers an action");
        assert!(harness.app().has_action_handler(&choose_action));
        assert!(harness.app().has_action_handler(&mode_action));

        drive(&mut harness, &mut pane, |widget, cx| widget.release(cx));

        assert!(pane.widget.stream.is_none(), "the bridge is dropped");
        assert!(
            pane.widget.manifest.is_none(),
            "the catalogue bridge is dropped"
        );
        assert!(pane.widget.content.is_none());
        assert!(pane.widget.header_left.is_none());
        assert!(pane.widget.header_right.is_none());
        assert!(pane.widget.flash_timer.is_none());
        assert!(pane.widget.selector.is_none());
        assert!(
            !harness.app().has_action_handler(&choose_action),
            "the released pane left its header handler behind"
        );
        assert!(!harness.app().has_action_handler(&mode_action));
        assert!(harness.layer().is_empty(harness.window()));
        assert!(
            harness.node_count() < live_before,
            "the content the pane built is removed"
        );

        harness.drain();
        let requests = harness.frame_requests();
        harness.drain();
        assert_eq!(
            harness.frame_requests(),
            requests,
            "a released pane asks for no further frame"
        );
    }

    #[test]
    fn every_message_this_pane_shows_should_resolve_to_text() {
        let source = FakeSource::with_session(4);
        let mut harness = test_support::harness();
        let mut pane = pane(&mut harness, source.clone(), config(&[selected("HSI")]));
        follow(&mut harness, &mut pane, &source);

        for language in [LanguageCode::ZhCn, LanguageCode::En] {
            harness.switch_language(language);
            let notices = [
                Notice::message("widget.financial_index.logged_out"),
                Notice::message("widget.financial_index.select_at_least_one"),
            ];
            for notice in notices {
                let text = notice.resolve(&harness.services().translate);
                assert!(!text.is_empty(), "{notice:?} resolves in {language:?}");
                assert!(!text.is_empty(), "the sentence resolves in the language");
            }
            report_failure(&mut harness, &mut pane);
            let failure = failure_of(&harness, &pane);
            assert!(!failure.is_empty(), "a failure resolves in {language:?}");
            assert!(!failure.contains('<'));
        }
        assert_eq!(
            harness
                .services()
                .translate
                .translate("widget.financial_index.index"),
            "Index"
        );
    }
}

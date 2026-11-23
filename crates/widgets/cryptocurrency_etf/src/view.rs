use std::{cell::Cell, rc::Rc, sync::Arc};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::{App, TimerId},
    error::EngineError,
    event::Event,
    node::{builder::NodeBuilder, dependency::ReadCtx, id::NodeId, signal::Signal},
    paint::style::StylePatch,
    text::TextStyle,
    window::WindowId,
};
use ui_kit::{
    kit::{
        button::Button,
        icon::IconKind,
        select::{Select, SelectItem, SelectOptions},
        separator::Separator,
        size::{UnifiedSizable, UnifiedSize},
    },
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};
use widget::{
    binding::{Inbound, WidgetBinding, WidgetRpcResult},
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

use crate::{
    chart::{CryptocurrencyEtfChart, Metrics},
    model::{
        CryptocurrencyEtfAsset, CryptocurrencyEtfFundSelection, CryptocurrencyEtfParseError,
        CryptocurrencyEtfSnapshot, CryptocurrencyEtfWindow,
    },
    service::{
        CryptocurrencyEtfFetchError, CryptocurrencyEtfProtocol, CryptocurrencyEtfRequest,
        CryptocurrencyEtfResponse, CryptocurrencyEtfService, CryptocurrencyEtfServiceError,
        CryptocurrencyEtfSource, CryptocurrencyEtfStatus, CryptocurrencyEtfStreamKey,
        CryptocurrencyEtfStreamMessage,
    },
};

pub(crate) const COUNTDOWN_TICK: std::time::Duration = std::time::Duration::from_millis(250);

const METRICS_HEIGHT: f32 = 58.0;

const REFRESH_KEY_SUFFIX: &str = "cryptocurrency_etf.refresh";
const ASSET_KEY_SUFFIX: &str = "cryptocurrency_etf.asset";
const FUND_KEY_SUFFIX: &str = "cryptocurrency_etf.fund";
const WINDOW_KEY_SUFFIX: &str = "cryptocurrency_etf.window";

const ALL_FUNDS_ID: &str = "all";

const FUND_ID_PREFIX: &str = "fund:";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CryptocurrencyEtfConfig;

impl WidgetConfig for CryptocurrencyEtfConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CryptocurrencyEtfFailure {
    Fetch(CryptocurrencyEtfFetchError),
    Rpc(RpcCallError<CryptocurrencyEtfServiceError>),
}

impl TranslatedError for CryptocurrencyEtfFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Fetch(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
}

impl TranslatedError for CryptocurrencyEtfFetchError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Service(error) => error.translated_message(translate),
            Self::Parse(error) => error.translated_message(translate),
            Self::Timeout(timeout) => t_smol!(
                translate,
                "error.cryptocurrency_etf.timeout",
                seconds = timeout.as_secs()
            ),
        }
    }
}

impl TranslatedError for CryptocurrencyEtfServiceError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::StreamClosed | Self::StreamBusy => {
                t_smol!(translate, "error.cryptocurrency_etf.stream_unavailable")
            }
        }
    }
}

impl TranslatedError for CryptocurrencyEtfParseError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::EmptyAssetMap { .. } => t_smol!(translate, "error.cryptocurrency_etf.no_data"),
            Self::InvalidDate { value, .. } => t_smol!(
                translate,
                "error.cryptocurrency_etf.invalid_date",
                value = value.as_str()
            ),
            Self::DuplicateDate { date, .. } => t_smol!(
                translate,
                "error.cryptocurrency_etf.duplicate_date",
                value = date.to_string()
            ),
            Self::TooManyRows { maximum, .. } => t_smol!(
                translate,
                "error.cryptocurrency_etf.too_many_rows",
                maximum = maximum,
                value = maximum.saturating_add(1)
            ),
            Self::InvalidNumber { value, .. } => t_smol!(
                translate,
                "error.cryptocurrency_etf.invalid_number",
                value = value.as_str()
            ),
            Self::NonFiniteTotal { .. } | Self::NonFiniteAssets { .. } => {
                t_smol!(translate, "error.cryptocurrency_etf.non_finite")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum HeaderRequest {
    Refresh,
    Asset(CryptocurrencyEtfAsset),
    Window(CryptocurrencyEtfWindow),
    Fund(CryptocurrencyEtfFundSelection),
}

struct Countdown {
    window: WindowId,
    deadline: Cell<Option<i64>>,
    label: Signal<Option<SmolStr>>,
    now: Rc<dyn Fn() -> i64>,
    timer: std::cell::RefCell<Option<TimerId>>,
}

impl Countdown {
    fn new(cx: &mut WidgetCx<'_>, now: Rc<dyn Fn() -> i64>) -> Self {
        Self {
            window: cx.window(),
            deadline: Cell::new(None),
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
                log::error!("the ETF countdown could not arm its next tick: {error}");
            }
        }
    }

    fn tick(self: &Rc<Self>, app: &mut App) {
        *self.timer.borrow_mut() = None;
        self.write_label(app);
        self.arm(app);
    }
}

struct HeaderControls {
    root: NodeId,
    refresh_slot: NodeId,
    language: LanguageCode,
    button: Button,
    asset: Select,
    fund: Select,
    window: Select,
    fund_items: Vec<SmolStr>,
}

pub struct CryptocurrencyEtfWidget {
    asset: CryptocurrencyEtfAsset,
    window: CryptocurrencyEtfWindow,
    fund: CryptocurrencyEtfFundSelection,
    binding: Option<WidgetBinding<CryptocurrencyEtfProtocol>>,
    status: WidgetStatus,
    recovery: WidgetAction,
    chart: Option<CryptocurrencyEtfChart>,
    chart_host: Option<NodeId>,
    metrics_parent: Option<NodeId>,
    metrics_cells: Vec<NodeId>,
    metrics_show_net_assets: bool,
    metrics: Signal<Option<Metrics>>,
    snapshot: Option<Arc<CryptocurrencyEtfSnapshot>>,
    accepted: Option<u64>,
    ready: Option<u64>,
    next_refresh_at: Option<i64>,
    countdown: Rc<Countdown>,
    refresh_pending: bool,
    load_pending: bool,
    last_request_id: Option<RequestId>,
    account_id: Option<SmolStr>,
    requests: Signal<Option<HeaderRequest>>,
    header: Option<HeaderControls>,
}

impl WidgetView for CryptocurrencyEtfWidget {
    type Config = CryptocurrencyEtfConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.cryptocurrency_etf");
    const DISPLAY_NAME: &'static str = "widget.names.cryptocurrency_etf";
    const CATEGORY: WidgetCategory = WidgetCategory::Statistics;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.cryptocurrency_etf");
    const ICON: Option<&'static str> = Some("widgets/cryptocurrency_etf.png");

    fn new(
        _instance_id: WidgetInstanceId,
        _config: Self::Config,
        status: WidgetStatus,
        _reload: ReloadHandle,
        _config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError> {
        let source = cx.services().kytrix_service().cloned();
        let now: Rc<dyn Fn() -> i64> = Rc::new(unix_millis);

        Ok(Self::assemble(source, now, status, recovery, cx))
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        let retry = self.status.is_failed(cx.app());
        let account = cx.account();
        let kept = retry && account == self.account_id;
        self.account_id = account;
        self.start(kept, cx);
    }

    fn has_content(&self) -> bool {
        self.chart.is_some() && self.accepted.is_some()
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let root = NodeBuilder::boxed()
            .id("TEST_ETF")
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .size_full()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;

        let row = NodeBuilder::boxed()
            .id("TEST_ETF_ROW")
            .style(
                StyleBuilder::new()
                    .width_full()
                    .height(METRICS_HEIGHT)
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(12.0)
                    .padding_x(12.0)
                    .build(),
            )
            .build(cx.tree_mut(), root)?;
        Separator::horizontal(cx.theme().clone()).build(cx.tree_mut(), root)?;
        let chart_host = NodeBuilder::boxed()
            .id("TEST_ETF_CHART_HOST")
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .min_height_0()
                    .min_width_0()
                    .flex_col()
                    .build(),
            )
            .build(cx.tree_mut(), root)?;

        let show_net_assets = self.show_net_assets();
        self.rebuild_metrics_cells(row, show_net_assets, cx)?;
        self.metrics_show_net_assets = show_net_assets;
        self.metrics_parent = Some(row);
        self.chart_host = Some(chart_host);

        if let Some(chart) = self.chart.as_mut() {
            chart.build(cx, chart_host)?;
        }
        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.drain_inbox(cx);

        let account = cx.account();
        if account != self.account_id {
            self.account_id = account;
            self.start(false, cx);
        }

        let request = cx.read(&self.requests).clone();
        if let Some(request) = request {
            cx.write(&self.requests, None);
            self.apply_header_request(request, cx);
        }

        if self.has_content() && self.chart_host.is_none() {
            cx.request_frame()?;
        }

        self.sync_chart_locale(cx);
        self.sync_metrics_row(cx)?;
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
        if group != HeaderGroup::Right {
            return Ok(());
        }
        self.release_header(cx, true);
        self.build_header(parent, cx)
    }

    fn dispatch(&mut self, event: &Event, cx: &mut WidgetCx<'_>) -> bool {
        let Event::Input(input) = event else {
            return false;
        };
        let Some(chart) = self.chart.as_mut() else {
            return false;
        };
        let moved = match input {
            ui_core::event::InputEvent::PointerMove { position, .. }
            | ui_core::event::InputEvent::PointerEnter { position, .. } => {
                chart.point_at(*position)
            }
            ui_core::event::InputEvent::PointerLeave { .. } => chart.clear_hover(),
            _ => return false,
        };
        if moved {
            let _ = cx.request_frame();
        }
        true
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        self.binding = None;
        self.release_header(cx, false);
        if let Some(mut chart) = self.chart.take() {
            chart.release(cx);
        }
        Countdown::update(&self.countdown, cx.app_mut(), None);
        self.chart_host = None;
        self.metrics_parent = None;
        self.metrics_cells.clear();
        self.snapshot = None;
        self.accepted = None;
        self.ready = None;
        self.next_refresh_at = None;
    }
}

impl CryptocurrencyEtfWidget {
    fn assemble<S: CryptocurrencyEtfSource>(
        source: Option<S>,
        now: Rc<dyn Fn() -> i64>,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let binding =
            source.map(|source| WidgetBinding::spawn(cx, CryptocurrencyEtfService::new(source)));
        let countdown = Rc::new(Countdown::new(cx, now));
        let metrics = cx.signal(None::<Metrics>);
        let requests = cx.signal(None::<HeaderRequest>);
        let account_id = cx.account();

        Self {
            asset: CryptocurrencyEtfAsset::default(),
            window: CryptocurrencyEtfWindow::default(),
            fund: CryptocurrencyEtfFundSelection::default(),
            binding,
            status,
            recovery,
            chart: None,
            chart_host: None,
            metrics_parent: None,
            metrics_cells: Vec::new(),
            metrics_show_net_assets: true,
            metrics,
            snapshot: None,
            accepted: None,
            ready: None,
            next_refresh_at: None,
            countdown,
            refresh_pending: false,
            load_pending: false,
            last_request_id: None,
            account_id,
            requests,
            header: None,
        }
    }

    #[cfg(test)]
    fn new_with_source<S: CryptocurrencyEtfSource>(
        source: S,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let now: Rc<dyn Fn() -> i64> = Rc::new(unix_millis);
        Self::assemble(Some(source), now, status, recovery, cx)
    }

    #[cfg(test)]
    fn new_with_source_at_clock<S: CryptocurrencyEtfSource>(
        source: S,
        now: Rc<dyn Fn() -> i64>,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        Self::assemble(Some(source), now, status, recovery, cx)
    }

    #[cfg(test)]
    fn new_without_source(
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let now: Rc<dyn Fn() -> i64> = Rc::new(unix_millis);
        Self::assemble(
            None::<crate::service::test_support::FakeSource>,
            now,
            status,
            recovery,
            cx,
        )
    }

    fn current_key(&self) -> Option<CryptocurrencyEtfStreamKey> {
        self.account_id
            .clone()
            .map(|account_id| CryptocurrencyEtfStreamKey { account_id })
    }

    fn has_scope(&self) -> bool {
        self.current_key().is_some()
    }

    fn show_net_assets(&self) -> bool {
        matches!(self.fund, CryptocurrencyEtfFundSelection::All)
    }

    fn countdown_deadline(&self, app: &App) -> Option<i64> {
        let status = self.status.state(app);
        if status.is_busy() || status.is_failed() {
            return None;
        }
        self.next_refresh_at
    }

    fn refresh_is_disabled(&self, app: &App) -> bool {
        self.refresh_pending || self.countdown_deadline(app).is_none()
    }

    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) {
        self.ready = None;
        self.next_refresh_at = None;
        self.refresh_pending = false;
        self.load_pending = false;
        let Some(key) = self.current_key() else {
            self.clear_answer(cx);
            self.status.report(
                cx.app_mut(),
                WidgetState::waiting(Notice::message("widget.cryptocurrency_etf.logged_out")),
            );
            let _ = cx.request_frame();
            return;
        };
        if !retry {
            self.clear_answer(cx);
        }
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        let request = if retry {
            CryptocurrencyEtfRequest::Retry(key)
        } else {
            CryptocurrencyEtfRequest::Load(key)
        };
        let Some(binding) = self.binding.as_mut() else {
            let recovery = self.recovery.clone();
            let error = CryptocurrencyEtfFetchError::Service(
                kytrix_service_provider::service::engine::KytrixServiceError::Disconnected,
            );
            self.status.report(
                cx.app_mut(),
                WidgetState::failed(Arc::new(CryptocurrencyEtfFailure::Fetch(error)), recovery),
            );
            return;
        };
        self.load_pending = true;
        binding.call(cx, request);
        let _ = cx.request_frame();
    }

    fn clear_answer(&mut self, cx: &mut WidgetCx<'_>) {
        self.snapshot = None;
        self.accepted = None;
        if let Some(mut chart) = self.chart.take() {
            chart.release(cx);
        }
        cx.write(&self.metrics, None);
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
                Inbound::Reliable { key, message } => {
                    self.apply_reliable(key, message, cx);
                }
                Inbound::Latest { key, message } => {
                    self.apply_latest(key, message, cx);
                }
                Inbound::Rpc { request_id, result } => {
                    self.apply_rpc(request_id, result, cx);
                }
            }
        }
    }

    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<CryptocurrencyEtfProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.status.is_failed(cx.app()) {
            return;
        }
        if self
            .last_request_id
            .is_some_and(|answered| request_id <= answered)
        {
            log::debug!(
                "the ETF pane ignored reply {request_id}: it is not newer than the last one it answered"
            );
            return;
        }
        self.last_request_id = Some(request_id);
        if self.load_pending {
            self.load_pending = false;
            self.apply_rpc_result(result, cx);
        } else if self.refresh_pending {
            self.apply_rpc_result(result, cx);
        }
    }

    fn apply_rpc_result(
        &mut self,
        result: WidgetRpcResult<CryptocurrencyEtfProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        match result {
            Ok(
                CryptocurrencyEtfResponse::StreamStarted | CryptocurrencyEtfResponse::RefreshQueued,
            ) => {}
            Err(error) => {
                self.refresh_pending = false;
                let recovery = self.recovery.clone();
                self.status.report(
                    cx.app_mut(),
                    WidgetState::failed(Arc::new(CryptocurrencyEtfFailure::Rpc(error)), recovery),
                );
            }
        }
    }

    fn apply_reliable(
        &mut self,
        key: CryptocurrencyEtfStreamKey,
        message: Arc<CryptocurrencyEtfStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
            return;
        }
        let CryptocurrencyEtfStreamMessage::Status(status) = message.as_ref() else {
            return;
        };
        self.next_refresh_at = status.next_refresh_at();
        match status {
            CryptocurrencyEtfStatus::Connecting => {
                self.status.report(cx.app_mut(), WidgetState::Connecting);
            }
            CryptocurrencyEtfStatus::Refreshing { .. } => {
                self.refresh_pending = false;
                self.status.report(cx.app_mut(), WidgetState::Loading);
            }
            CryptocurrencyEtfStatus::Ready { revision, .. } => {
                self.refresh_pending = false;
                self.ready = Some(*revision);
                if self.accepted.is_some_and(|held| held >= *revision) {
                    self.status.report(cx.app_mut(), WidgetState::Ready);
                } else {
                    self.status.report(cx.app_mut(), WidgetState::Loading);
                }
            }
            CryptocurrencyEtfStatus::Failed { error } => {
                self.refresh_pending = false;
                let recovery = self.recovery.clone();
                self.status.report(
                    cx.app_mut(),
                    WidgetState::failed(
                        Arc::new(CryptocurrencyEtfFailure::Fetch(error.clone())),
                        recovery,
                    ),
                );
            }
        }
    }

    fn apply_latest(
        &mut self,
        key: CryptocurrencyEtfStreamKey,
        message: Arc<CryptocurrencyEtfStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
            return;
        }
        let CryptocurrencyEtfStreamMessage::Snapshot { revision, snapshot } = message.as_ref()
        else {
            return;
        };
        if self.accepted.is_some_and(|held| *revision < held) {
            return;
        }
        if self
            .snapshot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, snapshot))
        {
            return;
        }
        self.snapshot = Some(Arc::clone(snapshot));
        self.accepted = Some(*revision);
        self.sync_chart_view(cx);
        if self.ready.is_some_and(|ready| *revision >= ready) {
            self.status.report(cx.app_mut(), WidgetState::Ready);
        }
        let _ = cx.request_frame();
    }

    fn sync_chart_view(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let dataset = snapshot.dataset(self.asset);
        let selection = self.fund.clone();
        let window = self.window;
        match self.chart.as_mut() {
            Some(chart) => {
                chart.set_view(cx, dataset, selection, window);
            }
            None => {
                self.chart = Some(CryptocurrencyEtfChart::new(cx, dataset, selection, window));
            }
        }
        self.sync_chart_metrics(cx);
    }

    fn sync_chart_locale(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(chart) = self.chart.as_mut() else {
            return;
        };
        if chart.sync_locale(cx) {
            self.sync_chart_metrics(cx);
            let _ = cx.request_frame();
        }
    }

    fn sync_chart(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(host) = self.chart_host else {
            return Ok(());
        };
        let Some(chart) = self.chart.as_mut() else {
            return Ok(());
        };
        if chart.node().is_none() {
            chart.build(cx, host)?;
            let _ = cx.request_frame();
        }
        if chart.sync(cx)? {
            self.sync_chart_metrics(cx);
        }
        Ok(())
    }

    fn sync_chart_metrics(&mut self, cx: &mut WidgetCx<'_>) {
        let metrics = self
            .chart
            .as_ref()
            .and_then(CryptocurrencyEtfChart::metrics);
        cx.write(&self.metrics, metrics);
    }

    fn normalize_fund(&mut self) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let CryptocurrencyEtfFundSelection::Named(name) = &self.fund else {
            return;
        };
        let dataset = snapshot.dataset(self.asset);
        if dataset.fund_index(name.as_str()).is_none() {
            self.fund = CryptocurrencyEtfFundSelection::All;
        }
    }

    fn apply_header_request(&mut self, request: HeaderRequest, cx: &mut WidgetCx<'_>) {
        match request {
            HeaderRequest::Refresh => {
                self.refresh(cx);
                return;
            }
            HeaderRequest::Asset(asset) => {
                if self.asset == asset {
                    return;
                }
                self.asset = asset;
                self.normalize_fund();
            }
            HeaderRequest::Window(window) => {
                if self.window == window {
                    return;
                }
                self.window = window;
            }
            HeaderRequest::Fund(fund) => {
                if self.fund == fund {
                    return;
                }
                self.fund = fund;
            }
        }
        self.sync_chart_view(cx);
        let _ = cx.request_frame();
    }

    fn refresh(&mut self, cx: &mut WidgetCx<'_>) {
        if self.refresh_is_disabled(cx.app()) {
            return;
        }
        self.refresh_pending = true;
        let Some(binding) = self.binding.as_mut() else {
            return;
        };
        binding.call(cx, CryptocurrencyEtfRequest::Refresh);
        let _ = cx.request_frame();
    }

    fn rebuild_metrics_cells(
        &mut self,
        parent: NodeId,
        show_net_assets: bool,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let metrics = self.metrics;
        let language = cx.language();
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();

        let date = self.build_metric_cell(
            parent,
            "TEST_ETF_DATE",
            {
                let translate = translate.clone();
                move |read| {
                    let _language = *read.read(&language);
                    t_smol!(translate, "widget.cryptocurrency_etf.date")
                }
            },
            {
                let theme = theme.clone();
                move |read| {
                    let _ = theme.appearance(read);
                    Role::FgDefault
                }
            },
            move |read| {
                let value: Option<Metrics> = (*read.read(&metrics)).clone();
                value.map_or_else(SmolStr::default, |metrics| metrics.date)
            },
            cx,
        )?;
        let flow = self.build_metric_cell(
            parent,
            "TEST_ETF_FLOW",
            move |read| {
                let value: Option<Metrics> = (*read.read(&metrics)).clone();
                value.map_or_else(SmolStr::default, |metrics| metrics.caption)
            },
            {
                let theme = theme.clone();
                move |read| {
                    let _ = theme.appearance(read);
                    let reading: Option<Metrics> = (*read.read(&metrics)).clone();
                    let value = reading.map(|metrics| metrics.flow_value);
                    match value {
                        Some(value) if value > 0.0 => Role::MarketUp,
                        Some(value) if value < 0.0 => Role::MarketDown,
                        _ => Role::MarketFlat,
                    }
                }
            },
            move |read| {
                let value: Option<Metrics> = (*read.read(&metrics)).clone();
                value.map_or_else(SmolStr::default, |metrics| metrics.flow)
            },
            cx,
        )?;
        let mut cells = vec![date, flow];
        if show_net_assets {
            let net_assets = self.build_metric_cell(
                parent,
                "TEST_ETF_NET_ASSETS",
                move |read| {
                    let _language = *read.read(&language);
                    t_smol!(translate, "widget.cryptocurrency_etf.total_net_assets")
                },
                move |read| {
                    let _ = theme.appearance(read);
                    Role::FgDefault
                },
                move |read| {
                    let value: Option<Metrics> = (*read.read(&metrics)).clone();
                    value
                        .and_then(|metrics| metrics.net_assets)
                        .unwrap_or_default()
                },
                cx,
            )?;
            cells.push(net_assets);
        }
        self.metrics_cells = cells;
        Ok(())
    }

    fn build_metric_cell(
        &self,
        parent: NodeId,
        id: &'static str,
        title: impl Fn(&mut ReadCtx<'_>) -> SmolStr + 'static,
        role: impl Fn(&mut ReadCtx<'_>) -> Role + 'static,
        value: impl Fn(&mut ReadCtx<'_>) -> SmolStr + 'static,
        cx: &mut WidgetCx<'_>,
    ) -> Result<NodeId, EngineError> {
        let theme = cx.theme().clone();
        let cell = NodeBuilder::boxed()
            .id(id)
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .min_width_0()
                    .flex_col()
                    .gap(4.0)
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        let title_theme = theme.clone();
        NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(UnifiedSize::Small.text_size()),
        )
        .bind_text(title)
        .bind_style(move |read| StylePatch {
            foreground: Some(title_theme.role(read, Role::FgMuted)),
            ..StylePatch::default()
        })
        .build(cx.tree_mut(), cell)?;
        NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(UnifiedSize::Medium.text_size()),
        )
        .bind_text(value)
        .bind_style(move |read| {
            let role = role(read);
            StylePatch {
                foreground: Some(theme.role(read, role)),
                ..StylePatch::default()
            }
        })
        .build(cx.tree_mut(), cell)?;
        Ok(cell)
    }

    fn sync_metrics_row(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if self.metrics_show_net_assets == self.show_net_assets() {
            return Ok(());
        }
        let show_net_assets = self.show_net_assets();
        let Some(row) = self.metrics_parent else {
            return Ok(());
        };
        if cx.tree().get(row).is_err() {
            self.metrics_cells.clear();
            self.metrics_parent = None;
            return Ok(());
        }
        for cell in std::mem::take(&mut self.metrics_cells) {
            remove_own_node(cx, cell);
        }
        self.rebuild_metrics_cells(row, show_net_assets, cx)?;
        self.metrics_show_net_assets = show_net_assets;
        cx.request_frame()?;
        Ok(())
    }

    fn build_header(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let language = *cx.read(&cx.language());
        let theme = cx.theme().clone();
        let translate = cx.translate().clone();
        let row = NodeBuilder::boxed()
            .id("TEST_ETF_HEADER")
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .gap(4.0)
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;

        let refresh_slot = NodeBuilder::boxed()
            .id("TEST_ETF_REFRESH")
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .gap(4.0)
                    .build(),
            )
            .build(cx.tree_mut(), row)?;
        let button = self.build_refresh_button(cx, refresh_slot)?;

        let countdown = self.countdown.label;
        let countdown_theme = theme.clone();
        NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(UnifiedSize::Small.text_size()),
        )
        .id("TEST_ETF_COUNTDOWN")
        .bind_text(move |read| {
            let label: Option<SmolStr> = read.read(&countdown).clone();
            label.unwrap_or_default()
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(countdown_theme.role(read, Role::FgDefault)),
            ..StylePatch::default()
        })
        .build(cx.tree_mut(), refresh_slot)?;

        let asset_id = SmolStr::new_static(self.asset.label());
        let asset = self.build_select(
            cx,
            theme.clone(),
            ASSET_KEY_SUFFIX,
            "widget.cryptocurrency_etf.asset",
            asset_items(),
            asset_id.clone(),
            asset_id,
            RequestKind::Asset,
            row,
        )?;
        let fund_items = self.fund_items(cx);
        let fund_ids = item_ids(&fund_items);
        let fund = self.build_select(
            cx,
            theme.clone(),
            FUND_KEY_SUFFIX,
            "widget.cryptocurrency_etf.fund",
            fund_items,
            fund_id(&self.fund),
            t_smol!(translate, "widget.cryptocurrency_etf.all"),
            RequestKind::Fund,
            row,
        )?;
        let window_select = self.build_select(
            cx,
            theme,
            WINDOW_KEY_SUFFIX,
            "widget.cryptocurrency_etf.time_window",
            window_items(&translate),
            SmolStr::new_static(window_id(self.window)),
            window_label(&translate, self.window),
            RequestKind::Window,
            row,
        )?;

        self.header = Some(HeaderControls {
            root: row,
            refresh_slot,
            language,
            button,
            asset,
            fund,
            window: window_select,
            fund_items: fund_ids,
        });
        Ok(())
    }

    fn build_refresh_button(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<Button, EngineError> {
        let requests = self.requests;
        let label = t_smol!(cx.translate(), "widget.cryptocurrency_etf.refresh_now");
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

    fn build_select(
        &self,
        cx: &mut WidgetCx<'_>,
        theme: ThemeHandle,
        key_suffix: &str,
        label_key: &str,
        items: Vec<SelectItem>,
        chosen: SmolStr,
        placeholder: SmolStr,
        kind: RequestKind,
        parent: NodeId,
    ) -> Result<Select, EngineError> {
        let requests = self.requests;
        let disabled = !self.has_scope();
        let options = SelectOptions::default()
            .size(UnifiedSize::Small)
            .label(t_smol!(cx.translate(), label_key))
            .placeholder(placeholder);
        let window = cx.window();
        let key = cx.unique_key(key_suffix);
        let layer = ui_kit::kit::layer::Layer::clone(cx.layer());
        let router = ui_kit::kit::overlay::AnimationRouter::clone(cx.animation());
        let select = Select::new(cx.app_mut(), theme, layer, router, key, options).on_change(
            move |app, chosen| {
                hand_over_select(requests, kind, window, app, &chosen);
            },
        );
        select
            .set_items(cx.app_mut(), items)
            .unwrap_or_else(|error| {
                log::warn!("the ETF header picker `{key_suffix}` could not set its items: {error}");
            });
        select.set_chosen(cx.app_mut(), [chosen.as_str()], false);
        let _ = select.set_disabled(cx.app_mut(), disabled);
        select.build(cx.app_mut(), window, parent)?;
        Ok(select)
    }

    fn fund_items(&self, cx: &WidgetCx<'_>) -> Vec<SelectItem> {
        let mut items = vec![SelectItem::new(
            SmolStr::new_static(ALL_FUNDS_ID),
            t_smol!(cx.translate(), "widget.cryptocurrency_etf.all"),
        )];
        let Some(snapshot) = self.snapshot.as_ref() else {
            return items;
        };
        for fund in snapshot.dataset(self.asset).funds.iter() {
            items.push(SelectItem::new(
                format_smolstr!("{FUND_ID_PREFIX}{fund}"),
                fund.clone(),
            ));
        }
        items
    }

    fn sync_header(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some((root, slot, language)) = self
            .header
            .as_ref()
            .map(|header| (header.root, header.refresh_slot, header.language))
        else {
            return Ok(());
        };
        if cx.tree().get(root).is_err() {
            self.release_header(cx, false);
            return Ok(());
        }
        if language != *cx.read(&cx.language()) {
            self.replace_refresh_button(cx, slot)?;
            let translate = cx.translate().clone();
            let asset = asset_items();
            let window = window_items(&translate);
            let fund = self.fund_items(cx);
            let fund_ids = item_ids(&fund);
            let Some(header) = self.header.as_mut() else {
                return Ok(());
            };
            for (select, items) in [
                (&header.asset, asset),
                (&header.fund, fund),
                (&header.window, window),
            ] {
                select
                    .set_items(cx.app_mut(), items)
                    .unwrap_or_else(|error| {
                        log::warn!("the ETF header picker could not replace its items: {error}");
                    });
            }
            header.fund_items = fund_ids;
            header.language = *cx.read(&cx.language());
        }
        let disabled = !self.has_scope();
        let refresh_disabled = self.refresh_is_disabled(cx.app());
        let fund_items = self.fund_items(cx);
        let fund_ids = item_ids(&fund_items);
        let asset = SmolStr::new_static(self.asset.label());
        let fund = fund_id(&self.fund);
        let window = SmolStr::new_static(window_id(self.window));
        let Some(header) = self.header.as_mut() else {
            return Ok(());
        };
        if fund_ids != header.fund_items {
            header
                .fund
                .set_items(cx.app_mut(), fund_items)
                .unwrap_or_else(|error| {
                    log::warn!("the ETF fund picker could not replace its items: {error}");
                });
            header.fund_items = fund_ids;
        }
        header.button.sync(cx.app_mut())?;
        let _ = header.button.set_disabled(cx.app_mut(), refresh_disabled);
        sync_chosen(&header.asset, cx, &asset, disabled);
        sync_chosen(&header.fund, cx, &fund, disabled);
        sync_chosen(&header.window, cx, &window, disabled);
        header.asset.sync(cx.app_mut())?;
        header.fund.sync(cx.app_mut())?;
        header.window.sync(cx.app_mut())?;
        Ok(())
    }

    fn replace_refresh_button(
        &mut self,
        cx: &mut WidgetCx<'_>,
        slot: NodeId,
    ) -> Result<(), EngineError> {
        let Some(header) = self.header.as_mut() else {
            return Ok(());
        };
        if let Some(node) = header.button.node() {
            remove_own_node(cx, node);
        }
        let _ = header.button.release(cx.app_mut());
        let button = self.build_refresh_button(cx, slot)?;
        let Some(header) = self.header.as_mut() else {
            return Ok(());
        };
        header.button = button;
        Ok(())
    }

    fn release_header(&mut self, cx: &mut WidgetCx<'_>, remove_node: bool) {
        let Some(header) = self.header.take() else {
            return;
        };
        if remove_node {
            remove_own_node(cx, header.root);
        }
        let _ = header.button.release(cx.app_mut());
        let _ = header.asset.release(cx.app_mut());
        let _ = header.fund.release(cx.app_mut());
        let _ = header.window.release(cx.app_mut());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestKind {
    Asset,
    Fund,
    Window,
}

fn hand_over_select(
    requests: Signal<Option<HeaderRequest>>,
    kind: RequestKind,
    window: WindowId,
    app: &mut App,
    chosen: &[SmolStr],
) {
    let Some(id) = chosen.first() else {
        return;
    };
    let request = match kind {
        RequestKind::Asset => CryptocurrencyEtfAsset::ALL
            .iter()
            .find(|asset| asset.label() == id.as_str())
            .copied()
            .map(HeaderRequest::Asset),
        RequestKind::Fund => Some(HeaderRequest::Fund(fund_from_id(id))),
        RequestKind::Window => window_from_id(id).map(HeaderRequest::Window),
    };
    let Some(request) = request else {
        return;
    };
    app.write(&requests, Some(request));
    let _ = app.request_repaint(window);
}

fn sync_chosen(select: &Select, cx: &mut WidgetCx<'_>, id: &SmolStr, disabled: bool) {
    let stated = select.chosen(cx.app());
    if stated.len() != 1 || stated.first() != Some(id) {
        select.set_chosen(cx.app_mut(), [id.as_str()], false);
    }
    let _ = select.set_disabled(cx.app_mut(), disabled);
}

fn item_ids(items: &[SelectItem]) -> Vec<SmolStr> {
    items.iter().map(|item| item.id.clone()).collect()
}

fn asset_items() -> Vec<SelectItem> {
    CryptocurrencyEtfAsset::ALL
        .iter()
        .map(|asset| {
            let ticker = SmolStr::new_static(asset.label());
            SelectItem::new(ticker.clone(), ticker)
        })
        .collect()
}

fn window_items(translate: &Translate) -> Vec<SelectItem> {
    CryptocurrencyEtfWindow::ALL
        .iter()
        .map(|window| {
            SelectItem::new(
                SmolStr::new_static(window_id(*window)),
                window_label(translate, *window),
            )
        })
        .collect()
}

fn remove_own_node(cx: &mut WidgetCx<'_>, node: NodeId) {
    if cx.tree().get(node).is_err() {
        return;
    }
    let window = cx.window();
    let _ = cx.app_mut().remove_node(window, node);
}

fn countdown_label(deadline: Option<i64>, now: i64) -> Option<SmolStr> {
    deadline.map(|deadline| format_countdown(deadline.saturating_sub(now)))
}

fn window_id(window: CryptocurrencyEtfWindow) -> &'static str {
    match window {
        CryptocurrencyEtfWindow::Days30 => "days_30",
        CryptocurrencyEtfWindow::Days90 => "days_90",
        CryptocurrencyEtfWindow::Days180 => "days_180",
        CryptocurrencyEtfWindow::Days360 => "days_360",
        CryptocurrencyEtfWindow::All => "all",
    }
}

fn window_from_id(id: &str) -> Option<CryptocurrencyEtfWindow> {
    CryptocurrencyEtfWindow::ALL
        .into_iter()
        .find(|window| window_id(*window) == id)
}

fn window_label(translate: &Translate, window: CryptocurrencyEtfWindow) -> SmolStr {
    match window {
        CryptocurrencyEtfWindow::Days30 => {
            t_smol!(translate, "widget.cryptocurrency_etf.days", count = 30)
        }
        CryptocurrencyEtfWindow::Days90 => {
            t_smol!(translate, "widget.cryptocurrency_etf.days", count = 90)
        }
        CryptocurrencyEtfWindow::Days180 => {
            t_smol!(translate, "widget.cryptocurrency_etf.days", count = 180)
        }
        CryptocurrencyEtfWindow::Days360 => {
            t_smol!(translate, "widget.cryptocurrency_etf.days", count = 360)
        }
        CryptocurrencyEtfWindow::All => t_smol!(translate, "widget.cryptocurrency_etf.all"),
    }
}

fn fund_id(fund: &CryptocurrencyEtfFundSelection) -> SmolStr {
    match fund {
        CryptocurrencyEtfFundSelection::All => SmolStr::new_static(ALL_FUNDS_ID),
        CryptocurrencyEtfFundSelection::Named(name) => {
            format_smolstr!("{FUND_ID_PREFIX}{name}")
        }
    }
}

fn fund_from_id(id: &str) -> CryptocurrencyEtfFundSelection {
    match id.strip_prefix(FUND_ID_PREFIX) {
        Some(name) => CryptocurrencyEtfFundSelection::Named(SmolStr::new(name)),
        None => CryptocurrencyEtfFundSelection::All,
    }
}

#[cfg(test)]
mod tests;

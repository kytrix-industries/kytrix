use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use app_config::{
    settings::Config,
    widget::{WidgetConfig, WidgetConfigSchemaVersion},
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use time::{Date, UtcOffset};
use ui_core::{
    a11y::A11yPatch,
    app::{App, TimerId},
    error::EngineError,
    math::{LogicalPx, px},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::{
        style::{PaintSpec, StylePatch},
        style_builder::StyleBuilder,
    },
    text::{TextOverflow, TextStyle, TextWrap},
    window::WindowId,
};
use ui_kit::{
    kit::{
        button::Button,
        icon::IconKind,
        list::SelectionMode,
        select::{Select, SelectItem, SelectOptions},
        size::{UnifiedSizable, UnifiedSize},
        table::{Column, ColumnAlign, Table, TableOptions},
    },
    theme::{components::InteractiveVariant, handle::ThemeHandle, role::Role},
};
use widget::{
    binding::{Inbound, Pending, WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    error::TranslatedError,
    identity::{WidgetId, WidgetInstanceId},
    protocol::RequestId,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    time_zone::data_time_zone,
    util::{format_countdown, unix_millis},
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

use crate::{
    model::{
        CalendarFilter, CalendarParseError, CalendarRow, CalendarSnapshot, Importance, format_time,
    },
    service::{
        CalendarFetchError, CalendarProtocol, CalendarRequest, CalendarResponse, CalendarService,
        CalendarServiceError, CalendarSource, CalendarStreamKey, CalendarStreamMessage,
    },
    table::{
        COLUMN_WEIGHTS, COUNTRY, CalendarTable, DAY_SELECTOR, IMPORTANCE, NAME, TIME,
        day_label_text,
    },
};

pub(crate) const COUNTDOWN_TICK: Duration = Duration::from_secs(1);

const MARK_SIZE: f32 = 6.0;

const ALL_CHOICE: &str = "all";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinancialCalendarConfig;

impl WidgetConfig for FinancialCalendarConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

impl TranslatedError for CalendarParseError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::InvalidDate { year, month, day } => t_smol!(
                translate,
                "error.financial_calendar.invalid_date",
                year = year,
                month = month,
                day = day
            ),
            Self::InvalidTimeOfDay { seconds } => t_smol!(
                translate,
                "error.financial_calendar.invalid_time_of_day",
                seconds = seconds
            ),
        }
    }
}

impl TranslatedError for CalendarServiceError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::StreamClosed => t_smol!(translate, "error.financial_calendar.stream_unavailable"),
            Self::StreamBusy => {
                t_smol!(
                    translate,
                    "error.financial_calendar.refresh_already_waiting"
                )
            }
        }
    }
}

impl TranslatedError for CalendarFetchError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Service(error) => error.translated_message(translate),
            Self::Parse(error) => error.translated_message(translate),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum HeaderRequest {
    Importance(Option<Importance>),
    Date(Option<Date>),
    Country(Option<SmolStr>),
    Reset,
    Refresh,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct HeaderOptions {
    dates: Vec<Date>,
    countries: Vec<SmolStr>,
}

struct FilterControl {
    wrapper: NodeId,
    select: Select,
}

struct HeaderLeft {
    parent: NodeId,
    language: LanguageCode,
    filter: CalendarFilter,
    options: HeaderOptions,
    controls: Vec<FilterControl>,
    reset: Button,
    reset_wrapper: NodeId,
}

struct HeaderRight {
    parent: NodeId,
    language: LanguageCode,
    wrapper: NodeId,
    refresh: Button,
}

struct Countdown {
    window: WindowId,
    deadline: Cell<Option<i64>>,
    label: Signal<Option<SmolStr>>,
    now: Rc<dyn Fn() -> i64>,
    timer: RefCell<Option<TimerId>>,
}

impl Countdown {
    fn new(cx: &mut WidgetCx<'_>, now: Rc<dyn Fn() -> i64>) -> Self {
        Self {
            window: cx.window(),
            deadline: Cell::new(None),
            label: cx.signal(None),
            now,
            timer: RefCell::new(None),
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

    fn stop(self: &Rc<Self>, app: &mut App) {
        self.deadline.set(None);
        if let Some(id) = self.timer.borrow_mut().take() {
            app.cancel_timer(&id);
        }
        app.write(&self.label, None);
    }

    fn write_label(&self, app: &mut App) -> bool {
        let label = self
            .deadline
            .get()
            .map(|due| format_countdown(due.saturating_sub((self.now)())));
        app.write(&self.label, label)
    }

    fn arm(self: &Rc<Self>, app: &mut App) {
        if self.deadline.get().is_none() {
            return;
        }
        let this = Rc::clone(self);
        if let Ok(id) = app.after(self.window, COUNTDOWN_TICK, move |app| this.tick(app)) {
            *self.timer.borrow_mut() = Some(id);
        }
    }

    fn tick(self: &Rc<Self>, app: &mut App) {
        *self.timer.borrow_mut() = None;
        if self.write_label(app) {
            let _ = app.request_repaint(self.window);
        }
        self.arm(app);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    Load,
    Refresh,
    Stop,
}

pub struct FinancialCalendarWidget {
    binding: Option<WidgetBinding<CalendarProtocol>>,
    pending: Pending<PendingCommand>,
    status: WidgetStatus,
    recovery: WidgetAction,
    translate: Translate,
    theme: ThemeHandle,
    config: Signal<Config>,
    language: Signal<LanguageCode>,
    table_model: Signal<Arc<CalendarTable>>,
    spans: Rc<RefCell<Arc<CalendarTable>>>,
    countdown: Rc<Countdown>,
    requests: Signal<Option<HeaderRequest>>,
    model: Arc<CalendarTable>,
    table: Option<Table>,
    table_slot: Option<NodeId>,
    table_built: Option<(LanguageCode, bool)>,
    snapshot: Option<Arc<CalendarSnapshot>>,
    next_refresh_at: Option<i64>,
    refresh_pending: bool,
    account_id: Option<SmolStr>,
    seen_zone: UtcOffset,
    header_left: Option<HeaderLeft>,
    header_right: Option<HeaderRight>,
    rows_dirty: bool,
}

impl WidgetView for FinancialCalendarWidget {
    type Config = FinancialCalendarConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.financial_calendar");
    const DISPLAY_NAME: &'static str = "widget.names.financial_calendar";
    const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.financial_calendar");
    const ICON: Option<&'static str> = Some("widgets/financial_calendar.png");

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
        Ok(Self::assemble(
            source,
            Rc::new(unix_millis),
            status,
            recovery,
            cx,
        ))
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        let retry = self.status.is_failed(cx.app());
        self.account_id = cx.account();
        self.start(retry, cx);
    }

    fn has_content(&self) -> bool {
        self.snapshot.is_some()
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let translate = self.translate.clone();
        let language = self.language;
        let frame = NodeBuilder::boxed()
            .id("FinancialCalendarFrame")
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
                    label: Some(t_smol!(translate, "widget.names.financial_calendar")),
                    ..A11yPatch::default()
                }
            })
            .build(cx.tree_mut(), parent)?;

        let table = self.build_table(cx)?;
        let window = cx.window();
        table.mount(cx.app_mut(), window, frame)?;
        self.table = Some(table);
        self.table_slot = Some(frame);
        let language = *cx.read(&self.language);
        self.table_built = Some((language, self.model.filter().is_all()));
        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let account = cx.account();
        if account != self.account_id {
            self.account_id = account;
            self.start(false, cx);
        }

        let zone = cx.data_time_zone();
        if zone != self.seen_zone {
            self.seen_zone = zone;
            let mut model = (*self.model).clone();
            model.regroup(zone);
            self.model = Arc::new(model);
            self.publish_model(cx);
            let _ = cx.request_frame();
        }

        let request = cx.read(&self.requests).clone();
        if let Some(request) = request {
            cx.write(&self.requests, None);
            self.apply_request(request, cx);
        }

        if let Some(binding) = self.binding.as_ref() {
            for inbound in binding.take() {
                self.apply(inbound, cx);
            }
        }

        let language = *cx.read(&self.language);
        if self.table_built != Some((language, self.model.filter().is_all())) {
            self.rebuild_table(cx)?;
        }

        if self.rows_dirty {
            self.rows_dirty = false;
            if let Some(table) = self.table.as_ref() {
                table.set_rows(cx.app_mut(), self.model.len())?;
                table.sync(cx.app_mut())?;
            }
            let _ = cx.request_frame();
        } else if let Some(table) = self.table.as_ref() {
            table.sync(cx.app_mut())?;
        }

        let deadline = self.countdown_deadline(cx.app());
        if deadline != self.countdown.deadline() {
            Countdown::update(&self.countdown, cx.app_mut(), deadline);
        }

        self.sync_header(cx)?;

        if self.snapshot.is_some() && self.table.is_none() {
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
        Countdown::stop(&self.countdown, cx.app_mut());
        if let Some(table) = self.table.take() {
            table.release(cx.app_mut());
        }
        self.table_slot = None;
        if let Err(error) = self.release_header(cx, false) {
            log::error!("the financial calendar header could not be released: {error}");
        }
        self.binding = None;
    }
}

impl FinancialCalendarWidget {
    fn assemble<S: CalendarSource>(
        source: Option<S>,
        now: Rc<dyn Fn() -> i64>,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let binding = source.map(|source| WidgetBinding::spawn(cx, CalendarService::new(source)));
        let table_model: Signal<Arc<CalendarTable>> = cx.signal(Arc::new(CalendarTable::default()));
        let model = Arc::new(CalendarTable::default());
        let spans = Rc::new(RefCell::new(Arc::clone(&model)));
        let countdown = Rc::new(Countdown::new(cx, now));
        let requests: Signal<Option<HeaderRequest>> = cx.signal(None);
        let config = cx.services().config;
        let language = cx.language();
        let seen_zone = cx.data_time_zone();
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let account_id = cx.account();

        Self {
            binding,
            pending: Pending::new(),
            status,
            recovery,
            translate,
            theme,
            config,
            language,
            table_model,
            spans,
            countdown,
            requests,
            model,
            table: None,
            table_slot: None,
            table_built: None,
            snapshot: None,
            next_refresh_at: None,
            refresh_pending: false,
            account_id,
            seen_zone,
            header_left: None,
            header_right: None,
            rows_dirty: false,
        }
    }

    #[cfg(test)]
    fn with_source<S: CalendarSource>(
        source: S,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        Self::assemble(Some(source), Rc::new(unix_millis), status, recovery, cx)
    }

    #[cfg(test)]
    fn with_clock<S: CalendarSource>(
        source: S,
        now: Rc<dyn Fn() -> i64>,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        Self::assemble(Some(source), now, status, recovery, cx)
    }

    fn current_key(&self) -> Option<CalendarStreamKey> {
        self.account_id
            .clone()
            .map(|account_id| CalendarStreamKey { account_id })
    }

    fn countdown_deadline(&self, app: &App) -> Option<i64> {
        let status = self.status.state(app);
        if self.refresh_pending || status.is_busy() || status.is_failed() {
            return None;
        }
        self.next_refresh_at
    }

    fn refresh_is_disabled(&self, app: &App) -> bool {
        self.countdown_deadline(app).is_none()
    }

    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) {
        self.refresh_pending = false;
        self.pending.clear();
        let key = self.current_key();
        let has_service = self.binding.is_some();
        match key {
            None => {
                self.clear_answer(cx);
                if let Some(binding) = self.binding.as_mut() {
                    let request_id = binding.call(cx, CalendarRequest::Stop);
                    self.pending.track(request_id, PendingCommand::Stop);
                }
                self.status.report(
                    cx.app_mut(),
                    WidgetState::waiting(Notice::message("widget.financial_calendar.logged_out")),
                );
                let _ = cx.request_frame();
            }
            Some(_) if !has_service => {
                self.clear_answer(cx);
                self.status.report(
                    cx.app_mut(),
                    WidgetState::failed(
                        Arc::new(CalendarServiceError::StreamClosed),
                        self.recovery.clone(),
                    ),
                );
                let _ = cx.request_frame();
            }
            Some(key) => {
                if !retry {
                    self.clear_answer(cx);
                }
                self.status.report(cx.app_mut(), WidgetState::Connecting);
                if let Some(binding) = self.binding.as_mut() {
                    let request_id = binding.call(
                        cx,
                        if retry {
                            CalendarRequest::Retry(key)
                        } else {
                            CalendarRequest::Load(key)
                        },
                    );
                    self.pending.track(request_id, PendingCommand::Load);
                }
                let _ = cx.request_frame();
            }
        }
    }

    fn clear_answer(&mut self, cx: &mut WidgetCx<'_>) {
        self.snapshot = None;
        self.next_refresh_at = None;
        let mut model = (*self.model).clone();
        model.clear();
        self.model = Arc::new(model);
        self.publish_model(cx);
        self.rows_dirty = true;
        Countdown::stop(&self.countdown, cx.app_mut());
        let _ = cx.request_frame();
    }

    fn publish_model(&mut self, cx: &mut WidgetCx<'_>) {
        let model = Arc::clone(&self.model);
        *self.spans.borrow_mut() = Arc::clone(&model);
        if cx.write(&self.table_model, model) {
            self.rows_dirty = true;
        }
    }

    fn apply_snapshot(
        &mut self,
        calendar: Arc<CalendarSnapshot>,
        next_refresh_at: i64,
        cx: &mut WidgetCx<'_>,
    ) {
        let zone = cx.data_time_zone();
        let mut model = (*self.model).clone();
        model.rebuild(Arc::clone(&calendar), zone);
        self.snapshot = Some(calendar);
        self.next_refresh_at = Some(next_refresh_at);
        self.refresh_pending = false;
        self.model = Arc::new(model);
        self.publish_model(cx);
        self.status.report(cx.app_mut(), WidgetState::Ready);
        let _ = cx.request_frame();
    }

    fn refresh(&mut self, cx: &mut WidgetCx<'_>) {
        if self.refresh_is_disabled(cx.app()) {
            return;
        }
        self.refresh_pending = true;
        Countdown::update(&self.countdown, cx.app_mut(), None);
        if let Some(binding) = self.binding.as_mut() {
            let request_id = binding.call(cx, CalendarRequest::Refresh);
            self.pending.track(request_id, PendingCommand::Refresh);
        }
        let _ = cx.request_frame();
    }

    fn apply_request(&mut self, request: HeaderRequest, cx: &mut WidgetCx<'_>) {
        if matches!(request, HeaderRequest::Refresh) {
            self.refresh(cx);
            return;
        }
        let zone = cx.data_time_zone();
        let mut model = (*self.model).clone();
        match request {
            HeaderRequest::Importance(importance) => model.select_importance(importance, zone),
            HeaderRequest::Date(date) => model.select_date(date, zone),
            HeaderRequest::Country(country) => model.select_country(country, zone),
            HeaderRequest::Reset => model.reset_filter(zone),
            HeaderRequest::Refresh => return,
        }
        self.model = Arc::new(model);
        self.publish_model(cx);
        let _ = cx.request_frame();
    }

    fn apply(&mut self, inbound: Inbound<CalendarProtocol>, cx: &mut WidgetCx<'_>) {
        match inbound {
            Inbound::Reliable { key, message } => self.apply_reliable(key, &message, cx),
            Inbound::Latest { key, message } => self.apply_latest(key, &message, cx),
            Inbound::Rpc { request_id, result } => {
                self.apply_rpc_result(request_id, result, cx);
            }
        }
    }

    fn apply_reliable(
        &mut self,
        key: CalendarStreamKey,
        message: &CalendarStreamMessage,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
            return;
        }
        let CalendarStreamMessage::Failed { error } = message else {
            return;
        };
        self.refresh_pending = false;
        Countdown::stop(&self.countdown, cx.app_mut());
        self.status.report(
            cx.app_mut(),
            WidgetState::failed(Arc::new(error.clone()), self.recovery.clone()),
        );
        let _ = cx.request_frame();
    }

    fn apply_latest(
        &mut self,
        key: CalendarStreamKey,
        message: &CalendarStreamMessage,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
            return;
        }
        let CalendarStreamMessage::Snapshot {
            calendar,
            next_refresh_at,
        } = message
        else {
            return;
        };
        self.apply_snapshot(Arc::clone(calendar), *next_refresh_at, cx);
    }

    fn apply_rpc_result(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<CalendarProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        match self.pending.take(request_id) {
            Some(PendingCommand::Load | PendingCommand::Refresh) => {
                if self.status.is_failed(cx.app()) || self.current_key().is_none() {
                    return;
                }
                match result {
                    Ok(
                        CalendarResponse::StreamStarted
                        | CalendarResponse::RefreshQueued
                        | CalendarResponse::Stopped,
                    ) => {}
                    Err(error) => {
                        self.refresh_pending = false;
                        Countdown::stop(&self.countdown, cx.app_mut());
                        self.status.report(
                            cx.app_mut(),
                            WidgetState::failed(Arc::new(error), self.recovery.clone()),
                        );
                        let _ = cx.request_frame();
                    }
                }
            }
            Some(PendingCommand::Stop) => {}
            None => log::debug!(
                "the financial calendar pane ignored reply {request_id}: no command is waiting for it"
            ),
        }
    }

    fn build_table(&self, cx: &mut WidgetCx<'_>) -> Result<Table, EngineError> {
        let theme = self.theme.clone();
        let cell_theme = self.theme.clone();
        let font_size = theme.tokens().medium.font_size;
        let small_font_size = theme.tokens().small.font_size;
        let row_height = theme.tokens().medium.height;
        let translate = self.translate.clone();
        let cell_translate = self.translate.clone();
        let language = self.language;
        let config = self.config;
        let table_model = self.table_model;
        let spans = Rc::clone(&self.spans);
        let cell_model = Rc::clone(&self.spans);
        let columns: Vec<Column> = columns(&translate).collect();
        let cells = move |tree: &mut NodeTree, parent: NodeId, row: usize, column: usize| {
            let sink = CellSink {
                table: table_model,
                model: Rc::clone(&cell_model),
                config,
                language,
                translate: cell_translate.clone(),
                theme: cell_theme.clone(),
                font_size,
                small_font_size,
            };
            sink.build(tree, parent, row, column);
        };
        let key = cx.unique_key("financial_calendar.table");
        let table = Table::new(
            cx.app_mut(),
            theme,
            key,
            columns,
            table_options(row_height),
            cells,
            t_smol!(self.translate, self.model.empty_message_key()),
        );
        table.set_column_span(move |row, column| spans.borrow().column_span(row, column));
        table.set_initial_rows(cx.app_mut(), self.model.len());
        Ok(table)
    }

    fn rebuild_table(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let (Some(slot), Some(previous)) = (self.table_slot, self.table.take()) else {
            return Ok(());
        };
        let window = cx.window();
        if let Some(outer) = previous.outer_node()
            && cx.tree().get(outer).is_ok()
        {
            cx.app_mut().remove_node(window, outer)?;
        }
        previous.release(cx.app_mut());

        let table = self.build_table(cx)?;
        table.mount(cx.app_mut(), window, slot)?;
        self.table = Some(table);
        let language = *cx.read(&self.language);
        self.table_built = Some((language, self.model.filter().is_all()));
        Ok(())
    }

    fn build_header_left(
        &mut self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        let filter = self.model.filter().clone();
        let disabled = self.snapshot.is_none();
        let all = t_smol!(self.translate, "widget.financial_calendar.all");
        let mut controls = Vec::with_capacity(3);

        let importance_name = t_smol!(self.translate, "widget.financial_calendar.importance");
        let importance_choice = filter.importance.map_or_else(
            || all.clone(),
            |importance| importance_label(&self.translate, importance),
        );
        let mut importance_items = Vec::with_capacity(Importance::LEVELS.len() + 1);
        importance_items.push(SelectItem::new(ALL_CHOICE, all.clone()));
        importance_items.extend(Importance::LEVELS.into_iter().map(|importance| {
            SelectItem::new(
                importance_choice_id(importance),
                importance_label(&self.translate, importance),
            )
        }));
        let importance = self.build_filter_control(
            parent,
            "TEST_FINANCIAL_CALENDAR_IMPORTANCE",
            "financial_calendar.header.importance",
            importance_name.clone(),
            filter_trigger(&self.translate, &importance_name, &importance_choice),
            importance_items,
            Rc::new(|chosen| {
                HeaderRequest::Importance(chosen.as_deref().and_then(importance_of_choice_id))
            }),
            cx,
        )?;
        controls.push(importance);

        let time_name = t_smol!(self.translate, "common.time");
        let time_choice = filter
            .date
            .map_or_else(|| all.clone(), |date| day_label_text(date, &self.translate));
        let mut time_items = Vec::with_capacity(self.model.dates().len() + 1);
        time_items.push(SelectItem::new(ALL_CHOICE, all.clone()));
        time_items.extend(self.model.dates().iter().map(|date| {
            SelectItem::new(
                date_choice_id(*date),
                day_label_text(*date, &self.translate),
            )
        }));
        let dates = self.model.dates().to_vec();
        let time = self.build_filter_control(
            parent,
            "TEST_FINANCIAL_CALENDAR_TIME",
            "financial_calendar.header.time",
            time_name.clone(),
            filter_trigger(&self.translate, &time_name, &time_choice),
            time_items,
            Rc::new(move |chosen| {
                let date = chosen.and_then(|chosen| {
                    dates
                        .iter()
                        .copied()
                        .find(|date| date_choice_id(*date) == chosen)
                });
                HeaderRequest::Date(date)
            }),
            cx,
        )?;
        controls.push(time);

        let country_name = t_smol!(self.translate, "widget.financial_calendar.country");
        let country_choice = filter.country.clone().unwrap_or_else(|| all.clone());
        let mut country_items = Vec::with_capacity(self.model.countries().len() + 1);
        country_items.push(SelectItem::new(ALL_CHOICE, all));
        country_items.extend(
            self.model
                .countries()
                .iter()
                .map(|country| SelectItem::new(country.clone(), country.clone())),
        );
        let country = self.build_filter_control(
            parent,
            "TEST_FINANCIAL_CALENDAR_COUNTRY",
            "financial_calendar.header.country",
            country_name.clone(),
            filter_trigger(&self.translate, &country_name, &country_choice),
            country_items,
            Rc::new(|chosen| {
                HeaderRequest::Country(
                    chosen
                        .filter(|choice| choice.as_str() != ALL_CHOICE)
                        .map(SmolStr::new),
                )
            }),
            cx,
        )?;
        controls.push(country);

        let reset_label = t_smol!(self.translate, "widget.financial_calendar.reset");
        let requests = self.requests;
        let window = cx.window();
        let reset_key = cx.unique_key("financial_calendar.header.reset");
        let mut reset = Button::new(cx.app_mut(), self.theme.clone(), reset_key)
            .ghost()
            .small()
            .icon(IconKind::FunnelX)
            .a11y_label(reset_label.clone())
            .tooltip_text(reset_label)
            .on_click(move |ctx| {
                let _ = ctx.update(&requests, |request| {
                    *request = Some(HeaderRequest::Reset);
                });
                let window = ctx.window();
                let _ = ctx.app_mut().request_repaint(window);
            });
        reset = reset.disabled(disabled || filter.is_all());
        let reset_wrapper = NodeBuilder::boxed()
            .id("TEST_FINANCIAL_CALENDAR_RESET")
            .style(StyleBuilder::new().flex_none().build())
            .build(cx.tree_mut(), parent)?;
        reset.build(cx.app_mut(), window, reset_wrapper)?;

        self.header_left = Some(HeaderLeft {
            parent,
            language,
            filter,
            options: HeaderOptions {
                dates: self.model.dates().to_vec(),
                countries: self.model.countries().to_vec(),
            },
            controls,
            reset,
            reset_wrapper,
        });
        Ok(())
    }

    fn build_filter_control(
        &self,
        parent: NodeId,
        selector: &'static str,
        key: &'static str,
        name: SmolStr,
        trigger: SmolStr,
        items: Vec<SelectItem>,
        request: Rc<dyn Fn(Option<SmolStr>) -> HeaderRequest>,
        cx: &mut WidgetCx<'_>,
    ) -> Result<FilterControl, EngineError> {
        let wrapper = NodeBuilder::boxed()
            .id(selector)
            .style(StyleBuilder::new().flex_none().build())
            .build(cx.tree_mut(), parent)?;
        let requests = self.requests;
        let window = cx.window();
        let disabled = self.snapshot.is_none();
        let options = SelectOptions::default()
            .size(UnifiedSize::Small)
            .variant(InteractiveVariant::Ghost)
            .label(name)
            .placeholder(trigger);
        let select_key = cx.unique_key(key);
        let select = Select::new(cx.app_mut(), self.theme.clone(), select_key, options).on_change(
            move |app, chosen| {
                let chosen = chosen.last().cloned();
                let request = request(chosen);
                app.write(&requests, Some(request));
                let _ = app.request_repaint(window);
            },
        );
        select.mount(cx.app_mut(), window, wrapper)?;
        select.set_items(cx.app_mut(), items)?;
        select.set_disabled(cx.app_mut(), disabled);
        Ok(FilterControl { wrapper, select })
    }

    fn build_header_right(
        &mut self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        let small_font_size = self.theme.tokens().small.font_size;
        let wrapper = NodeBuilder::boxed()
            .id("TEST_FINANCIAL_CALENDAR_REFRESH")
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;

        let label = t_smol!(self.translate, "widget.financial_calendar.refresh");
        let requests = self.requests;
        let window = cx.window();
        let disabled = self.refresh_is_disabled(cx.app());
        let refresh_key = cx.unique_key("financial_calendar.header.refresh");
        let mut refresh = Button::new(cx.app_mut(), self.theme.clone(), refresh_key)
            .ghost()
            .small()
            .icon(IconKind::ArrowsClockwise)
            .a11y_label(label.clone())
            .tooltip_text(label)
            .on_click(move |ctx| {
                let _ = ctx.update(&requests, |request| {
                    *request = Some(HeaderRequest::Refresh);
                });
                let window = ctx.window();
                let _ = ctx.app_mut().request_repaint(window);
            });
        refresh = refresh.disabled(disabled);
        refresh.build(cx.app_mut(), window, wrapper)?;

        let countdown_signal = self.countdown.label;
        let countdown_theme = self.theme.clone();
        NodeBuilder::text(SmolStr::default(), TextStyle::new(small_font_size))
            .id("TEST_FINANCIAL_CALENDAR_COUNTDOWN")
            .bind_text(move |read| read.read(&countdown_signal).clone().unwrap_or_default())
            .bind_style(move |read| {
                let label = read.read(&countdown_signal);
                StylePatch {
                    foreground: Some(countdown_theme.role(read, Role::FgMuted)),
                    visible: Some(label.is_some()),
                    ..StylePatch::default()
                }
            })
            .build(cx.tree_mut(), wrapper)?;

        self.header_right = Some(HeaderRight {
            parent,
            language,
            wrapper,
            refresh,
        });
        Ok(())
    }

    fn sync_header(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        let filter = self.model.filter().clone();
        let options = HeaderOptions {
            dates: self.model.dates().to_vec(),
            countries: self.model.countries().to_vec(),
        };
        let stale_left = self.header_left.as_ref().is_some_and(|left| {
            left.language != language || left.filter != filter || left.options != options
        });
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
            for control in &left.controls {
                control.select.sync(cx.app_mut())?;
            }
            left.reset.sync(cx.app_mut())?;
            let disabled = self.snapshot.is_none();
            for control in &left.controls {
                control.select.set_disabled(cx.app_mut(), disabled);
            }
            left.reset
                .set_disabled(cx.app_mut(), disabled || self.model.filter().is_all());
        }

        let stale_right = self
            .header_right
            .as_ref()
            .is_some_and(|right| right.language != language);
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
            right.refresh.sync(cx.app_mut())?;
            let disabled = self.refresh_is_disabled(cx.app());
            right.refresh.set_disabled(cx.app_mut(), disabled);
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
            remove_own_nodes(cx, left.controls.iter().map(|control| control.wrapper))?;
            remove_own_nodes(cx, [left.reset_wrapper])?;
        }
        for control in left.controls {
            control.select.release(cx.app_mut());
        }
        left.reset.release(cx.app_mut());
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
            remove_own_nodes(cx, [right.wrapper])?;
        }
        right.refresh.release(cx.app_mut());
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

fn columns(translate: &Translate) -> impl Iterator<Item = Column> + '_ {
    [
        t_smol!(translate, "widget.financial_calendar.importance"),
        t_smol!(translate, "common.time"),
        t_smol!(translate, "widget.financial_calendar.country"),
        t_smol!(translate, "widget.financial_calendar.name"),
    ]
    .into_iter()
    .zip(COLUMN_WEIGHTS)
    .map(|(header, weight)| Column::flex(header, weight).align(ColumnAlign::Start))
}

fn table_options(row_height: LogicalPx) -> TableOptions {
    TableOptions::fixed(row_height).selection(SelectionMode::None)
}

struct CellSink {
    table: Signal<Arc<CalendarTable>>,
    model: Rc<RefCell<Arc<CalendarTable>>>,
    config: Signal<Config>,
    language: Signal<LanguageCode>,
    translate: Translate,
    theme: ThemeHandle,
    font_size: LogicalPx,
    small_font_size: LogicalPx,
}

impl CellSink {
    fn build(&self, tree: &mut NodeTree, parent: NodeId, row: usize, column: usize) {
        if let Err(error) = self.build_cell(tree, parent, row, column) {
            log::error!("a financial calendar cell could not be built: {error}");
        }
    }

    fn build_cell(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
        column: usize,
    ) -> Result<(), EngineError> {
        let is_day = matches!(self.model.borrow().row(row), Some(CalendarRow::Day(_)));
        if is_day {
            return self.build_day_label(tree, parent, row);
        }
        match column {
            IMPORTANCE => self.build_importance(tree, parent, row),
            TIME => self.build_time(tree, parent, row),
            COUNTRY => self.build_country(tree, parent, row),
            NAME => self.build_name(tree, parent, row),
            _ => Ok(()),
        }
    }

    fn build_day_label(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
    ) -> Result<(), EngineError> {
        let table = self.table;
        let language = self.language;
        let translate = self.translate.clone();
        let theme = self.theme.clone();
        let mut style = TextStyle::new(self.small_font_size);
        style.weight = 600;
        let cell = NodeBuilder::boxed()
            .id(DAY_SELECTOR)
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .min_width_0()
                    .build(),
            )
            .build(tree, parent)?;
        NodeBuilder::text(SmolStr::default(), style)
            .bind_text(move |read| {
                let _language = *read.read(&language);
                let table = read.read(&table);
                match table.row(row) {
                    Some(CalendarRow::Day(date)) => day_label_text(date, &translate),
                    _ => SmolStr::default(),
                }
            })
            .bind_style(move |read| StylePatch {
                foreground: Some(theme.role(read, Role::FgMuted)),
                ..StylePatch::default()
            })
            .build(tree, cell)?;
        Ok(())
    }

    fn build_importance(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
    ) -> Result<(), EngineError> {
        let table = self.table;
        let theme = self.theme.clone();
        let cell = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .build(),
            )
            .build(tree, parent)?;
        for index in 0..3 {
            let theme = theme.clone();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .size(px(MARK_SIZE), px(MARK_SIZE))
                        .flex_none()
                        .build(),
                )
                .paint(PaintSpec {
                    corner_radius: [px(MARK_SIZE / 2.0); 4],
                    ..PaintSpec::default()
                })
                .bind_style(move |read| {
                    let table = read.read(&table);
                    let importance = table.event(row).map(|event| event.importance);
                    let marks = importance.map_or(0, Importance::marks);
                    StylePatch {
                        background: Some(
                            theme.role(read, importance.map_or(Role::FgSubtle, importance_role)),
                        ),
                        visible: Some(index < marks),
                        ..StylePatch::default()
                    }
                })
                .build(tree, cell)?;
        }
        Ok(())
    }

    fn build_time(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
    ) -> Result<(), EngineError> {
        let table = self.table;
        let config = self.config;
        let theme = self.theme.clone();
        NodeBuilder::text(SmolStr::default(), TextStyle::new(self.font_size))
            .bind_text(move |read| {
                let zone = {
                    let config = read.read(&config);
                    data_time_zone(&config)
                };
                let table = read.read(&table);
                table
                    .event(row)
                    .map(|event| format_time(event, zone))
                    .unwrap_or_default()
            })
            .bind_style(move |read| StylePatch {
                foreground: Some(theme.role(read, Role::FgMuted)),
                ..StylePatch::default()
            })
            .build(tree, parent)?;
        Ok(())
    }

    fn build_country(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
    ) -> Result<(), EngineError> {
        let table = self.table;
        let theme = self.theme.clone();
        NodeBuilder::text(SmolStr::default(), TextStyle::new(self.font_size))
            .bind_text(move |read| {
                let table = read.read(&table);
                table.event(row).map_or_else(SmolStr::default, |event| {
                    if event.country.is_empty() {
                        SmolStr::new_static("—")
                    } else {
                        event.country.clone()
                    }
                })
            })
            .bind_style(move |read| StylePatch {
                foreground: Some(theme.role(read, Role::FgMuted)),
                ..StylePatch::default()
            })
            .build(tree, parent)?;
        Ok(())
    }

    fn build_name(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        row: usize,
    ) -> Result<(), EngineError> {
        let table = self.table;
        let cell = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .min_width_0()
                    .build(),
            )
            .build(tree, parent)?;

        let name_table = table;
        let name_theme = self.theme.clone();
        let mut name_style = TextStyle::new(self.font_size);
        name_style.wrap = TextWrap::NoWrap;
        name_style.overflow = TextOverflow::Ellipsis;
        NodeBuilder::text(SmolStr::default(), name_style)
            .style(StyleBuilder::new().flex_1().min_width_0().build())
            .bind_text(move |read| {
                let table = read.read(&name_table);
                table
                    .event(row)
                    .map(|event| event.name.clone())
                    .unwrap_or_default()
            })
            .bind_style(move |read| StylePatch {
                foreground: Some(name_theme.role(read, Role::FgDefault)),
                ..StylePatch::default()
            })
            .build(tree, cell)?;

        let reference_theme = self.theme.clone();
        NodeBuilder::text(SmolStr::default(), TextStyle::new(self.small_font_size))
            .style(StyleBuilder::new().flex_none().min_width_0().build())
            .bind_text(move |read| {
                let table = read.read(&table);
                table
                    .event(row)
                    .and_then(|event| event.reference.clone())
                    .unwrap_or_default()
            })
            .bind_style(move |read| StylePatch {
                foreground: Some(reference_theme.role(read, Role::FgSubtle)),
                ..StylePatch::default()
            })
            .build(tree, cell)?;
        Ok(())
    }
}

fn importance_label(translate: &Translate, importance: Importance) -> SmolStr {
    let key = match importance {
        Importance::Low => "widget.financial_calendar.importance_level.low",
        Importance::Medium => "widget.financial_calendar.importance_level.medium",
        Importance::High => "widget.financial_calendar.importance_level.high",
        Importance::Unspecified => "widget.financial_calendar.importance_level.unspecified",
    };

    t_smol!(translate, key)
}

fn importance_choice_id(importance: Importance) -> SmolStr {
    match importance {
        Importance::Low => SmolStr::new_static("low"),
        Importance::Medium => SmolStr::new_static("medium"),
        Importance::High => SmolStr::new_static("high"),
        Importance::Unspecified => SmolStr::new_static("unspecified"),
    }
}

fn importance_of_choice_id(choice: &str) -> Option<Importance> {
    Importance::LEVELS
        .into_iter()
        .find(|importance| importance_choice_id(*importance) == choice)
}

fn date_choice_id(date: Date) -> SmolStr {
    format_smolstr!("{date}")
}

fn filter_trigger(translate: &Translate, filter: &SmolStr, value: &SmolStr) -> SmolStr {
    t_smol!(
        translate,
        "widget.financial_calendar.filter_choice",
        filter = filter,
        value = value
    )
}

fn importance_role(importance: Importance) -> Role {
    match importance {
        Importance::Unspecified => Role::FgSubtle,
        Importance::Low => Role::StatusSuccessBase,
        Importance::Medium => Role::StatusWarningBase,
        Importance::High => Role::StatusDangerBase,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell as StdCell;

    use app_config::{
        settings::{AccountConfig, AccountWalletKind, Config},
        time_zone::DataTimeZone,
    };
    use kytrix_proto::types::v1 as proto_types;
    use kytrix_service_provider::service::engine::KytrixServiceError;
    use ui_core::{
        event::{Action, ActionSource},
        node::{builder::NodeBuilder, tree::NodeKind},
        paint::color::Color,
    };
    use ui_kit::theme::appearance::Appearance;
    use widget::{rpc::RpcCallError, test_support::WidgetTestApp};

    use super::*;
    use crate::{
        table::COLUMN_COUNT,
        test_support::{self, ACCOUNT, FakeSource, FetchPlan},
    };

    fn product_translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    fn harness() -> WidgetTestApp {
        WidgetTestApp::with_translate(product_translate(LanguageCode::En))
    }

    fn current_config(harness: &WidgetTestApp) -> Config {
        let signal = harness.services().config;
        (*harness.app().read(&signal)).clone()
    }

    fn write_config(harness: &mut WidgetTestApp, config: Config) -> bool {
        let signal = harness.services().config;
        harness.app_mut().write(&signal, config)
    }

    fn sign_in(harness: &mut WidgetTestApp) {
        let mut config = current_config(harness);
        config.account = Some(AccountConfig {
            user_id: ACCOUNT.to_owned(),
            wallet_address: "0x0000000000000000000000000000000000000001".to_owned(),
            wallet_kind: AccountWalletKind::Ethereum,
        });
        assert!(
            write_config(harness, config),
            "signing in is a settings change"
        );
    }

    fn sign_out(harness: &mut WidgetTestApp) {
        let mut config = current_config(harness);
        config.account = None;
        assert!(
            write_config(harness, config),
            "signing out is a settings change"
        );
    }

    fn set_zone(harness: &mut WidgetTestApp, hours: i8) {
        let mut config = current_config(harness);
        config.data_time_zone =
            DataTimeZone::from_hours(hours).expect("the test's offset is one the setting offers");
        assert!(write_config(harness, config), "a zone move is a change");
    }

    fn key(account: &str) -> CalendarStreamKey {
        CalendarStreamKey {
            account_id: SmolStr::new(account),
        }
    }

    fn snapshot_with(events: Vec<proto_types::FinancialCalendarEvent>) -> Arc<CalendarSnapshot> {
        Arc::new(
            CalendarSnapshot::parse(proto_types::FinancialCalendar {
                events,
                timestamp: test_support::COLLECTED_AT,
            })
            .expect("a fixture is a readable calendar"),
        )
    }

    fn named_snapshot(events: usize, name: &str) -> Arc<CalendarSnapshot> {
        snapshot_with(
            (0..events)
                .map(|index| {
                    test_support::proto_event(
                        test_support::at(25, 9) + index as i64 * 60 * 60 * 1_000,
                        "US",
                        name,
                        Importance::Medium,
                        None,
                    )
                })
                .collect(),
        )
    }

    struct Pane {
        widget: FinancialCalendarWidget,
        status: WidgetStatus,
        parent: NodeId,
    }

    fn pane_with(harness: &mut WidgetTestApp, source: FakeSource) -> Pane {
        let scope = harness.scope();
        let status = WidgetStatus::new(harness.app_mut(), scope);
        let parent = harness.placeholder_parent();
        let widget = harness.with_cx(|cx| {
            FinancialCalendarWidget::with_source(
                source,
                status,
                WidgetAction::new("common.retry", |_| {}),
                cx,
            )
        });
        Pane {
            widget,
            status,
            parent,
        }
    }

    fn pane(harness: &mut WidgetTestApp) -> Pane {
        pane_with(harness, FakeSource::new([FetchPlan::Pending]))
    }

    fn pane_with_clock(
        harness: &mut WidgetTestApp,
        source: FakeSource,
        clock: Rc<StdCell<i64>>,
    ) -> Pane {
        let scope = harness.scope();
        let status = WidgetStatus::new(harness.app_mut(), scope);
        let parent = harness.placeholder_parent();
        let now: Rc<dyn Fn() -> i64> = Rc::new(move || clock.get());
        let widget = harness.with_cx(|cx| {
            FinancialCalendarWidget::with_clock(
                source,
                now,
                status,
                WidgetAction::new("common.retry", |_| {}),
                cx,
            )
        });
        Pane {
            widget,
            status,
            parent,
        }
    }

    fn accept_answer(
        harness: &mut WidgetTestApp,
        pane: &mut Pane,
        clock: &StdCell<i64>,
        in_ms: i64,
    ) {
        report_snapshot(
            harness,
            pane,
            &test_support::snapshot(2),
            clock.get() + in_ms,
        );
        sync(harness, pane);
    }

    fn load(harness: &mut WidgetTestApp, pane: &mut Pane) {
        harness.with_cx(|cx| pane.widget.load(cx));
    }

    fn sync(harness: &mut WidgetTestApp, pane: &mut Pane) {
        harness
            .with_cx(|cx| pane.widget.sync(cx))
            .expect("the widget syncs");
    }

    fn build_body(harness: &mut WidgetTestApp, pane: &mut Pane) {
        let parent = pane.parent;
        harness
            .with_cx(|cx| pane.widget.body(parent, cx))
            .expect("the body builds");
        settle_frames(harness, pane);
    }

    fn settle_frames(harness: &mut WidgetTestApp, pane: &mut Pane) {
        for _ in 0..4 {
            sync(harness, pane);
            harness.drain();
        }
    }

    fn report_snapshot(
        harness: &mut WidgetTestApp,
        pane: &mut Pane,
        calendar: &Arc<CalendarSnapshot>,
        next_refresh_at: i64,
    ) {
        if let Some(binding) = pane.widget.binding.as_mut() {
            binding.cancel();
        }
        harness.with_cx(|cx| {
            pane.widget.apply_latest(
                key(ACCOUNT),
                &CalendarStreamMessage::Snapshot {
                    calendar: Arc::clone(calendar),
                    next_refresh_at,
                },
                cx,
            );
        });
    }

    fn report_failure(harness: &mut WidgetTestApp, pane: &mut Pane) {
        if let Some(binding) = pane.widget.binding.as_mut() {
            binding.cancel();
        }
        harness.with_cx(|cx| {
            pane.widget.apply_reliable(
                key(ACCOUNT),
                &CalendarStreamMessage::Failed {
                    error: CalendarFetchError::Service(KytrixServiceError::Connect(
                        "the test source refused".into(),
                    )),
                },
                cx,
            );
        });
    }

    fn rows(pane: &Pane) -> usize {
        pane.widget.model.len()
    }

    fn failure_of(harness: &WidgetTestApp, pane: &Pane) -> SmolStr {
        pane.status
            .state(harness.app())
            .failure()
            .expect("the pane should have reported a failure")
            .text(&harness.services().translate)
    }

    fn countdown_label(harness: &WidgetTestApp, pane: &Pane) -> Option<SmolStr> {
        harness.app().read(&pane.widget.countdown.label).clone()
    }

    fn timer_armed(pane: &Pane) -> bool {
        pane.widget.countdown.timer.borrow().is_some()
    }

    const SETTLE_TURNS: usize = 64;

    fn settle(
        harness: &mut WidgetTestApp,
        pane: &mut Pane,
        what: &str,
        ready: impl Fn(&WidgetTestApp, &Pane) -> bool,
    ) {
        for _ in 0..SETTLE_TURNS {
            harness.drain_runtime();
            sync(harness, pane);
            harness.drain();
            if ready(harness, pane) {
                return;
            }
        }
        panic!("{what} should happen");
    }

    fn table(pane: &Pane) -> &Table {
        pane.widget
            .table
            .as_ref()
            .expect("the body built the table")
    }

    fn cell_text(harness: &WidgetTestApp, pane: &Pane, row: usize, column: usize) -> String {
        let node = table(pane)
            .cell_node(harness.app(), row, column)
            .unwrap_or_else(|| panic!("row {row} column {column} should be mounted"));
        harness.text_of(node)
    }

    fn day_label_text_of(harness: &WidgetTestApp, pane: &Pane) -> String {
        let frame = pane.widget.table_slot.expect("the body built the frame");
        let node = harness
            .find_by_debug_prefix(frame, DAY_SELECTOR)
            .expect("the first day of the answer is headed");
        harness.text_of(node)
    }

    fn first_foreground(harness: &WidgetTestApp, node: NodeId) -> Color {
        let tree = harness.tree();
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            let target = tree.get(current).expect("a node of the cell");
            if matches!(target.kind(), NodeKind::Text(_)) {
                return target
                    .style_patch()
                    .foreground
                    .expect("the text binds a foreground colour");
            }
            stack.extend(target.children().iter().copied());
        }
        panic!("the cell has a text node");
    }

    fn foregrounds(harness: &WidgetTestApp, node: NodeId) -> Vec<Color> {
        let tree = harness.tree();
        let mut found = Vec::new();
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            let target = tree.get(current).expect("a node of the cell");
            if matches!(target.kind(), NodeKind::Text(_))
                && let Some(foreground) = target.style_patch().foreground
            {
                found.push(foreground);
            }
            stack.extend(target.children().iter().copied());
        }
        found
    }

    fn backgrounds(harness: &WidgetTestApp, node: NodeId) -> Vec<Color> {
        let tree = harness.tree();
        let mut found = Vec::new();
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            let target = tree.get(current).expect("a node of the cell");
            if let Some(background) = target.style_patch().background {
                found.push(background);
            }
            stack.extend(target.children().iter().copied());
        }
        found
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
        harness
            .with_cx(|cx| {
                pane.widget.header(HeaderGroup::Left, left, cx)?;
                pane.widget.header(HeaderGroup::Right, right, cx)
            })
            .expect("the header builds");
        harness.drain();
        (left, right)
    }

    fn countdown_text(harness: &WidgetTestApp, right: NodeId) -> String {
        let node = harness
            .find_by_debug_prefix(right, "TEST_FINANCIAL_CALENDAR_COUNTDOWN")
            .expect("the header draws a countdown label");
        harness.text_of(node)
    }

    fn click(harness: &mut WidgetTestApp, node: NodeId) {
        let debug = harness
            .tree()
            .debug_id(node)
            .expect("the control carries its debug id")
            .to_owned();
        let key = debug
            .strip_suffix(":interactive")
            .expect("a kit control's node id ends with its interactive suffix");
        let action = Action::Custom(format_smolstr!("{key}.click"));
        let window = harness.window();
        assert!(
            harness
                .app_mut()
                .dispatch_action(window, node, action, ActionSource::Programmatic),
            "the control is registered"
        );
    }

    fn request(harness: &mut WidgetTestApp, pane: &mut Pane, request: HeaderRequest) {
        let requests = pane.widget.requests;
        assert!(harness.app_mut().write(&requests, Some(request)));
        sync(harness, pane);
    }

    #[test]
    fn every_message_this_pane_shows_should_resolve_to_text() {
        let translate = product_translate(LanguageCode::En);

        for key in [
            "error.financial_calendar.invalid_date",
            "error.financial_calendar.invalid_time_of_day",
            "error.financial_calendar.refresh_already_waiting",
            "error.financial_calendar.stream_unavailable",
            "widget.descriptions.financial_calendar",
            "widget.financial_calendar.all",
            "widget.financial_calendar.country",
            "widget.financial_calendar.day_label",
            "widget.financial_calendar.filter_choice",
            "widget.financial_calendar.importance",
            "widget.financial_calendar.logged_out",
            "widget.financial_calendar.name",
            "widget.financial_calendar.refresh",
            "widget.financial_calendar.reset",
            "widget.names.financial_calendar",
        ] {
            assert!(!t_smol!(translate, key).is_empty(), "{key} has no copy");
        }
    }

    #[test]
    fn a_pane_without_an_account_should_ask_for_one() {
        let mut harness = harness();
        let source = FakeSource::new([FetchPlan::Snapshot(2)]);
        let reads = source.clone();
        let mut pane = pane_with(&mut harness, source);
        load(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);

        assert_eq!(
            pane.status.state(harness.app()),
            WidgetState::waiting(Notice::message("widget.financial_calendar.logged_out")),
            "a pane nobody is signed in for says what it is waiting for"
        );
        assert_eq!(
            reads.fetch_count(),
            0,
            "and asks the service for nothing it cannot be served"
        );
        assert!(!pane.widget.has_content());
        assert_eq!(
            rows(&pane),
            0,
            "there is nothing to draw without an account"
        );
    }

    #[test]
    fn a_pane_with_an_account_should_draw_what_arrives() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);

        assert!(
            !pane.widget.has_content(),
            "a pane is not content until it has an answer"
        );
        assert_eq!(pane.status.state(harness.app()), WidgetState::Connecting);

        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(3),
            i64::MAX,
        );

        assert!(pane.widget.has_content());
        assert_eq!(pane.status.state(harness.app()), WidgetState::Ready);
        assert_eq!(
            rows(&pane),
            4,
            "three events of one day are one label and three rows"
        );
    }

    #[test]
    fn a_second_answer_should_rebuild_the_table_rather_than_append_to_it() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(3),
            i64::MAX,
        );
        assert_eq!(rows(&pane), 4);

        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(1),
            i64::MAX,
        );

        assert_eq!(
            rows(&pane),
            2,
            "the answer replaces every row the table drew before it"
        );
    }

    #[test]
    fn a_late_answer_should_be_ignored_for_either_account_it_does_not_belong_to() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        if let Some(binding) = pane.widget.binding.as_mut() {
            binding.cancel();
        }

        harness.with_cx(|cx| {
            pane.widget.apply_latest(
                key("another-account"),
                &CalendarStreamMessage::Snapshot {
                    calendar: test_support::snapshot(2),
                    next_refresh_at: i64::MAX,
                },
                cx,
            );
        });
        assert!(
            !pane.widget.has_content(),
            "another account's answer is not this pane's"
        );

        pane.widget.account_id = Some(SmolStr::new_static("another-account"));
        harness.with_cx(|cx| {
            pane.widget.apply_latest(
                key(ACCOUNT),
                &CalendarStreamMessage::Snapshot {
                    calendar: test_support::snapshot(2),
                    next_refresh_at: i64::MAX,
                },
                cx,
            );
        });
        assert!(
            !pane.widget.has_content(),
            "the answer of the account the pane has left is not one it takes either"
        );
    }

    #[test]
    fn a_failure_should_keep_the_calendar_on_screen_and_offer_a_recovery() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            i64::MAX,
        );
        report_failure(&mut harness, &mut pane);

        assert!(pane.status.state(harness.app()).is_failed());
        assert!(
            !failure_of(&harness, &pane).is_empty(),
            "a failure has to say what went wrong"
        );
        let state = pane.status.state(harness.app());
        let failure = state.failure().expect("a failure should be published");
        assert_eq!(
            failure.action().label_key(),
            "common.retry",
            "a failure always offers a way out"
        );
        assert!(
            pane.widget.has_content(),
            "the calendar the user was reading stays on screen"
        );
        assert_eq!(rows(&pane), 3, "and so do its rows");
    }

    #[test]
    fn a_refused_refresh_should_be_reported_as_a_failure() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            i64::MAX,
        );

        harness.with_cx(|cx| {
            pane.widget.refresh(cx);
            assert!(pane.widget.refresh_pending);
            pane.widget.apply_rpc_result(
                RequestId::new(2),
                Err(RpcCallError::Service {
                    request_id: RequestId::new(2),
                    source: CalendarServiceError::StreamBusy,
                }),
                cx,
            );
        });

        assert!(pane.status.state(harness.app()).is_failed());
        assert!(
            !pane.widget.refresh_pending,
            "a refused command is not one to keep waiting for"
        );
        assert_eq!(
            failure_of(&harness, &pane),
            t_smol!(
                harness.services().translate,
                "error.financial_calendar.refresh_already_waiting"
            ),
            "the refusal names the wait it collided with rather than the stream"
        );
    }

    #[test]
    fn a_failure_should_be_terminal_until_the_user_recovers() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_failure(&mut harness, &mut pane);
        assert!(pane.status.state(harness.app()).is_failed());

        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            i64::MAX,
        );

        assert!(
            pane.status.state(harness.app()).is_failed(),
            "a failed pane takes no more answers"
        );

        load(&mut harness, &mut pane);

        assert!(!pane.status.state(harness.app()).is_failed());
        assert_eq!(
            pane.status.state(harness.app()),
            WidgetState::Connecting,
            "the recovery the failure offered is what starts it again"
        );
    }

    #[test]
    fn a_failure_should_not_be_rewritten_by_a_late_command_reply() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            i64::MAX,
        );
        report_failure(&mut harness, &mut pane);
        let failure = failure_of(&harness, &pane);

        harness.with_cx(|cx| {
            pane.widget.apply(
                Inbound::Rpc {
                    request_id: RequestId::new(3),
                    result: Err(RpcCallError::Service {
                        request_id: RequestId::new(3),
                        source: CalendarServiceError::StreamBusy,
                    }),
                },
                cx,
            );
        });

        assert!(
            pane.status.state(harness.app()).is_failed(),
            "a failed pane is terminal until the user recovers it"
        );
        assert_eq!(
            failure_of(&harness, &pane),
            failure,
            "the failure sentence the user is reading is not rewritten by a late reply"
        );
        assert!(
            pane.widget.countdown.deadline().is_none(),
            "and the late reply does not put a schedule back on the header"
        );
        assert_eq!(
            pane.widget.pending.dropped(),
            1,
            "the late reply was dropped and counted"
        );
    }

    #[test]
    fn signing_out_should_not_be_failed_by_a_late_command_reply() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            unix_millis() + 60_000,
        );
        sync(&mut harness, &mut pane);
        harness.with_cx(|cx| pane.widget.refresh(cx));
        assert!(pane.widget.refresh_pending);

        sign_out(&mut harness);
        sync(&mut harness, &mut pane);
        let waiting = WidgetState::waiting(Notice::message("widget.financial_calendar.logged_out"));
        assert_eq!(pane.status.state(harness.app()), waiting);

        harness.with_cx(|cx| {
            pane.widget.apply(
                Inbound::Rpc {
                    request_id: RequestId::new(4),
                    result: Err(RpcCallError::Service {
                        request_id: RequestId::new(4),
                        source: CalendarServiceError::StreamBusy,
                    }),
                },
                cx,
            );
        });

        assert_eq!(
            pane.status.state(harness.app()),
            waiting,
            "a refusal of the read signing out replaced may not fail the waiting pane"
        );
        assert_eq!(
            pane.widget.pending.dropped(),
            1,
            "the refusal of the replaced read was dropped and counted"
        );
    }

    #[test]
    fn a_command_reply_older_than_the_one_already_applied_should_be_ignored() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            unix_millis() + 60_000,
        );

        harness.with_cx(|cx| {
            pane.widget.refresh(cx);
            pane.widget.apply_rpc_result(
                RequestId::new(2),
                Ok(CalendarResponse::RefreshQueued),
                cx,
            );
            pane.widget.apply_rpc_result(
                RequestId::new(1),
                Err(RpcCallError::Service {
                    request_id: RequestId::new(1),
                    source: CalendarServiceError::StreamBusy,
                }),
                cx,
            );
        });

        assert!(
            !pane.status.state(harness.app()).is_failed(),
            "the replaced command's refusal is not this pane's failure"
        );
        assert!(
            pane.widget.refresh_pending,
            "the refresh the pane replaced its call with is still the one in flight"
        );
        assert_eq!(
            pane.widget.pending.dropped(),
            1,
            "the answer to the replaced command was dropped and counted"
        );
    }

    #[test]
    fn a_failed_pane_should_stop_counting_down() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            unix_millis() + 60_000,
        );
        sync(&mut harness, &mut pane);
        assert!(
            countdown_label(&harness, &pane).is_some(),
            "a ready pane counts down to its next read"
        );

        report_failure(&mut harness, &mut pane);

        assert!(
            pane.widget.countdown.deadline().is_none(),
            "the failure ends the schedule"
        );
        assert!(
            countdown_label(&harness, &pane).is_none(),
            "the failure takes the countdown off at once rather than a tick later"
        );
    }

    #[test]
    fn an_acknowledged_refresh_should_keep_the_control_closed_until_the_answer() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            unix_millis() + 60_000,
        );
        sync(&mut harness, &mut pane);
        assert!(countdown_label(&harness, &pane).is_some());

        harness.with_cx(|cx| pane.widget.refresh(cx));
        assert!(
            countdown_label(&harness, &pane).is_none(),
            "the click takes the schedule it stepped ahead of off the control"
        );
        assert!(
            pane.widget.refresh_is_disabled(harness.app()),
            "the click closes the window itself, before anything has answered"
        );
        harness.with_cx(|cx| {
            pane.widget.apply_rpc_result(
                RequestId::new(2),
                Ok(CalendarResponse::RefreshQueued),
                cx,
            );
        });
        assert!(
            pane.widget.refresh_is_disabled(harness.app()),
            "a queued read is still a read in flight"
        );
    }

    #[test]
    fn a_refresh_a_failed_pane_has_nothing_to_do_for_should_not_leave_one_waiting() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_failure(&mut harness, &mut pane);
        assert!(pane.widget.refresh_is_disabled(harness.app()));

        harness.with_cx(|cx| pane.widget.refresh(cx));

        assert!(
            !pane.widget.refresh_pending,
            "a click with nothing behind it is not a refresh in flight"
        );
        assert!(
            pane.status.state(harness.app()).is_failed(),
            "and it is not a reason to leave the failure"
        );
    }

    #[test]
    fn a_refresh_should_read_again_and_reopen_the_control() {
        let mut harness = harness();
        sign_in(&mut harness);
        let source = FakeSource::new([FetchPlan::Snapshot(2), FetchPlan::Snapshot(3)]);
        let reads = source.clone();
        let mut pane = pane_with(&mut harness, source);
        load(&mut harness, &mut pane);
        settle(
            &mut harness,
            &mut pane,
            "the first answer",
            |harness, pane| pane.status.state(harness.app()) == WidgetState::Ready,
        );
        assert_eq!(
            rows(&pane),
            3,
            "two events of one day are one label and two rows"
        );
        assert!(!pane.widget.refresh_is_disabled(harness.app()));

        harness.with_cx(|cx| pane.widget.refresh(cx));
        assert!(pane.widget.refresh_pending);
        assert!(
            pane.widget.refresh_is_disabled(harness.app()),
            "the click closes the control until the answer arrives"
        );

        settle(
            &mut harness,
            &mut pane,
            "the refreshed answer",
            |_, pane| rows(pane) == 4,
        );

        assert!(!pane.widget.refresh_pending, "the answer ends the wait");
        assert!(!pane.widget.refresh_is_disabled(harness.app()));
        assert_eq!(
            reads.fetch_count(),
            2,
            "the first answer and the one the click asked for"
        );
    }

    #[test]
    fn a_failure_should_end_a_pending_refresh() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            i64::MAX,
        );
        harness.with_cx(|cx| pane.widget.refresh(cx));
        assert!(pane.widget.refresh_pending);

        report_failure(&mut harness, &mut pane);

        assert!(
            !pane.widget.refresh_pending,
            "the failure the stream reported ends the wait"
        );
    }

    #[test]
    fn an_unchanged_answer_should_end_a_pending_refresh() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        let calendar = test_support::snapshot(2);
        report_snapshot(&mut harness, &mut pane, &calendar, i64::MAX);
        harness.with_cx(|cx| pane.widget.refresh(cx));
        assert!(pane.widget.refresh_pending);

        report_snapshot(&mut harness, &mut pane, &calendar, i64::MAX);

        assert!(!pane.widget.refresh_pending, "the answer ends the wait");
        assert_eq!(pane.status.state(harness.app()), WidgetState::Ready);
    }

    #[test]
    fn a_zone_change_should_regroup_the_rows_without_reading_again() {
        let mut harness = harness();
        sign_in(&mut harness);
        let source = FakeSource::new([FetchPlan::Pending]);
        let reads = source.clone();
        let mut pane = pane_with(&mut harness, source);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::boundary_snapshot(),
            i64::MAX,
        );
        assert_eq!(rows(&pane), 4, "two days and their two events at UTC");
        let reads_before = reads.fetch_count();

        set_zone(&mut harness, 8);
        sync(&mut harness, &mut pane);

        assert_eq!(
            rows(&pane),
            3,
            "both events are the next day at UTC+8, so one label heads them"
        );
        assert_eq!(
            reads.fetch_count(),
            reads_before,
            "a zone is not a reason to read the calendar again"
        );
    }

    #[test]
    fn signing_out_should_take_the_calendar_with_it() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            i64::MAX,
        );
        assert!(pane.widget.has_content());

        sign_out(&mut harness);
        sync(&mut harness, &mut pane);

        assert!(!pane.widget.has_content());
        assert_eq!(rows(&pane), 0, "the rows went with the answer");
        assert!(
            pane.widget.next_refresh_at.is_none(),
            "and so did the schedule the signed-in pane was counting down to"
        );
        assert_eq!(
            pane.status.state(harness.app()),
            WidgetState::waiting(Notice::message("widget.financial_calendar.logged_out")),
            "and the pane asks for an account rather than reporting a failure"
        );
    }

    #[test]
    fn signing_out_should_stop_the_poll_loop() {
        let mut harness = harness();
        sign_in(&mut harness);
        let source = FakeSource::new([FetchPlan::Pending]);
        let reads = source.clone();
        let mut pane = pane_with(&mut harness, source);
        load(&mut harness, &mut pane);
        settle(&mut harness, &mut pane, "the first read", |_, _| {
            reads.fetch_count() == 1
        });

        sign_out(&mut harness);
        sync(&mut harness, &mut pane);

        settle(&mut harness, &mut pane, "the read to be dropped", |_, _| {
            reads.cancelled_fetches() == 1
        });
        assert_eq!(
            reads.fetch_count(),
            1,
            "a signed-out pane reads nothing more"
        );
        assert_eq!(
            pane.status.state(harness.app()),
            WidgetState::waiting(Notice::message("widget.financial_calendar.logged_out"))
        );
    }

    #[test]
    fn another_account_should_end_a_pending_refresh() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            i64::MAX,
        );
        harness.with_cx(|cx| pane.widget.refresh(cx));
        assert!(pane.widget.refresh_pending);

        pane.widget.account_id = Some(SmolStr::new_static("another-account"));
        harness.with_cx(|cx| pane.widget.start(false, cx));

        assert!(
            !pane.widget.refresh_pending,
            "the command belonged to the stream the account change replaced"
        );
        assert!(
            !pane.widget.has_content(),
            "and the calendar went with the account it was served to"
        );
    }

    #[test]
    fn an_account_change_should_read_the_new_account() {
        let mut harness = harness();
        sign_in(&mut harness);
        let source = FakeSource::new([FetchPlan::Pending, FetchPlan::Snapshot(2)]);
        let reads = source.clone();
        let mut pane = pane_with(&mut harness, source);
        load(&mut harness, &mut pane);
        settle(&mut harness, &mut pane, "the first read", |_, _| {
            reads.fetch_count() == 1
        });

        let mut config = current_config(&harness);
        config.account = Some(AccountConfig {
            user_id: "someone-else".to_owned(),
            wallet_address: "0x0000000000000000000000000000000000000002".to_owned(),
            wallet_kind: AccountWalletKind::Ethereum,
        });
        assert!(write_config(&mut harness, config));
        sync(&mut harness, &mut pane);

        settle(
            &mut harness,
            &mut pane,
            "the replacement answer",
            |harness, pane| pane.status.state(harness.app()) == WidgetState::Ready,
        );
        assert_eq!(
            pane.widget.account_id.as_deref(),
            Some("someone-else"),
            "the pane follows the account the settings hold"
        );
        assert_eq!(
            reads.fetch_count(),
            2,
            "the new account is what the second read was for"
        );
        assert_eq!(rows(&pane), 3, "and its answer is what is drawn");
    }

    #[test]
    fn a_pane_that_has_not_answered_should_draw_no_rows() {
        let mut harness = harness();
        sign_in(&mut harness);
        let pane = pane(&mut harness);

        assert_eq!(pane.widget.model.len(), 0);
        assert!(
            !pane.widget.has_content(),
            "and the framework draws the connecting placeholder instead of its body"
        );
    }

    #[test]
    fn a_pane_without_a_service_connection_should_report_a_failure() {
        let mut harness = harness();
        sign_in(&mut harness);
        let widget = harness.mount::<FinancialCalendarWidget>(FinancialCalendarConfig);
        harness.drain();

        let state = widget.status().state(harness.app());
        assert!(state.is_failed(), "there is no stream to read");
        assert_eq!(
            state
                .failure()
                .expect("a failure is reported")
                .text(&harness.services().translate),
            t_smol!(
                harness.services().translate,
                "error.financial_calendar.stream_unavailable"
            )
        );
    }

    #[test]
    fn the_table_should_keep_the_four_column_dataset_semantics() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            i64::MAX,
        );
        build_body(&mut harness, &mut pane);

        let table = table(&pane);
        assert_eq!(table.columns().len(), COLUMN_COUNT, "four columns");
        for (index, column) in table.columns().iter().enumerate() {
            assert_eq!(
                column.width,
                ui_kit::kit::table::ColumnWidth::Flex(COLUMN_WEIGHTS[index]),
                "column {index} keeps its declared width weight"
            );
            assert_eq!(column.align, ColumnAlign::Start);
            assert!(!column.sortable, "the dataset never sorted its columns");
        }
        assert_eq!(
            table.columns()[3].header.as_str(),
            "Name",
            "the headers are the localized dataset's"
        );
        assert_eq!(table.rows(), 3, "the accepted row count reaches the table");
    }

    #[test]
    fn the_table_should_not_offer_row_selection() {
        let options = table_options(px(30.0));

        assert_eq!(
            options.selection,
            SelectionMode::None,
            "the old dataset had no selection: rows are display-only"
        );
        assert_eq!(
            options.row_height,
            ui_kit::kit::list::RowHeight::Fixed(px(30.0))
        );
    }

    #[test]
    fn the_day_labels_should_span_every_column() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            i64::MAX,
        );
        build_body(&mut harness, &mut pane);

        let table = table(&pane);
        assert!(
            table.cell_node(harness.app(), 0, 0).is_some(),
            "the label is drawn"
        );
        for column in 1..COLUMN_COUNT {
            assert!(
                table.cell_node(harness.app(), 0, column).is_none(),
                "column {column} of the label row is covered by the span"
            );
        }
        for column in 0..COLUMN_COUNT {
            assert!(
                table.cell_node(harness.app(), 1, column).is_some(),
                "an event row is a cell of every column"
            );
        }
    }

    #[test]
    fn the_body_should_draw_the_day_head_and_event_cells() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::boundary_snapshot(),
            i64::MAX,
        );
        build_body(&mut harness, &mut pane);

        assert_eq!(
            day_label_text_of(&harness, &pane),
            "Sunday, October 25, 2026",
            "the label above a day is written the way the language writes a date"
        );
        assert_eq!(cell_text(&harness, &pane, 1, 1), "23:00");
        assert_eq!(cell_text(&harness, &pane, 1, 2), "US");
        assert_eq!(cell_text(&harness, &pane, 1, 3), "late\nSEP");
        assert_eq!(
            cell_text(&harness, &pane, 3, 2),
            "CN",
            "the second day's event is drawn under its own label"
        );
    }

    #[test]
    fn the_cells_should_draw_in_the_theme_colours() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::boundary_snapshot(),
            i64::MAX,
        );
        build_body(&mut harness, &mut pane);
        let light = harness.services().theme.theme_for(Appearance::Light);

        let time = table(&pane)
            .cell_node(harness.app(), 1, 1)
            .expect("the time cell is mounted");
        assert_eq!(first_foreground(&harness, time), light.role(Role::FgMuted));
        let name = table(&pane)
            .cell_node(harness.app(), 1, 3)
            .expect("the name cell is mounted");
        let colours = foregrounds(&harness, name);
        assert!(
            colours.contains(&light.role(Role::FgDefault)),
            "the name draws in the default foreground"
        );
        assert!(
            colours.contains(&light.role(Role::FgSubtle)),
            "the period beside it draws in the subtle one"
        );
        let importance = table(&pane)
            .cell_node(harness.app(), 1, 0)
            .expect("the importance cell is mounted");
        assert!(
            backgrounds(&harness, importance).contains(&light.role(Role::StatusDangerBase)),
            "a high-importance event draws its marks in the danger role"
        );
    }

    #[test]
    fn a_language_change_should_rebuild_the_tables_static_copy() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(1),
            i64::MAX,
        );
        build_body(&mut harness, &mut pane);
        assert_eq!(table(&pane).columns()[1].header.as_str(), "Time");

        harness.switch_language(LanguageCode::ZhCn);
        sync(&mut harness, &mut pane);
        harness.drain();

        let rebuilt = pane
            .widget
            .table
            .as_ref()
            .expect("the rebuilt table is mounted");
        assert_eq!(rebuilt.columns()[1].header.as_str(), "时间");
        assert_eq!(rebuilt.rows(), 2, "the rebuilt table keeps the rows");
        let empty = rebuilt
            .empty_node()
            .expect("the rebuilt table keeps its empty overlay");
        assert!(
            harness.text_of(empty).contains("暂无数据"),
            "the empty sentence is read in the language showing now"
        );
    }

    #[test]
    fn a_language_change_should_not_leak_the_old_table() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(1),
            i64::MAX,
        );
        build_body(&mut harness, &mut pane);
        let nodes = harness.node_count();
        let previous = table(&pane)
            .outer_node()
            .expect("the table mounted its nodes");

        harness.switch_language(LanguageCode::ZhCn);
        sync(&mut harness, &mut pane);
        harness.drain();

        assert_eq!(
            harness.node_count(),
            nodes,
            "the rebuilt table replaces the old one instead of adding to it"
        );
        assert!(
            harness.tree().get(previous).is_err(),
            "the old table's subtree was removed from the tree"
        );
    }

    #[test]
    fn an_answer_with_nothing_in_it_should_draw_the_no_data_sentence() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(0),
            i64::MAX,
        );
        build_body(&mut harness, &mut pane);

        assert!(pane.widget.has_content(), "an empty answer is an answer");
        let empty = table(&pane).empty_node().expect("an empty table says so");
        assert!(
            harness.text_of(empty).contains("No data"),
            "the table's own empty sentence stands in"
        );
    }

    #[test]
    fn a_filter_that_matches_nothing_should_draw_the_no_matches_sentence() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            i64::MAX,
        );
        build_body(&mut harness, &mut pane);

        request(
            &mut harness,
            &mut pane,
            HeaderRequest::Importance(Some(Importance::High)),
        );
        sync(&mut harness, &mut pane);
        harness.drain();

        assert_eq!(rows(&pane), 0, "the fixture events are medium");
        let empty = table(&pane)
            .empty_node()
            .expect("a table with no row says so");
        assert!(
            harness.text_of(empty).contains("No matches"),
            "a filter that matched nothing is named as such"
        );
    }

    #[test]
    fn the_table_should_redraw_the_rows_when_the_answer_changes() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &named_snapshot(2, "first"),
            i64::MAX,
        );
        build_body(&mut harness, &mut pane);
        assert_eq!(cell_text(&harness, &pane, 1, 3).trim_end(), "first");
        let before = table(&pane).cell_node(harness.app(), 1, 3);

        report_snapshot(
            &mut harness,
            &mut pane,
            &named_snapshot(2, "second"),
            i64::MAX,
        );
        settle_frames(&mut harness, &mut pane);

        assert_eq!(cell_text(&harness, &pane, 1, 3).trim_end(), "second");
        assert_ne!(
            table(&pane).cell_node(harness.app(), 1, 3),
            before,
            "the row window was rebuilt against the new model"
        );
    }

    #[test]
    fn moving_the_display_zone_should_redraw_the_time_column() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::boundary_snapshot(),
            i64::MAX,
        );
        build_body(&mut harness, &mut pane);
        assert_eq!(cell_text(&harness, &pane, 1, 1), "23:00");

        set_zone(&mut harness, 8);
        sync(&mut harness, &mut pane);
        settle_frames(&mut harness, &mut pane);

        assert_eq!(
            cell_text(&harness, &pane, 1, 1),
            "07:00",
            "the time column redraws in the zone the settings now name"
        );
        assert_eq!(
            cell_text(&harness, &pane, 1, 2),
            "US",
            "the country is a code rather than a clock"
        );
    }

    #[test]
    fn the_body_should_mount_the_table_under_its_frame() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(1),
            i64::MAX,
        );
        build_body(&mut harness, &mut pane);

        let frame = pane.widget.table_slot.expect("the body built the frame");
        let outer = table(&pane)
            .outer_node()
            .expect("the table mounted its nodes");
        assert_eq!(
            harness
                .tree()
                .get(outer)
                .expect("the table exists")
                .parent(),
            Some(frame),
            "the table is built into the frame"
        );
        assert_eq!(
            harness.tree().debug_id(frame),
            Some("FinancialCalendarFrame")
        );
    }

    #[test]
    fn releasing_the_widget_should_release_the_table() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(1),
            i64::MAX,
        );
        build_body(&mut harness, &mut pane);
        let sort = Action::Custom(format_smolstr!("{}.sort", table(&pane).key()));
        assert!(
            harness.app().has_action_handler(&sort),
            "the mounted table registers its handler"
        );

        harness.with_cx(|cx| pane.widget.release(cx));

        assert!(
            pane.widget.table.is_none(),
            "release drops the widget's table"
        );
        assert!(
            !harness.app().has_action_handler(&sort),
            "a table left un-released keeps its handler registered"
        );
    }

    #[test]
    fn a_failure_should_not_throw_away_the_answer() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            i64::MAX,
        );
        report_failure(&mut harness, &mut pane);

        assert!(pane.widget.has_content());
        assert_eq!(pane.widget.model.len(), 3);
    }

    #[test]
    fn the_header_should_carry_the_filters_the_refresh_and_its_countdown() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            unix_millis() + 125_000,
        );
        sync(&mut harness, &mut pane);
        let (left, right) = build_header(&mut harness, &mut pane);

        for selector in [
            "TEST_FINANCIAL_CALENDAR_IMPORTANCE",
            "TEST_FINANCIAL_CALENDAR_TIME",
            "TEST_FINANCIAL_CALENDAR_COUNTRY",
            "TEST_FINANCIAL_CALENDAR_RESET",
        ] {
            assert!(
                harness.find_by_debug_prefix(left, selector).is_some(),
                "the left group draws {selector}"
            );
        }
        assert!(
            harness
                .find_by_debug_prefix(right, "TEST_FINANCIAL_CALENDAR_REFRESH")
                .is_some(),
            "the right group draws the refresh control"
        );
        assert_eq!(
            countdown_text(&harness, right),
            "02:05",
            "the countdown is the time left until the next read"
        );
        assert!(!pane.widget.refresh_is_disabled(harness.app()));
    }

    #[test]
    fn the_header_should_draw_the_filters_in_order_and_the_reset_last() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::boundary_snapshot(),
            i64::MAX,
        );
        let (left, _) = build_header(&mut harness, &mut pane);

        let order = harness
            .tree()
            .get(left)
            .expect("the left group exists")
            .children()
            .iter()
            .filter_map(|child| harness.tree().debug_id(*child))
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            [
                "TEST_FINANCIAL_CALENDAR_IMPORTANCE",
                "TEST_FINANCIAL_CALENDAR_TIME",
                "TEST_FINANCIAL_CALENDAR_COUNTRY",
                "TEST_FINANCIAL_CALENDAR_RESET",
            ]
        );
    }

    #[test]
    fn a_filter_trigger_should_name_its_filter_and_its_choice() {
        let translate = product_translate(LanguageCode::En);
        let filter = t_smol!(translate, "widget.financial_calendar.importance");
        let all = t_smol!(translate, "widget.financial_calendar.all");

        assert_eq!(
            filter_trigger(&translate, &filter, &all).as_str(),
            "Importance: ALL"
        );

        let chinese = product_translate(LanguageCode::ZhCn);
        assert_eq!(
            filter_trigger(
                &chinese,
                &t_smol!(chinese, "widget.financial_calendar.importance"),
                &t_smol!(chinese, "widget.financial_calendar.all"),
            )
            .as_str(),
            "重要性：全部"
        );
    }

    #[test]
    fn a_filter_menu_should_offer_the_answers_options() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::boundary_snapshot(),
            i64::MAX,
        );
        let _ = build_header(&mut harness, &mut pane);

        let left = pane
            .widget
            .header_left
            .as_ref()
            .expect("the header's left group was built");
        let country = left.controls[2].select.items();
        assert_eq!(
            country
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            ["ALL", "CN", "US"]
        );
        let importance = left.controls[0].select.items();
        assert_eq!(
            importance.len(),
            5,
            "the importance scale is offered whole, under its \"all\" option"
        );
        assert_eq!(importance[1].label.as_str(), "Low");
    }

    #[test]
    fn a_choice_should_narrow_the_rows_and_the_reset_should_bring_them_back() {
        let mut harness = harness();
        sign_in(&mut harness);
        let source = FakeSource::new([FetchPlan::Pending]);
        let reads = source.clone();
        let mut pane = pane_with(&mut harness, source);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            i64::MAX,
        );
        assert_eq!(rows(&pane), 3, "one label and the two events");
        let reads_before = reads.fetch_count();

        request(
            &mut harness,
            &mut pane,
            HeaderRequest::Country(Some(SmolStr::new_static("US"))),
        );

        assert_eq!(rows(&pane), 2, "one label and the one American event");
        assert_eq!(
            reads.fetch_count(),
            reads_before,
            "a filter is about the rows already on screen, not a reason to read again"
        );

        request(
            &mut harness,
            &mut pane,
            HeaderRequest::Importance(Some(Importance::High)),
        );
        assert_eq!(
            rows(&pane),
            0,
            "the two choices narrow the calendar together: the fixture events are medium"
        );

        request(&mut harness, &mut pane, HeaderRequest::Reset);
        assert_eq!(rows(&pane), 3, "the reset brings every row back");
        assert!(pane.widget.model.filter().is_all());
    }

    #[test]
    fn clicking_the_reset_control_should_clear_every_choice() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            i64::MAX,
        );
        let _ = build_header(&mut harness, &mut pane);
        request(
            &mut harness,
            &mut pane,
            HeaderRequest::Country(Some(SmolStr::new_static("US"))),
        );
        assert_eq!(rows(&pane), 2);

        let reset = pane
            .widget
            .header_left
            .as_ref()
            .expect("the header's left group was built")
            .reset
            .node()
            .expect("the reset control is mounted");
        click(&mut harness, reset);
        sync(&mut harness, &mut pane);

        assert_eq!(rows(&pane), 3, "the click put the choice back to \"all\"");
        assert!(pane.widget.model.filter().is_all());
    }

    #[test]
    fn a_choice_the_new_answer_no_longer_offers_should_be_reset() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            i64::MAX,
        );
        request(
            &mut harness,
            &mut pane,
            HeaderRequest::Country(Some(SmolStr::new_static("DE"))),
        );
        assert_eq!(rows(&pane), 2, "one label and the one German event");

        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::boundary_snapshot(),
            i64::MAX,
        );

        assert_eq!(
            pane.widget.model.filter().country,
            None,
            "the new answer holds no German event, so the choice is not one to keep"
        );
        assert_eq!(rows(&pane), 4, "and every event of it is drawn again");
    }

    #[test]
    fn an_importance_choice_should_outlive_an_answer_with_no_event_of_that_level() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::boundary_snapshot(),
            i64::MAX,
        );
        request(
            &mut harness,
            &mut pane,
            HeaderRequest::Importance(Some(Importance::High)),
        );
        assert_eq!(rows(&pane), 2, "one label and the one event of that level");

        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            i64::MAX,
        );

        assert_eq!(
            pane.widget.model.filter().importance,
            Some(Importance::High),
            "the levels are fixed, so the answer cannot take one away"
        );
        assert_eq!(rows(&pane), 0, "and nothing of the new answer is of it");
    }

    #[test]
    fn a_day_the_zone_takes_away_should_reset_the_choice() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::boundary_snapshot(),
            i64::MAX,
        );
        request(
            &mut harness,
            &mut pane,
            HeaderRequest::Date(Some(
                Date::from_calendar_date(2026, time::Month::October, 25).expect("a real date"),
            )),
        );
        assert_eq!(rows(&pane), 2, "the late event of the 25th alone");

        set_zone(&mut harness, 8);
        sync(&mut harness, &mut pane);

        assert_eq!(
            pane.widget.model.filter().date,
            None,
            "eight hours east the answer holds no 25th to filter by"
        );
        assert_eq!(
            rows(&pane),
            3,
            "so both events are drawn under the one day they fall on"
        );
    }

    #[test]
    fn signing_out_should_reset_the_filters() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            i64::MAX,
        );
        request(
            &mut harness,
            &mut pane,
            HeaderRequest::Country(Some(SmolStr::new_static("US"))),
        );
        assert!(!pane.widget.model.filter().is_all());

        sign_out(&mut harness);
        sync(&mut harness, &mut pane);

        assert!(
            pane.widget.model.filter().is_all(),
            "the signed-out pane holds no option a filter could name"
        );
        assert_eq!(rows(&pane), 0);
    }

    #[test]
    fn a_language_change_should_neither_read_again_nor_drop_the_rows() {
        let mut harness = harness();
        sign_in(&mut harness);
        let source = FakeSource::new([FetchPlan::Pending]);
        let reads = source.clone();
        let mut pane = pane_with(&mut harness, source);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            i64::MAX,
        );
        let before = rows(&pane);
        let reads_before = reads.fetch_count();

        harness.switch_language(LanguageCode::ZhCn);
        sync(&mut harness, &mut pane);

        assert_eq!(rows(&pane), before, "the rows are the same answer");
        assert_eq!(
            reads.fetch_count(),
            reads_before,
            "a language is not a reason to read the calendar again"
        );
        assert!(pane.widget.has_content());
    }

    #[test]
    fn a_language_change_should_rebuild_the_header_copy() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            i64::MAX,
        );
        let (left, _) = build_header(&mut harness, &mut pane);
        let nodes = harness.node_count();

        harness.switch_language(LanguageCode::ZhCn);
        sync(&mut harness, &mut pane);

        let rebuilt = harness
            .find_by_debug_prefix(left, "TEST_FINANCIAL_CALENDAR_IMPORTANCE")
            .expect("the group was rebuilt in place");
        assert!(harness.tree().get(rebuilt).is_ok());
        assert_eq!(
            harness.node_count(),
            nodes,
            "the rebuilt group replaces the old one instead of adding to it"
        );
    }

    #[test]
    fn a_ready_pane_should_not_request_frames_in_the_steady_state() {
        let mut harness = harness();
        sign_in(&mut harness);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        report_snapshot(
            &mut harness,
            &mut pane,
            &test_support::snapshot(2),
            unix_millis() + 60_000,
        );
        build_body(&mut harness, &mut pane);
        let _ = build_header(&mut harness, &mut pane);
        settle_frames(&mut harness, &mut pane);
        let frames = harness.app().frame_requests();

        for _ in 0..2 {
            sync(&mut harness, &mut pane);
            harness.drain();
        }

        assert_eq!(
            harness.app().frame_requests(),
            frames,
            "nothing changed, so nothing asked for a frame"
        );
    }

    fn countdown_pane(harness: &mut WidgetTestApp, clock: &Rc<StdCell<i64>>) -> Pane {
        let mut pane = pane_with_clock(
            harness,
            FakeSource::new([FetchPlan::Pending]),
            Rc::clone(clock),
        );
        load(harness, &mut pane);
        build_body(harness, &mut pane);
        pane
    }

    #[test]
    fn a_countdown_tick_should_ask_for_a_frame_only_when_its_label_changed() {
        let mut harness = harness();
        sign_in(&mut harness);
        let clock = Rc::new(StdCell::new(0_i64));
        let mut pane = countdown_pane(&mut harness, &clock);
        accept_answer(&mut harness, &mut pane, &clock, 600_000);
        let (_, right) = build_header(&mut harness, &mut pane);
        harness.drain();

        assert_eq!(countdown_text(&harness, right), "10:00");
        assert!(timer_armed(&pane), "a ready pane keeps one timer armed");
        let frames = harness.frame_requests();

        harness.advance(COUNTDOWN_TICK);
        assert_eq!(countdown_text(&harness, right), "10:00");
        assert_eq!(
            harness.frame_requests(),
            frames,
            "a tick that leaves the label where it was asks for nothing"
        );

        clock.set(1_000);
        harness.advance(COUNTDOWN_TICK);
        assert_eq!(countdown_text(&harness, right), "09:59");
        assert!(
            harness.frame_requests() > frames,
            "the tick that changed the label asked for the frame that draws it"
        );
    }

    #[test]
    fn a_countdown_should_rearm_its_timer_for_each_tick() {
        let mut harness = harness();
        sign_in(&mut harness);
        let clock = Rc::new(StdCell::new(0_i64));
        let mut pane = countdown_pane(&mut harness, &clock);
        accept_answer(&mut harness, &mut pane, &clock, 600_000);
        let (_, right) = build_header(&mut harness, &mut pane);

        for second in 1..=3 {
            clock.set(second * 1_000);
            harness.advance(COUNTDOWN_TICK);
            assert_eq!(
                countdown_text(&harness, right),
                format!("09:{:02}", 60 - second),
                "tick {second} moved the label"
            );
            assert!(timer_armed(&pane), "the timer re-armed itself");
        }
    }

    #[test]
    fn releasing_the_widget_should_cancel_the_countdown_timer() {
        let mut harness = harness();
        sign_in(&mut harness);
        let clock = Rc::new(StdCell::new(0_i64));
        let mut pane = countdown_pane(&mut harness, &clock);
        accept_answer(&mut harness, &mut pane, &clock, 600_000);
        let _ = build_header(&mut harness, &mut pane);
        assert!(timer_armed(&pane));

        harness.with_cx(|cx| pane.widget.release(cx));

        assert!(!timer_armed(&pane), "release cancels the timer");
        assert!(
            countdown_label(&harness, &pane).is_none(),
            "and clears the label"
        );
        let frames = harness.frame_requests();
        harness.advance(COUNTDOWN_TICK * 4);
        assert_eq!(
            harness.frame_requests(),
            frames,
            "a released pane's timer never fires"
        );
    }

    #[test]
    fn a_failure_should_stop_the_countdown_timer() {
        let mut harness = harness();
        sign_in(&mut harness);
        let clock = Rc::new(StdCell::new(0_i64));
        let mut pane = countdown_pane(&mut harness, &clock);
        accept_answer(&mut harness, &mut pane, &clock, 60_000);
        assert!(timer_armed(&pane));

        report_failure(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);

        assert!(!timer_armed(&pane), "the failed pane's timer is cancelled");
        assert!(countdown_label(&harness, &pane).is_none());
    }

    #[test]
    fn an_accepted_command_should_leave_nothing_to_count_down_to() {
        let mut harness = harness();
        sign_in(&mut harness);
        let clock = Rc::new(StdCell::new(0_i64));
        let mut pane = countdown_pane(&mut harness, &clock);
        accept_answer(&mut harness, &mut pane, &clock, 60_000);
        assert!(countdown_label(&harness, &pane).is_some());

        harness.with_cx(|cx| pane.widget.refresh(cx));
        assert!(pane.widget.refresh_pending);
        sync(&mut harness, &mut pane);

        assert!(
            countdown_label(&harness, &pane).is_none(),
            "the read in flight has no schedule"
        );
        assert!(!timer_armed(&pane));
    }

    #[test]
    fn the_header_should_draw_the_countdown_beside_the_refresh_control() {
        let mut harness = harness();
        sign_in(&mut harness);
        let clock = Rc::new(StdCell::new(0_i64));
        let mut pane = pane_with_clock(
            &mut harness,
            FakeSource::new([FetchPlan::Pending]),
            Rc::clone(&clock),
        );
        load(&mut harness, &mut pane);
        let right = header_parent(&mut harness, "TEST_HEADER_RIGHT");
        harness
            .with_cx(|cx| pane.widget.header(HeaderGroup::Right, right, cx))
            .expect("the header builds");
        harness.drain();

        let countdown = harness
            .find_by_debug_prefix(right, "TEST_FINANCIAL_CALENDAR_COUNTDOWN")
            .expect("the header draws the countdown label");
        assert_eq!(
            harness.text_of(countdown),
            "",
            "nothing has been accepted yet"
        );
        assert!(
            !harness.visible(countdown),
            "and there is nothing to count down to"
        );

        accept_answer(&mut harness, &mut pane, &clock, 600_000);
        harness.drain();

        assert_eq!(harness.text_of(countdown), "10:00");
        assert!(harness.visible(countdown));
    }
}

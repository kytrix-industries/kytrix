use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_service_provider::service::engine::KytrixServiceError;
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use time::{OffsetDateTime, UtcOffset};
use ui_core::{
    app::{App, TimerId},
    error::EngineError,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::{
        style::{PaintSpec, StylePatch},
        style_builder::StyleBuilder,
    },
    text::TextStyle,
    window::WindowId,
};
use ui_kit::{
    kit::{
        button::Button,
        icon::IconKind,
        size::{UnifiedSizable, UnifiedSize},
    },
    theme::role::Role,
};
use widget::{
    binding::{Inbound, Pending, WidgetBinding},
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    error::TranslatedError,
    identity::{WidgetId, WidgetInstanceId},
    protocol::RequestId,
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    time_zone::data_time_zone,
    util::{format_countdown, unix_millis},
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

use crate::{
    model::{PizzaLevel, PizzaReading},
    service::{
        PizzaFetchError, PizzaProtocol, PizzaRequest, PizzaResponse, PizzaService,
        PizzaServiceError, PizzaSource, PizzaStatus, PizzaStreamKey, PizzaStreamMessage,
    },
};

const COUNTDOWN_TICK: Duration = Duration::from_millis(250);

const LEVEL_BAR_HEIGHT: f32 = 6.0;
const LEVEL_BAR_RADIUS: f32 = LEVEL_BAR_HEIGHT / 2.0;

const LEVEL_BAR_MAX_WIDTH: f32 = 220.0;

const LEVEL_NAME_SIZE: f32 = 24.0;

const LEVEL_NUMBER_WEIGHT: u16 = 500;
const LEVEL_NAME_WEIGHT: u16 = 700;

const BODY_GAP: f32 = 12.0;
const BODY_PADDING: f32 = 16.0;

const LEVEL_COLUMN_GAP: f32 = 4.0;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PentagonPizzaIndexConfig;

impl WidgetConfig for PentagonPizzaIndexConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PizzaFailure {
    Fetch(PizzaFetchError),
    Rpc(RpcCallError<PizzaServiceError>),
}

impl TranslatedError for PizzaFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Fetch(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
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
                log::error!(
                    "the Pentagon Pizza Index countdown could not arm its next tick: {error}"
                );
            }
        }
    }

    fn tick(self: &Rc<Self>, app: &mut App) {
        *self.timer.borrow_mut() = None;
        self.write_label(app);
        self.arm(app);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    Load,
    Refresh,
    Unload,
}

pub struct PentagonPizzaIndexWidget {
    binding: Option<WidgetBinding<PizzaProtocol>>,
    pending: Pending<PendingCommand>,
    status: WidgetStatus,
    recovery: WidgetAction,
    reading: Option<PizzaReading>,
    reading_signal: Signal<Option<PizzaReading>>,
    accepted: Option<u64>,
    ready: Option<u64>,
    next_refresh_at: Option<i64>,
    countdown: Rc<Countdown>,
    refresh_pending: bool,
    account_id: Option<SmolStr>,
    refresh_requests: Signal<u64>,
    seen_refresh_requests: u64,
    body_root: Option<NodeId>,
    refresh_button: Option<Button>,
    refresh_root: Option<NodeId>,
    refresh_parent: Option<NodeId>,
    refresh_language: Option<LanguageCode>,
}

impl WidgetView for PentagonPizzaIndexWidget {
    type Config = PentagonPizzaIndexConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.pentagon_pizza_index");
    const DISPLAY_NAME: &'static str = "widget.names.pentagon_pizza_index";
    const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.pentagon_pizza_index");
    const ICON: Option<&'static str> = Some("widgets/pentagon_pizza_index.png");

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
        let retry = self.status.is_failed(cx.app()) && self.account_id == cx.account();
        self.account_id = cx.account();
        if let Err(error) = self.start(retry, cx) {
            log::error!("the Pentagon Pizza Index pane could not start: {error}");
        }
    }

    fn has_content(&self) -> bool {
        self.reading.is_some()
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let root = NodeBuilder::boxed()
            .id("TEST_PENTAGON_PIZZA_INDEX")
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .size_full()
                    .min_width_0()
                    .min_height_0()
                    .items_center()
                    .justify_center()
                    .gap(BODY_GAP)
                    .padding(BODY_PADDING)
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        self.body_root = Some(root);

        let level = NodeBuilder::boxed()
            .id("TEST_PENTAGON_PIZZA_INDEX_LEVEL")
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .items_center()
                    .gap(LEVEL_COLUMN_GAP)
                    .build(),
            )
            .build(cx.tree_mut(), root)?;
        self.build_level_number(level, cx)?;
        self.build_level_name(level, cx)?;
        self.build_level_bar(root, cx)?;
        self.build_observed_at(root, cx)?;

        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.drain_inbox(cx);

        let account = cx.account();
        if account != self.account_id {
            self.account_id = account;
            self.start(false, cx)?;
        }

        let requests = *cx.read(&self.refresh_requests);
        if requests != self.seen_refresh_requests {
            self.seen_refresh_requests = requests;
            self.refresh(cx)?;
        }

        if self.reading.is_some() && self.body_root.is_none() {
            cx.request_frame()?;
        }

        let deadline = self.countdown_deadline(cx.app());
        if deadline != self.countdown.deadline() {
            Countdown::update(&self.countdown, cx.app_mut(), deadline);
        }
        self.sync_refresh_control(cx)?;

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
        self.release_refresh_control(cx, true)?;
        self.build_refresh_control(parent, cx)
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        self.binding = None;
        let _ = self.release_refresh_control(cx, false);
        Countdown::update(&self.countdown, cx.app_mut(), None);
        self.body_root = None;
        self.reading = None;
        self.ready = None;
        self.accepted = None;
        self.next_refresh_at = None;
    }
}

impl PentagonPizzaIndexWidget {
    fn assemble<S: PizzaSource>(
        source: Option<S>,
        now: Rc<dyn Fn() -> i64>,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let binding = source.map(|source| WidgetBinding::spawn(cx, PizzaService::new(source)));
        let reading_signal: Signal<Option<PizzaReading>> = cx.signal(None);
        let countdown = Rc::new(Countdown::new(cx, now));
        let refresh_requests = cx.signal(0_u64);
        let account_id = cx.account();

        Self {
            binding,
            pending: Pending::new(),
            status,
            recovery,
            reading: None,
            reading_signal,
            accepted: None,
            ready: None,
            next_refresh_at: None,
            countdown,
            refresh_pending: false,
            account_id,
            refresh_requests,
            seen_refresh_requests: 0,
            body_root: None,
            refresh_button: None,
            refresh_root: None,
            refresh_parent: None,
            refresh_language: None,
        }
    }

    #[cfg(test)]
    fn new_with_source<S: PizzaSource>(
        source: S,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let now: Rc<dyn Fn() -> i64> = Rc::new(unix_millis);
        Self::assemble(Some(source), now, status, recovery, cx)
    }

    #[cfg(test)]
    fn new_with_source_at_clock<S: PizzaSource>(
        source: S,
        now: Rc<dyn Fn() -> i64>,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        Self::assemble(Some(source), now, status, recovery, cx)
    }

    fn current_key(&self) -> Option<PizzaStreamKey> {
        self.account_id
            .clone()
            .map(|account_id| PizzaStreamKey { account_id })
    }

    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.ready = None;
        self.next_refresh_at = None;
        self.accepted = None;
        self.refresh_pending = false;
        self.pending.clear();
        let Some(key) = self.current_key() else {
            self.set_reading(None, cx);
            self.status.report(
                cx.app_mut(),
                WidgetState::waiting(Notice::message("widget.pentagon_pizza_index.logged_out")),
            );
            if let Some(binding) = self.binding.as_mut() {
                let request_id = binding.call(cx, PizzaRequest::Unload);
                self.pending.track(request_id, PendingCommand::Unload);
            }
            cx.request_frame()?;
            return Ok(());
        };
        if !retry {
            self.set_reading(None, cx);
        }
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        let Some(binding) = self.binding.as_mut() else {
            self.fail(PizzaFetchError::Service(KytrixServiceError::Closed), cx);
            return Ok(());
        };
        let request_id = binding.call(cx, PizzaRequest::Load(key));
        self.pending.track(request_id, PendingCommand::Load);
        cx.request_frame()?;

        Ok(())
    }

    fn set_reading(&mut self, reading: Option<PizzaReading>, cx: &mut WidgetCx<'_>) {
        self.reading = reading;
        cx.write(&self.reading_signal, reading);
    }

    fn fail(&mut self, error: PizzaFetchError, cx: &mut WidgetCx<'_>) {
        let recovery = self.recovery.clone();
        self.status.report(
            cx.app_mut(),
            WidgetState::failed(Arc::new(PizzaFailure::Fetch(error)), recovery),
        );
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
        result: Result<PizzaResponse, RpcCallError<PizzaServiceError>>,
        cx: &mut WidgetCx<'_>,
    ) {
        match self.pending.take(request_id) {
            Some(PendingCommand::Load) => self.apply_load_result(result, cx),
            Some(PendingCommand::Refresh) => self.apply_refresh_result(result, cx),
            Some(PendingCommand::Unload) => {}
            None => log::debug!(
                "the Pentagon Pizza Index pane ignored reply {request_id}: no command is waiting for it"
            ),
        }
    }

    fn apply_load_result(
        &mut self,
        result: Result<PizzaResponse, RpcCallError<PizzaServiceError>>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.status.is_failed(cx.app()) {
            return;
        }
        if let Err(error) = result {
            let recovery = self.recovery.clone();
            self.status.report(
                cx.app_mut(),
                WidgetState::failed(Arc::new(PizzaFailure::Rpc(error)), recovery),
            );
        }
    }

    fn apply_refresh_result(
        &mut self,
        result: Result<PizzaResponse, RpcCallError<PizzaServiceError>>,
        cx: &mut WidgetCx<'_>,
    ) {
        if !self.refresh_pending || self.status.is_failed(cx.app()) {
            return;
        }
        if let Err(error) = result {
            self.refresh_pending = false;
            let recovery = self.recovery.clone();
            self.status.report(
                cx.app_mut(),
                WidgetState::failed(Arc::new(PizzaFailure::Rpc(error)), recovery),
            );
        }
    }

    fn apply_reliable(
        &mut self,
        key: PizzaStreamKey,
        message: Arc<PizzaStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
            return false;
        }
        let PizzaStreamMessage::Status(status) = message.as_ref() else {
            return false;
        };
        self.next_refresh_at = status.next_refresh_at();

        match status {
            PizzaStatus::Connecting => {
                self.status.report(cx.app_mut(), WidgetState::Connecting);
            }
            PizzaStatus::Refreshing { .. } => {
                self.refresh_pending = false;
                self.status.report(cx.app_mut(), WidgetState::Loading);
            }
            PizzaStatus::Ready { revision, .. } => {
                self.refresh_pending = false;
                self.ready = Some(*revision);
                if self.accepted.is_some_and(|held| held >= *revision) {
                    self.status.report(cx.app_mut(), WidgetState::Ready);
                } else {
                    self.status.report(cx.app_mut(), WidgetState::Loading);
                }
            }
            PizzaStatus::Failed(error) => {
                self.refresh_pending = false;
                let recovery = self.recovery.clone();
                self.status.report(
                    cx.app_mut(),
                    WidgetState::failed(Arc::new(PizzaFailure::Fetch(error.clone())), recovery),
                );
            }
        }

        false
    }

    fn apply_latest(
        &mut self,
        key: PizzaStreamKey,
        message: Arc<PizzaStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
            return false;
        }
        let PizzaStreamMessage::Snapshot(snapshot) = message.as_ref() else {
            return false;
        };
        if self.accepted.is_some_and(|held| snapshot.revision() < held) {
            return false;
        }
        let changed = self.reading != Some(snapshot.reading());
        self.set_reading(Some(snapshot.reading()), cx);
        self.accepted = Some(snapshot.revision());
        if self.ready.is_some_and(|ready| snapshot.revision() >= ready) {
            self.status.report(cx.app_mut(), WidgetState::Ready);
        }

        changed
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

    fn refresh(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if self.refresh_is_disabled(cx.app()) {
            return Ok(());
        }
        self.refresh_pending = true;
        if let Some(binding) = self.binding.as_mut() {
            let request_id = binding.call(cx, PizzaRequest::Refresh);
            self.pending.track(request_id, PendingCommand::Refresh);
        }
        cx.request_frame()?;

        Ok(())
    }

    fn build_level_number(&self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let reading = self.reading_signal;
        let language = cx.language();
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let mut style = TextStyle::new(UnifiedSize::Medium.text_size());
        style.weight = LEVEL_NUMBER_WEIGHT;
        NodeBuilder::text(SmolStr::default(), style)
            .id("TEST_PENTAGON_PIZZA_INDEX_NUMBER")
            .bind_text(move |read| {
                let _language = read.read(&language);
                let reading = *read.read(&reading);
                reading.map_or_else(SmolStr::default, |reading| {
                    t_smol!(
                        translate,
                        "widget.pentagon_pizza_index.level_number",
                        level = reading.level().number()
                    )
                })
            })
            .bind_style(move |read| {
                let level = *read.read(&reading);
                StylePatch {
                    foreground: Some(theme.role(read, reading_role(level))),
                    ..StylePatch::default()
                }
            })
            .build(cx.tree_mut(), parent)?;

        Ok(())
    }

    fn build_level_name(&self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let reading = self.reading_signal;
        let language = cx.language();
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let mut style = TextStyle::new(LEVEL_NAME_SIZE);
        style.weight = LEVEL_NAME_WEIGHT;
        NodeBuilder::text(SmolStr::default(), style)
            .id("TEST_PENTAGON_PIZZA_INDEX_NAME")
            .bind_text(move |read| {
                let _language = read.read(&language);
                let reading = *read.read(&reading);
                reading.map_or_else(SmolStr::default, |reading| reading.level().name(&translate))
            })
            .bind_style(move |read| {
                let level = *read.read(&reading);
                StylePatch {
                    foreground: Some(theme.role(read, reading_role(level))),
                    ..StylePatch::default()
                }
            })
            .build(cx.tree_mut(), parent)?;

        Ok(())
    }

    fn build_level_bar(&self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let reading = self.reading_signal;
        let theme = cx.theme().clone();
        NodeBuilder::boxed()
            .id("TEST_PENTAGON_PIZZA_INDEX_BAR")
            .style(
                StyleBuilder::new()
                    .width_full()
                    .max_width(LEVEL_BAR_MAX_WIDTH)
                    .height(LEVEL_BAR_HEIGHT)
                    .build(),
            )
            .paint(PaintSpec {
                corner_radius: [LEVEL_BAR_RADIUS; 4],
                ..PaintSpec::default()
            })
            .bind_style(move |read| {
                let level = *read.read(&reading);
                StylePatch {
                    background: Some(theme.role(read, reading_role(level))),
                    ..StylePatch::default()
                }
            })
            .build(cx.tree_mut(), parent)?;

        Ok(())
    }

    fn build_observed_at(&self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let reading = self.reading_signal;
        let config = cx.services().config;
        let language = cx.language();
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(UnifiedSize::Small.text_size()),
        )
        .id("TEST_PENTAGON_PIZZA_INDEX_OBSERVED")
        .bind_text(move |read| {
            let _language = read.read(&language);
            let reading = *read.read(&reading);
            let settings = read.read(&config);
            let zone = data_time_zone(&settings);
            reading.map_or_else(SmolStr::default, |reading| {
                t_smol!(
                    translate,
                    "widget.pentagon_pizza_index.observed_at",
                    time = format_observed_at(reading.observed_at(), zone)
                )
            })
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(theme.role(read, Role::FgMuted)),
            ..StylePatch::default()
        })
        .build(cx.tree_mut(), parent)?;

        Ok(())
    }

    fn build_refresh_control(
        &mut self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let language = *cx.read(&cx.language());
        let label = t_smol!(cx.translate(), "widget.pentagon_pizza_index.refresh_now");
        let theme = cx.theme().clone();
        let window = cx.window();
        let countdown = self.countdown.label;
        let countdown_theme = cx.theme().clone();
        let disabled = self.refresh_is_disabled(cx.app());

        let root = NodeBuilder::boxed()
            .id("TEST_PENTAGON_PIZZA_INDEX_REFRESH")
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .gap(UnifiedSize::Small.gap())
                    .flex_none()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;

        let requests = self.refresh_requests;
        let key = cx.unique_key("pentagon_pizza_index.refresh");
        let button = Button::new(cx.app_mut(), theme, key)
            .ghost()
            .small()
            .icon(IconKind::ArrowsClockwise)
            .disabled(disabled)
            .a11y_label(label.clone())
            .tooltip_text(label)
            .on_click(move |ctx| {
                ctx.update(&requests, |requests| *requests += 1);
                let window = ctx.window();
                let _ = ctx.app_mut().request_repaint(window);
            });
        button.build(cx.app_mut(), window, root)?;

        NodeBuilder::text(
            SmolStr::default(),
            TextStyle::new(UnifiedSize::Small.text_size()),
        )
        .id("TEST_PENTAGON_PIZZA_INDEX_COUNTDOWN")
        .style(StyleBuilder::new().flex_none().build())
        .bind_text(move |read| {
            let label: Option<SmolStr> = (*read.read(&countdown)).clone();
            label.unwrap_or_default()
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(countdown_theme.role(read, Role::FgDefault)),
            ..StylePatch::default()
        })
        .build(cx.tree_mut(), root)?;

        self.refresh_button = Some(button);
        self.refresh_root = Some(root);
        self.refresh_parent = Some(parent);
        self.refresh_language = Some(language);

        Ok(())
    }

    fn sync_refresh_control(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(button) = self.refresh_button.as_ref() else {
            return Ok(());
        };
        button.sync(cx.app_mut())?;
        let disabled = self.refresh_is_disabled(cx.app());
        let _ = button.set_disabled(cx.app_mut(), disabled);

        if self.refresh_language == Some(*cx.read(&cx.language())) {
            return Ok(());
        }
        let Some(parent) = self.refresh_parent else {
            return Ok(());
        };
        if cx.tree().get(parent).is_err() {
            return Ok(());
        }

        self.release_refresh_control(cx, true)?;
        self.build_refresh_control(parent, cx)
    }

    fn release_refresh_control(
        &mut self,
        cx: &mut WidgetCx<'_>,
        remove_node: bool,
    ) -> Result<(), EngineError> {
        let Some(button) = self.refresh_button.take() else {
            self.refresh_root = None;
            self.refresh_parent = None;
            self.refresh_language = None;

            return Ok(());
        };
        if remove_node && let Some(root) = self.refresh_root.take() {
            remove_own_node(cx, root)?;
        }
        let _ = button.release(cx.app_mut());
        self.refresh_root = None;
        self.refresh_parent = None;
        self.refresh_language = None;

        Ok(())
    }
}

const fn level_role(level: PizzaLevel) -> Role {
    match level {
        PizzaLevel::FadeOut => Role::StatusSuccessBase,
        PizzaLevel::DoubleTake => Role::StatusInfoBase,
        PizzaLevel::RoundHouse => Role::StatusWarningBase,
        PizzaLevel::FastPace => Role::StatusElevatedBase,
        PizzaLevel::ExerciseTerm => Role::StatusDangerBase,
    }
}

fn reading_role(reading: Option<PizzaReading>) -> Role {
    reading.map_or(Role::FgDefault, |reading| level_role(reading.level()))
}

fn countdown_label(deadline: Option<i64>, now: i64) -> Option<SmolStr> {
    deadline.map(|deadline| format_countdown(deadline.saturating_sub(now)))
}

fn remove_own_node(cx: &mut WidgetCx<'_>, node: NodeId) -> Result<(), EngineError> {
    if cx.tree().get(node).is_err() {
        return Ok(());
    }
    let window = cx.window();
    cx.app_mut().remove_node(window, node).map(|_| ())
}

fn format_observed_at(observed_at: i64, zone: UtcOffset) -> SmolStr {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(observed_at) * 1_000_000)
        .ok()
        .and_then(|time| time.checked_to_offset(zone))
        .map_or_else(
            || SmolStr::new_static("--"),
            |time| {
                format_smolstr!(
                    "{:04}-{:02}-{:02} {:02}:{:02}",
                    time.year(),
                    u8::from(time.month()),
                    time.day(),
                    time.hour(),
                    time.minute()
                )
            },
        )
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use app_config::settings::{AccountConfig, AccountWalletKind, Config};
    use i18n::{language::codes::LanguageCode, translate::Translate};
    use kytrix_service_provider::service::engine::KytrixServiceError;
    use ui_core::{
        event::{Action, ActionSource},
        paint::color::Color,
    };
    use ui_kit::theme::{appearance::Appearance, resolved::Theme};
    use widget::{protocol::RequestId, test_support::WidgetTestApp};

    use super::*;
    use crate::{
        model::PizzaDataError,
        service::{
            PizzaResponse, PizzaServiceError, PizzaSnapshot, PizzaStatus, PizzaStreamMessage,
        },
        test_support::{ACCOUNT, FakeSource, FetchPlan, OBSERVED_AT, reading},
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

    fn sign_in(harness: &mut WidgetTestApp, user_id: &str) {
        let mut config = current_config(harness);
        config.account = Some(AccountConfig {
            user_id: user_id.to_owned(),
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

    struct Pane {
        widget: PentagonPizzaIndexWidget,
        status: WidgetStatus,
        audience_parent: NodeId,
    }

    fn recovery() -> WidgetAction {
        WidgetAction::new("common.retry", |_| {})
    }

    fn pane_with_source(harness: &mut WidgetTestApp, source: FakeSource) -> Pane {
        let scope = harness.scope();
        let status = WidgetStatus::new(harness.app_mut(), scope);
        let parent = harness.placeholder_parent();
        let widget = harness.with_cx(|cx| {
            PentagonPizzaIndexWidget::new_with_source(source, status, recovery(), cx)
        });

        Pane {
            widget,
            status,
            audience_parent: parent,
        }
    }

    fn pane_without_source(harness: &mut WidgetTestApp) -> Pane {
        let scope = harness.scope();
        let status = WidgetStatus::new(harness.app_mut(), scope);
        let parent = harness.placeholder_parent();
        let widget = harness.with_cx(|cx| {
            PentagonPizzaIndexWidget::new(
                WidgetInstanceId::new(),
                PentagonPizzaIndexConfig,
                status,
                ReloadHandle::new(|_| {}),
                WidgetConfigChange::default(),
                recovery(),
                cx,
            )
            .expect("the pane constructs")
        });

        Pane {
            widget,
            status,
            audience_parent: parent,
        }
    }

    fn pane(harness: &mut WidgetTestApp) -> Pane {
        pane_with_source(harness, FakeSource::default())
    }

    struct TestClock {
        now: Rc<Cell<i64>>,
        reads: Rc<Cell<usize>>,
    }

    impl TestClock {
        fn new(now: i64) -> Self {
            Self {
                now: Rc::new(Cell::new(now)),
                reads: Rc::new(Cell::new(0)),
            }
        }

        fn set(&self, now: i64) {
            self.now.set(now);
        }

        fn reads(&self) -> usize {
            self.reads.get()
        }

        fn now(&self) -> Rc<dyn Fn() -> i64> {
            let now = Rc::clone(&self.now);
            let reads = Rc::clone(&self.reads);
            Rc::new(move || {
                reads.set(reads.get() + 1);
                now.get()
            })
        }
    }

    fn pane_with_source_at_clock(
        harness: &mut WidgetTestApp,
        source: FakeSource,
        now: Rc<dyn Fn() -> i64>,
    ) -> Pane {
        let scope = harness.scope();
        let status = WidgetStatus::new(harness.app_mut(), scope);
        let parent = harness.placeholder_parent();
        let widget = harness.with_cx(|cx| {
            PentagonPizzaIndexWidget::new_with_source_at_clock(source, now, status, recovery(), cx)
        });

        Pane {
            widget,
            status,
            audience_parent: parent,
        }
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
        let parent = pane.audience_parent;
        harness
            .with_cx(|cx| pane.widget.body(parent, cx))
            .expect("the body builds");
    }

    fn build_header(harness: &mut WidgetTestApp, pane: &mut Pane) {
        let parent = pane.audience_parent;
        harness
            .with_cx(|cx| pane.widget.header(HeaderGroup::Right, parent, cx))
            .expect("the header builds");
    }

    fn frame(harness: &mut WidgetTestApp, pane: &mut Pane) {
        sync(harness, pane);
        harness.drain();
        harness.drain();
    }

    fn key() -> PizzaStreamKey {
        PizzaStreamKey {
            account_id: SmolStr::new_static(ACCOUNT),
        }
    }

    fn other_key() -> PizzaStreamKey {
        PizzaStreamKey {
            account_id: SmolStr::new_static("second-account"),
        }
    }

    fn report_status(harness: &mut WidgetTestApp, pane: &mut Pane, status: PizzaStatus) -> bool {
        report_from(harness, pane, key(), PizzaStreamMessage::Status(status))
    }

    fn report_from(
        harness: &mut WidgetTestApp,
        pane: &mut Pane,
        key: PizzaStreamKey,
        message: PizzaStreamMessage,
    ) -> bool {
        harness.with_cx(|cx| match message {
            PizzaStreamMessage::Status(_) => pane.widget.apply_reliable(key, Arc::new(message), cx),
            PizzaStreamMessage::Snapshot(_) => pane.widget.apply_latest(key, Arc::new(message), cx),
        })
    }

    fn report_reading(
        harness: &mut WidgetTestApp,
        pane: &mut Pane,
        revision: u64,
        reading: PizzaReading,
    ) -> bool {
        report_from(
            harness,
            pane,
            key(),
            PizzaStreamMessage::Snapshot(Arc::new(PizzaSnapshot::new(revision, reading))),
        )
    }

    fn ready_pane(harness: &mut WidgetTestApp, level: u8, due: i64) -> Pane {
        sign_in(harness, ACCOUNT);
        let mut pane = pane(harness);
        report_reading(harness, &mut pane, 1, reading(level));
        report_status(
            harness,
            &mut pane,
            PizzaStatus::Ready {
                revision: 1,
                next_refresh_at: due,
            },
        );
        pane
    }

    fn ready_pane_at_clock(
        harness: &mut WidgetTestApp,
        level: u8,
        due: i64,
        now: Rc<dyn Fn() -> i64>,
    ) -> Pane {
        sign_in(harness, ACCOUNT);
        let mut pane = pane_with_source_at_clock(harness, FakeSource::default(), now);
        report_reading(harness, &mut pane, 1, reading(level));
        report_status(
            harness,
            &mut pane,
            PizzaStatus::Ready {
                revision: 1,
                next_refresh_at: due,
            },
        );
        pane
    }

    fn reported(pane: &Pane, harness: &WidgetTestApp) -> WidgetState {
        pane.status.state(harness.app())
    }

    fn level_of(pane: &Pane) -> Option<PizzaLevel> {
        pane.widget.reading.map(|reading| reading.level())
    }

    fn node_with_prefix(harness: &WidgetTestApp, parent: NodeId, prefix: &str) -> NodeId {
        harness
            .find_by_debug_prefix(parent, prefix)
            .unwrap_or_else(|| panic!("the pane built no node with `{prefix}`"))
    }

    fn text_node(harness: &WidgetTestApp, prefix: &str) -> String {
        let node = node_with_prefix(harness, harness.root(), prefix);
        harness.text_of(node)
    }

    fn bar_fill(harness: &WidgetTestApp) -> Color {
        let bar = node_with_prefix(harness, harness.root(), "TEST_PENTAGON_PIZZA_INDEX_BAR");
        harness
            .tree()
            .get(bar)
            .expect("the bar node exists")
            .style_patch()
            .background
            .expect("the bar's colour is written by its bind_style")
    }

    fn read<T: Clone + 'static>(harness: &WidgetTestApp, signal: &Signal<T>) -> T {
        (*harness.app().read(signal)).clone()
    }

    #[test]
    fn a_reading_should_draw_the_moment_it_was_observed_at() {
        assert_eq!(
            format_observed_at(OBSERVED_AT, UtcOffset::UTC).as_str(),
            "2026-10-24 06:00",
            "the fixture reading read as a wall clock in UTC"
        );
        assert_eq!(
            format_observed_at(OBSERVED_AT, UtcOffset::from_hms(8, 0, 0).unwrap()).as_str(),
            "2026-10-24 14:00",
            "the display zone moves the clock without moving the instant"
        );
        assert_eq!(
            format_observed_at(i64::MAX, UtcOffset::UTC).as_str(),
            "--",
            "an instant past the calendar is a placeholder, not a made-up date"
        );
    }

    #[test]
    fn a_countdown_should_be_read_from_the_deadline_and_the_clock() {
        assert_eq!(countdown_label(None, 1_000_000), None);
        assert_eq!(
            countdown_label(Some(1_600_000), 1_000_000),
            Some(SmolStr::new_static("10:00")),
            "a ten-minute wait reads as ten minutes"
        );
        assert_eq!(
            countdown_label(Some(1_598_001), 1_000_000),
            Some(SmolStr::new_static("09:59")),
            "a second left is never rounded away"
        );
        assert_eq!(
            countdown_label(Some(1_000), 1_000_000),
            Some(SmolStr::new_static("00:00")),
            "a deadline that has passed is zero, not a negative wait"
        );
        assert_eq!(
            countdown_label(Some(4_600_000), 1_000_000),
            Some(SmolStr::new_static("01:00:00")),
            "hours appear only when there are hours left"
        );
    }

    #[test]
    fn the_widget_should_have_a_stable_identity() {
        assert_eq!(
            PentagonPizzaIndexWidget::ID.as_str(),
            "kytrix.pentagon_pizza_index"
        );
        assert_eq!(
            PentagonPizzaIndexWidget::DISPLAY_NAME,
            "widget.names.pentagon_pizza_index"
        );
        assert_eq!(
            PentagonPizzaIndexWidget::CATEGORY,
            WidgetCategory::Monitoring
        );
        assert_eq!(
            PentagonPizzaIndexWidget::DESCRIPTION,
            Some("widget.descriptions.pentagon_pizza_index")
        );
        assert_eq!(
            PentagonPizzaIndexWidget::ICON,
            Some("widgets/pentagon_pizza_index.png")
        );
        assert_eq!(
            <PentagonPizzaIndexConfig as WidgetConfig>::SCHEMA_VERSION,
            WidgetConfigSchemaVersion::V1
        );
    }

    #[test]
    fn a_reading_should_be_the_panes_content() {
        let mut harness = harness();
        let pane = ready_pane(&mut harness, 3, 0);

        assert!(pane.widget.has_content());
        assert_eq!(reported(&pane, &harness), WidgetState::Ready);
        assert_eq!(level_of(&pane), Some(PizzaLevel::RoundHouse));
    }

    #[test]
    fn a_failed_round_should_report_a_failure_and_hold_no_reading() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);

        report_status(
            &mut harness,
            &mut pane,
            PizzaStatus::Failed(PizzaFetchError::Data(PizzaDataError::UnknownLevel {
                level: 9,
            })),
        );

        assert!(pane.status.is_failed(harness.app()));
        assert!(
            !pane.widget.has_content(),
            "a failed read is not content: the framework draws the failure placeholder"
        );
        assert!(pane.widget.reading.is_none());
    }

    #[test]
    fn a_late_message_should_not_revive_a_failed_pane() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);

        report_status(
            &mut harness,
            &mut pane,
            PizzaStatus::Failed(PizzaFetchError::Service(KytrixServiceError::Closed)),
        );
        report_reading(&mut harness, &mut pane, 1, reading(1));

        assert!(pane.status.is_failed(harness.app()));
        assert!(
            pane.widget.reading.is_none(),
            "a reading that arrives after a failure is about a stream that is gone"
        );
    }

    #[test]
    fn a_message_about_another_account_should_not_be_drawn() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);

        assert!(report_reading(&mut harness, &mut pane, 1, reading(3)));
        assert_eq!(level_of(&pane), Some(PizzaLevel::RoundHouse));

        report_from(
            &mut harness,
            &mut pane,
            other_key(),
            PizzaStreamMessage::Snapshot(Arc::new(PizzaSnapshot::new(2, reading(1)))),
        );

        assert_eq!(
            level_of(&pane),
            Some(PizzaLevel::RoundHouse),
            "the reading served to another account did not replace the one on screen"
        );
        assert_eq!(
            pane.widget.accepted,
            Some(1),
            "and it did not take the revision with it either"
        );
    }

    #[test]
    fn an_older_revision_should_not_replace_the_reading_on_screen() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);

        report_status(
            &mut harness,
            &mut pane,
            PizzaStatus::Ready {
                revision: 2,
                next_refresh_at: 0,
            },
        );
        report_reading(&mut harness, &mut pane, 2, reading(1));
        report_reading(&mut harness, &mut pane, 1, reading(5));

        assert_eq!(
            pane.widget.accepted,
            Some(2),
            "the reading on screen is still revision two"
        );
        assert_eq!(
            level_of(&pane),
            Some(PizzaLevel::ExerciseTerm),
            "the older reading did not replace the one on screen"
        );
    }

    #[test]
    fn a_retry_should_keep_the_reading_and_drop_its_revision() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);

        report_reading(&mut harness, &mut pane, 2, reading(1));
        report_status(
            &mut harness,
            &mut pane,
            PizzaStatus::Failed(PizzaFetchError::Service(KytrixServiceError::Closed)),
        );
        harness
            .with_cx(|cx| pane.widget.start(true, cx))
            .expect("the retry starts");

        assert_eq!(
            pane.widget.accepted, None,
            "the revision numbers one stream, and this is another one"
        );
        assert_eq!(
            level_of(&pane),
            Some(PizzaLevel::ExerciseTerm),
            "the reading the user was looking at is still there"
        );
        assert!(
            pane.widget.has_content(),
            "a pane that kept its reading still has something to draw"
        );

        assert!(
            report_reading(&mut harness, &mut pane, 1, reading(5)),
            "the first reading of the new stream is a change"
        );
        assert_eq!(
            level_of(&pane),
            Some(PizzaLevel::FadeOut),
            "and it is what the pane now draws"
        );
    }

    #[test]
    fn a_load_answer_for_a_reader_that_signed_out_should_not_fail_the_pane() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);

        sign_out(&mut harness);
        sync(&mut harness, &mut pane);
        harness.with_cx(|cx| {
            pane.widget.apply_rpc(
                RequestId::new(1),
                Err(RpcCallError::ServiceClosed {
                    request_id: RequestId::new(1),
                }),
                cx,
            );
        });

        assert_eq!(
            reported(&pane, &harness),
            WidgetState::waiting(Notice::message("widget.pentagon_pizza_index.logged_out")),
            "the load answered for the account that left"
        );
        assert_eq!(
            pane.widget.pending.dropped(),
            1,
            "the answer to the replaced load is dropped and counted"
        );
    }

    #[test]
    fn a_refresh_answer_for_a_reader_that_signed_out_should_not_fail_the_pane() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 3, due);
        sync(&mut harness, &mut pane);

        harness
            .with_cx(|cx| pane.widget.refresh(cx))
            .expect("the refresh starts");
        assert!(
            pane.widget.refresh_pending,
            "the click puts a refresh in flight"
        );

        sign_out(&mut harness);
        sync(&mut harness, &mut pane);
        harness.with_cx(|cx| {
            pane.widget.apply_rpc(
                RequestId::new(1),
                Err(RpcCallError::ServiceClosed {
                    request_id: RequestId::new(1),
                }),
                cx,
            );
        });

        assert_eq!(
            reported(&pane, &harness),
            WidgetState::waiting(Notice::message("widget.pentagon_pizza_index.logged_out")),
            "the refresh answered for the account that left"
        );
        assert_eq!(
            pane.widget.pending.dropped(),
            1,
            "the answer to the replaced refresh is dropped and counted"
        );
    }

    #[test]
    fn a_signed_out_load_should_wait_for_an_account() {
        let mut harness = harness();
        let mut pane = pane(&mut harness);

        load(&mut harness, &mut pane);

        assert_eq!(
            reported(&pane, &harness),
            WidgetState::waiting(Notice::message("widget.pentagon_pizza_index.logged_out"))
        );
        assert!(!pane.widget.has_content());
        assert!(pane.widget.current_key().is_none());
    }

    #[test]
    fn a_reading_that_did_not_change_should_not_ask_for_a_frame() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);

        report_status(
            &mut harness,
            &mut pane,
            PizzaStatus::Ready {
                revision: 1,
                next_refresh_at: 0,
            },
        );
        assert!(report_reading(&mut harness, &mut pane, 1, reading(3)));
        assert!(
            !report_reading(&mut harness, &mut pane, 1, reading(3)),
            "the same reading twice is one change"
        );
    }

    #[test]
    fn the_two_channels_should_meet_at_the_readings_revision() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);

        report_reading(&mut harness, &mut pane, 1, reading(2));
        assert_eq!(
            reported(&pane, &harness),
            WidgetState::Connecting,
            "a reading without its status is a pane that is still connecting"
        );

        report_status(
            &mut harness,
            &mut pane,
            PizzaStatus::Ready {
                revision: 1,
                next_refresh_at: 0,
            },
        );
        assert_eq!(reported(&pane, &harness), WidgetState::Ready);
    }

    #[test]
    fn the_countdown_should_belong_to_a_ready_pane() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 2, due);
        frame(&mut harness, &mut pane);

        assert!(
            read(&harness, &pane.widget.countdown.label).is_some(),
            "a ready pane counts down to its next read"
        );
        assert!(!pane.widget.refresh_is_disabled(harness.app()));

        report_status(
            &mut harness,
            &mut pane,
            PizzaStatus::Failed(PizzaFetchError::Service(KytrixServiceError::Timeout)),
        );
        frame(&mut harness, &mut pane);
        assert!(
            read(&harness, &pane.widget.countdown.label).is_none(),
            "a failed pane is not counting down to anything"
        );
        assert!(pane.widget.refresh_is_disabled(harness.app()));
    }

    #[test]
    fn signing_out_should_take_the_reading_with_it() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 3, due);
        frame(&mut harness, &mut pane);
        assert!(pane.widget.has_content());

        sign_out(&mut harness);
        sync(&mut harness, &mut pane);

        assert!(!pane.widget.has_content());
        assert!(
            pane.widget.reading.is_none(),
            "the reading was served to an account that just left"
        );
        assert_eq!(pane.widget.accepted, None);
        assert_eq!(
            reported(&pane, &harness),
            WidgetState::waiting(Notice::message("widget.pentagon_pizza_index.logged_out")),
            "a signed-out pane waits for an account"
        );
        assert!(pane.widget.current_key().is_none());
        assert!(
            read(&harness, &pane.widget.countdown.label).is_none(),
            "a pane waiting for an account is not counting down to a read"
        );
    }

    #[test]
    fn another_account_should_take_the_reading_with_it() {
        let mut harness = harness();
        let mut pane = ready_pane(&mut harness, 3, 0);

        assert!(pane.widget.has_content());
        sign_in(&mut harness, "second-account");
        sync(&mut harness, &mut pane);

        assert!(
            !pane.widget.has_content(),
            "the reading was served to the account that just left"
        );
        assert_eq!(reported(&pane, &harness), WidgetState::Connecting);
        assert_eq!(
            pane.widget.current_key(),
            Some(other_key()),
            "the pane follows the account the settings now hold"
        );
    }

    #[test]
    fn an_account_change_should_re_scope_the_pane() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);

        report_reading(&mut harness, &mut pane, 1, reading(3));
        assert!(pane.widget.has_content());

        sign_in(&mut harness, "second-account");
        sync(&mut harness, &mut pane);
        assert_eq!(
            pane.widget.account_id.as_deref(),
            Some("second-account"),
            "the pane reads the account from the settings snapshot"
        );
        assert!(
            !pane.widget.has_content(),
            "the reading was served to the account that left"
        );
        assert_eq!(pane.widget.current_key(), Some(other_key()));

        sign_out(&mut harness);
        sync(&mut harness, &mut pane);
        assert_eq!(
            reported(&pane, &harness),
            WidgetState::waiting(Notice::message("widget.pentagon_pizza_index.logged_out")),
            "signing out asks for an account"
        );
    }

    #[test]
    fn the_body_should_draw_the_level_its_number_its_bar_and_the_moment() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 3, due);
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);

        assert_eq!(
            text_node(&harness, "TEST_PENTAGON_PIZZA_INDEX_NUMBER"),
            "Level 3"
        );
        assert_eq!(
            text_node(&harness, "TEST_PENTAGON_PIZZA_INDEX_NAME"),
            "Increase in force readiness"
        );
        assert_eq!(
            text_node(&harness, "TEST_PENTAGON_PIZZA_INDEX_OBSERVED"),
            "Observed at 2026-10-24 06:00"
        );

        let bar = node_with_prefix(&harness, harness.root(), "TEST_PENTAGON_PIZZA_INDEX_BAR");
        let bounds = harness
            .tree()
            .get(bar)
            .expect("the bar exists")
            .layout()
            .bounds;
        assert!(
            (bounds.size.height - LEVEL_BAR_HEIGHT).abs() < f32::EPSILON,
            "the bar is one thin rule under the reading: {}",
            bounds.size.height
        );
        assert!(
            bounds.size.width > 0.0 && bounds.size.width <= LEVEL_BAR_MAX_WIDTH + f32::EPSILON,
            "the bar is written across the pane up to its cap: {}",
            bounds.size.width
        );
        assert_eq!(
            harness
                .tree()
                .get(bar)
                .expect("the bar")
                .style_patch()
                .background,
            Some(Theme::of(Appearance::Light).role(level_role(PizzaLevel::RoundHouse))),
            "the bar is the rung's status colour"
        );
    }

    #[test]
    fn every_rung_should_be_drawn_in_its_own_theme_colour() {
        let light = Theme::of(Appearance::Light);
        let dark = Theme::of(Appearance::Dark);
        let levels = [
            PizzaLevel::FadeOut,
            PizzaLevel::DoubleTake,
            PizzaLevel::RoundHouse,
            PizzaLevel::FastPace,
            PizzaLevel::ExerciseTerm,
        ];
        let colours: Vec<_> = levels
            .iter()
            .map(|level| light.role(level_role(*level)))
            .collect();
        for (index, colour) in colours.iter().enumerate() {
            assert!(
                !colours[..index].contains(colour),
                "two rungs share a colour, so two readings would look the same"
            );
        }
        for (level, role) in [
            (PizzaLevel::FadeOut, Role::StatusSuccessBase),
            (PizzaLevel::DoubleTake, Role::StatusInfoBase),
            (PizzaLevel::RoundHouse, Role::StatusWarningBase),
            (PizzaLevel::FastPace, Role::StatusElevatedBase),
            (PizzaLevel::ExerciseTerm, Role::StatusDangerBase),
        ] {
            assert_eq!(
                level_role(level),
                role,
                "the rung {level:?} changed severity"
            );
        }
        for level in levels {
            assert_ne!(
                light.role(level_role(level)),
                dark.role(level_role(level)),
                "{level:?} does not follow the appearance"
            );
        }

        let mut harness = harness();
        let mut pane = ready_pane(&mut harness, 5, 0);
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        assert_eq!(
            bar_fill(&harness),
            light.role(level_role(PizzaLevel::FadeOut)),
            "the calmest rung is the success colour"
        );

        let theme = harness.services().theme.clone();
        theme.set_appearance(harness.app_mut(), Appearance::Dark);
        harness.drain();
        assert_eq!(
            bar_fill(&harness),
            dark.role(level_role(PizzaLevel::FadeOut)),
            "the bar follows the appearance through bind_style"
        );
    }

    #[test]
    fn the_body_should_follow_the_language() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 3, due);
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        assert!(
            text_node(&harness, "TEST_PENTAGON_PIZZA_INDEX_NAME")
                .contains("Increase in force readiness")
        );

        harness.switch_language(LanguageCode::ZhCn);
        harness.drain();
        harness.drain();

        let name = text_node(&harness, "TEST_PENTAGON_PIZZA_INDEX_NAME");
        assert!(
            !name.is_empty() && !name.contains("Increase in force readiness"),
            "the level's name follows the language: {name}"
        );
        let observed = text_node(&harness, "TEST_PENTAGON_PIZZA_INDEX_OBSERVED");
        assert!(
            observed.starts_with("观测于"),
            "the observed-at sentence follows the language: {observed}"
        );
    }

    #[test]
    fn moving_the_display_zone_should_redraw_the_moment() {
        let mut harness = harness();
        let mut pane = ready_pane(&mut harness, 3, 0);
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        assert_eq!(
            text_node(&harness, "TEST_PENTAGON_PIZZA_INDEX_OBSERVED"),
            "Observed at 2026-10-24 06:00"
        );

        let mut config = current_config(&harness);
        config.data_time_zone =
            app_config::time_zone::DataTimeZone::from_hours(8).expect("UTC+8 is offered");
        assert!(write_config(&mut harness, config));
        harness.drain();
        harness.drain();

        assert_eq!(
            text_node(&harness, "TEST_PENTAGON_PIZZA_INDEX_OBSERVED"),
            "Observed at 2026-10-24 14:00"
        );
    }

    #[test]
    fn the_header_should_build_the_refresh_control_and_its_countdown() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 2, due);
        build_header(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);

        let refresh = node_with_prefix(
            &harness,
            pane.audience_parent,
            "TEST_PENTAGON_PIZZA_INDEX_REFRESH",
        );
        assert_eq!(
            harness.tree().get(refresh).expect("the control").kind(),
            &ui_core::node::tree::NodeKind::Box
        );
        assert_eq!(
            text_node(&harness, "TEST_PENTAGON_PIZZA_INDEX_COUNTDOWN"),
            "10:00"
        );

        let button = pane
            .widget
            .refresh_button
            .as_ref()
            .expect("the control is kept for release");
        assert_eq!(
            button.action_identity(),
            Some(Action::Custom(format_smolstr!("{}.click", button.key()))),
            "the control's click is an engine action named by the control's key"
        );
        assert!(
            button.key().starts_with("pentagon_pizza_index.refresh@"),
            "the key is this pane's own rather than a constant two panes would share: {}",
            button.key()
        );
        assert!(
            !*harness.app().read(&button.interactive().disabled_signal()),
            "a ready pane can refresh"
        );
    }

    #[test]
    fn the_refresh_control_should_be_disabled_with_nothing_to_refresh() {
        let mut harness = harness();
        let mut pane = pane(&mut harness);
        load(&mut harness, &mut pane);
        build_header(&mut harness, &mut pane);

        let button = pane.widget.refresh_button.as_ref().expect("the control");
        assert!(
            read(&harness, &button.interactive().disabled_signal()),
            "the control is born disabled when there is nothing to refresh"
        );

        frame(&mut harness, &mut pane);
        let button = pane.widget.refresh_button.as_ref().expect("the control");
        assert!(
            read(&harness, &button.interactive().disabled_signal()),
            "a pane waiting for an account has nothing to refresh"
        );

        let window = harness.window();
        let node = button.node().expect("the control is built");
        let action = button.action_identity().expect("the control has a click");
        harness.with_cx(|cx| {
            let _ = cx
                .app_mut()
                .dispatch_action(window, node, action, ActionSource::Programmatic);
        });
        frame(&mut harness, &mut pane);
        assert!(
            !pane.widget.refresh_pending,
            "the disabled control did not start a refresh"
        );
    }

    #[test]
    fn a_click_should_become_the_refresh_the_pane_starts() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 2, due);
        build_header(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);

        let (node, action) = {
            let button = pane.widget.refresh_button.as_ref().expect("the control");
            (
                button.node().expect("the control is built"),
                button.action_identity().expect("the control has a click"),
            )
        };
        let window = harness.window();
        harness.with_cx(|cx| {
            let _ = cx
                .app_mut()
                .dispatch_action(window, node, action, ActionSource::Programmatic);
        });
        sync(&mut harness, &mut pane);

        assert!(
            pane.widget.refresh_pending,
            "the click became the refresh the pane started"
        );
    }

    #[test]
    fn rebuilding_the_header_should_not_accumulate_controls() {
        let mut harness = harness();
        let mut pane = ready_pane(&mut harness, 2, 0);
        build_header(&mut harness, &mut pane);
        let after_first = harness.node_count();
        assert!(after_first > 0);

        build_header(&mut harness, &mut pane);

        assert_eq!(
            harness.node_count(),
            after_first,
            "the replaced control's nodes were taken down with it"
        );
    }

    #[test]
    fn a_language_change_should_rebuild_the_refresh_control() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 2, due);
        build_header(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        let old = {
            let button = pane.widget.refresh_button.as_ref().expect("the control");
            (
                button.node().expect("the control is built"),
                button.action_identity(),
            )
        };

        harness.switch_language(LanguageCode::ZhCn);
        frame(&mut harness, &mut pane);

        let rebuilt = pane.widget.refresh_button.as_ref().expect("the control");
        let new_node = rebuilt.node().expect("the rebuilt control is built");
        assert_ne!(new_node, old.0, "the control was rebuilt, not reused");
        assert!(
            harness.tree().get(old.0).is_err(),
            "the English control's nodes were taken down"
        );
        assert_eq!(
            rebuilt.action_identity(),
            old.1,
            "the rebuilt control is the same action: its key is the pane group, not the copy"
        );
        assert_eq!(
            pane.widget.refresh_language,
            Some(LanguageCode::ZhCn),
            "the pane recorded the language it rebuilt the control in"
        );
    }

    #[test]
    fn a_countdown_tick_should_move_the_label_on_the_clock() {
        let clock = TestClock::new(0);
        let mut harness = harness();
        let mut pane = ready_pane_at_clock(&mut harness, 2, 600_000, clock.now());
        build_header(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        assert_eq!(countdown_text(&harness), "10:00");

        let requests = harness.frame_requests();
        assert!(
            !harness.advance(Duration::from_millis(125)),
            "half the interval is an idle round"
        );
        assert_eq!(countdown_text(&harness), "10:00");

        clock.set(1_000);
        assert!(
            harness.advance(Duration::from_millis(125)),
            "the tick that reached the deadline drew the label it moved"
        );
        assert_eq!(countdown_text(&harness), "09:59");
        assert!(harness.frame_requests() > requests);
    }

    #[test]
    fn a_countdown_should_rearm_its_timer_for_each_tick() {
        let clock = TestClock::new(0);
        let mut harness = harness();
        let mut pane = ready_pane_at_clock(&mut harness, 2, 600_000, clock.now());
        build_header(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        assert!(pane.widget.countdown.timer.borrow().is_some());

        for second in 1..=3 {
            clock.set(second * 1_000);
            assert!(
                harness.advance(COUNTDOWN_TICK),
                "tick {second} moved the label"
            );
            assert_eq!(countdown_text(&harness), format!("09:{:02}", 60 - second));
            assert!(
                pane.widget.countdown.timer.borrow().is_some(),
                "the chain re-armed itself after tick {second}"
            );
        }
    }

    #[test]
    fn a_failed_or_signed_out_pane_should_stop_the_countdown() {
        {
            let clock = TestClock::new(0);
            let mut harness = harness();
            let mut pane = ready_pane_at_clock(&mut harness, 2, 600_000, clock.now());
            build_header(&mut harness, &mut pane);
            frame(&mut harness, &mut pane);
            assert!(pane.widget.countdown.timer.borrow().is_some());

            report_status(
                &mut harness,
                &mut pane,
                PizzaStatus::Failed(PizzaFetchError::Service(KytrixServiceError::Timeout)),
            );
            frame(&mut harness, &mut pane);
            assert!(
                pane.widget.countdown.timer.borrow().is_none(),
                "the failed pane's tick is cancelled"
            );
            assert!(
                read(&harness, &pane.widget.countdown.label).is_none(),
                "a failed pane is not counting down to anything"
            );
            harness.drain();
            let requests = harness.frame_requests();
            assert!(
                !harness.advance(COUNTDOWN_TICK * 4),
                "a pane that stopped has no timer left to fire"
            );
            assert_eq!(harness.frame_requests(), requests);
            assert_eq!(countdown_text(&harness), "");
        }

        let clock = TestClock::new(0);
        let mut harness = harness();
        let mut pane = ready_pane_at_clock(&mut harness, 2, 600_000, clock.now());
        build_header(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        assert!(pane.widget.countdown.timer.borrow().is_some());

        sign_out(&mut harness);
        sync(&mut harness, &mut pane);
        assert!(
            pane.widget.countdown.timer.borrow().is_none(),
            "the signed-out pane's tick is cancelled"
        );
        assert!(read(&harness, &pane.widget.countdown.label).is_none());
        harness.drain();
        let requests = harness.frame_requests();
        assert!(!harness.advance(COUNTDOWN_TICK * 4));
        assert_eq!(harness.frame_requests(), requests);
        assert_eq!(
            countdown_text(&harness),
            "",
            "the label does not come back after the account left"
        );
    }

    #[test]
    fn releasing_the_pane_should_cancel_the_countdown_timer() {
        let clock = TestClock::new(0);
        let mut harness = harness();
        let mut pane = ready_pane_at_clock(&mut harness, 2, 600_000, clock.now());
        build_header(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);

        assert!(pane.widget.countdown.timer.borrow().is_some());

        harness.with_cx(|cx| pane.widget.release(cx));

        assert!(
            pane.widget.countdown.timer.borrow().is_none(),
            "release cancels the tick"
        );
        assert!(read(&harness, &pane.widget.countdown.label).is_none());
        harness.drain();
        let requests = harness.frame_requests();
        assert!(
            !harness.advance(COUNTDOWN_TICK * 4),
            "a released pane's timer never fires"
        );
        assert_eq!(harness.frame_requests(), requests);
    }

    #[test]
    fn a_deadline_that_comes_back_should_not_arm_a_second_timer_chain() {
        let clock = TestClock::new(0);
        let mut harness = harness();
        let mut pane = ready_pane_at_clock(&mut harness, 2, 600_000, clock.now());
        build_header(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        assert!(pane.widget.countdown.timer.borrow().is_some());

        report_status(
            &mut harness,
            &mut pane,
            PizzaStatus::Refreshing {
                next_refresh_at: 600_000,
            },
        );
        sync(&mut harness, &mut pane);
        assert!(
            pane.widget.countdown.timer.borrow().is_none(),
            "the read in flight cancelled the tick it replaced"
        );
        assert!(read(&harness, &pane.widget.countdown.label).is_none());

        report_status(
            &mut harness,
            &mut pane,
            PizzaStatus::Ready {
                revision: 1,
                next_refresh_at: 600_000,
            },
        );
        sync(&mut harness, &mut pane);

        let reads = clock.reads();
        let requests = harness.frame_requests();
        harness.advance(COUNTDOWN_TICK);
        assert_eq!(
            clock.reads() - reads,
            1,
            "the deadline that came back arms one chain, not two"
        );
        assert_eq!(
            harness.frame_requests(),
            requests,
            "the tick that changed nothing asks for no frame"
        );

        clock.set(1_000);
        let reads = clock.reads();
        harness.advance(COUNTDOWN_TICK);
        assert_eq!(countdown_text(&harness), "09:59");
        assert_eq!(
            clock.reads() - reads,
            1,
            "and the surviving chain ticks once per round"
        );
    }

    fn countdown_text(harness: &WidgetTestApp) -> String {
        text_node(harness, "TEST_PENTAGON_PIZZA_INDEX_COUNTDOWN")
    }

    fn built_control_key(harness: &WidgetTestApp, parent: NodeId, before: &[NodeId]) -> SmolStr {
        let wrapper = harness
            .tree()
            .get(parent)
            .expect("the header row exists")
            .children()
            .iter()
            .find(|child| !before.contains(child))
            .copied()
            .expect("the header built a control");
        let debug = harness
            .find_by_debug_prefix(wrapper, "pentagon_pizza_index.refresh")
            .and_then(|node| harness.tree().debug_id(node))
            .expect("the refresh control is built");
        SmolStr::new(
            debug
                .strip_suffix(":interactive")
                .expect("the kit names its interactive node after the key"),
        )
    }

    fn children_of(harness: &WidgetTestApp, parent: NodeId) -> Vec<NodeId> {
        harness
            .tree()
            .get(parent)
            .expect("the header row exists")
            .children()
            .to_vec()
    }

    #[test]
    fn two_panes_should_keep_their_own_refresh_controls() {
        let mut harness = harness();
        let first = harness.mount::<PentagonPizzaIndexWidget>(PentagonPizzaIndexConfig);
        let second = harness.mount::<PentagonPizzaIndexWidget>(PentagonPizzaIndexConfig);
        harness.drain();
        let parent = harness.placeholder_parent();

        let before = children_of(&harness, parent);
        first
            .header(harness.app_mut(), HeaderGroup::Right, parent)
            .expect("the first pane's header builds");
        let first_key = built_control_key(&harness, parent, &before);
        let before = children_of(&harness, parent);
        second
            .header(harness.app_mut(), HeaderGroup::Right, parent)
            .expect("the second pane's header builds");
        let second_key = built_control_key(&harness, parent, &before);

        let first_action = Action::Custom(format_smolstr!("{first_key}.click"));
        let second_action = Action::Custom(format_smolstr!("{second_key}.click"));
        assert!(
            harness.app().has_action_handler(&first_action),
            "the first pane's control has a click handler"
        );
        assert!(
            harness.app().has_action_handler(&second_action),
            "building the second pane did not take the first pane's handler"
        );

        first.release(harness.app_mut());

        assert!(
            !harness.app().has_action_handler(&first_action),
            "releasing the first pane took its own handler"
        );
        assert!(
            harness.app().has_action_handler(&second_action),
            "and left the second pane's handler alone"
        );
        assert_ne!(
            first_key, second_key,
            "two panes build their controls under different keys"
        );
        assert!(
            harness.tree().get(parent).is_ok(),
            "the header row both panes built into is not the released pane's to remove"
        );
    }

    #[test]
    fn a_replaced_commands_answer_should_not_clear_the_command_that_replaced_it() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 2, due);
        frame(&mut harness, &mut pane);

        harness
            .with_cx(|cx| pane.widget.refresh(cx))
            .expect("the refresh starts");
        assert!(pane.widget.refresh_pending);
        let mut delivered = false;
        for _ in 0..64 {
            harness.drain_runtime();
            if !pane
                .widget
                .binding
                .as_ref()
                .expect("the pane has a bridge")
                .inbox()
                .is_empty()
            {
                delivered = true;
                break;
            }
        }
        assert!(delivered, "the refresh was answered before the frame");

        sign_in(&mut harness, "second-account");
        sync(&mut harness, &mut pane);

        assert!(
            !pane.widget.pending.is_empty(),
            "the replacement's own answer is still waited for"
        );
        assert!(!pane.widget.refresh_pending);
        assert_eq!(
            reported(&pane, &harness),
            WidgetState::Connecting,
            "the old command's answer did not become the new account's failure"
        );

        for _ in 0..64 {
            harness.drain_runtime();
            sync(&mut harness, &mut pane);
            if pane.widget.pending.is_empty() {
                break;
            }
        }
        assert!(
            pane.widget.pending.is_empty(),
            "the load's own acknowledgement was applied"
        );
    }

    #[test]
    fn a_reply_the_pane_already_answered_should_not_clear_the_command_in_flight() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness);
        frame(&mut harness, &mut pane);

        harness
            .with_cx(|cx| pane.widget.start(false, cx))
            .expect("the load starts");
        assert!(!pane.widget.pending.is_empty());
        harness.with_cx(|cx| {
            pane.widget
                .apply_rpc(RequestId::new(1), Ok(PizzaResponse::StreamStarted), cx);
        });
        assert!(
            pane.widget.pending.is_empty(),
            "the load's acknowledgement was applied"
        );

        harness
            .with_cx(|cx| pane.widget.start(false, cx))
            .expect("the load starts");
        assert!(!pane.widget.pending.is_empty());

        harness.with_cx(|cx| {
            pane.widget.apply_rpc(
                RequestId::new(1),
                Err(RpcCallError::ServiceClosed {
                    request_id: RequestId::new(1),
                }),
                cx,
            );
        });

        assert!(
            !pane.widget.pending.is_empty(),
            "the stale reply did not answer the command in flight"
        );
        assert_eq!(pane.widget.pending.dropped(), 1);
        assert_eq!(reported(&pane, &harness), WidgetState::Connecting);
    }

    #[test]
    fn a_round_the_stream_publishes_should_reach_the_pane() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane_with_source(&mut harness, FakeSource::new([FetchPlan::Snapshot(4)]));
        load(&mut harness, &mut pane);

        for _ in 0..64 {
            harness.drain_runtime();
            sync(&mut harness, &mut pane);
            harness.drain();
            if pane.widget.reading.is_some() && pane.widget.ready.is_some() {
                break;
            }
        }

        assert_eq!(level_of(&pane), Some(PizzaLevel::DoubleTake));
        assert_eq!(pane.widget.accepted, Some(1));
        assert_eq!(reported(&pane, &harness), WidgetState::Ready);
    }

    #[test]
    fn a_host_without_a_service_connection_should_report_a_failure() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane_without_source(&mut harness);

        load(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);

        let state = reported(&pane, &harness);
        let failure = state.failure().expect("the pane reports a failure");
        assert!(
            !failure.text(&harness.services().translate).is_empty(),
            "the failure renders"
        );
        assert_eq!(
            failure.action().label_key(),
            "common.retry",
            "the framework's recovery is the one the failure offers"
        );
        assert!(
            !pane.widget.has_content(),
            "a failed pane has no content: the surface draws the failure placeholder"
        );
    }

    #[test]
    fn a_signed_out_pane_should_be_presented_in_the_language_showing_now() {
        let mut harness = harness();
        let widget = harness.mount::<PentagonPizzaIndexWidget>(PentagonPizzaIndexConfig);
        harness.drain();
        assert!(
            widget.config().is_none(),
            "the index is the same index for every reader, so the pane persists nothing"
        );

        let placeholder = node_with_prefix(&harness, widget.surface(), "WIDGET_PLACEHOLDER");
        assert!(harness.visible(placeholder));
        let english = harness.text_of(placeholder);
        assert!(
            english.contains("register or log in"),
            "the notice is drawn as text: {english}"
        );
        assert!(
            !english.contains("<p") && !english.contains("</p"),
            "the notice's markup is not drawn as text: {english}"
        );

        harness.switch_language(LanguageCode::ZhCn);
        harness.drain();
        harness.drain();
        let placeholder = node_with_prefix(&harness, widget.surface(), "WIDGET_PLACEHOLDER");
        let chinese = harness.text_of(placeholder);
        assert!(
            chinese.contains("注册") || chinese.contains("登陆"),
            "the notice follows the language: {chinese}"
        );
    }

    #[test]
    fn a_failed_pane_should_be_presented_with_the_frameworks_recovery() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let widget = harness.mount::<PentagonPizzaIndexWidget>(PentagonPizzaIndexConfig);
        harness.drain();

        assert!(widget.status().is_failed(harness.app()));
        let placeholder = node_with_prefix(&harness, widget.surface(), "WIDGET_PLACEHOLDER");
        let text = harness.text_of(placeholder);
        assert!(!text.is_empty(), "the failure is presented: {text}");
        assert!(
            harness
                .find_by_debug_prefix(placeholder, "widget.placeholder.action.")
                .is_some(),
            "the failure placeholder carries the framework's recovery control"
        );
        assert!(
            !harness.has_canvas(placeholder),
            "a failure is not a spinner"
        );
    }

    #[test]
    fn releasing_the_pane_should_release_what_it_built() {
        let mut harness = harness();
        let due = unix_millis().saturating_add(600_000);
        let mut pane = ready_pane(&mut harness, 2, due);
        build_body(&mut harness, &mut pane);
        build_header(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        let action = pane
            .widget
            .refresh_button
            .as_ref()
            .and_then(Button::action_identity)
            .expect("the control's click is an engine action");
        let refresh_node = pane
            .widget
            .refresh_button
            .as_ref()
            .and_then(Button::node)
            .expect("the control is built");

        harness.with_cx(|cx| pane.widget.release(cx));

        assert!(pane.widget.binding.is_none(), "the bridge is dropped");
        assert!(
            pane.widget.refresh_button.is_none(),
            "the control is released"
        );
        assert!(pane.widget.body_root.is_none());
        assert!(
            !harness.app().has_action_handler(&action),
            "the control's handler was removed with its scope"
        );
        assert!(
            harness.tree().get(refresh_node).is_ok(),
            "the header row's nodes belong to the pane, not to the widget's release"
        );
    }

    #[test]
    fn every_message_this_pane_shows_should_resolve() {
        for language in [LanguageCode::En, LanguageCode::ZhCn] {
            let translate = product_translate(language);

            for level in [
                PizzaLevel::FadeOut,
                PizzaLevel::DoubleTake,
                PizzaLevel::RoundHouse,
                PizzaLevel::FastPace,
                PizzaLevel::ExerciseTerm,
            ] {
                assert!(
                    !level.name(&translate).is_empty(),
                    "{} has no copy in {language}",
                    level.name_key()
                );
            }
            for key in [
                "widget.names.pentagon_pizza_index",
                "widget.descriptions.pentagon_pizza_index",
                "widget.pentagon_pizza_index.refresh_now",
                "widget.pentagon_pizza_index.observed_at",
                "widget.pentagon_pizza_index.level_number",
                "widget.pentagon_pizza_index.logged_out",
            ] {
                assert!(
                    !t_smol!(translate, key).is_empty(),
                    "{key} has no copy in {language}"
                );
            }
            assert!(
                !t_smol!(
                    translate,
                    "widget.pentagon_pizza_index.observed_at",
                    time = "2026-10-24 06:00"
                )
                .is_empty(),
                "widget.pentagon_pizza_index.observed_at has no copy in {language}"
            );
            assert!(
                !t_smol!(
                    translate,
                    "widget.pentagon_pizza_index.level_number",
                    level = 3
                )
                .is_empty(),
                "widget.pentagon_pizza_index.level_number has no copy in {language}"
            );
        }
    }

    #[test]
    fn every_error_this_pane_reports_should_resolve() {
        for language in [LanguageCode::En, LanguageCode::ZhCn] {
            let translate = product_translate(language);

            for error in [
                PizzaDataError::MissingObservation,
                PizzaDataError::UnknownLevel { level: 9 },
            ] {
                assert!(
                    !error.translated_message(&translate).is_empty(),
                    "{error:?} renders empty in {language}"
                );
            }
            for error in [
                PizzaServiceError::StreamClosed,
                PizzaServiceError::StreamBusy,
            ] {
                assert!(
                    !error.translated_message(&translate).is_empty(),
                    "{error:?} renders empty in {language}"
                );
            }
            for error in [
                KytrixServiceError::Closed,
                KytrixServiceError::Timeout,
                KytrixServiceError::Authentication("expired".into()),
                KytrixServiceError::Protocol("missing data".into()),
            ] {
                assert!(
                    !PizzaFailure::Fetch(PizzaFetchError::Service(error.clone()))
                        .translated_message(&translate)
                        .is_empty(),
                    "{error:?} renders empty in {language}"
                );
            }
            assert!(
                !PizzaFailure::Fetch(PizzaFetchError::Data(PizzaDataError::MissingObservation))
                    .translated_message(&translate)
                    .is_empty(),
                "a data failure renders empty in {language}"
            );
            assert!(
                !PizzaFailure::Rpc(RpcCallError::ServiceClosed {
                    request_id: RequestId::new(1),
                })
                .translated_message(&translate)
                .is_empty(),
                "an RPC failure renders empty in {language}"
            );
        }
    }
}

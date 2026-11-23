use std::{cell::RefCell, rc::Rc, sync::Arc, time::Instant};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_service_provider::auth::constants::ACCOUNT_ORIGIN;
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::A11yPatch,
    error::EngineError,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::{style::StylePatch, style_builder::StyleBuilder},
    text::{TextOverflow, TextStyle},
};
use ui_kit::{
    kit::{
        badge::Badge,
        button::Button,
        empty_state::EmptyState,
        icon::IconKind,
        list::{List, ListOptions, SelectionMode},
        separator::Separator,
        size::{UnifiedSizable, UnifiedSize},
    },
    theme::{handle::ThemeHandle, role::Role},
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
    services::WidgetServices,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    time_zone::data_time_zone,
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

#[cfg(feature = "audio")]
use super::sound::{NOTIFICATION_CLIP, SoundPlayer};
use super::{
    model::{MAX_NEWS_ITEMS, NewsCategory, NewsSnapshot, has_new_news, unshown},
    service::{
        NewsProtocol, NewsRequest, NewsScope, NewsService, NewsSource, NewsStatus, NewsStreamError,
        NewsStreamMessage,
    },
    sound::{NotificationSound, SilentSound, Throttle},
};

const ROW_ESTIMATE: f32 = 76.0;

const ROW_TEXT_SIZE: UnifiedSize = UnifiedSize::Medium;

const ROW_TIME_SIZE: UnifiedSize = UnifiedSize::Small;

type RowCategories = Rc<RefCell<Arc<[NewsCategory]>>>;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NewsConfig {
    #[serde(rename = "sound")]
    pub sound: bool,
}

impl Default for NewsConfig {
    fn default() -> Self {
        Self { sound: true }
    }
}

impl WidgetConfig for NewsConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum NewsFailure {
    Stream(NewsStreamError),
    Rpc(RpcCallError<std::convert::Infallible>),
}

impl TranslatedError for NewsFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Stream(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    Start,
    Stop,
}

pub struct NewsWidget {
    config: NewsConfig,
    binding: Option<WidgetBinding<NewsProtocol>>,
    pending: Pending<PendingCommand>,
    status: WidgetStatus,
    recovery: WidgetAction,
    config_change: WidgetConfigChange,
    account_id: Option<SmolStr>,
    snapshot: Option<NewsSnapshot>,
    ready: bool,
    items: Signal<NewsSnapshot>,
    list: Option<List>,
    body_root: Option<NodeId>,
    categories: RowCategories,
    rows_language: Option<LanguageCode>,
    empty: Option<NodeId>,
    empty_language: Option<LanguageCode>,
    sound: Box<dyn NotificationSound>,
    throttle: Throttle,
    sound_button: Option<Button>,
    sound_parent: Option<NodeId>,
    sound_built: Option<(LanguageCode, bool, bool)>,
    toggle_requests: Signal<u64>,
    seen_toggle_requests: u64,
}

impl WidgetView for NewsWidget {
    type Config = NewsConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.news");
    const DISPLAY_NAME: &'static str = "widget.names.news";
    const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.news");
    const ICON: Option<&'static str> = Some("widgets/news.png");

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
        let sound = notification_sound(cx.services());

        Ok(Self::assemble(
            config,
            source,
            sound,
            status,
            config_change,
            recovery,
            cx,
        ))
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        let retry = self.status.is_failed(cx.app());
        self.account_id = cx.account();
        if let Err(error) = self.start(retry, cx) {
            log::error!("the news pane could not start: {error}");
        }
    }

    fn has_content(&self) -> bool {
        self.snapshot.is_some()
    }

    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let root = NodeBuilder::boxed()
            .id("TEST_NEWS_BODY")
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .size_full()
                    .min_width_0()
                    .min_height_0()
                    .relative()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        self.body_root = Some(root);
        self.list = Some(self.make_list(cx, root)?);
        self.rows_language = Some(*cx.read(&cx.language()));
        self.sync_empty_state(cx)?;

        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let account = cx.account();
        if account != self.account_id {
            self.account_id = account;
            self.start(false, cx)?;
        }

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

        let requests = *cx.read(&self.toggle_requests);
        if requests != self.seen_toggle_requests {
            self.seen_toggle_requests = requests;
            self.toggle_sound(cx);
        }

        self.rebuild_rows_for_language(cx)?;
        if let Some(list) = &self.list {
            list.sync(cx.app_mut())?;
        }
        self.sync_empty_state(cx)?;
        self.sync_sound_button(cx)?;

        if self.snapshot.is_some() && self.body_root.is_none() {
            cx.request_frame()?;
        }

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
        self.release_sound_button(cx, true)?;
        self.build_sound_button(parent, cx)
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        self.binding = None;
        let _ = self.release_sound_button(cx, false);
        if let Some(root) = self.body_root.take() {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, root);
        }
        if let Some(list) = self.list.take() {
            let _ = list.release(cx.app_mut());
        }
        self.empty = None;
        self.empty_language = None;
        self.snapshot = None;
    }
}

impl NewsWidget {
    fn assemble<S: NewsSource>(
        config: NewsConfig,
        source: Option<S>,
        sound: Box<dyn NotificationSound>,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let binding = source.map(|source| WidgetBinding::spawn(cx, NewsService::new(source)));
        let items = cx.signal(NewsSnapshot::default());
        let toggle_requests = cx.signal(0_u64);
        let account_id = cx.account();

        Self {
            config,
            binding,
            pending: Pending::new(),
            status,
            recovery,
            config_change,
            account_id,
            snapshot: None,
            ready: false,
            items,
            list: None,
            body_root: None,
            categories: Rc::new(RefCell::new(Arc::from([]))),
            rows_language: None,
            empty: None,
            empty_language: None,
            sound,
            throttle: Throttle::default(),
            sound_button: None,
            sound_parent: None,
            sound_built: None,
            toggle_requests,
            seen_toggle_requests: 0,
        }
    }

    #[cfg(test)]
    fn new_with_source<S: NewsSource>(
        config: NewsConfig,
        source: S,
        sound: Box<dyn NotificationSound>,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        Self::assemble(
            config,
            Some(source),
            sound,
            status,
            config_change,
            recovery,
            cx,
        )
    }

    fn current_scope(&self) -> Option<NewsScope> {
        NewsScope::new(self.account_id.as_ref())
    }

    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(scope) = self.current_scope() else {
            self.clear(cx)?;
            self.report_logged_out(cx);
            if let Some(binding) = self.binding.as_mut() {
                let request_id = binding.call(cx, NewsRequest::Stop);
                self.pending.track(request_id, PendingCommand::Stop);
            }
            cx.request_frame()?;
            return Ok(());
        };
        self.ready = false;
        if !retry {
            self.clear(cx)?;
        }
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        let Some(binding) = self.binding.as_mut() else {
            self.fail(
                NewsStreamError::Service(
                    kytrix_service_provider::service::engine::KytrixServiceError::Closed,
                ),
                cx,
            );
            return Ok(());
        };
        let request_id = binding.call(
            cx,
            if retry {
                NewsRequest::Retry(scope)
            } else {
                NewsRequest::Load(scope)
            },
        );
        self.pending.track(request_id, PendingCommand::Start);
        cx.request_frame()?;

        Ok(())
    }

    fn clear(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.snapshot = None;
        self.ready = false;
        cx.write(&self.items, NewsSnapshot::default());
        *self.categories.borrow_mut() = Arc::from([]);
        if let Some(list) = &self.list {
            list.set_items(cx.app_mut(), 0)?;
        }
        self.sync_empty_state(cx)?;
        cx.request_frame()?;

        Ok(())
    }

    fn report_logged_out(&self, cx: &mut WidgetCx<'_>) {
        self.status.report(
            cx.app_mut(),
            WidgetState::waiting(Notice::message("widget.news.logged_out")),
        );
    }

    fn announce(&mut self) {
        if self.config.sound && self.throttle.allows(Instant::now()) {
            self.sound.play();
        }
    }

    fn toggle_sound(&mut self, cx: &mut WidgetCx<'_>) {
        self.config.sound = !self.config.sound;
        self.config_change.publish(cx.app_mut());
        if let Some(button) = &self.sound_button {
            let available = self.sound.is_available();
            let _ = button.set_selected(cx.app_mut(), !self.config.sound);
            let _ = button.set_disabled(cx.app_mut(), !available);
        }
        let _ = cx.request_frame();
    }

    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: widget::binding::WidgetRpcResult<NewsProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        match self.pending.take(request_id) {
            Some(PendingCommand::Start) => self.apply_load_result(result, cx),
            Some(PendingCommand::Stop) => {}
            None => log::debug!(
                "the news pane ignored reply {request_id}: no command is waiting for it"
            ),
        }
    }

    fn apply_load_result(
        &mut self,
        result: widget::binding::WidgetRpcResult<NewsProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.status.is_failed(cx.app()) {
            return;
        }

        if let Err(error) = result {
            self.ready = false;
            let recovery = self.recovery.clone();
            self.status.report(
                cx.app_mut(),
                WidgetState::failed(Arc::new(NewsFailure::Rpc(error)), recovery),
            );
        }
    }

    fn apply_latest(
        &mut self,
        key: NewsScope,
        message: Arc<NewsStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        if self.current_scope().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
            return false;
        }
        let NewsStreamMessage::Snapshot(snapshot) = message.as_ref() else {
            return false;
        };

        let arrived = has_new_news(self.snapshot.as_ref(), snapshot);
        if let Err(error) = self.accept(snapshot.clone(), cx) {
            log::error!("the news pane could not take the feed it was published: {error}");
            return false;
        }
        if arrived {
            self.announce();
        }
        if self.ready {
            self.status.report(cx.app_mut(), WidgetState::Ready);
        }
        true
    }

    fn apply_reliable(
        &mut self,
        key: NewsScope,
        message: Arc<NewsStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        if self.current_scope().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
            return false;
        }
        let NewsStreamMessage::Status(status) = message.as_ref() else {
            return false;
        };

        match status {
            NewsStatus::Connecting => {
                self.ready = false;
                self.status.report(cx.app_mut(), WidgetState::Connecting);
            }
            NewsStatus::Ready => {
                self.ready = true;
                if self.has_content() {
                    self.status.report(cx.app_mut(), WidgetState::Ready);
                } else {
                    self.status.report(cx.app_mut(), WidgetState::Loading);
                }
            }
            NewsStatus::Failed(error) => {
                self.ready = false;
                self.fail(error.clone(), cx);
            }
        }

        false
    }

    fn fail(&mut self, error: NewsStreamError, cx: &mut WidgetCx<'_>) {
        let failure = NewsFailure::Stream(error);
        if matches!(
            &failure,
            NewsFailure::Stream(NewsStreamError::EntitlementRequired)
        ) {
            let action = view_plans(cx.services().open_url.clone());
            self.status
                .report(cx.app_mut(), WidgetState::failed(Arc::new(failure), action));
            return;
        }
        let recovery = self.recovery.clone();
        self.status.report(
            cx.app_mut(),
            WidgetState::failed(Arc::new(failure), recovery),
        );
    }

    fn accept(&mut self, snapshot: NewsSnapshot, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let unshown = unshown(
            self.snapshot.as_ref().map_or(&[], NewsSnapshot::items),
            snapshot.items(),
        );

        cx.write(&self.items, snapshot.clone());
        *self.categories.borrow_mut() = snapshot
            .items()
            .iter()
            .map(|item| item.category())
            .collect();
        let count = snapshot.items().len();
        match (unshown, self.list.as_ref()) {
            (Some(pushed), Some(list)) => {
                for _ in 0..pushed {
                    list.push(cx.app_mut())?;
                }
            }
            (None, Some(list)) => list.set_items(cx.app_mut(), count)?,
            _ => {}
        }
        self.snapshot = Some(snapshot);
        cx.request_frame()?;

        Ok(())
    }

    fn make_list(&self, cx: &mut WidgetCx<'_>, parent: NodeId) -> Result<List, EngineError> {
        let items = self.items;
        let language = cx.language();
        let config = cx.services().config;
        let categories = Rc::clone(&self.categories);
        let theme = cx.theme().clone();
        let translate = cx.translate().clone();
        let count = cx.read(&items).items().len();
        let key = format_smolstr!("NewsList:{}", parent.to_bits());

        let list = List::new(
            cx.app_mut(),
            theme.clone(),
            key,
            ListOptions::variable(ROW_ESTIMATE)
                .deque(MAX_NEWS_ITEMS)
                .reverse()
                .keep_on_newest()
                .selection(SelectionMode::None),
            move |tree, row_parent, index| {
                build_row(
                    tree,
                    row_parent,
                    index,
                    items,
                    language,
                    config,
                    &categories,
                    theme.clone(),
                    translate.clone(),
                )
            },
        );
        list.set_initial_count(cx.app_mut(), count);
        let window = cx.window();
        list.mount(cx.app_mut(), window, parent)?;

        Ok(list)
    }

    fn rebuild_rows_for_language(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(list) = self.list.as_ref() else {
            return Ok(());
        };
        let language = *cx.read(&cx.language());
        if self.rows_language == Some(language) {
            return Ok(());
        }
        self.rows_language = Some(language);
        let count = list.count(cx.app());
        if count == 0 {
            return Ok(());
        }
        list.set_count(cx.app_mut(), count)?;
        cx.request_frame()?;

        Ok(())
    }

    fn sync_empty_state(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let empty = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.items().is_empty());
        if !empty {
            if let Some(node) = self.empty.take() {
                remove_own_node(cx, node)?;
                self.empty_language = None;
            }
            return Ok(());
        }

        let language = *cx.read(&cx.language());
        let stale = self
            .empty
            .is_none_or(|_| self.empty_language != Some(language));
        if !stale {
            return Ok(());
        }
        if let Some(node) = self.empty.take() {
            remove_own_node(cx, node)?;
        }
        let Some(root) = self.body_root else {
            return Ok(());
        };

        let overlay = NodeBuilder::boxed()
            .id("TEST_NEWS_EMPTY")
            .style(
                StyleBuilder::new()
                    .absolute()
                    .top(0.0)
                    .right(0.0)
                    .bottom(0.0)
                    .left(0.0)
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .build(),
            )
            .build(cx.tree_mut(), root)?;
        let text = t_smol!(cx.translate(), "common.no_data");
        EmptyState::new(cx.theme().clone(), "NewsEmpty")
            .text(text)
            .build(cx.tree_mut(), overlay)?;
        self.empty = Some(overlay);
        self.empty_language = Some(language);

        Ok(())
    }

    fn build_sound_button(
        &mut self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let language = *cx.read(&cx.language());
        let available = self.sound.is_available();
        let enabled = self.config.sound;
        let label = if enabled {
            t_smol!(cx.translate(), "widget.news.disable_sound")
        } else {
            t_smol!(cx.translate(), "widget.news.enable_sound")
        };
        let icon = if enabled {
            IconKind::SpeakerSimpleHigh
        } else {
            IconKind::SpeakerSimpleSlash
        };
        let key = format_smolstr!("news.sound.{}", parent.to_bits());
        let theme = cx.theme().clone();
        let window = cx.window();
        let toggle_requests = self.toggle_requests;

        let mut button = Button::new(cx.app_mut(), theme, key)
            .ghost()
            .small()
            .icon(icon)
            .a11y_label(label.clone())
            .tooltip_text(label)
            .selected(!enabled);
        if available {
            button = button.on_click(move |ctx| {
                ctx.update(&toggle_requests, |requests| *requests += 1);
                let window = ctx.window();
                let _ = ctx.app_mut().request_repaint(window);
            });
        } else {
            button = button.disabled(true);
        }
        button.build(cx.app_mut(), window, parent)?;

        self.sound_button = Some(button);
        self.sound_parent = Some(parent);
        self.sound_built = Some((language, enabled, available));

        Ok(())
    }

    fn sync_sound_button(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(button) = self.sound_button.as_ref() else {
            return Ok(());
        };
        button.sync(cx.app_mut())?;
        let available = self.sound.is_available();
        let _ = button.set_disabled(cx.app_mut(), !available);
        let _ = button.set_selected(cx.app_mut(), !self.config.sound);

        let state = (*cx.read(&cx.language()), self.config.sound, available);
        if self.sound_built == Some(state) {
            return Ok(());
        }
        let Some(parent) = self.sound_parent else {
            return Ok(());
        };
        if cx.tree().get(parent).is_err() {
            return Ok(());
        }

        self.release_sound_button(cx, true)?;
        self.build_sound_button(parent, cx)
    }

    fn release_sound_button(
        &mut self,
        cx: &mut WidgetCx<'_>,
        remove_node: bool,
    ) -> Result<(), EngineError> {
        let Some(button) = self.sound_button.take() else {
            return Ok(());
        };
        if remove_node && let Some(node) = button.node() {
            remove_own_node(cx, node)?;
        }
        let _ = button.release(cx.app_mut());
        self.sound_parent = None;
        self.sound_built = None;

        Ok(())
    }
}

fn remove_own_node(cx: &mut WidgetCx<'_>, node: NodeId) -> Result<(), EngineError> {
    if cx.tree().get(node).is_err() {
        return Ok(());
    }
    let window = cx.window();
    cx.app_mut().remove_node(window, node).map(|_| ())
}

#[cfg(feature = "audio")]
fn notification_sound(services: &WidgetServices) -> Box<dyn NotificationSound> {
    match (services.sound)(NOTIFICATION_CLIP) {
        Some(clip) => Box::new(SoundPlayer::new(clip)),
        None => Box::new(SilentSound),
    }
}

#[cfg(not(feature = "audio"))]
fn notification_sound(_services: &WidgetServices) -> Box<dyn NotificationSound> {
    Box::new(SilentSound)
}

fn view_plans(open_url: Rc<dyn Fn(&str)>) -> WidgetAction {
    WidgetAction::new("widget.news.view_plans", move |_| {
        (open_url)(ACCOUNT_ORIGIN);
    })
}

fn build_row(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    items: Signal<NewsSnapshot>,
    language: Signal<LanguageCode>,
    config: Signal<app_config::settings::Config>,
    categories: &RowCategories,
    theme: ThemeHandle,
    translate: Translate,
) -> Result<(), EngineError> {
    let a11y_items = items;
    let a11y_language = language;
    let a11y_config = config;
    let a11y_translate = translate.clone();
    let row = NodeBuilder::boxed()
        .id(format_smolstr!("TEST_NEWS_ROW:{index}"))
        .style(
            StyleBuilder::new()
                .flex_col()
                .width_full()
                .min_width_0()
                .build(),
        )
        .bind_a11y(move |read| {
            let _language: ui_core::node::signal::SignalRef<LanguageCode> =
                read.read(&a11y_language);
            let snapshot = read.read(&a11y_items);
            let settings = read.read(&a11y_config);
            let Some(item) = snapshot.items().get(index) else {
                return A11yPatch::default();
            };
            let zone = data_time_zone(&settings);
            let time = item.display(zone).time.clone();
            let category = category_label(item.category(), &a11y_translate);

            A11yPatch {
                label: Some(t_smol!(
                    a11y_translate,
                    "widget.news.item",
                    category = category,
                    time = time
                )),
                ..A11yPatch::default()
            }
        })
        .build(tree, parent)?;

    if index > 0 {
        Separator::horizontal(theme.clone()).build(tree, row)?;
    }

    let content = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_col()
                .gap(8.0)
                .width_full()
                .min_width_0()
                .padding(8.0)
                .build(),
        )
        .build(tree, row)?;

    let meta = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(8.0)
                .width_full()
                .min_width_0()
                .build(),
        )
        .build(tree, content)?;
    let leading = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .gap(8.0)
                .flex_1()
                .min_width_0()
                .build(),
        )
        .build(tree, meta)?;

    build_category_badge(tree, leading, index, categories, &theme, &translate)?;
    build_title(tree, leading, index, items, theme.clone())?;
    build_time(tree, meta, index, items, config, theme.clone())?;
    build_content(tree, content, index, items, theme)?;

    Ok(())
}

fn build_category_badge(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    categories: &RowCategories,
    theme: &ThemeHandle,
    translate: &Translate,
) -> Result<(), EngineError> {
    let text = categories
        .borrow()
        .get(index)
        .map_or_else(SmolStr::default, |category| {
            category_label(*category, translate)
        });

    Badge::new(theme.clone())
        .outline()
        .small()
        .text(text)
        .build(tree, parent)?;

    Ok(())
}

fn build_title(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    items: Signal<NewsSnapshot>,
    theme: ThemeHandle,
) -> Result<(), EngineError> {
    let mut style = TextStyle::new(ROW_TEXT_SIZE.text_size());
    style.max_lines = 1;
    style.overflow = TextOverflow::Ellipsis;
    NodeBuilder::text(SmolStr::default(), style)
        .style(StyleBuilder::new().flex_1().min_width_0().build())
        .bind_text(move |read| {
            let snapshot = read.read(&items);
            snapshot
                .items()
                .get(index)
                .and_then(|item| item.title().cloned())
                .unwrap_or_default()
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(theme.role(read, Role::FgDefault)),
            ..StylePatch::default()
        })
        .build(tree, parent)?;

    Ok(())
}

fn build_time(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    items: Signal<NewsSnapshot>,
    config: Signal<app_config::settings::Config>,
    theme: ThemeHandle,
) -> Result<(), EngineError> {
    NodeBuilder::text(
        SmolStr::default(),
        TextStyle::new(ROW_TIME_SIZE.text_size()),
    )
    .style(StyleBuilder::new().flex_none().build())
    .bind_text(move |read| {
        let snapshot = read.read(&items);
        let settings = read.read(&config);
        let zone = data_time_zone(&settings);
        snapshot
            .items()
            .get(index)
            .map_or_else(SmolStr::default, |item| item.display(zone).time.clone())
    })
    .bind_style(move |read| StylePatch {
        foreground: Some(theme.role(read, Role::FgMuted)),
        ..StylePatch::default()
    })
    .build(tree, parent)?;

    Ok(())
}

fn build_content(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    items: Signal<NewsSnapshot>,
    theme: ThemeHandle,
) -> Result<(), EngineError> {
    NodeBuilder::text(
        SmolStr::default(),
        TextStyle::new(ROW_TEXT_SIZE.text_size()),
    )
    .style(StyleBuilder::new().min_width_0().build())
    .bind_text(move |read| {
        let snapshot = read.read(&items);
        snapshot
            .items()
            .get(index)
            .map_or_else(SmolStr::default, |item| item.content().clone())
    })
    .bind_style(move |read| StylePatch {
        foreground: Some(theme.role(read, Role::FgDefault)),
        ..StylePatch::default()
    })
    .build(tree, parent)?;

    Ok(())
}

fn category_label(category: NewsCategory, translate: &Translate) -> SmolStr {
    match category {
        NewsCategory::General => t_smol!(translate, "widget.news.general"),
        NewsCategory::Crypto => t_smol!(translate, "widget.news.crypto"),
        NewsCategory::Political => t_smol!(translate, "widget.news.political"),
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc, sync::Arc};

    use app_config::{
        settings::{AccountConfig, AccountWalletKind, Config},
        time_zone::DataTimeZone,
        widget::{VersionedWidgetConfig, decode_or_default},
    };
    use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
    use kytrix_service_provider::service::engine::KytrixServiceError;
    use network::transport::error::TransportError;
    use ui_core::event::ActionSource;
    use ui_kit::theme::appearance::Appearance;
    use widget::{protocol::RequestId, test_support::WidgetTestApp};

    use super::*;
    use crate::{
        service::NewsStreamError,
        sound::MINIMUM_INTERVAL,
        test_support::{ACCOUNT, FakeFeed, FakeSource, RecordedSound, event},
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
        widget: NewsWidget,
        status: WidgetStatus,
        parent: NodeId,
    }

    fn pane_with(
        harness: &mut WidgetTestApp,
        source: FakeSource,
        sound: Box<dyn NotificationSound>,
        config_change: WidgetConfigChange,
    ) -> Pane {
        let scope = harness.scope();
        let status = WidgetStatus::new(harness.app_mut(), scope);
        let parent = harness.placeholder_parent();
        let widget = harness.with_cx(|cx| {
            NewsWidget::new_with_source(
                NewsConfig::default(),
                source,
                sound,
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
        }
    }

    fn pane(harness: &mut WidgetTestApp, source: FakeSource) -> Pane {
        pane_with(
            harness,
            source,
            Box::new(SilentSound),
            WidgetConfigChange::default(),
        )
    }

    fn audible_pane(harness: &mut WidgetTestApp, source: FakeSource) -> (Pane, Arc<RecordedSound>) {
        let sound = RecordedSound::with_device();
        let pane = pane_with(
            harness,
            source,
            Box::new(sound.clone()),
            WidgetConfigChange::default(),
        );

        (pane, sound)
    }

    fn load(harness: &mut WidgetTestApp, pane: &mut Pane) {
        harness.with_cx(|cx| pane.widget.load(cx));
    }

    fn sync(harness: &mut WidgetTestApp, pane: &mut Pane) {
        harness
            .with_cx(|cx| pane.widget.sync(cx))
            .expect("the widget syncs");
    }

    fn report_feed(harness: &mut WidgetTestApp, pane: &mut Pane, feed: NewsSnapshot) -> bool {
        let scope = pane
            .widget
            .current_scope()
            .expect("a pane with an account follows one");
        if let Some(binding) = pane.widget.binding.as_mut() {
            binding.cancel();
        }
        harness.with_cx(|cx| {
            pane.widget
                .apply_latest(scope, Arc::new(NewsStreamMessage::Snapshot(feed)), cx)
        })
    }

    fn report_status(harness: &mut WidgetTestApp, pane: &mut Pane, status: NewsStatus) -> bool {
        let scope = pane
            .widget
            .current_scope()
            .expect("a pane with an account follows one");
        harness.with_cx(|cx| {
            pane.widget
                .apply_reliable(scope, Arc::new(NewsStreamMessage::Status(status)), cx)
        })
    }

    fn failure_of(harness: &WidgetTestApp, pane: &Pane) -> SmolStr {
        pane.status
            .state(harness.app())
            .failure()
            .expect("the pane should have reported a failure")
            .text(&harness.services().translate)
    }

    fn snapshot_contents(widget: &NewsWidget) -> Vec<String> {
        widget
            .snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .items()
                    .iter()
                    .map(|item| item.content().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn build_body(harness: &mut WidgetTestApp, pane: &mut Pane) {
        let parent = pane.parent;
        harness
            .with_cx(|cx| pane.widget.body(parent, cx))
            .expect("the body builds");
    }

    fn frame(harness: &mut WidgetTestApp, pane: &mut Pane) {
        sync(harness, pane);
        harness.drain();
        harness.drain();
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

    #[test]
    fn every_message_this_pane_shows_should_resolve_to_text() {
        let translate = product_translate(LanguageCode::En);

        for key in [
            "error.news.entitlement_required",
            "widget.names.news",
            "widget.news.crypto",
            "widget.news.disable_sound",
            "widget.news.enable_sound",
            "widget.news.general",
            "widget.news.logged_out",
            "widget.news.political",
            "widget.news.scroll_vertical",
            "widget.news.view_plans",
        ] {
            assert!(!t_smol!(translate, key).is_empty(), "{key} has no copy");
        }
        assert!(
            !t_smol!(
                translate,
                "widget.news.item",
                category = t_smol!(translate, "widget.news.general"),
                time = "2023-11-14 22:13:20"
            )
            .is_empty(),
            "widget.news.item has no copy"
        );

        for failure in [
            NewsFailure::Stream(NewsStreamError::EntitlementRequired),
            NewsFailure::Stream(NewsStreamError::Service(KytrixServiceError::Closed)),
            NewsFailure::Stream(NewsStreamError::Transport(TransportError::Closed)),
            NewsFailure::Rpc(RpcCallError::DeadlineExceeded {
                request_id: RequestId::new(2),
            }),
        ] {
            assert!(
                !failure.translated_message(&translate).is_empty(),
                "{failure:?} has no copy"
            );
        }
    }

    #[test]
    fn the_widget_should_have_a_stable_identity() {
        assert_eq!(NewsWidget::ID.as_str(), "kytrix.news");
        assert_eq!(NewsWidget::DISPLAY_NAME, "widget.names.news");
        assert_eq!(
            <NewsConfig as WidgetConfig>::SCHEMA_VERSION,
            WidgetConfigSchemaVersion::V1
        );
    }

    #[test]
    fn the_sound_should_be_on_unless_a_reader_turns_it_off() {
        assert!(
            NewsConfig::default().sound,
            "a reader is told about news by default"
        );
        assert_eq!(
            decode_or_default::<NewsConfig>(None),
            NewsConfig::default(),
            "a pane stored without options is a pane with the default ones"
        );
    }

    #[test]
    fn the_config_should_round_trip_through_its_envelope() {
        let config = NewsConfig { sound: false };
        let stored = VersionedWidgetConfig::encode(&config).expect("a feed config should encode");

        assert_eq!(stored.schema_version(), WidgetConfigSchemaVersion::V1);
        assert_eq!(stored.decode::<NewsConfig>(), config);
        assert_eq!(decode_or_default::<NewsConfig>(None), NewsConfig::default());
    }

    #[test]
    fn a_signed_out_pane_should_say_what_it_is_waiting_for() {
        let mut harness = harness();
        let source = FakeSource::default();
        let mut pane = pane(&mut harness, source.clone());

        load(&mut harness, &mut pane);

        assert!(!pane.widget.has_content());
        assert_eq!(pane.widget.current_scope(), None);
        assert_eq!(
            pane.status.state(harness.app()).notice(),
            Some(&Notice::message("widget.news.logged_out")),
            "a pane nobody signed in for is not a failure"
        );
        assert!(!pane.status.state(harness.app()).is_failed());
        assert_eq!(
            source.subscriptions(),
            0,
            "a pane nobody reads for subscribes to nothing"
        );
    }

    #[test]
    fn a_signed_out_pane_should_follow_the_language() {
        let mut harness = harness();
        let mut pane = pane(&mut harness, FakeSource::default());
        load(&mut harness, &mut pane);

        let notice = pane
            .status
            .state(harness.app())
            .notice()
            .cloned()
            .expect("a pane nobody signed in for waits");
        assert_eq!(notice, Notice::message("widget.news.logged_out"));
        let copy = notice.resolve(&harness.services().translate);
        assert!(
            copy.contains("register"),
            "the notice shows the language that was installed: {copy}"
        );

        harness.switch_language(LanguageCode::ZhCn);

        let copy = notice.resolve(&harness.services().translate);
        assert!(
            copy.contains("登陆"),
            "the same reference resolves in the language showing now: {copy}"
        );
    }

    #[test]
    fn the_state_should_move_through_connecting_loading_and_ready() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let source = FakeSource::default();
        let _control = source.subscribe();
        let mut pane = pane(&mut harness, source);

        load(&mut harness, &mut pane);
        assert_eq!(
            pane.status.state(harness.app()),
            WidgetState::Connecting,
            "a pane that started reading is connecting"
        );

        report_status(&mut harness, &mut pane, NewsStatus::Ready);
        assert_eq!(
            pane.status.state(harness.app()),
            WidgetState::Loading,
            "a live subscription with nothing to show is still reading"
        );

        let mut feed = FakeFeed::new();
        report_feed(&mut harness, &mut pane, feed.push(1));
        report_status(&mut harness, &mut pane, NewsStatus::Ready);
        assert_eq!(pane.status.state(harness.app()), WidgetState::Ready);
        assert!(pane.widget.has_content());
    }

    #[test]
    fn an_empty_feed_should_still_be_content() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness, FakeSource::default());

        report_feed(&mut harness, &mut pane, FakeFeed::new().snapshot());
        report_status(&mut harness, &mut pane, NewsStatus::Ready);

        assert!(
            pane.widget.has_content(),
            "a feed with nothing in it is a feed"
        );
        assert!(
            pane.status.state(harness.app()).notice().is_none(),
            "the feed says it holds no news itself rather than through a placeholder"
        );
        assert!(!pane.status.state(harness.app()).is_busy());
    }

    #[test]
    fn a_feed_nothing_of_which_survives_should_be_replaced() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness, FakeSource::default());

        report_feed(&mut harness, &mut pane, FakeFeed::new().push(1));
        report_feed(&mut harness, &mut pane, FakeFeed::new().push(9));

        assert_eq!(snapshot_contents(&pane.widget), ["News 9"]);
    }

    #[test]
    fn a_report_for_an_account_the_pane_left_should_be_ignored() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();

        report_feed(&mut harness, &mut pane, feed.push(1));
        let other = NewsScope::new(Some(&SmolStr::new_static("someone-else")))
            .expect("another account is another scope");
        let accepted = harness.with_cx(|cx| {
            (
                pane.widget.apply_latest(
                    other.clone(),
                    Arc::new(NewsStreamMessage::Snapshot(feed.push(2))),
                    cx,
                ),
                pane.widget.apply_reliable(
                    other,
                    Arc::new(NewsStreamMessage::Status(NewsStatus::Ready)),
                    cx,
                ),
            )
        });

        assert_eq!(
            accepted,
            (false, false),
            "a feed published for another account is not this pane's to draw"
        );
        assert_eq!(snapshot_contents(&pane.widget), ["News 1"]);
    }

    #[test]
    fn a_failed_pane_should_ignore_late_reports_until_it_is_loaded_again() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let source = FakeSource::default();
        let _control = source.subscribe();
        let mut pane = pane(&mut harness, source);
        let mut feed = FakeFeed::new();

        report_feed(&mut harness, &mut pane, feed.push(1));
        report_status(&mut harness, &mut pane, NewsStatus::Ready);
        report_status(
            &mut harness,
            &mut pane,
            NewsStatus::Failed(NewsStreamError::Transport(TransportError::Closed)),
        );
        assert!(pane.status.is_failed(harness.app()));

        assert!(
            !report_feed(&mut harness, &mut pane, feed.push(2)),
            "a pane that has failed is finished with the feed it was reading"
        );
        assert_eq!(snapshot_contents(&pane.widget), ["News 1"]);

        load(&mut harness, &mut pane);
        assert!(!pane.status.is_failed(harness.app()));
        assert!(
            pane.widget.has_content(),
            "a recovery reads again without dropping what was read"
        );
        assert_eq!(snapshot_contents(&pane.widget), ["News 1"]);
    }

    #[test]
    fn the_answer_to_the_read_it_sent_should_be_reported() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness, FakeSource::default());
        load(&mut harness, &mut pane);

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
            pane.status.is_failed(harness.app()),
            "the read's refusal is the pane's failure"
        );
        assert!(
            failure_of(&harness, &pane).contains("service"),
            "and it is the sentence the typed error states"
        );
    }

    #[test]
    fn the_answer_to_a_replaced_command_should_be_dropped() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness, FakeSource::default());
        load(&mut harness, &mut pane);
        sign_out(&mut harness);
        sync(&mut harness, &mut pane);
        let waiting = WidgetState::waiting(Notice::message("widget.news.logged_out"));
        assert_eq!(pane.status.state(harness.app()), waiting);

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
            pane.status.state(harness.app()),
            waiting,
            "the answer to the replaced read is not this pane's failure"
        );
        assert_eq!(
            pane.widget.pending.dropped(),
            1,
            "the late answer is dropped and counted"
        );
    }

    #[test]
    fn a_failed_pane_should_not_repaint_its_failure_from_a_late_reply() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness, FakeSource::default());
        load(&mut harness, &mut pane);
        report_status(
            &mut harness,
            &mut pane,
            NewsStatus::Failed(NewsStreamError::Transport(TransportError::Closed)),
        );
        let failure = failure_of(&harness, &pane);

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
            failure_of(&harness, &pane),
            failure,
            "the sentence the user is reading is not rewritten"
        );
    }

    #[test]
    fn a_missing_entitlement_should_offer_the_subscription_instead_of_a_retry() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness, FakeSource::default());

        report_status(
            &mut harness,
            &mut pane,
            NewsStatus::Failed(NewsStreamError::EntitlementRequired),
        );

        assert_eq!(
            failure_of(&harness, &pane).as_str(),
            t_smol!(
                harness.services().translate,
                "error.news.entitlement_required"
            )
        );
        let action = pane
            .status
            .state(harness.app())
            .failure()
            .expect("the refusal should offer a recovery")
            .action()
            .clone();
        assert_eq!(
            action.label(&harness.services().translate).as_str(),
            t_smol!(harness.services().translate, "widget.news.view_plans")
        );
        assert_eq!(action.label_key(), "widget.news.view_plans");
    }

    #[test]
    fn the_plans_recovery_should_open_the_account_page() {
        let opened: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let open_url = {
            let opened = Rc::clone(&opened);
            Rc::new(move |url: &str| opened.borrow_mut().push(url.to_owned()))
        };
        let action = view_plans(open_url);
        let mut harness = harness();

        action.run(harness.app_mut());

        assert_eq!(opened.borrow().as_slice(), [ACCOUNT_ORIGIN]);
    }

    #[test]
    fn an_account_change_should_read_the_new_account() {
        let mut harness = harness();
        let source = FakeSource::default();
        sign_in(&mut harness, ACCOUNT);
        let first = source.subscribe();
        let _second = source.subscribe();
        let mut pane = pane(&mut harness, source.clone());

        load(&mut harness, &mut pane);
        settle(&mut harness, &mut pane, "the pane to subscribe", |_, _| {
            source.subscriptions() == 1
        });
        first
            .events
            .try_send(Ok(event(1)))
            .expect("the live subscription should accept news");
        settle(
            &mut harness,
            &mut pane,
            "the feed to reach the pane",
            |_, pane| snapshot_contents(&pane.widget) == ["News 1"],
        );

        sign_in(&mut harness, "someone-else");

        settle(
            &mut harness,
            &mut pane,
            "the replacement subscription",
            |_, _| source.subscriptions() == 2,
        );
        assert_eq!(pane.widget.account_id.as_deref(), Some("someone-else"));
        assert!(
            snapshot_contents(&pane.widget).is_empty(),
            "another account is another feed, so what was read is dropped"
        );
        settle(
            &mut harness,
            &mut pane,
            "the first subscription to be let go",
            |_, _| first.drops.load(std::sync::atomic::Ordering::SeqCst) == 1,
        );
    }

    #[test]
    fn signing_out_should_take_the_feed_with_it() {
        let mut harness = harness();
        let source = FakeSource::default();
        sign_in(&mut harness, ACCOUNT);
        let control = source.subscribe();
        let mut pane = pane(&mut harness, source.clone());

        load(&mut harness, &mut pane);
        settle(&mut harness, &mut pane, "the pane to subscribe", |_, _| {
            source.subscriptions() == 1
        });
        control
            .events
            .try_send(Ok(event(1)))
            .expect("the live subscription should accept news");
        settle(
            &mut harness,
            &mut pane,
            "the feed to reach the pane",
            |_, pane| snapshot_contents(&pane.widget) == ["News 1"],
        );

        sign_out(&mut harness);

        settle(
            &mut harness,
            &mut pane,
            "the subscription to be released",
            |_, _| control.drops.load(std::sync::atomic::Ordering::SeqCst) == 1,
        );
        assert!(!pane.widget.has_content());
        assert_eq!(pane.widget.current_scope(), None);
        assert_eq!(
            pane.status.state(harness.app()).notice(),
            Some(&Notice::message("widget.news.logged_out"))
        );
    }

    #[test]
    fn the_feed_should_reach_a_live_pane_end_to_end() {
        let mut harness = harness();
        let source = FakeSource::default();
        sign_in(&mut harness, ACCOUNT);
        let control = source.subscribe();
        let mut pane = pane(&mut harness, source.clone());

        load(&mut harness, &mut pane);
        settle(&mut harness, &mut pane, "the pane to subscribe", |_, _| {
            source.subscriptions() == 1
        });
        for timestamp in [1, 2, 3] {
            control
                .events
                .try_send(Ok(event(timestamp)))
                .expect("the live subscription should accept news");
        }

        settle(
            &mut harness,
            &mut pane,
            "the feed to reach the pane",
            |_, pane| snapshot_contents(&pane.widget) == ["News 3", "News 2", "News 1"],
        );
        settle(
            &mut harness,
            &mut pane,
            "the subscription to report itself live",
            |harness, pane| !pane.status.is_busy(harness.app()),
        );
        assert!(pane.widget.has_content());
    }

    #[test]
    fn the_body_should_draw_the_rows_of_the_feed() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();

        report_feed(&mut harness, &mut pane, feed.push(2));
        report_feed(&mut harness, &mut pane, feed.push(1));
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);

        let list = pane.widget.list.as_ref().expect("the body built the list");
        assert_eq!(
            list.count(harness.app()),
            2,
            "the list is told how many reports the feed holds"
        );
        assert_eq!(
            list.mounted_rows(harness.app()).len(),
            2,
            "a feed of two reports draws two rows"
        );

        let body = pane.widget.body_root.expect("the body was built");
        let text = harness.text_of(body);
        assert!(text.contains("News 2"), "the feed's rows are drawn: {text}");
        assert!(text.contains("News 1"), "every report is drawn: {text}");
        assert!(text.contains("Title"), "a row draws its title: {text}");
        assert!(
            text.contains("General"),
            "a row names the kind of news it is: {text}"
        );
    }

    #[test]
    fn a_feed_nothing_of_which_survives_should_be_replaced_in_the_body() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();

        report_feed(&mut harness, &mut pane, feed.push(1));
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        report_feed(&mut harness, &mut pane, FakeFeed::new().push(9));
        frame(&mut harness, &mut pane);

        let list = pane.widget.list.as_ref().expect("the body built the list");
        assert_eq!(snapshot_contents(&pane.widget), ["News 9"]);
        assert_eq!(list.count(harness.app()), 1);
        assert_eq!(list.mounted_rows(harness.app()).len(), 1);
        let text = harness.text_of(pane.widget.body_root.expect("the body was built"));
        assert!(text.contains("News 9"));
        assert!(
            !text.contains("News 1"),
            "no row of the feed the pane left is still drawn: {text}"
        );
    }

    #[test]
    fn the_body_should_say_when_the_feed_is_empty() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness, FakeSource::default());

        report_feed(&mut harness, &mut pane, FakeFeed::new().snapshot());
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);

        let body = pane.widget.body_root.expect("the body was built");
        let empty = harness
            .find_by_debug_prefix(body, "TEST_NEWS_EMPTY")
            .expect("an empty feed draws its stand-in");
        assert!(
            harness.text_of(empty).contains("No data"),
            "the stand-in says the feed holds nothing: {:?}",
            harness.text_of(empty)
        );

        harness.switch_language(LanguageCode::ZhCn);
        frame(&mut harness, &mut pane);
        assert!(
            harness.tree().get(empty).is_err(),
            "the stand-in built in the old language is taken down, not left on screen"
        );
        let rebuilt = harness
            .find_by_debug_prefix(body, "TEST_NEWS_EMPTY")
            .expect("the stand-in is still there");
        assert!(
            harness.text_of(rebuilt).contains("暂无数据"),
            "the stand-in is rebuilt in the language showing now: {:?}",
            harness.text_of(rebuilt)
        );

        let mut feed = FakeFeed::new();
        report_feed(&mut harness, &mut pane, feed.push(1));
        frame(&mut harness, &mut pane);
        assert!(
            harness.tree().get(rebuilt).is_err(),
            "the stand-in leaves when the feed stops being empty"
        );
    }

    #[test]
    fn the_rows_should_follow_the_language() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();

        report_feed(&mut harness, &mut pane, feed.push(1));
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        let body = pane.widget.body_root.expect("the body was built");
        assert!(harness.text_of(body).contains("General"));

        harness.switch_language(LanguageCode::ZhCn);
        frame(&mut harness, &mut pane);

        let text = harness.text_of(body);
        assert!(
            text.contains("综合"),
            "the row names the kind of news in the language showing now: {text}"
        );
        assert!(
            !text.contains("General"),
            "the old language is gone from the row: {text}"
        );
    }

    #[test]
    fn moving_the_display_zone_should_re_read_the_row_time() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();

        report_feed(&mut harness, &mut pane, feed.push(1_700_000_000_000));
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        let body = pane.widget.body_root.expect("the body was built");
        assert!(
            harness.text_of(body).contains("2023-11-14 22:13:20"),
            "the row reads the instant in the default zone: {:?}",
            harness.text_of(body)
        );

        let mut config = current_config(&harness);
        config.data_time_zone = DataTimeZone::from_hours(8).expect("UTC+8 is offered");
        let before = harness.app().frame_requests();
        assert!(write_config(&mut harness, config));
        assert!(
            harness.app().frame_requests() > before,
            "the row's text read the settings signal while rendering, so a settings write \
             schedules the frame that redraws it"
        );
        frame(&mut harness, &mut pane);

        assert!(
            harness.text_of(body).contains("2023-11-15 06:13:20"),
            "the row reads the instant in the zone showing now: {:?}",
            harness.text_of(body)
        );
    }

    #[test]
    fn the_header_should_build_a_sound_control() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let (mut pane, sound) = audible_pane(&mut harness, FakeSource::default());
        let header = harness.placeholder_parent();

        harness
            .with_cx(|cx| pane.widget.header(HeaderGroup::Right, header, cx))
            .expect("the header control builds");

        let button = pane
            .widget
            .sound_button
            .as_ref()
            .expect("a pane with audio offers its sound");
        assert!(
            button
                .node()
                .is_some_and(|node| harness.tree().get(node).is_ok()),
            "the control is in the header the pane built"
        );
        assert!(
            !*harness.app().read(&button.interactive().disabled_signal()),
            "a pane with audio offers the control"
        );

        sound.set_available(false);
        sync(&mut harness, &mut pane);
        let disabled = pane
            .widget
            .sound_button
            .as_ref()
            .expect("the control stays in the header");
        assert!(
            *harness
                .app()
                .read(&disabled.interactive().disabled_signal()),
            "a host without audio draws the control disabled"
        );
        assert!(!pane.widget.sound.is_available());
    }

    #[test]
    fn taking_the_sound_control_should_take_the_sound_the_other_way() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let published = Rc::new(Cell::new(0_usize));
        let config_change = {
            let published = Rc::clone(&published);
            WidgetConfigChange::new(move |_| published.set(published.get() + 1))
        };
        let sound = RecordedSound::with_device();
        let mut pane = pane_with(
            &mut harness,
            FakeSource::default(),
            Box::new(sound),
            config_change,
        );
        let header = harness.placeholder_parent();
        harness
            .with_cx(|cx| pane.widget.header(HeaderGroup::Right, header, cx))
            .expect("the header control builds");
        let (node, action) = {
            let button = pane.widget.sound_button.as_ref().expect("built");
            (
                button.node().expect("the control is in the tree"),
                button.action_identity().expect("the control acts"),
            )
        };
        let window = harness.window();

        assert!(
            harness
                .app_mut()
                .dispatch_action(window, node, action, ActionSource::Programmatic),
            "the control is registered"
        );
        sync(&mut harness, &mut pane);

        assert!(
            !pane.widget.config().expect("persisted").sound,
            "the control is what writes the option"
        );
        assert_eq!(published.get(), 1, "the change is reported for write-back");
        let rebuilt = pane
            .widget
            .sound_button
            .as_ref()
            .and_then(Button::node)
            .expect("the control is rebuilt for the state it now shows");
        assert_ne!(
            rebuilt, node,
            "a kit button's icon is chosen when it is built, so the pane rebuilds it"
        );
    }

    #[test]
    fn a_host_without_audio_should_not_panic_and_offer_no_sound() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();
        let header = harness.placeholder_parent();

        report_feed(&mut harness, &mut pane, feed.push(1));
        report_feed(&mut harness, &mut pane, feed.push(2));
        assert!(!pane.widget.sound.is_available());

        harness
            .with_cx(|cx| pane.widget.header(HeaderGroup::Right, header, cx))
            .expect("the header still builds with no audio");
        let button = pane
            .widget
            .sound_button
            .as_ref()
            .expect("a pane with no audio still draws its silent control");
        assert!(
            *harness.app().read(&button.interactive().disabled_signal()),
            "the control cannot be taken on a host with no audio"
        );
        assert_eq!(snapshot_contents(&pane.widget), ["News 2", "News 1"]);
    }

    #[test]
    fn opening_a_pane_on_a_feed_should_not_make_a_sound() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let (mut pane, sound) = audible_pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();

        report_feed(&mut harness, &mut pane, feed.push(3));
        report_status(&mut harness, &mut pane, NewsStatus::Ready);

        assert_eq!(
            sound.plays(),
            0,
            "the feed a pane opens on is what the feed is, not what arrived"
        );
    }

    #[test]
    fn the_first_report_a_live_feed_publishes_should_make_a_sound() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let (mut pane, sound) = audible_pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();

        report_feed(&mut harness, &mut pane, feed.snapshot());
        report_status(&mut harness, &mut pane, NewsStatus::Ready);
        assert_eq!(sound.plays(), 0, "an empty feed has nothing to announce");

        report_feed(&mut harness, &mut pane, feed.push(1));

        assert_eq!(
            sound.plays(),
            1,
            "the report that filled a live feed is news a reader has not seen"
        );
    }

    #[test]
    fn a_report_should_be_announced_with_the_notification_sound() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let (mut pane, sound) = audible_pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();

        report_feed(&mut harness, &mut pane, feed.push(1));
        report_status(&mut harness, &mut pane, NewsStatus::Ready);
        report_feed(&mut harness, &mut pane, feed.push(2));

        assert_eq!(
            sound.plays(),
            1,
            "the report a reader has not seen is heard"
        );
    }

    #[test]
    fn a_feed_that_did_not_move_should_not_be_announced() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let (mut pane, sound) = audible_pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();

        let grown = feed.push(1);
        report_feed(&mut harness, &mut pane, grown.clone());
        report_feed(&mut harness, &mut pane, grown);

        let added = feed.push(2);
        report_feed(&mut harness, &mut pane, added.clone());
        report_feed(&mut harness, &mut pane, added);

        assert_eq!(
            sound.plays(),
            1,
            "the publication that moved the feed is the one a reader is told about"
        );
    }

    #[test]
    fn a_run_of_reports_should_be_announced_once() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let (mut pane, sound) = audible_pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();

        report_feed(&mut harness, &mut pane, feed.push(1));
        for timestamp in 2..=6 {
            report_feed(&mut harness, &mut pane, feed.push(timestamp));
        }

        assert_eq!(
            sound.plays(),
            1,
            "five reports inside one window are one sound"
        );
    }

    #[test]
    fn the_sound_should_be_heard_again_a_window_later() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let (mut pane, sound) = audible_pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();

        report_feed(&mut harness, &mut pane, feed.push(1));
        report_feed(&mut harness, &mut pane, feed.push(2));
        pane.widget.throttle = Throttle::heard_at(MINIMUM_INTERVAL);
        report_feed(&mut harness, &mut pane, feed.push(3));

        assert_eq!(
            sound.plays(),
            2,
            "a report a window after the last sound is one a reader hears"
        );
    }

    #[test]
    fn a_silent_pane_should_not_be_heard_and_should_hear_again_when_it_is_turned_back_on() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let (mut pane, sound) = audible_pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();

        report_feed(&mut harness, &mut pane, feed.push(1));
        harness.with_cx(|cx| pane.widget.toggle_sound(cx));
        assert!(
            !pane.widget.config().expect("persisted").sound,
            "the control is what writes the option"
        );
        report_feed(&mut harness, &mut pane, feed.push(2));
        assert_eq!(
            sound.plays(),
            0,
            "a reader who turned the sound off hears nothing"
        );

        harness.with_cx(|cx| pane.widget.toggle_sound(cx));
        report_feed(&mut harness, &mut pane, feed.push(3));

        assert_eq!(
            sound.plays(),
            1,
            "the report after the sound is turned back on is heard"
        );
    }

    #[test]
    fn releasing_the_body_should_take_it_and_its_list_with_it() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();

        report_feed(&mut harness, &mut pane, feed.push(1));
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        let body = pane.widget.body_root.expect("the body was built");
        let list_node = pane
            .widget
            .list
            .as_ref()
            .and_then(List::outer_node)
            .expect("the list built its container");
        let before = harness.node_count();

        harness.with_cx(|cx| pane.widget.release(cx));

        assert!(harness.tree().get(body).is_err(), "the body is gone");
        assert!(
            harness.tree().get(list_node).is_err(),
            "the list's nodes went with the body"
        );
        assert!(
            harness.node_count() < before,
            "the pane took its own subtree down"
        );
        assert!(pane.widget.body_root.is_none());
        assert!(pane.widget.list.is_none());
    }

    #[test]
    fn a_host_without_the_service_should_report_a_typed_failure_and_release_cleanly() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let before = harness.node_count();
        let widget = harness.mount::<NewsWidget>(NewsConfig::default());
        assert!(harness.node_count() > before);
        harness.drain();
        harness.drain();

        let failure = widget
            .status()
            .state(harness.app())
            .failure()
            .expect("a host without the service is a failure")
            .text(&harness.services().translate);
        assert_eq!(
            failure.as_str(),
            "The Kytrix service connector is closed",
            "the failure is the service's typed error, not a sentence this pane writes"
        );
        assert!(
            harness.text_of(widget.surface()).contains(failure.as_str()),
            "the framework draws the failure it was reported"
        );

        let released = widget.release(harness.app_mut());
        assert!(released > 0, "the scope owned the state cell");
        assert_eq!(
            harness.node_count(),
            before,
            "no node of the presentation is left behind"
        );
        assert!(widget.is_released());
        assert!(
            harness.router().is_empty(),
            "the frame route is unregistered"
        );
    }

    #[test]
    fn a_settings_write_should_wake_a_pane_that_has_no_body() {
        let mut harness = harness();
        let widget = harness.mount::<NewsWidget>(NewsConfig::default());
        harness.drain();
        harness.drain();
        assert_eq!(
            widget.status().state(harness.app()).notice(),
            Some(&Notice::message("widget.news.logged_out")),
            "a pane nobody signed in for waits on its stand-in"
        );
        let before = harness.app().frame_requests();

        sign_in(&mut harness, ACCOUNT);

        assert!(
            harness.app().frame_requests() > before,
            "the surface root read the settings signal while rendering, so signing in \
             schedules the frame that runs the pane's sync"
        );
        harness.drain();
        assert_ne!(
            widget.status().state(harness.app()).notice(),
            Some(&Notice::message("widget.news.logged_out")),
            "the pane left the signed-out stand-in on the frame the settings write scheduled"
        );
        assert!(
            widget.status().state(harness.app()).is_failed(),
            "a host without the Kytrix service is a typed failure, not a signed-out pane"
        );
        let text = harness.text_of(widget.surface());
        assert!(
            !text.contains("register") && !text.contains("log in"),
            "the signed-out copy is gone from the pane: {text:?}"
        );
    }

    #[test]
    fn the_logged_out_stand_in_should_be_drawn_as_rich_text() {
        let mut harness = harness();
        let widget = harness.mount::<NewsWidget>(NewsConfig::default());
        harness.drain();
        harness.drain();

        let surface = widget.surface();
        assert!(
            harness.find_by_debug_prefix(surface, "MARKDOWN").is_some(),
            "the notice is authored as markup, so it goes through the markdown component"
        );
        let text = harness.text_of(surface);
        assert!(
            !text.contains("<p") && !text.contains("</p>"),
            "the notice's source markup is not drawn as text: {text:?}"
        );
        assert!(
            text.contains("register") || text.contains("log in"),
            "the reader gets the sentence the catalogue authors: {text:?}"
        );

        harness.switch_language(LanguageCode::ZhCn);
        harness.drain();
        harness.drain();

        let text = harness.text_of(surface);
        assert!(
            text.contains("登陆"),
            "the stand-in follows the language: {text:?}"
        );
        assert!(
            !text.contains("<p") && !text.contains("</p>"),
            "the markup is still not drawn: {text:?}"
        );
    }

    #[test]
    fn the_rows_should_be_rebuilt_when_the_language_moves() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let (mut pane, _sound) = audible_pane(&mut harness, FakeSource::default());
        let header = harness.placeholder_parent();
        harness
            .with_cx(|cx| pane.widget.header(HeaderGroup::Right, header, cx))
            .expect("the header control builds");
        let sound_node = pane
            .widget
            .sound_button
            .as_ref()
            .and_then(Button::node)
            .expect("the control is drawn");

        report_feed(&mut harness, &mut pane, FakeFeed::new().push(1));
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        let body = pane.widget.body_root.expect("the body was built");
        let count = pane
            .widget
            .list
            .as_ref()
            .expect("the body built the list")
            .count(harness.app());
        let old_row = harness
            .find_by_debug_prefix(body, "TEST_NEWS_ROW")
            .expect("the feed draws a row");
        assert!(harness.text_of(old_row).contains("General"));

        harness.switch_language(LanguageCode::ZhCn);
        frame(&mut harness, &mut pane);

        assert!(
            harness.tree().get(old_row).is_err(),
            "the row built in the old language is gone"
        );
        let new_row = harness
            .find_by_debug_prefix(body, "TEST_NEWS_ROW")
            .expect("the row is rebuilt");
        let text = harness.text_of(new_row);
        assert!(
            text.contains("综合"),
            "the badge names the kind of news in the language showing now: {text:?}"
        );
        assert!(
            !text.contains("General"),
            "the old language is gone from the row: {text:?}"
        );
        assert_eq!(
            pane.widget
                .list
                .as_ref()
                .expect("the body kept its list")
                .count(harness.app()),
            count,
            "rebuilding the rows does not change the feed the list holds"
        );

        let rebuilt_sound = pane
            .widget
            .sound_button
            .as_ref()
            .and_then(Button::node)
            .expect("the header control is rebuilt");
        assert_ne!(
            rebuilt_sound, sound_node,
            "the control's label is chosen when it is built, so the new language rebuilds it"
        );
        assert!(
            harness.tree().get(rebuilt_sound).is_ok(),
            "the control is drawn in the header the pane built"
        );
    }

    #[test]
    fn the_header_control_should_survive_the_pane_rebuilding_its_row() {
        let mut harness = harness();
        let (mut pane, _sound) = audible_pane(&mut harness, FakeSource::default());
        let first = harness.placeholder_parent();
        harness
            .with_cx(|cx| pane.widget.header(HeaderGroup::Right, first, cx))
            .expect("the control builds into the first row");
        let first_control = pane
            .widget
            .sound_button
            .as_ref()
            .and_then(Button::node)
            .expect("the control is drawn");

        let window = harness.window();
        harness
            .with_cx(|cx| cx.app_mut().remove_node(window, first))
            .expect("the pane removes its old row");
        assert!(
            harness.tree().get(first_control).is_err(),
            "the old control went with the row it was built into"
        );

        let second = harness.placeholder_parent();
        harness
            .with_cx(|cx| pane.widget.header(HeaderGroup::Right, second, cx))
            .expect("the control is built again rather than removed twice");
        let second_control = pane
            .widget
            .sound_button
            .as_ref()
            .and_then(Button::node)
            .expect("the control is drawn in the new row");
        assert!(harness.tree().get(second_control).is_ok());
        assert!(
            harness
                .find_by_debug_prefix(second, "news.sound.")
                .is_some(),
            "the control sits in the row the pane built now"
        );

        let window = harness.window();
        harness
            .with_cx(|cx| cx.app_mut().remove_node(window, second))
            .expect("the pane removes its row again");
        let third = harness.placeholder_parent();
        harness
            .with_cx(|cx| pane.widget.header(HeaderGroup::Right, third, cx))
            .expect("the control is built a third time");
        let third_control = pane
            .widget
            .sound_button
            .as_ref()
            .and_then(Button::node)
            .expect("the sound control is still drawn");
        assert!(
            harness.tree().get(third_control).is_ok(),
            "the sound control is still in the header"
        );
        assert!(
            harness.find_by_debug_prefix(third, "news.sound.").is_some(),
            "the rebuilt control is the one the pane's row holds"
        );
    }

    #[test]
    fn the_body_should_grow_when_the_feed_publishes_more_reports() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();

        report_feed(&mut harness, &mut pane, feed.push(1));
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        let body = pane.widget.body_root.expect("the body was built");
        {
            let list = pane.widget.list.as_ref().expect("the body built the list");
            assert_eq!(list.count(harness.app()), 1);
            assert!(
                list.offset(harness.app())
                    .is_some_and(|offset| offset.abs() < 0.5),
                "the reader is on the newest report the pane opened with"
            );
        }

        feed.push(2);
        let grown = feed.push(3);
        report_feed(&mut harness, &mut pane, grown);
        frame(&mut harness, &mut pane);

        let list = pane.widget.list.as_ref().expect("the body kept its list");
        assert_eq!(
            list.count(harness.app()),
            3,
            "the two reports the list had not been shown are pushed into it"
        );
        assert_eq!(
            list.mounted_rows(harness.app()).len(),
            3,
            "every report the list holds is drawn"
        );
        assert!(
            list.offset(harness.app())
                .is_some_and(|offset| offset.abs() < 0.5),
            "a reader on the newest report keeps their place when one arrives"
        );
        let text = harness.text_of(body);
        for expected in ["News 3", "News 2", "News 1"] {
            assert!(text.contains(expected), "{expected} is drawn: {text:?}");
        }
    }

    #[test]
    fn switching_the_appearance_should_request_the_redrawing_frame() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let mut pane = pane(&mut harness, FakeSource::default());

        report_feed(&mut harness, &mut pane, FakeFeed::new().push(1));
        build_body(&mut harness, &mut pane);
        frame(&mut harness, &mut pane);
        let before = harness.app().frame_requests();
        let theme = harness.services().theme.clone();

        assert!(theme.set_appearance(harness.app_mut(), Appearance::Dark));

        assert!(
            harness.app().frame_requests() > before,
            "the rows read their colours through the theme while rendering, so the \
             appearance switch schedules the frame that redraws them"
        );
    }
}

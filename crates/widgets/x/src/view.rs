use std::{
    cell::{Cell, RefCell},
    ops::Range,
    rc::Rc,
    sync::Arc,
};

use ahash::AHashMap as HashMap;
use app_config::{
    network::Config as NetworkConfig,
    settings::Config as SettingsConfig,
    widget::{WidgetConfig, WidgetConfigSchemaVersion},
};
use bytes::Bytes;
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_service_provider::service::{
    engine::KytrixServiceError,
    x::{XApiError, XMutationOutcome, XMutationUnknown, XSelectionStatus},
};
use network::transport::hub::{TransportHub, TransportPolicy};
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use time::OffsetDateTime;
use ui_core::{
    a11y::{A11yOverride, A11yPatch},
    error::EngineError,
    event::{Action, Event, InputEvent, PointerButton},
    node::{
        builder::NodeBuilder,
        dependency::ReadCtx,
        id::NodeId,
        image::{ImageFit, ImageSource},
        signal::Signal,
        tree::NodeTree,
    },
    paint::{color::Color, style::StylePatch},
    text::{TextAlign, TextOverflow, TextRun, TextStyle},
};
use ui_kit::{
    kit::{
        button::Button,
        empty_state::EmptyState,
        icon::{IconKind, RasterTier, raster},
        list::{List, ListOptions, SelectionMode},
        size::{UnifiedSizable, UnifiedSize},
    },
    style_builder::StyleBuilder,
    theme::{appearance::Appearance, handle::ThemeHandle, role::Role},
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
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

use super::{
    dialog::{DialogRequest, DialogView, SearchAnswer, TweetDialog},
    media::{MediaDownloader, SharedTransport},
    model::{
        MAX_TWEETS, MediaState, MediaStates, SelectionData, TweetAge, TweetItem, TweetMediaItem,
        TweetReferenceItem, TweetSnapshot, absolute_time, retained_hints, tweet_age, unshown,
    },
    service::{
        TweetProtocol, TweetRequest, TweetResponse, TweetScope, TweetService, TweetStatus,
        TweetStreamError, TweetStreamMessage, TweetUpdate,
    },
};

const ROW_ESTIMATE: f32 = 240.0;

const AVATAR_SIZE: f32 = 40.0;

const QUOTE_AVATAR_SIZE: f32 = 28.0;

const GALLERY_WIDE_WIDTH: f32 = 510.0;
const GALLERY_HEIGHT: f32 = 320.0;

const BODY_SIZE: UnifiedSize = UnifiedSize::Medium;

const META_SIZE: UnifiedSize = UnifiedSize::Small;

const FEED_CAPACITY: usize = MAX_TWEETS;

const MAX_SEARCH_QUERY_BYTES: usize = 100;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct XConfig;

impl WidgetConfig for XConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum XFailure {
    Stream(TweetStreamError),
    Rpc(RpcCallError<XApiError>),
}

impl TranslatedError for XFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Stream(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SelectionOperation {
    Loading,
    Refreshing,
    Adding,
    Deleting,
}

impl SelectionOperation {
    pub(super) const fn is_read(self) -> bool {
        matches!(self, Self::Loading | Self::Refreshing)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SelectionPending {
    operation: SelectionOperation,
    target: Option<SmolStr>,
    sent: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SelectionNotice {
    Added,
    Problem(SelectionFailure),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SelectionFailure {
    Rejected(XApiError),
    Unknown(XMutationUnknown),
    Rpc(RpcCallError<XApiError>),
}

impl TranslatedError for SelectionFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Rejected(error) => error.translated_message(translate),
            Self::Unknown(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingCommand {
    Start,
    Stop,
    FetchSelections,
    WatchSelections,
    Search,
    Add,
    Delete,
    RetryMedia,
}

type ImageMap = HashMap<SmolStr, ImageSource>;

type RowImages = Rc<RefCell<ImageMap>>;

#[derive(Clone)]
struct RowIcons {
    repost: ImageSource,
    verified: ImageSource,
    warning: ImageSource,
    play: ImageSource,
    retry: ImageSource,
}

#[derive(Clone, Debug, PartialEq)]
struct MediaPlan {
    url: SmolStr,
    kind: MediaKind,
    ratio: f32,
    poster_only: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MediaKind {
    Image,
    Video,
    Gif,
}

#[derive(Clone, Debug, PartialEq)]
struct RowPlan {
    tweet_id: SmolStr,
    row_id: SmolStr,
    quote_of: bool,
    url: SmolStr,
    reposted_by: Option<SmolStr>,
    author_name: SmolStr,
    author_handle: SmolStr,
    avatar_url: SmolStr,
    verified: bool,
    published_at: i64,
    text: SmolStr,
    has_text: bool,
    links: Rc<[(Range<usize>, SmolStr)]>,
    media: Arc<[MediaPlan]>,
    quote: Option<Box<RowPlan>>,
}

#[derive(Clone, Default)]
struct RowHits {
    rows: Rc<RefCell<HashMap<NodeId, SmolStr>>>,
    links: Rc<RefCell<HashMap<NodeId, Rc<[(Range<usize>, SmolStr)]>>>>,
    failed: Rc<RefCell<HashMap<NodeId, SmolStr>>>,
}

impl RowHits {
    fn clear(&self) {
        self.rows.borrow_mut().clear();
        self.links.borrow_mut().clear();
        self.failed.borrow_mut().clear();
    }
}

#[derive(Clone)]
struct RowContext {
    plans: Rc<RefCell<Arc<[RowPlan]>>>,
    plan_signal: Signal<Arc<[RowPlan]>>,
    language: Signal<LanguageCode>,
    config: Signal<SettingsConfig>,
    theme: ThemeHandle,
    appearance: Rc<Cell<Appearance>>,
    translate: Translate,
    images: RowImages,
    media: Rc<RefCell<MediaStates>>,
    icons: RowIcons,
    hits: RowHits,
    retry: Action,
}

impl RowContext {
    fn media(&self) -> MediaStates {
        Arc::clone(&self.media.borrow())
    }
}

pub struct XWidget {
    binding: Option<WidgetBinding<TweetProtocol>>,
    pending: Pending<PendingCommand>,
    status: WidgetStatus,
    recovery: WidgetAction,
    open_url: Rc<dyn Fn(&str)>,
    account_id: Option<SmolStr>,
    feed: Option<TweetSnapshot>,
    media: MediaStates,
    media_mirror: Rc<RefCell<MediaStates>>,
    revision: u64,
    ready: bool,
    plans: Rc<RefCell<Arc<[RowPlan]>>>,
    plan_signal: Signal<Arc<[RowPlan]>>,
    built_media: Option<MediaStates>,
    appearance: Rc<Cell<Appearance>>,
    images: RowImages,
    media_retry_action: Option<Action>,
    media_retry: Signal<Option<SmolStr>>,
    icons: Option<RowIcons>,
    hits: RowHits,
    list: Option<List>,
    body_root: Option<NodeId>,
    empty: Option<NodeId>,
    empty_language: Option<LanguageCode>,
    selections: Option<SelectionData>,
    searching: Option<SmolStr>,
    selection_pending: Option<SelectionPending>,
    notice: Option<SelectionNotice>,
    hints: Arc<HashMap<SmolStr, SmolStr>>,
    dialog: Option<TweetDialog>,
    manage_button: Option<Button>,
    manage_parent: Option<NodeId>,
    manage_built: Option<(LanguageCode, bool)>,
    open_dialog_requests: Signal<u64>,
    seen_open_requests: u64,
}

impl WidgetView for XWidget {
    type Config = XConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.x");
    const DISPLAY_NAME: &'static str = "widget.names.x";
    const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.x");
    const ICON: Option<&'static str> = Some("widgets/x.png");

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
        let config: NetworkConfig = cx.config_ref(|config| config.network_config());
        let transport = SharedTransport::new(
            Arc::new(TransportHub::new(TransportPolicy::default())),
            config,
        );
        let binding = source.map(|source| {
            WidgetBinding::spawn(
                cx,
                TweetService::new(source, MediaDownloader::new(transport)),
            )
        });

        Ok(Self::assemble(binding, status, recovery, cx))
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        let retry = self.status.is_failed(cx.app());
        self.account_id = cx.account();
        if let Err(error) = self.start(retry, cx) {
            log::error!("the x pane could not start: {error}");
        }
    }

    fn has_content(&self) -> bool {
        self.feed.is_some()
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let root = NodeBuilder::boxed()
            .id("TEST_TWEET_BODY")
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
        self.icons = Some(register_row_icons(cx)?);
        self.list = Some(self.make_list(cx, root)?);
        self.sync_empty_state(cx)?;
        cx.request_frame()?;

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
                    let _ = self.apply_reliable(key, message, cx);
                }
                Inbound::Latest { key, message } => {
                    let _ = self.apply_latest(key, message, cx);
                }
                Inbound::Rpc { request_id, result } => {
                    self.apply_rpc(request_id, result, cx);
                }
            }
        }

        self.register_media(cx);

        let requests = *cx.read(&self.open_dialog_requests);
        if requests != self.seen_open_requests {
            self.seen_open_requests = requests;
            self.open_dialog(cx)?;
        }
        self.sync_dialog(cx)?;
        self.sync_media_retry(cx);

        self.sync_appearance(cx)?;

        if let Some(list) = &self.list {
            list.sync(cx.app_mut())?;
        }
        self.sync_empty_state(cx)?;
        self.sync_manage_button(cx)?;

        if self.feed.is_some() && self.body_root.is_none() {
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
        if group != HeaderGroup::Left {
            return Ok(());
        }
        self.release_manage_button(cx, true)?;
        self.build_manage_button(parent, cx)
    }

    fn dispatch(&mut self, event: &Event, cx: &mut WidgetCx<'_>) -> bool {
        let Event::Input(InputEvent::PointerUp {
            position, button, ..
        }) = event
        else {
            return false;
        };
        if *button != PointerButton::Primary {
            return false;
        }
        let Some(hit) = cx.tree().hit_test(*position) else {
            return false;
        };

        let links = self.hits.links.borrow().get(&hit.node).cloned();
        if let Some(links) = links {
            let byte = cx
                .window_mut()
                .text_byte_index_at(hit.node, hit.local_point)
                .ok();
            if let Some(byte) = byte
                && let Some((_, url)) = links.iter().find(|(range, _)| range.contains(&byte))
            {
                (self.open_url)(url);
                return true;
            }
        }

        let failed_url = {
            let failed = self.hits.failed.borrow();
            let mut node = Some(hit.node);
            let mut found = None;
            while let Some(current) = node {
                if let Some(url) = failed.get(&current) {
                    found = Some(url.clone());
                    break;
                }
                node = cx.tree().get(current).ok().and_then(|node| node.parent());
            }
            found
        };
        if let Some(url) = failed_url {
            let _ = cx.write(&self.media_retry, Some(url));
            let _ = cx.request_frame();
            return true;
        }

        let row_url = {
            let rows = self.hits.rows.borrow();
            let mut node = Some(hit.node);
            let mut found = None;
            while let Some(current) = node {
                if let Some(url) = rows.get(&current) {
                    found = Some(url.clone());
                    break;
                }
                node = cx.tree().get(current).ok().and_then(|node| node.parent());
            }
            found
        };
        let Some(url) = row_url else {
            return false;
        };
        (self.open_url)(&url);
        true
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        self.binding = None;
        self.pending.clear();
        let _ = self.release_manage_button(cx, false);
        if let Some(action) = self.media_retry_action.take() {
            let _ = cx.app_mut().off_action(&action);
        }
        if let Some(mut dialog) = self.dialog.take() {
            dialog.release(cx);
        }
        if let Some(root) = self.body_root.take() {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, root);
        }
        if let Some(list) = self.list.take() {
            let _ = list.release(cx.app_mut());
        }
        self.feed = None;
        self.hits.clear();
        self.images.borrow_mut().clear();
        self.icons = None;
        self.empty = None;
        self.empty_language = None;
    }
}

impl XWidget {
    fn assemble(
        binding: Option<WidgetBinding<TweetProtocol>>,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let open_url = Rc::clone(&cx.services().open_url);
        let plans: Arc<[RowPlan]> = Arc::from([]);
        let plans = Rc::new(RefCell::new(plans));
        let plan_signal = cx.signal(Arc::clone(&plans.borrow()));
        let open_dialog_requests = cx.signal(0_u64);
        let account_id = cx.account();
        let appearance = Rc::new(Cell::new(cx.theme().current(cx.app())));

        let hits = RowHits::default();
        let media_retry = cx.signal(None::<SmolStr>);
        let media_retry_action = Action::Custom(cx.unique_key("x.feed.retry_media"));
        let retry_hits = hits.clone();
        let retry_request = media_retry;
        let window = cx.window();
        cx.app_mut()
            .on_action(media_retry_action.clone(), move |ctx| {
                let url = retry_hits.failed.borrow().get(&ctx.node()).cloned();
                let Some(url) = url else {
                    return;
                };
                ctx.write(&retry_request, Some(url));
                let _ = ctx.app_mut().request_repaint(window);
            });

        Self {
            binding,
            pending: Pending::new(),
            status,
            recovery,
            open_url,
            account_id,
            feed: None,
            media: Arc::new(HashMap::new()),
            media_mirror: Rc::new(RefCell::new(Arc::new(HashMap::new()))),
            revision: 0,
            ready: false,
            plans,
            plan_signal,
            built_media: None,
            appearance,
            images: Rc::new(RefCell::new(HashMap::new())),
            media_retry_action: Some(media_retry_action),
            media_retry,
            icons: None,
            hits,
            list: None,
            body_root: None,
            empty: None,
            empty_language: None,
            selections: None,
            searching: None,
            selection_pending: None,
            notice: None,
            hints: Arc::new(HashMap::new()),
            dialog: None,
            manage_button: None,
            manage_parent: None,
            manage_built: None,
            open_dialog_requests,
            seen_open_requests: 0,
        }
    }

    #[cfg(test)]
    fn new_with_source<S, T>(
        source: S,
        downloader: MediaDownloader<T>,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self
    where
        S: super::service::TweetSource,
        T: super::media::MediaTransport,
    {
        let binding = WidgetBinding::spawn(cx, TweetService::new(source, downloader));
        Self::assemble(Some(binding), status, recovery, cx)
    }

    fn current_scope(&self) -> Option<TweetScope> {
        TweetScope::new(self.account_id.as_ref())
    }

    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(scope) = self.current_scope() else {
            self.clear(cx)?;
            self.report_no_content_notice(cx);
            self.hand_over(TweetRequest::Stop, PendingCommand::Stop, cx)?;
            cx.request_frame()?;
            return Ok(());
        };
        self.ready = false;
        self.revision = 0;
        if !retry {
            self.clear(cx)?;
        }
        self.selection_pending = None;
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        let request = if retry {
            TweetRequest::Retry(scope)
        } else {
            TweetRequest::Load(scope)
        };
        let Some(binding) = self.binding.as_mut() else {
            self.fail(TweetStreamError::Service(KytrixServiceError::Closed), cx);
            return Ok(());
        };
        let request_id = binding.call(cx, request);
        self.pending.track(request_id, PendingCommand::Start);
        cx.request_frame()?;

        Ok(())
    }

    fn clear(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.feed = None;
        self.media = Arc::new(HashMap::new());
        *self.media_mirror.borrow_mut() = Arc::clone(&self.media);
        self.built_media = None;
        let _ = cx.write(&self.media_retry, None);
        self.revision = 0;
        self.ready = false;
        self.replace_plans(Arc::from([]), cx);
        self.hits.clear();
        if let Some(list) = &self.list {
            list.set_items(cx.app_mut(), 0)?;
        }
        self.selections = None;
        self.searching = None;
        self.selection_pending = None;
        self.notice = None;
        self.hints = Arc::new(HashMap::new());
        self.sync_empty_state(cx)?;
        cx.request_frame()?;

        Ok(())
    }

    fn report_no_content_notice(&self, cx: &mut WidgetCx<'_>) {
        self.status.report(
            cx.app_mut(),
            WidgetState::waiting(Notice::message("widget.x.logged_out")),
        );
    }

    fn report_ready(&self, cx: &mut WidgetCx<'_>) {
        if self.feed.is_some() {
            self.status.report(cx.app_mut(), WidgetState::Ready);
        } else if self.current_scope().is_some() {
            self.status.report(cx.app_mut(), WidgetState::Loading);
        } else {
            self.report_no_content_notice(cx);
        }
    }

    fn fail(&self, error: TweetStreamError, cx: &mut WidgetCx<'_>) {
        let failure = Arc::new(XFailure::Stream(error));
        self.status.report(
            cx.app_mut(),
            WidgetState::failed(failure, self.recovery.clone()),
        );
    }

    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: widget::binding::WidgetRpcResult<TweetProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.status.is_failed(cx.app()) {
            return;
        }
        match self.pending.take(request_id) {
            Some(PendingCommand::Start) => self.apply_load_result(result, cx),
            Some(PendingCommand::FetchSelections) => self.apply_selections_result(result, cx),
            Some(PendingCommand::Search) => self.apply_search_result(result, cx),
            Some(PendingCommand::Add) => self.apply_add_result(result, cx),
            Some(PendingCommand::Delete) => self.apply_delete_result(result, cx),
            Some(
                PendingCommand::Stop | PendingCommand::WatchSelections | PendingCommand::RetryMedia,
            ) => {}
            None => {
                log::debug!("the x pane ignored reply {request_id}: no command is waiting for it")
            }
        }
    }

    fn apply_load_result(
        &mut self,
        result: widget::binding::WidgetRpcResult<TweetProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.status.is_failed(cx.app()) {
            return;
        }
        if let Err(error) = result {
            self.ready = false;
            self.selection_pending = None;
            let failure = Arc::new(XFailure::Rpc(error));
            self.status.report(
                cx.app_mut(),
                WidgetState::failed(failure, self.recovery.clone()),
            );
            let _ = cx.request_frame();
        }
    }

    fn apply_latest(
        &mut self,
        key: TweetScope,
        message: Arc<TweetStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        if self.current_scope().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
            return false;
        }
        let TweetStreamMessage::Update(update) = message.as_ref() else {
            return false;
        };
        if update.revision() <= self.revision {
            return false;
        }
        if let Err(error) = self.accept(update, cx) {
            log::error!("the x pane could not take the timeline it was published: {error}");
            return false;
        }
        if self.ready {
            self.report_ready(cx);
        }
        true
    }

    fn apply_reliable(
        &mut self,
        key: TweetScope,
        message: Arc<TweetStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        if self.current_scope().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
            return false;
        }
        let TweetStreamMessage::Status(status) = message.as_ref() else {
            return false;
        };

        match status {
            TweetStatus::Connecting => {
                self.ready = false;
                self.status.report(cx.app_mut(), WidgetState::Connecting);
            }
            TweetStatus::Ready => {
                self.ready = true;
                self.report_ready(cx);
                if self.selection_pending.is_some() {
                    let _ = self.send_pending_selections(cx);
                }
                if self.dialog.is_some() {
                    if let Err(error) = self.watch_selections(true, cx) {
                        log::error!("the x pane could not watch the accounts: {error}");
                    }
                    if self.selection_pending.is_none() {
                        self.load_selections(cx);
                    }
                }
            }
            TweetStatus::Failed(error) => {
                self.ready = false;
                if self
                    .selection_pending
                    .as_ref()
                    .is_some_and(|pending| !pending.sent)
                {
                    self.selection_pending = None;
                }
                self.fail(error.clone(), cx);
            }
        }
        let _ = cx.request_frame();

        false
    }

    fn accept(&mut self, update: &TweetUpdate, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let unshown = unshown(
            self.feed.as_ref().map_or(&[], TweetSnapshot::items),
            update.feed().items(),
        );
        let media_changed = self
            .built_media
            .as_ref()
            .is_none_or(|built| built != update.media());
        self.feed = Some(update.feed().clone());
        self.media = update.media().clone();
        *self.media_mirror.borrow_mut() = Arc::clone(&self.media);
        self.revision = update.revision();
        let plans = build_plans(update.feed());
        self.replace_plans(plans, cx);
        self.register_media(cx);

        let Some(list) = self.list.as_ref() else {
            cx.request_frame()?;
            return Ok(());
        };
        let count = update.feed().items().len();
        match unshown {
            Some(pushed) => {
                for _ in 0..pushed {
                    list.push(cx.app_mut())?;
                }
            }
            None => list.set_items(cx.app_mut(), count)?,
        }
        if media_changed {
            list.set_count(cx.app_mut(), count)?;
            self.built_media = Some(Arc::clone(update.media()));
        }
        cx.request_frame()?;

        Ok(())
    }

    fn replace_plans(&mut self, plans: Arc<[RowPlan]>, cx: &mut WidgetCx<'_>) {
        *self.plans.borrow_mut() = Arc::clone(&plans);
        cx.write(&self.plan_signal, plans);
    }

    fn register_media(&mut self, cx: &mut WidgetCx<'_>) {
        let mut registered = false;
        for (url, state) in self.media.iter() {
            if !matches!(state, MediaState::Ready(_)) {
                continue;
            }
            if self.images.borrow().contains_key(url) {
                continue;
            }
            let MediaState::Ready(path) = state else {
                continue;
            };
            let bytes = match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    log::warn!("the x pane could not read a cached picture: {error}");
                    continue;
                }
            };
            if bytes.is_empty() {
                continue;
            }
            let pending = cx
                .app_mut()
                .resources_mut()
                .insert_async(Bytes::from(bytes));
            self.images
                .borrow_mut()
                .insert(url.clone(), ImageSource::Pending(pending));
            registered = true;
        }
        if registered && let Some(list) = &self.list {
            let count = list.count(cx.app());
            if count > 0 {
                let _ = list.set_count(cx.app_mut(), count);
            }
        }
    }

    fn sync_media_retry(&mut self, cx: &mut WidgetCx<'_>) {
        let request = cx.read(&self.media_retry).clone();
        let Some(url) = request else {
            return;
        };
        cx.write(&self.media_retry, None);
        self.retry_media(url, cx);
    }

    fn sync_appearance(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let appearance = cx.theme().current(cx.app());
        if appearance == self.appearance.get() {
            return Ok(());
        }
        self.appearance.set(appearance);
        if let Some(list) = &self.list {
            let count = list.count(cx.app());
            if count > 0 {
                list.set_count(cx.app_mut(), count)?;
            }
        }
        cx.request_frame()?;

        Ok(())
    }

    fn make_list(&mut self, cx: &mut WidgetCx<'_>, parent: NodeId) -> Result<List, EngineError> {
        let context = RowContext {
            plans: Rc::clone(&self.plans),
            plan_signal: self.plan_signal,
            language: cx.language(),
            config: cx.services().config,
            theme: cx.theme().clone(),
            appearance: Rc::clone(&self.appearance),
            translate: cx.translate().clone(),
            images: Rc::clone(&self.images),
            media: Rc::clone(&self.media_mirror),
            icons: self
                .icons
                .clone()
                .expect("the row icons are registered before the list is built"),
            hits: self.hits.clone(),
            retry: self
                .media_retry_action
                .clone()
                .expect("the retry action is registered before the list is built"),
        };
        let count = self.feed.as_ref().map_or(0, |feed| feed.items().len());
        let key = cx.unique_key("x.feed.list");
        let theme = cx.theme().clone();
        let window = cx.window();
        let list = List::new(
            cx.app_mut(),
            theme,
            key,
            ListOptions::variable(ROW_ESTIMATE)
                .deque(FEED_CAPACITY)
                .reverse()
                .keep_on_newest()
                .separator()
                .selection(SelectionMode::None)
                .padding(0.0, 0.0),
            move |tree, row_parent, index| build_row(tree, row_parent, index, &context),
        );
        list.set_initial_count(cx.app_mut(), count);
        self.built_media = Some(Arc::clone(&self.media));
        list.mount(cx.app_mut(), window, parent)?;

        Ok(list)
    }

    fn sync_empty_state(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let empty = self.feed.as_ref().is_some_and(TweetSnapshot::is_empty);
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
            .id("TEST_TWEET_EMPTY")
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
        let key = cx.unique_key("x.empty");
        EmptyState::new(cx.theme().clone(), key)
            .text(text)
            .build(cx.tree_mut(), overlay)?;
        self.empty = Some(overlay);
        self.empty_language = Some(language);

        Ok(())
    }

    fn build_manage_button(
        &mut self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let language = *cx.read(&cx.language());
        let enabled = self.account_id.is_some();
        let label = t_smol!(cx.translate(), "widget.x.manage");
        let key = cx.unique_key("x.header.manage");
        let theme = cx.theme().clone();
        let window = cx.window();
        let requests = self.open_dialog_requests;

        let mut button = Button::new(cx.app_mut(), theme, key)
            .ghost()
            .small()
            .icon(IconKind::Faders)
            .a11y_label(label.clone())
            .tooltip_text(label);
        if enabled {
            button = button.on_click(move |ctx| {
                ctx.update(&requests, |requests| *requests += 1);
                let window = ctx.window();
                let _ = ctx.app_mut().request_repaint(window);
            });
        } else {
            button = button.disabled(true);
        }
        button.build(cx.app_mut(), window, parent)?;

        self.manage_button = Some(button);
        self.manage_parent = Some(parent);
        self.manage_built = Some((language, enabled));

        Ok(())
    }

    fn sync_manage_button(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(button) = self.manage_button.as_ref() else {
            return Ok(());
        };
        button.sync(cx.app_mut())?;
        let state = (*cx.read(&cx.language()), self.account_id.is_some());
        if self.manage_built == Some(state) {
            return Ok(());
        }
        let Some(parent) = self.manage_parent else {
            return Ok(());
        };
        if cx.tree().get(parent).is_err() {
            return Ok(());
        }
        self.release_manage_button(cx, true)?;
        self.build_manage_button(parent, cx)
    }

    fn release_manage_button(
        &mut self,
        cx: &mut WidgetCx<'_>,
        remove_node: bool,
    ) -> Result<(), EngineError> {
        let Some(button) = self.manage_button.take() else {
            return Ok(());
        };
        if remove_node && let Some(node) = button.node() {
            remove_own_node(cx, node)?;
        }
        let _ = button.release(cx.app_mut());
        self.manage_parent = None;
        self.manage_built = None;

        Ok(())
    }

    fn hand_over(
        &mut self,
        request: TweetRequest,
        command: PendingCommand,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        self.drop_displaced(cx);
        let Some(binding) = self.binding.as_mut() else {
            return Ok(());
        };
        let request_id = binding.call(cx, request);
        self.pending.track(request_id, command);
        Ok(())
    }

    fn drop_displaced(&mut self, cx: &mut WidgetCx<'_>) {
        let search = self.searching.take().is_some();
        let sent = self
            .selection_pending
            .as_ref()
            .is_some_and(|pending| pending.sent);
        if sent {
            self.selection_pending = None;
        }
        if search || sent {
            let _ = cx.request_frame();
        }
    }

    fn watch_selections(
        &mut self,
        watched: bool,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let Some(scope) = self.current_scope() else {
            return Ok(());
        };
        if !self.ready {
            return Ok(());
        }
        self.hand_over(
            TweetRequest::WatchSelections { scope, watched },
            PendingCommand::WatchSelections,
            cx,
        )
    }

    pub(super) fn load_selections(&mut self, cx: &mut WidgetCx<'_>) -> bool {
        let Some(scope) = self.current_scope() else {
            return false;
        };
        if self.selection_pending.is_some() {
            return false;
        }
        self.selection_pending = Some(SelectionPending {
            operation: if self.selections.is_some() {
                SelectionOperation::Refreshing
            } else {
                SelectionOperation::Loading
            },
            target: None,
            sent: false,
        });
        self.notice = None;
        if self.ready {
            let _ = self.send_selections(scope, cx);
        }
        let _ = cx.request_frame();
        true
    }

    fn send_pending_selections(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(scope) = self.current_scope() else {
            return Ok(());
        };
        if self
            .selection_pending
            .as_ref()
            .is_some_and(|pending| !pending.sent && pending.operation.is_read())
        {
            self.send_selections(scope, cx)?;
            cx.request_frame()?;
        }
        Ok(())
    }

    fn send_selections(
        &mut self,
        scope: TweetScope,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        if let Some(pending) = self.selection_pending.as_mut() {
            pending.sent = true;
        }
        let Some(binding) = self.binding.as_mut() else {
            return Ok(());
        };
        let request_id = binding.call(cx, TweetRequest::FetchSelections(scope));
        self.pending
            .track(request_id, PendingCommand::FetchSelections);
        Ok(())
    }

    pub(super) fn search_users(&mut self, query: SmolStr, cx: &mut WidgetCx<'_>) -> bool {
        let Some(scope) = self.current_scope() else {
            return false;
        };
        let query = SmolStr::new(query.trim());
        if !self.may_change(cx) || !valid_search_query(&query) {
            return false;
        }
        let Some(binding) = self.binding.as_mut() else {
            return false;
        };
        self.searching = Some(query.clone());
        let _ = cx.request_frame();
        let request_id = binding.call(cx, TweetRequest::SearchUsers { scope, query });
        self.pending.track(request_id, PendingCommand::Search);
        true
    }

    pub(super) fn add_selection(&mut self, username: SmolStr, cx: &mut WidgetCx<'_>) -> bool {
        let Some(scope) = self.current_scope() else {
            return false;
        };
        if !self.may_change(cx) {
            return false;
        }
        let Some(data) = self.selections.as_ref() else {
            return false;
        };
        if data.quota_full() || data.contains_username(&username, &self.hints) {
            return false;
        }
        if self.binding.is_none() {
            return false;
        }
        self.begin_change(SelectionOperation::Adding, Some(username.clone()), cx);
        let binding = self
            .binding
            .as_mut()
            .expect("the binding was checked before the change began");
        let request_id = binding.call(cx, TweetRequest::AddSelection { scope, username });
        self.pending.track(request_id, PendingCommand::Add);
        true
    }

    pub(super) fn delete_selection(&mut self, x_user_id: SmolStr, cx: &mut WidgetCx<'_>) -> bool {
        let Some(scope) = self.current_scope() else {
            return false;
        };
        if !self.may_change(cx) {
            return false;
        }
        if self.binding.is_none() {
            return false;
        }
        self.begin_change(SelectionOperation::Deleting, Some(x_user_id.clone()), cx);
        let binding = self
            .binding
            .as_mut()
            .expect("the binding was checked before the change began");
        let request_id = binding.call(cx, TweetRequest::DeleteSelection { scope, x_user_id });
        self.pending.track(request_id, PendingCommand::Delete);
        true
    }

    fn retry_media(&mut self, url: SmolStr, cx: &mut WidgetCx<'_>) {
        let Some(scope) = self.current_scope() else {
            return;
        };
        if let Err(error) = self.hand_over(
            TweetRequest::RetryMedia { scope, url },
            PendingCommand::RetryMedia,
            cx,
        ) {
            log::error!("the x pane could not ask for a picture again: {error}");
        }
    }

    fn may_change(&self, cx: &WidgetCx<'_>) -> bool {
        self.ready
            && self.selections.is_some()
            && self.selection_pending.is_none()
            && self.searching.is_none()
            && !self.status.is_failed(cx.app())
    }

    fn begin_change(
        &mut self,
        operation: SelectionOperation,
        target: Option<SmolStr>,
        cx: &mut WidgetCx<'_>,
    ) {
        self.selection_pending = Some(SelectionPending {
            operation,
            target,
            sent: true,
        });
        self.notice = None;
        let _ = cx.request_frame();
    }

    fn apply_selections_result(
        &mut self,
        result: widget::binding::WidgetRpcResult<TweetProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        self.selection_pending = None;
        match result {
            Ok(TweetResponse::Selections(data)) => {
                self.hints = Arc::new(retained_hints(&self.hints, data.selections()));
                self.selections = Some(data);
                self.notice = None;
            }
            Ok(_) => {}
            Err(error) => {
                self.notice = Some(SelectionNotice::Problem(SelectionFailure::Rpc(error)));
            }
        }
        let _ = cx.request_frame();
    }

    fn apply_search_result(
        &mut self,
        result: widget::binding::WidgetRpcResult<TweetProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        let Some(query) = self.searching.take() else {
            return;
        };
        let answer = match result {
            Ok(TweetResponse::SearchResults(profiles)) => SearchAnswer::Found(profiles),
            Ok(_) => return,
            Err(error) => SearchAnswer::Failed(error),
        };
        if let Some(dialog) = self.dialog.as_mut() {
            dialog.answered_search(cx, &query, answer);
        }
        let _ = cx.request_frame();
    }

    fn apply_add_result(
        &mut self,
        result: widget::binding::WidgetRpcResult<TweetProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        let requested = self
            .selection_pending
            .take()
            .and_then(|pending| pending.target);
        let outcome = match result {
            Ok(TweetResponse::Added(outcome)) => outcome,
            Ok(_) => return,
            Err(error) => {
                self.notice = Some(SelectionNotice::Problem(SelectionFailure::Rpc(error)));
                let _ = cx.request_frame();
                return;
            }
        };
        self.notice = match outcome {
            XMutationOutcome::Confirmed(selection) => {
                let grown = self
                    .selections
                    .as_ref()
                    .map(|data| data.with_added(selection.clone()));
                match grown {
                    Some(data) => {
                        self.selections = Some(data);
                        if selection.profile.is_none()
                            && let Some(username) = requested
                        {
                            Arc::make_mut(&mut self.hints)
                                .insert(selection.x_user_id, SmolStr::new(username.as_str()));
                        }
                        Some(SelectionNotice::Added)
                    }
                    None => None,
                }
            }
            XMutationOutcome::Rejected(error) => {
                Some(SelectionNotice::Problem(SelectionFailure::Rejected(error)))
            }
            XMutationOutcome::Unknown(unknown) => {
                Some(SelectionNotice::Problem(SelectionFailure::Unknown(unknown)))
            }
        };
        let _ = cx.request_frame();
    }

    fn apply_delete_result(
        &mut self,
        result: widget::binding::WidgetRpcResult<TweetProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        let requested = self
            .selection_pending
            .take()
            .and_then(|pending| pending.target);
        let outcome = match result {
            Ok(TweetResponse::Deleted(outcome)) => outcome,
            Ok(_) => return,
            Err(error) => {
                self.notice = Some(SelectionNotice::Problem(SelectionFailure::Rpc(error)));
                let _ = cx.request_frame();
                return;
            }
        };
        self.notice = match outcome {
            XMutationOutcome::Confirmed(()) => match requested {
                Some(x_user_id) => {
                    self.selections = self
                        .selections
                        .as_ref()
                        .map(|data| data.with_deleted(&x_user_id));
                    Arc::make_mut(&mut self.hints).remove(&x_user_id);
                    None
                }
                None => None,
            },
            XMutationOutcome::Rejected(error) => {
                Some(SelectionNotice::Problem(SelectionFailure::Rejected(error)))
            }
            XMutationOutcome::Unknown(unknown) => {
                Some(SelectionNotice::Problem(SelectionFailure::Unknown(unknown)))
            }
        };
        let _ = cx.request_frame();
    }

    fn dialog_view(&self) -> DialogView {
        DialogView {
            signed_in: self.account_id.is_some(),
            ready: self.ready,
            selections: self.selections.clone(),
            searching: self.searching.clone(),
            operation: self
                .selection_pending
                .as_ref()
                .map(|pending| pending.operation),
            notice: self.notice.clone(),
            hints: Arc::clone(&self.hints),
            media: Arc::clone(&self.media),
            images: Rc::clone(&self.images),
            recovery: self.recovery.clone(),
        }
    }

    fn open_dialog(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if self.dialog.is_some() {
            return Ok(());
        }
        let dialog = match TweetDialog::show(cx) {
            Ok(dialog) => dialog,
            Err(error) => {
                log::error!("the x pane could not open its account dialog: {error}");
                return Ok(());
            }
        };
        self.dialog = Some(dialog);
        if let Err(error) = self.watch_selections(true, cx) {
            log::error!("the x pane could not watch the accounts: {error}");
        }
        self.load_selections(cx);
        cx.request_frame()?;

        Ok(())
    }

    fn sync_dialog(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let finished = self.dialog.as_ref().is_some_and(TweetDialog::finished);
        if finished {
            return self.close_dialog(cx);
        }
        let view = self.dialog_view();
        let requests = {
            let Some(dialog) = self.dialog.as_mut() else {
                return Ok(());
            };
            if dialog.sync(cx, &view).is_err() {
                None
            } else {
                Some(dialog.take_requests(cx))
            }
        };
        let Some(requests) = requests else {
            return self.close_dialog(cx);
        };
        for request in requests {
            match request {
                DialogRequest::Reload => {
                    self.load_selections(cx);
                }
                DialogRequest::Search(query) => {
                    self.search_users(query, cx);
                }
                DialogRequest::Add(username) => {
                    self.add_selection(username, cx);
                }
                DialogRequest::Delete(x_user_id) => {
                    self.delete_selection(x_user_id, cx);
                }
            }
        }
        Ok(())
    }

    fn close_dialog(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(mut dialog) = self.dialog.take() else {
            return Ok(());
        };
        dialog.release(cx);
        self.searching = None;
        if let Err(error) = self.watch_selections(false, cx) {
            log::error!("the x pane could not stop watching the accounts: {error}");
        }
        cx.request_frame()?;

        Ok(())
    }
}

fn register_row_icons(cx: &mut WidgetCx<'_>) -> Result<RowIcons, EngineError> {
    let mut register = |kind: IconKind| -> Result<ImageSource, EngineError> {
        let image = cx
            .app_mut()
            .resources_mut()
            .insert(raster(kind, RasterTier::Small48))?;
        Ok(ImageSource::Ready(image.id))
    };
    Ok(RowIcons {
        repost: register(IconKind::Repeat)?,
        verified: register(IconKind::CheckCircle)?,
        warning: register(IconKind::WarningCircle)?,
        play: register(IconKind::Play)?,
        retry: register(IconKind::ArrowsClockwise)?,
    })
}

fn remove_own_node(cx: &mut WidgetCx<'_>, node: NodeId) -> Result<(), EngineError> {
    if cx.tree().get(node).is_err() {
        return Ok(());
    }
    let window = cx.window();
    cx.app_mut().remove_node(window, node).map(|_| ())
}

pub(super) fn valid_search_query(query: &str) -> bool {
    let query = query.trim();
    !query.is_empty()
        && query.len() <= MAX_SEARCH_QUERY_BYTES
        && !query.chars().any(char::is_control)
}

pub(super) fn tweet_url(username: &str, tweet_id: &str) -> String {
    format!("https://x.com/{username}/status/{tweet_id}")
}

fn build_plans(feed: &TweetSnapshot) -> Arc<[RowPlan]> {
    feed.items()
        .iter()
        .map(|item| {
            let author = feed.author(&item.x_user_id);
            plan_of(item, &author, feed)
        })
        .collect()
}

fn plan_of(
    item: &Arc<TweetItem>,
    outer_author: &super::model::TweetAuthor,
    feed: &TweetSnapshot,
) -> RowPlan {
    let (tweet, reposted) = item.displayed();
    let author = feed.author(&tweet.x_user_id);
    let links: Rc<[(Range<usize>, SmolStr)]> = tweet
        .text
        .links()
        .iter()
        .map(|link| (link.range.clone(), link.url.clone()))
        .collect();
    let tweet_id = SmolStr::new(tweet.tweet_id());
    let row_id = tweet_id.clone();
    RowPlan {
        tweet_id,
        row_id: row_id.clone(),
        quote_of: false,
        url: SmolStr::new(tweet_url(author.username.as_ref(), tweet.tweet_id())),
        reposted_by: reposted.then(|| SmolStr::new(outer_author.display_name.as_str())),
        author_name: SmolStr::new(author.display_name.as_str()),
        author_handle: format_smolstr!("@{}", author.username),
        avatar_url: SmolStr::new(author.avatar_url.as_str()),
        verified: author.verified,
        published_at: tweet.timestamp(),
        text: tweet.text.shared(),
        has_text: !tweet.text.is_empty(),
        links,
        media: media_plans(&tweet.media),
        quote: match tweet.reference.as_ref() {
            Some(TweetReferenceItem::Quote(quoted)) => {
                Some(Box::new(plan_of_quote(quoted, feed, row_id)))
            }
            _ => None,
        },
    }
}

fn plan_of_quote(tweet: &Arc<TweetItem>, feed: &TweetSnapshot, row_id: SmolStr) -> RowPlan {
    let author = feed.author(&tweet.x_user_id);
    let links: Rc<[(Range<usize>, SmolStr)]> = tweet
        .text
        .links()
        .iter()
        .map(|link| (link.range.clone(), link.url.clone()))
        .collect();
    RowPlan {
        tweet_id: SmolStr::new(tweet.tweet_id()),
        row_id,
        quote_of: true,
        url: SmolStr::new(tweet_url(author.username.as_ref(), tweet.tweet_id())),
        reposted_by: None,
        author_name: SmolStr::new(author.display_name.as_str()),
        author_handle: format_smolstr!("@{}", author.username),
        avatar_url: SmolStr::new(author.avatar_url.as_str()),
        verified: author.verified,
        published_at: tweet.timestamp(),
        text: tweet.text.shared(),
        has_text: !tweet.text.is_empty(),
        links,
        media: media_plans(&tweet.media),
        quote: None,
    }
}

fn plan_in<'a>(
    plans: &'a [RowPlan],
    row_id: &SmolStr,
    tweet_id: &SmolStr,
    quote_of: bool,
) -> Option<&'a RowPlan> {
    if !quote_of {
        return plans.iter().find(|plan| plan.tweet_id == *tweet_id);
    }
    plans
        .iter()
        .find(|plan| plan.tweet_id == *row_id)
        .and_then(|plan| plan.quote.as_deref())
        .filter(|quote| quote.tweet_id == *tweet_id)
}

fn media_plans(media: &[TweetMediaItem]) -> Arc<[MediaPlan]> {
    media
        .iter()
        .map(|item| MediaPlan {
            url: item.url().clone(),
            kind: match item {
                TweetMediaItem::Image { .. } => MediaKind::Image,
                TweetMediaItem::Video { .. } => MediaKind::Video,
                TweetMediaItem::Gif { .. } => MediaKind::Gif,
            },
            ratio: item.aspect_ratio(),
            poster_only: matches!(
                item,
                TweetMediaItem::Gif {
                    poster_only: true,
                    ..
                }
            ),
        })
        .collect()
}

fn build_row(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    context: &RowContext,
) -> Result<(), EngineError> {
    let plan = context.plans.borrow().get(index).cloned();
    let Some(plan) = plan else {
        return Ok(());
    };
    let row = NodeBuilder::boxed()
        .id(format_smolstr!("TEST_TWEET_ROW:{}", plan.tweet_id))
        .style(
            StyleBuilder::new()
                .flex_col()
                .gap(6.0)
                .padding_sides(12.0, 12.0, 12.0, 12.0)
                .width_full()
                .min_width_0()
                .build(),
        )
        .bind_a11y(row_a11y(plan.tweet_id.clone(), context))
        .build(tree, parent)?;
    context.hits.rows.borrow_mut().insert(row, plan.url.clone());

    if plan.reposted_by.is_some() {
        build_repost_line(tree, row, &plan, context)?;
    }
    build_tweet(tree, row, &plan, context)?;

    Ok(())
}

fn row_a11y(
    tweet_id: SmolStr,
    context: &RowContext,
) -> impl Fn(&mut ReadCtx<'_>) -> A11yPatch + 'static {
    let plan_signal = context.plan_signal;
    let language = context.language;
    let config = context.config;
    let translate = context.translate.clone();
    move |read| {
        let _language = read.read(&language);
        let plans = read.read(&plan_signal);
        let settings = read.read(&config);
        let zone = widget::time_zone::data_time_zone(&settings);
        let Some(plan) = plans.iter().find(|plan| plan.tweet_id == tweet_id) else {
            return A11yPatch::default();
        };
        let age = tweet_age(plan.published_at, OffsetDateTime::now_utc(), zone);
        A11yPatch {
            label: Some(t_smol!(
                translate,
                "widget.x.tweet",
                author = plan.author_handle.clone(),
                time = age_text(age, &translate)
            )),
            description: Some(absolute_time(plan.published_at, zone)),
            ..A11yPatch::default()
        }
    }
}

fn build_repost_line(
    tree: &mut NodeTree,
    parent: NodeId,
    plan: &RowPlan,
    context: &RowContext,
) -> Result<(), EngineError> {
    let line = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .gap(4.0)
                .padding_sides(0.0, 0.0, 0.0, AVATAR_SIZE + 12.0)
                .min_width_0()
                .build(),
        )
        .build(tree, parent)?;
    build_icon(
        tree,
        line,
        context.icons.repost,
        14.0,
        context.theme.clone(),
        Role::FgMuted,
    )?;
    let repost = t_smol!(
        context.translate,
        "widget.x.reposted",
        name = plan.reposted_by.clone().unwrap_or_default()
    );
    NodeBuilder::text(repost, TextStyle::new(META_SIZE.text_size()))
        .style(StyleBuilder::new().min_width_0().build())
        .bind_text({
            let plan_signal = context.plan_signal;
            let language = context.language;
            let translate = context.translate.clone();
            let row_id = plan.row_id.clone();
            let tweet_id = plan.tweet_id.clone();
            move |read| {
                let _language = read.read(&language);
                let plans = read.read(&plan_signal);
                plan_in(&plans, &row_id, &tweet_id, false)
                    .and_then(|plan| plan.reposted_by.clone())
                    .map_or_else(SmolStr::default, |name| {
                        t_smol!(translate, "widget.x.reposted", name = name)
                    })
            }
        })
        .bind_style(themed_foreground(context.theme.clone(), Role::FgMuted))
        .build(tree, line)?;

    Ok(())
}

fn build_tweet(
    tree: &mut NodeTree,
    parent: NodeId,
    plan: &RowPlan,
    context: &RowContext,
) -> Result<(), EngineError> {
    let content = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .gap(12.0)
                .min_width_0()
                .width_full()
                .build(),
        )
        .build(tree, parent)?;
    build_avatar(tree, content, plan, context)?;

    let column = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_col()
                .flex_1()
                .gap(6.0)
                .min_width_0()
                .build(),
        )
        .build(tree, content)?;
    build_tweet_header(tree, column, plan, context)?;
    if plan.has_text {
        build_text(tree, column, plan, context)?;
    }
    if !plan.media.is_empty() {
        build_gallery(tree, column, plan, context)?;
    }
    if let Some(quote) = plan.quote.as_deref() {
        build_quote(tree, column, quote, context)?;
    }

    Ok(())
}

fn build_avatar(
    tree: &mut NodeTree,
    parent: NodeId,
    plan: &RowPlan,
    context: &RowContext,
) -> Result<(), EngineError> {
    let size = if plan.quote_of {
        QUOTE_AVATAR_SIZE
    } else {
        AVATAR_SIZE
    };
    let style = StyleBuilder::new()
        .width(size)
        .height(size)
        .flex_none()
        .build();
    let image = if plan.avatar_url.is_empty() {
        None
    } else {
        context.images.borrow().get(&plan.avatar_url).copied()
    };
    let Some(image) = image else {
        let placeholder = NodeBuilder::boxed()
            .style(style)
            .bind_style(themed_background(
                context.theme.clone(),
                Role::SurfaceRaised,
            ))
            .build(tree, parent)?;
        let mut text_style = TextStyle::new(META_SIZE.text_size());
        text_style.alignment = TextAlign::Center;
        text_style.color = Color::WHITE;
        NodeBuilder::text(SmolStr::new_static("@"), text_style)
            .style(StyleBuilder::new().width_full().height_full().build())
            .bind_style(themed_foreground(context.theme.clone(), Role::FgMuted))
            .a11y(A11yOverride {
                hidden: true,
                ..A11yOverride::default()
            })
            .build(tree, placeholder)?;
        return Ok(());
    };
    let node = match image {
        ImageSource::Ready(resource) => NodeBuilder::image(resource, ImageFit::Cover),
        ImageSource::Pending(pending) => NodeBuilder::image_pending(pending, ImageFit::Cover),
    };
    node.style(style)
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        })
        .build(tree, parent)?;

    Ok(())
}

fn build_tweet_header(
    tree: &mut NodeTree,
    parent: NodeId,
    plan: &RowPlan,
    context: &RowContext,
) -> Result<(), EngineError> {
    let name_row = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .gap(4.0)
                .min_width_0()
                .build(),
        )
        .build(tree, parent)?;

    NodeBuilder::text(
        plan.author_name.clone(),
        TextStyle {
            size: BODY_SIZE.text_size(),
            weight: 600,
            ..TextStyle::new(BODY_SIZE.text_size())
        },
    )
    .style(StyleBuilder::new().min_width_0().build())
    .bind_text({
        let plan_signal = context.plan_signal;
        let row_id = plan.row_id.clone();
        let tweet_id = plan.tweet_id.clone();
        let quote_of = plan.quote_of;
        move |read| {
            let plans = read.read(&plan_signal);
            plan_in(&plans, &row_id, &tweet_id, quote_of)
                .map_or_else(SmolStr::default, |plan| plan.author_name.clone())
        }
    })
    .bind_style(themed_foreground(context.theme.clone(), Role::FgDefault))
    .build(tree, name_row)?;

    if plan.verified {
        build_icon(
            tree,
            name_row,
            context.icons.verified,
            16.0,
            context.theme.clone(),
            Role::BrandBase,
        )?;
    }

    NodeBuilder::text(
        SmolStr::new_static("·"),
        TextStyle::new(META_SIZE.text_size()),
    )
    .style(StyleBuilder::new().flex_none().build())
    .bind_style(themed_foreground(context.theme.clone(), Role::FgMuted))
    .build(tree, name_row)?;

    NodeBuilder::text(SmolStr::default(), TextStyle::new(META_SIZE.text_size()))
        .style(StyleBuilder::new().flex_none().build())
        .bind_text({
            let plan_signal = context.plan_signal;
            let config = context.config;
            let language = context.language;
            let translate = context.translate.clone();
            let row_id = plan.row_id.clone();
            let tweet_id = plan.tweet_id.clone();
            let quote_of = plan.quote_of;
            move |read| {
                let _language = read.read(&language);
                let plans = read.read(&plan_signal);
                let settings = read.read(&config);
                let zone = widget::time_zone::data_time_zone(&settings);
                plan_in(&plans, &row_id, &tweet_id, quote_of).map_or_else(
                    SmolStr::default,
                    |plan| {
                        age_text(
                            tweet_age(plan.published_at, OffsetDateTime::now_utc(), zone),
                            &translate,
                        )
                    },
                )
            }
        })
        .bind_style(themed_foreground(context.theme.clone(), Role::FgMuted))
        .build(tree, name_row)?;

    NodeBuilder::text(
        plan.author_handle.clone(),
        TextStyle::new(META_SIZE.text_size()),
    )
    .style(StyleBuilder::new().min_width_0().build())
    .bind_text({
        let plan_signal = context.plan_signal;
        let row_id = plan.row_id.clone();
        let tweet_id = plan.tweet_id.clone();
        let quote_of = plan.quote_of;
        move |read| {
            let plans = read.read(&plan_signal);
            plan_in(&plans, &row_id, &tweet_id, quote_of)
                .map_or_else(SmolStr::default, |plan| plan.author_handle.clone())
        }
    })
    .bind_style(themed_foreground(context.theme.clone(), Role::FgMuted))
    .build(tree, parent)?;

    Ok(())
}

fn build_text(
    tree: &mut NodeTree,
    parent: NodeId,
    plan: &RowPlan,
    context: &RowContext,
) -> Result<(), EngineError> {
    let mut style = TextStyle::new(if plan.quote_of {
        META_SIZE.text_size()
    } else {
        BODY_SIZE.text_size()
    });
    style.overflow = TextOverflow::Clip;
    let link_color = context
        .theme
        .theme_for(context.appearance.get())
        .role(Role::BrandBase);
    let mut run_style = style.clone();
    run_style.color = link_color;
    let runs: Vec<TextRun> = plan
        .links
        .iter()
        .map(|(range, _)| TextRun::new(range.clone(), run_style.clone()))
        .collect();
    let links = Rc::clone(&plan.links);
    let node = NodeBuilder::text(plan.text.clone(), style)
        .id(format_smolstr!("TEST_TWEET_TEXT:{}", plan.tweet_id))
        .runs(runs)
        .style(StyleBuilder::new().min_width_0().build())
        .bind_text({
            let plan_signal = context.plan_signal;
            let row_id = plan.row_id.clone();
            let tweet_id = plan.tweet_id.clone();
            let quote_of = plan.quote_of;
            move |read| {
                let plans = read.read(&plan_signal);
                plan_in(&plans, &row_id, &tweet_id, quote_of)
                    .map_or_else(SmolStr::default, |plan| plan.text.clone())
            }
        })
        .bind_style({
            let theme = context.theme.clone();
            move |read| StylePatch {
                foreground: Some(theme.role(read, Role::FgDefault)),
                ..StylePatch::default()
            }
        })
        .bind_a11y({
            let plan_signal = context.plan_signal;
            let row_id = plan.row_id.clone();
            let tweet_id = plan.tweet_id.clone();
            let quote_of = plan.quote_of;
            move |read| {
                let plans = read.read(&plan_signal);
                let text = plan_in(&plans, &row_id, &tweet_id, quote_of)
                    .map_or_else(SmolStr::default, |plan| plan.text.clone());
                A11yPatch {
                    description: Some(text),
                    ..A11yPatch::default()
                }
            }
        })
        .build(tree, parent)?;
    if !links.is_empty() {
        context.hits.links.borrow_mut().insert(node, links);
    }

    Ok(())
}

fn build_gallery(
    tree: &mut NodeTree,
    parent: NodeId,
    plan: &RowPlan,
    context: &RowContext,
) -> Result<(), EngineError> {
    let count = plan.media.len().min(4);
    let columns = if count == 1 { 1 } else { 2 };
    let tile_width = if count == 1 {
        GALLERY_WIDE_WIDTH.min(GALLERY_HEIGHT * plan.media[0].ratio)
    } else {
        (GALLERY_WIDE_WIDTH - 4.0) * 0.5
    };
    let rows = if count > 2 { 2 } else { 1 };
    let tile_height = GALLERY_HEIGHT / rows as f32;
    let grid = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_col()
                .gap(4.0)
                .width(tile_width * columns as f32 + 4.0 * (columns - 1) as f32)
                .max_width(GALLERY_WIDE_WIDTH)
                .build(),
        )
        .build(tree, parent)?;
    let mut index = 0;
    for _ in 0..rows {
        let line = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .gap(4.0)
                    .width_full()
                    .min_width_0()
                    .build(),
            )
            .build(tree, grid)?;
        let mut in_line = 0;
        while in_line < columns && index < count {
            build_media_tile(tree, line, plan, index, tile_width, tile_height, context)?;
            in_line += 1;
            index += 1;
        }
    }

    Ok(())
}

fn build_media_tile(
    tree: &mut NodeTree,
    parent: NodeId,
    plan: &RowPlan,
    index: usize,
    width: f32,
    height: f32,
    context: &RowContext,
) -> Result<(), EngineError> {
    let item = &plan.media[index];
    let media = context.media();
    let state = media.get(&item.url);
    let style = StyleBuilder::new()
        .width(width)
        .height(height)
        .flex_none()
        .build();
    let mut tile = NodeBuilder::boxed()
        .id(format_smolstr!(
            "TEST_TWEET_MEDIA:{}:{index}",
            plan.tweet_id
        ))
        .style(style)
        .bind_style(themed_background(
            context.theme.clone(),
            Role::SurfaceRaised,
        ))
        .bind_a11y({
            let media = Rc::clone(&context.media);
            let language = context.language;
            let translate = context.translate.clone();
            let url = item.url.clone();
            let poster_only = item.poster_only;
            move |read| {
                let _language = read.read(&language);
                let state = media.borrow().get(&url).cloned();
                match state {
                    Some(MediaState::Failed) => A11yPatch {
                        label: Some(t_smol!(translate, "common.retry")),
                        description: Some(t_smol!(translate, "widget.x.media_load_failed")),
                        hidden: Some(false),
                        ..A11yPatch::default()
                    },
                    Some(MediaState::Ready(_)) if poster_only => A11yPatch {
                        label: Some(SmolStr::new_static("GIF")),
                        hidden: Some(false),
                        ..A11yPatch::default()
                    },
                    _ => A11yPatch {
                        hidden: Some(true),
                        ..A11yPatch::default()
                    },
                }
            }
        });
    if matches!(state, Some(MediaState::Failed)) {
        tile = tile.on_action(context.retry.clone());
    }
    let tile = tile.build(tree, parent)?;

    match state {
        Some(MediaState::Ready(_)) => {
            let image = context.images.borrow().get(&item.url).copied();
            if let Some(image) = image {
                let node = match image {
                    ImageSource::Ready(resource) => NodeBuilder::image(resource, ImageFit::Cover),
                    ImageSource::Pending(pending) => {
                        NodeBuilder::image_pending(pending, ImageFit::Cover)
                    }
                };
                node.style(StyleBuilder::new().width(width).height(height).build())
                    .a11y(A11yOverride {
                        hidden: true,
                        ..A11yOverride::default()
                    })
                    .build(tree, tile)?;
            } else {
                build_icon_centered(
                    tree,
                    tile,
                    context.icons.warning,
                    20.0,
                    context.theme.clone(),
                    Role::FgMuted,
                )?;
            }
        }
        Some(MediaState::Failed) => {
            build_icon_centered(
                tree,
                tile,
                context.icons.warning,
                20.0,
                context.theme.clone(),
                Role::StatusDangerBase,
            )?;
            context
                .hits
                .failed
                .borrow_mut()
                .insert(tile, item.url.clone());
        }
        Some(MediaState::Loading) | None => {
            build_icon_centered(
                tree,
                tile,
                context.icons.retry,
                18.0,
                context.theme.clone(),
                Role::FgMuted,
            )?;
        }
    }
    if item.kind == MediaKind::Video {
        build_icon(
            tree,
            tile,
            context.icons.play,
            24.0,
            context.theme.clone(),
            Role::FgOnBrand,
        )?;
    }

    Ok(())
}

fn build_quote(
    tree: &mut NodeTree,
    parent: NodeId,
    plan: &RowPlan,
    context: &RowContext,
) -> Result<(), EngineError> {
    let quote = NodeBuilder::boxed()
        .id(format_smolstr!("TEST_TWEET_QUOTE:{}", plan.tweet_id))
        .style(
            StyleBuilder::new()
                .flex_row()
                .gap(8.0)
                .padding(8.0)
                .border(1.0)
                .min_width_0()
                .build(),
        )
        .bind_style({
            let theme = context.theme.clone();
            move |read| StylePatch {
                border: Some(ui_core::paint::primitive::Border {
                    width: 1.0,
                    color: theme.role(read, Role::LineDivider),
                }),
                ..StylePatch::default()
            }
        })
        .build(tree, parent)?;
    context
        .hits
        .rows
        .borrow_mut()
        .insert(quote, plan.url.clone());
    build_avatar(tree, quote, plan, context)?;
    let column = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_col()
                .flex_1()
                .gap(4.0)
                .min_width_0()
                .build(),
        )
        .build(tree, quote)?;
    build_tweet_header(tree, column, plan, context)?;
    if plan.has_text {
        build_text(tree, column, plan, context)?;
    }
    if !plan.media.is_empty() {
        build_gallery(tree, column, plan, context)?;
    }

    Ok(())
}

fn build_icon(
    tree: &mut NodeTree,
    parent: NodeId,
    icon: ImageSource,
    size: f32,
    theme: ThemeHandle,
    role: Role,
) -> Result<(), EngineError> {
    let node = match icon {
        ImageSource::Ready(resource) => NodeBuilder::image(resource, ImageFit::Contain),
        ImageSource::Pending(pending) => NodeBuilder::image_pending(pending, ImageFit::Contain),
    };
    node.style(
        StyleBuilder::new()
            .width(size)
            .height(size)
            .flex_none()
            .build(),
    )
    .bind_style(themed_foreground(theme, role))
    .a11y(A11yOverride {
        hidden: true,
        ..A11yOverride::default()
    })
    .build(tree, parent)?;

    Ok(())
}

fn build_icon_centered(
    tree: &mut NodeTree,
    parent: NodeId,
    icon: ImageSource,
    size: f32,
    theme: ThemeHandle,
    role: Role,
) -> Result<(), EngineError> {
    let node = match icon {
        ImageSource::Ready(resource) => NodeBuilder::image(resource, ImageFit::Contain),
        ImageSource::Pending(pending) => NodeBuilder::image_pending(pending, ImageFit::Contain),
    };
    node.style(StyleBuilder::new().width(size).height(size).build())
        .bind_style(themed_foreground(theme, role))
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        })
        .build(tree, parent)?;

    Ok(())
}

fn themed_foreground(
    theme: ThemeHandle,
    role: Role,
) -> impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static {
    move |read| StylePatch {
        foreground: Some(theme.role(read, role)),
        ..StylePatch::default()
    }
}

fn themed_background(
    theme: ThemeHandle,
    role: Role,
) -> impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static {
    move |read| StylePatch {
        background: Some(theme.role(read, role)),
        ..StylePatch::default()
    }
}

fn age_text(age: TweetAge, translate: &Translate) -> SmolStr {
    match age {
        TweetAge::Seconds(seconds) => t_smol!(translate, "widget.x.age_seconds", count = seconds),
        TweetAge::Minutes(minutes) => {
            t_smol!(translate, "widget.x.age_minutes", count = minutes)
        }
        TweetAge::Hours(hours) => t_smol!(translate, "widget.x.age_hours", count = hours),
        TweetAge::Date { month, day } => {
            t_smol!(translate, "widget.x.age_date", month = month, day = day)
        }
        TweetAge::FullDate { year, month, day } => t_smol!(
            translate,
            "widget.x.age_full_date",
            year = year,
            month = month,
            day = day
        ),
        TweetAge::Unknown => SmolStr::new_static("--"),
    }
}

pub(super) fn status_label(status: XSelectionStatus, translate: &Translate) -> SmolStr {
    match status {
        XSelectionStatus::Active => t_smol!(translate, "widget.x.active"),
        XSelectionStatus::Paused => t_smol!(translate, "widget.x.paused"),
        XSelectionStatus::Pending => t_smol!(translate, "widget.x.pending"),
    }
}

#[cfg(test)]
mod tests;

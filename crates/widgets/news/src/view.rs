//! The news pane: the feed it follows, and the rows it draws.
//!
//! # What a pane is scoped to
//!
//! One account. The account is the pane's scope and the stream key at once, so the pane
//! never has to decide whether a report belongs to the reader on screen: a message
//! published for an account the pane has left is a message the binding no longer
//! delivers, and one that arrives by hand is refused by comparing the key.
//!
//! # What it reports, and what it does not
//!
//! It reports through the framework's status cell and nothing else: a failure is a
//! failure, and the framework decides where that is shown and what the recovery looks
//! like. Nobody signed in is *not* a failure — it is neutral text for a pane that has
//! nothing to show yet, which is what `WidgetState::waiting` is for. A feed with
//! nothing in it is not a state either: a live feed that holds no news is an empty feed,
//! and a pane with an empty feed has content, which draws `common.no_data` in its body.
//!
//! # Why the list holds a signal rather than items
//!
//! The body is built once, and a kit list row's content closure has no [`App`] to read
//! current data with. So the rows read the feed through a signal: every row's text is a
//! `bind_text` closure over the feed signal, the display time zone signal and the
//! language signal, and the list itself only holds the row count and where the reader
//! is. A newer snapshot is therefore reported to the list as the reports it has not been
//! shown rather than as a new set of rows: a list rebuilt from scratch would re-measure
//! every row on screen and, worse, move a reader who had scrolled into the feed back to
//! the top. The one case that does move them is a report that arrives while the newest
//! row is the one they are reading — the newest news belongs on screen, and that is
//! exactly where they are looking.

use std::{rc::Rc, sync::Arc, time::Instant};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    kit::{
        button::Button,
        empty_state::EmptyState,
        icon::IconKind,
        list::{List, ListOptions, SelectionMode},
        separator::Separator,
        size::{UnifiedSizable, UnifiedSize, UnifiedSizeStyle},
    },
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{handle::ThemeHandle, role::Role},
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_service_provider::auth::constants::ACCOUNT_ORIGIN;
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::A11yPatch,
    error::EngineError,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::{primitive::Border, style::StylePatch},
    text::{TextOverflow, TextStyle},
};
use widget::{
    binding::{Inbound, WidgetBinding},
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    error::TranslatedError,
    identity::{WidgetId, WidgetInstanceId},
    rpc::RpcCallError,
    services::WidgetServices,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    time_zone::data_time_zone,
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

use super::{
    model::{MAX_NEWS_ITEMS, NewsCategory, NewsSnapshot, has_new_news, unshown},
    service::{
        NewsProtocol, NewsRequest, NewsScope, NewsService, NewsSource, NewsStatus, NewsStreamError,
        NewsStreamMessage,
    },
    sound::{NOTIFICATION_CLIP, NotificationSound, SilentSound, SoundPlayer, Throttle},
};

/// The height a row is estimated at until the list has measured one.
///
/// A row is a badge line and a paragraph of news, so a couple of them either side of
/// the viewport is enough to keep a row from appearing a moment after it was scrolled
/// to; the list measures each row it mounts and replaces the estimate with the truth.
const ROW_ESTIMATE: f32 = 76.0;

/// The news one pane follows.
///
/// What a feed shows is what the service published for the account it is reading as, so
/// there is nothing here to persist about the feed itself. What a reader chooses is
/// whether news is allowed to make itself heard, which is a choice about the pane rather
/// than about the news in it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NewsConfig {
    /// Whether a report arriving is played as the pane's notification clip.
    ///
    /// A reader is told about news by looking at it, so the sound is opt-out rather than
    /// opt-in: it is what a pane is for.
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

/// A failure a pane reports.
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

/// One pane: the feed it holds, and the list that draws it.
pub struct NewsWidget {
    /// The options the pane persists, which is whether it may be heard.
    config: NewsConfig,
    /// The executor bridge, absent on a host that installed no Kytrix service.
    binding: Option<WidgetBinding<NewsProtocol>>,
    status: WidgetStatus,
    recovery: WidgetAction,
    config_change: WidgetConfigChange,
    /// The account the pane reads for, which is its whole scope.
    account_id: Option<SmolStr>,
    /// The news the pane is showing, once a subscription has published a feed.
    snapshot: Option<NewsSnapshot>,
    /// Whether the subscription has reported itself live.
    ///
    /// It is what keeps readiness at the protocol's own boundary: a feed that arrived
    /// before the subscription came up is a pane with content that is still connecting,
    /// not a pane that is done.
    ready: bool,
    /// The feed the rows read. Rows have no `App` of their own, so the state they draw
    /// is a signal watched by their `bind_text` closures.
    items: Signal<NewsSnapshot>,
    /// The list itself, which holds the newest reports, measures their rows and remembers
    /// where a reader is. It is built by [`WidgetView::body`], where its parent is known.
    list: Option<List>,
    /// The root of the body this pane built, so teardown can take its own subtree down
    /// before releasing the list component.
    body_root: Option<NodeId>,
    /// The stand-in a feed with nothing in it draws, built beside the list rather than in
    /// it: the list holds rows, and an empty feed has none.
    empty: Option<NodeId>,
    /// The language the stand-in was built in: its copy is static text, so a language
    /// switch rebuilds it the way the surface rebuilds a static notice.
    empty_language: Option<LanguageCode>,
    /// The clip a report is announced with, and the device it is heard on.
    sound: Box<dyn NotificationSound>,
    /// How recently this pane was heard, so a run of reports is one sound.
    throttle: Throttle,
    /// The header control, kept so its state can be synchronized and it can be released.
    sound_button: Option<Button>,
    /// The header group the control lives in, so a state change can rebuild it in place
    /// (a kit button's icon and label are chosen when it is built).
    sound_parent: Option<NodeId>,
    /// What the control was built with: the language, the option, and whether the host
    /// has audio. A difference is what makes the control rebuild.
    sound_built: Option<(LanguageCode, bool, bool)>,
    /// Click requests from the header control: a button handler owns no `&mut self`, so
    /// it asks the next `sync` for the toggle instead of performing it.
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

    /// A pane is content once a subscription has published a feed for it.
    ///
    /// An empty feed is content: a feed with no news in it yet is still the feed the pane
    /// is about, and the pane has nothing to say about it beyond being empty.
    fn has_content(&self) -> bool {
        self.snapshot.is_some()
    }

    /// The one option this pane owns: whether news may be heard.
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
        self.sync_empty_state(cx)?;

        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        // The settings are the pane's scope: signing in as someone else is another feed,
        // and signing out is no feed at all. A settings write reaches this hook on the
        // frame it schedules, and the comparison is what makes it a reload only when the
        // account really moved.
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
                Inbound::Rpc { result, .. } => self.apply_load_result(result, cx),
            }
        }

        let requests = *cx.read(&self.toggle_requests);
        if requests != self.seen_toggle_requests {
            self.seen_toggle_requests = requests;
            self.toggle_sound(cx);
        }

        if let Some(list) = &self.list {
            list.sync(cx.app_mut())?;
        }
        self.sync_empty_state(cx)?;
        self.sync_sound_button(cx)?;

        // The body is built by the framework from the answer `has_content` gives, and it
        // asks that question before this hook runs: content that first appeared here is
        // one frame away, so ask for that frame.
        if self.snapshot.is_some() && self.body_root.is_none() {
            cx.request_frame()?;
        }

        Ok(())
    }

    /// The control that turns the pane's sound on and off.
    ///
    /// The header row belongs to the pane, so the widget only builds its own control into
    /// the group it was given; the control's state follows the option and the host on the
    /// frame after either moves (`sync` rebuilds it, because a kit button's icon and label
    /// are chosen when it is built).
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
        // Dropping the bridge aborts its forwarding tasks and cancels the service.
        self.binding = None;
        // The header's nodes belong to the pane, which removes its row: only the
        // component's signal scope and action handler are this widget's to release.
        let _ = self.release_sound_button(cx, false);
        // The list documents its own teardown order: its nodes first, then the component.
        // The body subtree is the widget's, not the framework's shell, so removing it here
        // is the widget cleaning up after itself.
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
    /// The seam the framework and the tests build a pane through.
    ///
    /// The pane has no opinion about where its news comes from, so the source is an
    /// argument: the production constructor answers it with the injected Kytrix service
    /// connection, and a test with a fake subscription.
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
        // The pane records the account it was built for, but it starts nothing: the
        // framework calls `load` once construction is done.
        let account_id = cx.account();

        Self {
            config,
            binding,
            status,
            recovery,
            config_change,
            account_id,
            snapshot: None,
            ready: false,
            items,
            list: None,
            body_root: None,
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

    /// A pane over a source the caller answers, for the crate's own tests.
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

    /// The scope this pane follows, which is the account it reads as.
    fn current_scope(&self) -> Option<NewsScope> {
        NewsScope::new(self.account_id.as_ref())
    }

    /// Starts reading, or restarts for a pane the user is recovering.
    ///
    /// A recovery keeps the news the user is still reading — blanking a feed to reconnect
    /// would flash it away — while an account change starts over, because the pane cannot
    /// vouch for what it was showing.
    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(scope) = self.current_scope() else {
            self.clear(cx)?;
            self.report_logged_out(cx);
            // Nothing is followed, so nothing should stay subscribed on this pane's
            // behalf: an account that signed out must not keep a stream it is not using.
            if let Some(binding) = self.binding.as_mut() {
                binding.call(cx, NewsRequest::Stop);
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
        binding.call(
            cx,
            if retry {
                NewsRequest::Retry(scope)
            } else {
                NewsRequest::Load(scope)
            },
        );
        cx.request_frame()?;

        Ok(())
    }

    /// Drops the feed a pane is leaving behind.
    fn clear(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.snapshot = None;
        self.ready = false;
        cx.write(&self.items, NewsSnapshot::default());
        if let Some(list) = &self.list {
            list.set_items(cx.app_mut(), 0)?;
        }
        self.sync_empty_state(cx)?;
        cx.request_frame()?;

        Ok(())
    }

    /// Reports the notice a pane nobody is signed in for is waiting with.
    fn report_logged_out(&self, cx: &mut WidgetCx<'_>) {
        self.status.report(
            cx.app_mut(),
            WidgetState::waiting(Notice::message("widget.news.logged_out")),
        );
    }

    /// Plays the notification clip, if a reader is to hear one.
    ///
    /// A report is announced rather than reported: the sound says something arrived, so what
    /// is worth hearing is the first report of a run and not each of them.
    fn announce(&mut self) {
        if self.config.sound && self.throttle.allows(Instant::now()) {
            self.sound.play();
        }
    }

    /// Takes the sound the other way, and reports the change the pane now holds.
    ///
    /// The throttle is left where it is: what a reader turns off is the sound, not the clock
    /// it is heard on, and a reader who turns it back on has already waited.
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

    fn apply_load_result(
        &mut self,
        result: widget::binding::WidgetRpcResult<NewsProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        // The answer says the service took the account; what the subscription then
        // reports about itself arrives on the stream, which is where a failure to
        // subscribe is drawn from.
        if let Err(error) = result {
            self.ready = false;
            let recovery = self.recovery.clone();
            self.status.report(
                cx.app_mut(),
                WidgetState::failed(Arc::new(NewsFailure::Rpc(error)), recovery),
            );
        }
    }

    /// Applies the feed a subscription published.
    ///
    /// Returns whether what a frame draws changed, which for a feed is always: the
    /// service publishes it when a report moved it.
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

        // What arrived is decided before the feed is taken: a feed says what is new by the
        // report it leads with, and the pane is about to stop holding the one before it.
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

    /// Applies a status the stream reported.
    ///
    /// Returns whether what a frame draws changed, which a status never does on its own:
    /// the framework draws the state around the body, and a row of news says nothing
    /// about the subscription it arrived on.
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
                // The news already read stays: a reconnecting feed is not a reason to
                // blank what a reader was looking at.
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

    /// Reports why a subscription stopped.
    ///
    /// An account without news access is not a broken connection: retrying it asks the
    /// same refused question, so the control offered is where a subscription is bought
    /// instead of a retry.
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

    /// Tells the list about the reports this feed added.
    ///
    /// The list holds the newest reports and remembers the one a reader is looking at, so it
    /// is told the reports it has not been shown rather than the feed itself: a list that
    /// keeps the reports it holds where they are is what keeps a reader on the report they are
    /// reading. The one report that does move them is one that arrives while the newest report
    /// is the one they are reading, which is where the newest news belongs — and a feed the
    /// list cannot be extended into is one it is rebuilt for, which is the engine list's
    /// `set_items` (a rebuild that starts the reader over at the newest report).
    fn accept(&mut self, snapshot: NewsSnapshot, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let unshown = unshown(
            self.snapshot.as_ref().map_or(&[], NewsSnapshot::items),
            snapshot.items(),
        );

        cx.write(&self.items, snapshot.clone());
        let count = snapshot.items().len();
        match (unshown, self.list.as_ref()) {
            // The oldest of the reports it has not been shown goes in first, so the newest of
            // them ends up where a list that draws its newest report first draws it.
            (Some(pushed), Some(list)) => {
                for _ in 0..pushed {
                    list.push(cx.app_mut())?;
                }
            }
            // Nothing the list holds survives, so there is no edit it can be told: every report
            // on screen is news the feed no longer has. The list is rebuilt for the feed it has
            // now, and the reader starts over on the report that feed draws first. A list that
            // does not exist yet is built for the feed by `body`, which reads the count from
            // this signal.
            (None, Some(list)) => list.set_items(cx.app_mut(), count)?,
            _ => {}
        }
        self.snapshot = Some(snapshot);
        cx.request_frame()?;

        Ok(())
    }

    /// Builds the list the body draws its feed with.
    fn make_list(&self, cx: &mut WidgetCx<'_>, parent: NodeId) -> Result<List, EngineError> {
        let items = self.items;
        let language = cx.language();
        let config = cx.services().config;
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

    /// Shows `common.no_data` over a feed that holds nothing.
    ///
    /// The stand-in is the widget's, not the framework's: an empty feed is *content*, so
    /// this is the body saying there is nothing in it rather than a placeholder saying
    /// there is no body. Its copy is static, so it is built for the language showing now
    /// and rebuilt when that moves (the same rule the surface follows for its notices).
    fn sync_empty_state(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let empty = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.items().is_empty());
        if !empty {
            if let Some(node) = self.empty.take() {
                let window = cx.window();
                cx.app_mut().remove_node(window, node)?;
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
            let window = cx.window();
            cx.app_mut().remove_node(window, node)?;
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

    /// Builds the pane's sound control into `parent`.
    fn build_sound_button(
        &mut self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let language = *cx.read(&cx.language());
        let available = self.sound.is_available();
        let enabled = self.config.sound;
        // A control that is drawn disabled must not be one that acts: the click it was
        // given would run all the same.
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
                // A button handler owns no widget; it asks the next sync for the toggle.
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

    /// Keeps the control in step with the option and the host, and rebuilds it when the
    /// icon or the label it was built with is no longer the truth.
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
        // The pane may have rebuilt the header row since: its group is not this widget's
        // node, so a stale parent means the control already went with it.
        if cx.tree().get(parent).is_err() {
            return Ok(());
        }

        self.release_sound_button(cx, true)?;
        self.build_sound_button(parent, cx)
    }

    /// Removes the control's node (when the widget owns the teardown) and releases the
    /// component's scope and action handler.
    fn release_sound_button(
        &mut self,
        cx: &mut WidgetCx<'_>,
        remove_node: bool,
    ) -> Result<(), EngineError> {
        let Some(button) = self.sound_button.take() else {
            return Ok(());
        };
        if remove_node && let Some(node) = button.node() {
            let window = cx.window();
            cx.app_mut().remove_node(window, node)?;
        }
        let _ = button.release(cx.app_mut());
        self.sound_parent = None;
        self.sound_built = None;

        Ok(())
    }
}

/// The sound a host hands over: the clip's bytes, or a silent sound when it has none.
fn notification_sound(services: &WidgetServices) -> Box<dyn NotificationSound> {
    match (services.sound)(NOTIFICATION_CLIP) {
        Some(clip) => Box::new(SoundPlayer::new(clip)),
        None => Box::new(SilentSound),
    }
}

/// The recovery an account without news access is offered.
///
/// The subscription is what is missing, so the control goes where one is bought.
fn view_plans(open_url: Rc<dyn Fn(&str)>) -> WidgetAction {
    WidgetAction::new("widget.news.view_plans", move |_| {
        (open_url)(ACCOUNT_ORIGIN);
    })
}

/// Builds one row of the feed into the list's row container.
///
/// The row closure has no `App`, so every dynamic part of it is a `bind_*` closure over
/// the signals the pane handed in: the feed for the report, the settings for the display
/// zone, and the language for the category's name.
fn build_row(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    items: Signal<NewsSnapshot>,
    language: Signal<LanguageCode>,
    config: Signal<app_config::settings::Config>,
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

    // The old list drew a separator between rows; the engine list leaves that to the row,
    // because it has no separator option of its own.
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

    // What it is, when it happened, and what it says — the line is laid out from its two
    // ends so the time sits at the trailing edge whether or not the report has a title.
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

    build_category_badge(
        tree,
        leading,
        index,
        items,
        language,
        theme.clone(),
        translate,
    )?;
    build_title(tree, leading, index, items, theme.clone())?;
    build_time(tree, meta, index, items, config, theme.clone())?;
    build_content(tree, content, index, items, theme)?;

    Ok(())
}

/// The pill that names the category of a report.
///
/// It is `kit::badge`'s look (a small outlined pill) built from the row's own nodes rather
/// than from the component: a badge's text is chosen when it is built, and a row's text has
/// to follow the language signal the row draws in.
fn build_category_badge(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    items: Signal<NewsSnapshot>,
    language: Signal<LanguageCode>,
    theme: ThemeHandle,
    translate: Translate,
) -> Result<(), EngineError> {
    let shell_theme = theme.clone();
    let badge = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_none()
                .unified_sized(UnifiedSize::Small)
                .build(),
        )
        .paint(PaintBuilder::new().rounded(4.0).build())
        .bind_style(move |read| StylePatch {
            background: Some(shell_theme.role(read, Role::SurfaceRaised)),
            border: Some(Border {
                width: 1.0,
                color: shell_theme.role(read, Role::LineBorder),
            }),
            ..StylePatch::default()
        })
        .build(tree, parent)?;

    let text_theme = theme;
    NodeBuilder::text(
        SmolStr::default(),
        TextStyle::new(UnifiedSize::Small.text_size()),
    )
    .bind_text(move |read| {
        let _language: ui_core::node::signal::SignalRef<LanguageCode> = read.read(&language);
        let snapshot = read.read(&items);
        let Some(item) = snapshot.items().get(index) else {
            return SmolStr::default();
        };
        category_label(item.category(), &translate)
    })
    .bind_style(move |read| StylePatch {
        foreground: Some(text_theme.role(read, Role::FgDefault)),
        ..StylePatch::default()
    })
    .build(tree, badge)?;

    Ok(())
}

/// The report's title, if it has one: an empty title still occupies the line's leading
/// space, which is what keeps the badge and the time at their two ends.
fn build_title(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    items: Signal<NewsSnapshot>,
    theme: ThemeHandle,
) -> Result<(), EngineError> {
    let mut style = TextStyle::new(13.0);
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

/// The time of the report, read in the zone the settings display data in on the frame
/// that draws it — moving the setting invalidates exactly the row nodes that read it.
fn build_time(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    items: Signal<NewsSnapshot>,
    config: Signal<app_config::settings::Config>,
    theme: ThemeHandle,
) -> Result<(), EngineError> {
    NodeBuilder::text(SmolStr::default(), TextStyle::new(11.0))
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

/// What the report says.
fn build_content(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    items: Signal<NewsSnapshot>,
    theme: ThemeHandle,
) -> Result<(), EngineError> {
    NodeBuilder::text(SmolStr::default(), TextStyle::new(13.0))
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

/// The name of a kind of news.
fn category_label(category: NewsCategory, translate: &Translate) -> SmolStr {
    match category {
        NewsCategory::General => t_smol!(translate, "widget.news.general"),
        NewsCategory::Crypto => t_smol!(translate, "widget.news.crypto"),
        NewsCategory::Political => t_smol!(translate, "widget.news.political"),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
        sync::Arc,
        time::Duration,
    };

    use app_config::{
        settings::{AccountConfig, AccountWalletKind, Config},
        time_zone::DataTimeZone,
        widget::{VersionedWidgetConfig, decode_or_default},
    };
    use base_ui::theme::appearance::Appearance;
    use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
    use kytrix_service_provider::service::engine::KytrixServiceError;
    use network::transport::error::TransportError;
    use ui_core::{
        app::App,
        event::ActionSource,
        node::signal::SignalScope,
        window::{WindowId, WindowOptions},
    };
    use widget::{
        frame::FrameRouter, protocol::RequestId, services::WidgetServices,
        test_support::WidgetTestApp,
    };

    use super::*;
    use crate::{
        service::NewsStreamError,
        sound::MINIMUM_INTERVAL,
        test_support::{ACCOUNT, FakeFeed, FakeSource, RecordedSound, event},
    };

    /// The translator the product ships: what these tests assert is the catalogue's copy.
    fn product_translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::global::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    /// The framework's headless host, over the product catalogue.
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

    /// Signs `user_id` in the way the composition root does: a settings write.
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

    /// A pane the framework would have built, driven by a fake session and a fake sound.
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

    /// Reports a feed as the stream would, with the binding taken out of the way so the
    /// reported message is the only input the pane has.
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

    /// Reports a status as the stream would.
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

    /// The text a failure the pane reported resolves to.
    fn failure_of(harness: &WidgetTestApp, pane: &Pane) -> SmolStr {
        pane.status
            .state(harness.app())
            .failure()
            .expect("the pane should have reported a failure")
            .text(&harness.services().translate)
    }

    /// What the reports the pane holds say.
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

    /// Builds the body the framework would build once the pane has content.
    fn build_body(harness: &mut WidgetTestApp, pane: &mut Pane) {
        let parent = pane.parent;
        harness
            .with_cx(|cx| pane.widget.body(parent, cx))
            .expect("the body builds");
    }

    /// Lets the engine's frame pipeline run until `ready` holds.
    ///
    /// The headless host's `drain` is the only frame primitive and the widget is not
    /// mounted through `AnyWidget` here (its source is injected), so the test drives
    /// `sync` itself and then drains a frame.
    fn frame(harness: &mut WidgetTestApp, pane: &mut Pane) {
        sync(harness, pane);
        harness.drain();
        harness.drain();
    }

    /// A host whose tasks run on a runtime the test owns.
    ///
    /// The framework's headless host runs on a current-thread runtime, and a synchronous
    /// test cannot drive that runtime's spawned tasks: `TestApp` keeps the runtime to
    /// itself. The live-stream tests therefore build the same context the composition
    /// root builds over a multi-thread runtime this test can block on.
    struct Live {
        app: App,
        runtime: tokio::runtime::Runtime,
        services: WidgetServices,
        window: WindowId,
        scope: SignalScope,
        router: FrameRouter,
    }

    struct LivePane {
        widget: NewsWidget,
        status: WidgetStatus,
    }

    impl Live {
        fn new() -> Self {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("the test runtime builds");
            let mut app = App::new(runtime.handle().clone());
            let window = app.create_window(WindowOptions::default());
            let scope = app.signal_scope();
            let language = app.signal_scoped(scope, LanguageCode::En);
            let config = app.signal_scoped(scope, Config::default());
            let catalog_generation = app.signal_scoped(scope, 0_u64);
            let reduce_motion = app.signal_scoped(scope, false);
            let theme = ThemeHandle::new(&mut app, Appearance::Light);
            let services = WidgetServices {
                translate: product_translate(LanguageCode::En),
                language,
                config,
                theme,
                market_participants: None,
                catalog_generation,
                kytrix_service: None,
                open_url: Rc::new(|_| {}),
                sound: Rc::new(|_| None),
                reduce_motion,
                runtime: runtime.handle().clone(),
            };

            Self {
                app,
                runtime,
                services,
                window,
                scope,
                router: FrameRouter::default(),
            }
        }

        fn with_cx<R>(&mut self, body: impl FnOnce(&mut WidgetCx<'_>) -> R) -> R {
            let mut cx = WidgetCx::new(
                &mut self.app,
                self.window,
                &self.services,
                self.scope,
                self.router.clone(),
            );
            body(&mut cx)
        }

        fn sign_in(&mut self, user_id: &str) {
            let signal = self.services.config;
            let mut config = {
                let reference = self.app.read(&signal);
                (*reference).clone()
            };
            config.account = Some(AccountConfig {
                user_id: user_id.to_owned(),
                wallet_address: "0x0000000000000000000000000000000000000001".to_owned(),
                wallet_kind: AccountWalletKind::Ethereum,
            });
            assert!(self.app.write(&signal, config), "signing in is a change");
        }

        fn sign_out(&mut self) {
            let signal = self.services.config;
            let mut config = {
                let reference = self.app.read(&signal);
                (*reference).clone()
            };
            config.account = None;
            assert!(self.app.write(&signal, config), "signing out is a change");
        }

        fn pane(&mut self, source: FakeSource) -> LivePane {
            let scope = self.scope;
            let status = WidgetStatus::new(&mut self.app, scope);
            let widget = self.with_cx(|cx| {
                NewsWidget::new_with_source(
                    NewsConfig::default(),
                    source,
                    Box::new(SilentSound),
                    status,
                    WidgetConfigChange::default(),
                    WidgetAction::new("common.retry", |_| {}),
                    cx,
                )
            });

            LivePane { widget, status }
        }

        fn load(&mut self, pane: &mut LivePane) {
            self.with_cx(|cx| pane.widget.load(cx));
        }

        fn sync(&mut self, pane: &mut LivePane) {
            self.with_cx(|cx| pane.widget.sync(cx))
                .expect("the widget syncs");
        }

        /// Lets the runtime's tasks run for a moment.
        fn drive(&self) {
            self.runtime.block_on(async {
                tokio::time::sleep(Duration::from_millis(1)).await;
            });
        }

        /// Runs the widget's synchronization and the runtime until `ready` holds.
        fn settle(
            &mut self,
            pane: &mut LivePane,
            what: &str,
            ready: impl Fn(&Live, &LivePane) -> bool,
        ) {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                self.sync(pane);
                self.drive();
                if ready(self, pane) {
                    return;
                }
                assert!(std::time::Instant::now() < deadline, "{what} should happen");
            }
        }
    }

    /// A missing key is an empty string on screen rather than an error, so a gap in the
    /// interface is what a typo looks like: this is the test that turns it into a failure.
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

    /// A pane is heard unless a reader says otherwise, and a stored config that says nothing
    /// about the sound is a pane that has not been asked: it starts as one that is.
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

    /// The words a signed-out pane waits with are the ones the language resolves them in:
    /// the pane hands over a reference into the catalogue, and the sentence is resolved
    /// where it is drawn, so moving the language is the whole of the work — nothing loads
    /// again, and the pane reports nothing again.
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

    /// The state machine the pane reports into: connecting, then reading without content,
    /// then ready once a feed is there.
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

    /// A feed that shares no report with what the pane holds is not one it can be
    /// extended into: every row on screen is news the feed no longer has.
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

        // A recovery keeps the news the reader has, so a reconnect does not blank the
        // screen they were reading.
        load(&mut harness, &mut pane);
        assert!(!pane.status.is_failed(harness.app()));
        assert!(
            pane.widget.has_content(),
            "a recovery reads again without dropping what was read"
        );
        assert_eq!(snapshot_contents(&pane.widget), ["News 1"]);
    }

    /// The recovery an account without news access is offered is the subscription it is
    /// missing, not a retry of the question the service already refused.
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

    /// The recovery goes where a subscription is bought: the URL is the service's own
    /// account origin, opened through the injected command.
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
        let mut live = Live::new();
        let source = FakeSource::default();
        live.sign_in(ACCOUNT);
        let first = source.subscribe();
        let _second = source.subscribe();
        let mut pane = live.pane(source.clone());

        live.load(&mut pane);
        live.settle(&mut pane, "the pane to subscribe", |_, _| {
            source.subscriptions() == 1
        });
        live.runtime.block_on(async {
            first
                .events
                .send(Ok(event(1)))
                .await
                .expect("the live subscription should accept news");
        });
        live.settle(&mut pane, "the feed to reach the pane", |_, pane| {
            snapshot_contents(&pane.widget) == ["News 1"]
        });

        live.sign_in("someone-else");

        live.settle(&mut pane, "the replacement subscription", |_, _| {
            source.subscriptions() == 2
        });
        assert_eq!(pane.widget.account_id.as_deref(), Some("someone-else"));
        assert!(
            snapshot_contents(&pane.widget).is_empty(),
            "another account is another feed, so what was read is dropped"
        );
        live.settle(&mut pane, "the first subscription to be let go", |_, _| {
            first.drops.load(std::sync::atomic::Ordering::SeqCst) == 1
        });
    }

    #[test]
    fn signing_out_should_take_the_feed_with_it() {
        let mut live = Live::new();
        let source = FakeSource::default();
        live.sign_in(ACCOUNT);
        let control = source.subscribe();
        let mut pane = live.pane(source.clone());

        live.load(&mut pane);
        live.settle(&mut pane, "the pane to subscribe", |_, _| {
            source.subscriptions() == 1
        });
        live.runtime.block_on(async {
            control
                .events
                .send(Ok(event(1)))
                .await
                .expect("the live subscription should accept news");
        });
        live.settle(&mut pane, "the feed to reach the pane", |_, pane| {
            snapshot_contents(&pane.widget) == ["News 1"]
        });

        live.sign_out();

        live.settle(&mut pane, "the subscription to be released", |_, _| {
            control.drops.load(std::sync::atomic::Ordering::SeqCst) == 1
        });
        assert!(!pane.widget.has_content());
        assert_eq!(pane.widget.current_scope(), None);
        assert_eq!(
            pane.status.state(&live.app).notice(),
            Some(&Notice::message("widget.news.logged_out"))
        );
    }

    #[test]
    fn the_feed_should_reach_a_live_pane_end_to_end() {
        let mut live = Live::new();
        let source = FakeSource::default();
        live.sign_in(ACCOUNT);
        let control = source.subscribe();
        let mut pane = live.pane(source.clone());

        live.load(&mut pane);
        live.settle(&mut pane, "the pane to subscribe", |_, _| {
            source.subscriptions() == 1
        });
        live.runtime.block_on(async {
            for timestamp in [1, 2, 3] {
                control
                    .events
                    .send(Ok(event(timestamp)))
                    .await
                    .expect("the live subscription should accept news");
            }
        });

        live.settle(&mut pane, "the feed to reach the pane", |_, pane| {
            snapshot_contents(&pane.widget) == ["News 3", "News 2", "News 1"]
        });
        live.settle(
            &mut pane,
            "the subscription to report itself live",
            |live, pane| !pane.status.is_busy(&live.app),
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

    /// A feed nothing the list held survives in is one the body rebuilds for: a list holds
    /// the reports it was given, so the feed a pane draws is the one it has now rather than
    /// the rows of the one before it.
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

    /// An empty feed is content, so the body — not the framework's placeholder — says it
    /// holds nothing, and the stand-in follows the language like every other sentence.
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

    /// The rows are drawn through `bind_text` closures over the language signal, so moving
    /// the language redraws them without the pane reporting anything again.
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

    /// A row's time is read in the settings' display zone on the frame that draws it: the
    /// zone is a signal, so moving it invalidates exactly the rows that read it.
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

    /// The header control is offered for the sound the pane was built with, and it can only
    /// be taken while the host has audio.
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

    /// What taking the control does: it writes the option the pane hands back, and the
    /// control is rebuilt for the state it now shows.
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

    /// A host that answered `None` for the clip is a pane with no sound: the control is
    /// drawn disabled, and a report arriving neither panics nor blocks.
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

    /// A pane that has just opened is not being told news: everything in the feed it is
    /// handed happened before the reader was looking, so nothing there is an arrival.
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

    /// A subscription that is live before it holds anything publishes the empty feed it
    /// holds, which is what a pane normally starts from: the report that fills it is the
    /// first news that pane ever has, and the one a reader most needs telling about.
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

    /// The service publishes a feed whenever it moves, and only one of those publications is
    /// news: the feed a pane already holds is the one that was published again.
    #[test]
    fn a_feed_that_did_not_move_should_not_be_announced() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let (mut pane, sound) = audible_pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();

        let grown = feed.push(1);
        report_feed(&mut harness, &mut pane, grown.clone());
        // The feed the pane is showing, published once more before anything was added
        // to it: the same reports on a feed that has not moved.
        report_feed(&mut harness, &mut pane, grown);

        let added = feed.push(2);
        report_feed(&mut harness, &mut pane, added.clone());
        // And again, once the feed it moved to is the one being published.
        report_feed(&mut harness, &mut pane, added);

        assert_eq!(
            sound.plays(),
            1,
            "the publication that moved the feed is the one a reader is told about"
        );
    }

    /// A service that publishes a run of reports is a reader who is told about the run
    /// rather than about each report in it.
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

    /// The window belongs to the sound that was heard: a run that starts again after it is
    /// news again, which is what keeps the limit from silencing a feed for good.
    #[test]
    fn the_sound_should_be_heard_again_a_window_later() {
        let mut harness = harness();
        sign_in(&mut harness, ACCOUNT);
        let (mut pane, sound) = audible_pane(&mut harness, FakeSource::default());
        let mut feed = FakeFeed::new();

        report_feed(&mut harness, &mut pane, feed.push(1));
        report_feed(&mut harness, &mut pane, feed.push(2));
        // The clock is the pane's to read, so a window that has passed is one a test
        // can say has passed: the sound is what is being tested, not the waiting.
        pane.widget.throttle = Throttle::heard_at(MINIMUM_INTERVAL);
        report_feed(&mut harness, &mut pane, feed.push(3));

        assert_eq!(
            sound.plays(),
            2,
            "a report a window after the last sound is one a reader hears"
        );
    }

    /// What a reader turns off is the sound, not the news: a pane that is quiet still holds
    /// the feed, and it hears the report that arrives after it is turned back on.
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

    /// The list is a component the pane owns: its node goes with the pane's own body, and
    /// its signal scope is released before the framework releases the pane's.
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

    /// A host without the Kytrix service is a failure the pane states itself, and releasing
    /// the mounted widget takes the whole presentation with it.
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
}

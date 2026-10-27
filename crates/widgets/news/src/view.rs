//! The news pane: the feed it follows, and the rows it draws.
//!
//! # What a pane is scoped to
//!
//! One account. The account is the pane's scope and the stream key at once, so the pane
//! never has to decide whether a report belongs to the reader on screen: a message
//! published for an account the pane has left is a message the framework does not
//! deliver, and one that arrives by hand is refused by comparing the key.
//!
//! # What it reports, and what it does not
//!
//! It reports through the framework's status cell and nothing else: a failure is a
//! failure, and the framework decides where that is shown and what the recovery looks
//! like. Nobody signed in is *not* a failure — it is neutral text for a pane that has
//! nothing to show yet, which is what `WidgetState::waiting` is for. A feed with
//! nothing in it is not a state either: a live feed that holds no news is an empty feed,
//! and a pane with an empty feed has content.
//!
//! # Why the list holds the news itself
//!
//! The body is a bounded list of rows of their own height which holds the newest reports and
//! remembers which row a reader is looking at. A newer snapshot is therefore reported to it as
//! the reports it has not been shown rather than as a new set of rows: a list rebuilt from
//! scratch would re-measure every row on screen and, worse, move a reader who had scrolled into
//! the feed back to the top. The one case that does move them is a report that arrives while
//! the newest row is the one they are reading — the newest news belongs on screen, and that is
//! exactly where they are looking.

use std::{convert::Infallible, sync::Arc, time::Instant};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use app_config_provider::{events::AccountChanged, manager::ConfigManager};
use base_ui::{
    badge::Badge,
    button::Button,
    deque_unsized_list::DequeUnsizedList,
    empty_state::EmptyState,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    separator::Separator,
    size::UnifiedSizable,
    style::theme::ThemeExt,
    tooltip::TooltipExt,
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, ElementId, Entity, InteractiveElement, IntoElement,
    ListAlignment, ParentElement, Render, Role, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Task, Window, base::Disableable, div, prelude::FluentBuilder, px,
};
use i18n::t;
use kytrix_service_provider::{
    auth::constants::ACCOUNT_ORIGIN, service::connector::KytrixServiceConnector,
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use widget::{
    account::account_of,
    binding::{WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    error::LocalizedError,
    identity::{WidgetId, WidgetInstanceId},
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    time_zone::{data_time_zone, on_data_time_zone_change},
    view::{WidgetHeader, WidgetView},
};
use widget_tokio_runtime::global;

use super::{
    model::{MAX_NEWS_ITEMS, NewsCategory, NewsItem, NewsSnapshot, has_new_news, unshown},
    service::{
        NewsProtocol, NewsRequest, NewsScope, NewsService, NewsSource, NewsStatus, NewsStreamError,
        NewsStreamMessage,
    },
    sound::{NotificationSound, SoundPlayer, Throttle},
};

/// How far above and below the viewport the list measures rows it cannot see, which is what
/// keeps a row from appearing a moment after it was scrolled to. A report is a badge, a title
/// and a line or two of text, so a row is cheap to measure and a couple of them either side of
/// the viewport is enough.
const LIST_OVERDRAW: f32 = 150.0;

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
    Rpc(RpcCallError<Infallible>),
}

impl LocalizedError for NewsFailure {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Stream(error) => error.localized_message(cx),
            Self::Rpc(error) => error.localized_message(cx),
        }
    }
}

/// One pane: the feed it holds, and the list that draws it.
pub struct NewsWidget {
    /// The options the pane persists, which is whether it may be heard.
    config: NewsConfig,
    binding: WidgetBinding<NewsProtocol>,
    status: Entity<WidgetStatus>,
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
    /// The list itself, which holds the newest reports, measures their rows and remembers
    /// where a reader is.
    list: Entity<DequeUnsizedList<Arc<NewsItem>>>,
    /// The clip a report is announced with, and the device it is heard on.
    sound: Box<dyn NotificationSound>,
    /// How recently this pane was heard, so a run of reports is one sound.
    throttle: Throttle,
    _account: Subscription,
    /// Wakes the pane once its sound has a device, so the control it draws is not a control
    /// for a sound that arrived after the last frame it drew.
    _device_task: Task<()>,
    /// Keeps the pane, and the list that draws its rows, following the display time zone.
    _time_zone: Option<Subscription>,
}

impl WidgetView for NewsWidget {
    type Config = NewsConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.news");
    const DISPLAY_NAME: &'static str = "widget.names.news";
    const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;
    const DESCRIPTION: &'static str = "Live news from the local NewsStream service";
    const ICON: Option<&'static str> = Some("widgets/news.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: Entity<WidgetStatus>,
        _reload: impl Fn(&mut App) + 'static,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = cx.global::<KytrixServiceConnector>().clone();
        Self::new_with_source(config, source, status, config_change, recovery, cx)
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let retry = self.status.is_failed(cx);
        self.account_id = account_of(&cx.global::<ConfigManager>().read());
        self.start(retry, cx);
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

    /// The control that turns the pane's sound on and off.
    ///
    /// What it draws is what the sound says about itself rather than what it was asked to do:
    /// a machine with no output device is a pane whose sound cannot be heard, and a control
    /// that could only fail is not one to offer. The device is opened on the sound's own
    /// thread within moments of the pane being built, and the header is built once per frame,
    /// so the control catches up on the frame after it does — the pane's own repaints are the
    /// clock it reads, and nothing here waits for a device.
    fn header(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> WidgetHeader {
        WidgetHeader::empty().right(self.sound_button(cx))
    }
}

impl NewsWidget {
    /// The seam a test can answer through: `new` only picks the production source.
    fn new_with_source<S>(
        config: NewsConfig,
        source: S,
        status: Entity<WidgetStatus>,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: NewsSource,
    {
        Self::new_with_sound(
            config,
            source,
            Box::new(SoundPlayer::new()),
            status,
            config_change,
            recovery,
            cx,
        )
    }

    /// The seam both the session and the sound are answered through.
    ///
    /// A pane that is built for a test is built for a machine that may not have an output
    /// device, so the sound is handed in rather than opened here: what the pane does with
    /// one is the pane's own behaviour, and it is the same behaviour either way.
    fn new_with_sound<S>(
        config: NewsConfig,
        source: S,
        sound: Box<dyn NotificationSound>,
        status: Entity<WidgetStatus>,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: NewsSource,
    {
        let binding = WidgetBinding::spawn_conditionally(
            global::handle(cx).handle(),
            NewsService::new(source),
            cx,
            Self::apply_reliable,
            Self::apply_latest,
        );
        // The account is the pane's scope, and the user can change it while the pane is
        // open: signing out has to take the feed with it, and signing in as someone else
        // has to read again.
        let events = cx.global::<ConfigManager>().events().clone();
        let account = cx.subscribe(&events, |this, _, _: &AccountChanged, cx| {
            this.reconcile_account(account_of(&cx.global::<ConfigManager>().read()), cx);
        });

        // The device is opened on the sound's own thread, which is a moment no frame knows
        // about: without this, a pane whose device opened while the window was idle would go
        // on drawing a control for a sound that is not there. The wait ends when the device
        // does, and it ends when the sound never has one too — a sound with nothing to hear
        // is not one a pane is ever woken about.
        let mut ready = sound.ready();
        let device_task = cx.spawn(async move |this, cx| {
            let _ = ready.wait_for(|ready| *ready).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        });

        // The rows are built by the list entity, which keeps what it drew until it is
        // told otherwise: moving the display zone has to wake the list itself.
        let time_zone = on_data_time_zone_change(cx, |this, _, cx| {
            this.list.update(cx, |_, cx| cx.notify());
            cx.notify();
        });

        // The list holds the feed itself, so what a reader is reading is the list's to keep:
        // it is told about the reports a snapshot added rather than about the snapshot.
        let list = cx.new(|_| feed_list());

        Self {
            config,
            binding,
            status,
            recovery,
            config_change,
            // The pane records the account it was built for, but it starts nothing: the
            // framework calls `load` once construction is done.
            account_id: account_of(&cx.global::<ConfigManager>().read()),
            snapshot: None,
            ready: false,
            list,
            sound,
            throttle: Throttle::default(),
            _account: account,
            _device_task: device_task,
            _time_zone: time_zone,
        }
    }

    /// The scope this pane follows, which is the account it reads as.
    fn current_scope(&self) -> Option<NewsScope> {
        NewsScope::new(self.account_id.as_ref())
    }

    /// Follows the account the settings now hold.
    ///
    /// Another account is another feed, so the news the pane is showing is dropped
    /// rather than kept under a new name.
    fn reconcile_account(&mut self, account: Option<SmolStr>, cx: &mut Context<Self>) {
        if self.account_id == account {
            return;
        }
        self.account_id = account;
        self.start(false, cx);
    }

    /// Starts reading, or restarts for a pane the user is recovering.
    ///
    /// A recovery keeps the news the user is still reading — blanking a feed to reconnect
    /// would flash it away — while an account change starts over, because the pane cannot
    /// vouch for what it was showing.
    fn start(&mut self, retry: bool, cx: &mut Context<Self>) {
        let Some(scope) = self.current_scope() else {
            self.clear(cx);
            self.report_logged_out(cx);
            // Nothing is followed, so nothing should stay subscribed on this pane's
            // behalf: an account that signed out must not keep a stream it is not using.
            self.binding.call(NewsRequest::Stop, cx, |_, _, _| {});
            cx.notify();
            return;
        };
        self.ready = false;
        if !retry {
            self.clear(cx);
        }
        self.status.report(WidgetState::Connecting, cx);
        self.binding.call(
            if retry {
                NewsRequest::Retry(scope)
            } else {
                NewsRequest::Load(scope)
            },
            cx,
            Self::apply_load_result,
        );
        cx.notify();
    }

    /// Drops the feed a pane is leaving behind.
    ///
    /// A list holds the reports it was given and can only take more of them, so the feed it
    /// held is left behind by building the list again rather than by telling it about a feed it
    /// no longer has.
    fn clear(&mut self, cx: &mut Context<Self>) {
        self.snapshot = None;
        self.ready = false;
        self.list = cx.new(|_| feed_list());
        cx.notify();
    }

    /// Reports the notice a pane nobody is signed in for is waiting with.
    ///
    /// This is the one place that sentence is referenced: [`Self::start`] reports it when the
    /// account goes away.
    fn report_logged_out(&self, cx: &mut Context<Self>) {
        self.status.report(
            WidgetState::waiting(Notice::message("widget.news.logged_out")),
            cx,
        );
    }

    /// The control that decides whether this pane may be heard.
    ///
    /// It shows the choice the pane persists, so the control and the option it writes are
    /// the same thing: a label that says what taking it does, rather than what the pane is
    /// doing now — the icon says that, and so does the tooltip.
    fn sound_button(&self, cx: &Context<Self>) -> AnyElement {
        // A control that is drawn disabled must not be one that acts: the click it was
        // given would run all the same.
        let enabled = self.sound.is_available();
        let icon = match self.config.sound {
            true => IconKind::SpeakerSimpleHigh,
            false => IconKind::SpeakerSimpleSlash,
        };
        let label = self.sound_label(cx);
        let pane = cx.entity().downgrade();

        div()
            .debug_selector(|| "TEST_NEWS_SOUND".into())
            .flex_none()
            // A control in the header row has to take its own clicks: the row is the pane's
            // drag area as well.
            .occlude()
            .child(
                Button::new(ElementId::Name(SharedString::new_static("NewsSound")))
                    .ghost()
                    .small()
                    .disabled(!enabled)
                    .aria_label(label.clone())
                    .with_tooltip(label)
                    .when(enabled, |button| {
                        button.on_click(move |_, _, cx| {
                            if let Some(pane) = pane.upgrade() {
                                pane.update(cx, |pane, cx| pane.toggle_sound(cx));
                            }
                        })
                    })
                    .child(Icon::new(icon).size_3()),
            )
            .into_any_element()
    }

    /// What the sound control says it does, which is the other way round from what the pane
    /// is doing now.
    fn sound_label(&self, cx: &Context<Self>) -> SharedString {
        match self.config.sound {
            true => t!(cx, "widget.news.disable_sound"),
            false => t!(cx, "widget.news.enable_sound"),
        }
    }

    /// Takes the sound the other way, and reports the change the pane now holds.
    ///
    /// The throttle is left where it is: what a reader turns off is the sound, not the clock
    /// it is heard on, and a reader who turns it back on has already waited.
    fn toggle_sound(&mut self, cx: &mut Context<Self>) {
        self.config.sound = !self.config.sound;
        self.config_change.publish(cx);
        cx.notify();
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

    fn apply_load_result(&mut self, result: WidgetRpcResult<NewsProtocol>, cx: &mut Context<Self>) {
        // The answer says the service took the account; what the subscription then
        // reports about itself arrives on the stream, which is where a failure to
        // subscribe is drawn from.
        if let Err(error) = result {
            self.ready = false;
            let recovery = self.recovery.clone();
            self.status.report(
                WidgetState::failed(Arc::new(NewsFailure::Rpc(error)), recovery),
                cx,
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
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_scope().as_ref() != Some(&key) || self.status.is_failed(cx) {
            return false;
        }
        let NewsStreamMessage::Snapshot(snapshot) = message.as_ref() else {
            return false;
        };

        // What arrived is decided before the feed is taken: a feed says what is new by the
        // report it leads with, and the pane is about to stop holding the one before it.
        let arrived = has_new_news(self.snapshot.as_ref(), snapshot);
        self.accept(snapshot.clone(), cx);
        if arrived {
            self.announce();
        }
        if self.ready {
            self.status.report(WidgetState::Ready, cx);
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
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_scope().as_ref() != Some(&key) || self.status.is_failed(cx) {
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
                self.status.report(WidgetState::Connecting, cx);
            }
            NewsStatus::Ready => {
                self.ready = true;
                if self.has_content() {
                    self.status.report(WidgetState::Ready, cx);
                } else {
                    self.status.report(WidgetState::Loading, cx);
                }
            }
            NewsStatus::Failed(error) => {
                self.ready = false;
                self.fail(error, cx);
            }
        }

        false
    }

    /// Reports why a subscription stopped.
    ///
    /// An account without news access is not a broken connection: retrying it asks the
    /// same refused question, so the control offered is where a subscription is bought
    /// instead of a retry.
    fn fail(&mut self, error: &NewsStreamError, cx: &mut Context<Self>) {
        let failure = NewsFailure::Stream(error.clone());
        if matches!(error, NewsStreamError::EntitlementRequired) {
            self.status
                .report(WidgetState::failed(Arc::new(failure), view_plans()), cx);
            return;
        }
        let recovery = self.recovery.clone();
        self.status
            .report(WidgetState::failed(Arc::new(failure), recovery), cx);
    }

    /// Tells the list about the reports this feed added.
    ///
    /// The list holds the newest reports and remembers the one a reader is looking at, so it
    /// is told the reports it has not been shown rather than the feed itself: a list that
    /// keeps the reports it holds where they are is what keeps a reader on the report they are
    /// reading. The one report that does move them is one that arrives while the newest report
    /// is the one they are reading, which is where the newest news belongs — and a feed the
    /// list cannot be extended into is one it holds no report of, which is built for instead.
    fn accept(&mut self, snapshot: NewsSnapshot, cx: &mut Context<Self>) {
        let unshown = unshown(
            self.snapshot.as_ref().map_or(&[], NewsSnapshot::items),
            snapshot.items(),
        );

        match unshown {
            // The oldest of the reports it has not been shown goes in first, so the newest of
            // them ends up where a list that draws its newest report first draws it.
            Some(count) => {
                self.list.update(cx, |list, cx| {
                    for report in snapshot.items()[..count].iter().rev() {
                        list.push(report.clone());
                    }
                    cx.notify();
                });
            }
            // Nothing the list holds survives, so there is no edit it can be told: every report
            // on screen is news the feed no longer has. A list holds the reports it was given
            // and can only take more of them, so the one this pane draws is built again for the
            // feed it has now, and the reader starts over on the report that feed draws first.
            None => {
                let reports: Vec<Arc<NewsItem>> = snapshot.items().to_vec();
                self.list = cx.new(move |_| {
                    let mut list = feed_list();
                    for report in reports.iter().rev() {
                        list.push(report.clone());
                    }
                    list
                });
                // The rows are drawn by the list built here rather than the one it replaced, so
                // the pane is what is told about it.
                cx.notify();
            }
        }

        self.snapshot = Some(snapshot);
    }

    /// The list, which draws the reports the pane holds.
    fn list_body(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .id(ElementId::Name(SharedString::new_static("NewsFrame")))
            .debug_selector(|| "TEST_NEWS_LIST".into())
            .aria_label(t!(cx, "widget.news.scroll_vertical"))
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(self.list.clone())
            .into_any_element()
    }
}

impl Render for NewsWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The body is only embedded while the pane has content, so this is the ready
        // state and nothing else. An empty feed is content — a feed that has published
        // nothing yet — so the pane says it holds no news rather than drawing a list
        // with no rows in it, which reads the same as a list that failed to load.
        let body = match self.snapshot.as_ref().map(NewsSnapshot::items) {
            Some([]) => div()
                .size_full()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .child(EmptyState::new("NewsEmpty", t!(cx, "common.no_data")))
                .into_any_element(),
            _ => self.list_body(cx),
        };

        div().size_full().min_w_0().min_h_0().child(body)
    }
}

/// The list a pane draws its feed with.
///
/// The list holds the feed itself, so what a reader is reading is the list's to keep: it is
/// told about the reports a snapshot added rather than about the snapshot. What it was given is
/// what it holds, which is why a feed that is not one the list it holds can be extended into is
/// a list built again rather than a list told about it.
fn feed_list() -> DequeUnsizedList<Arc<NewsItem>> {
    DequeUnsizedList::new(
        ElementId::Name(SharedString::new_static("NewsList")),
        ListAlignment::Top,
        MAX_NEWS_ITEMS,
        px(LIST_OVERDRAW),
        |index, report: &Arc<NewsItem>, _window, cx| news_row(report, index, cx),
    )
    .separator(|_, _| Separator::horizontal().into_any_element())
    .render_reverse()
    .keep_on_newest()
}

/// One row of the feed: what it is, when it happened, and what it says.
///
/// A row is separated from the one above it rather than framed: a feed is read as a
/// list, and the newest news is the row a reader starts at.
fn news_row(item: &NewsItem, index: usize, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let display = item.display(data_time_zone(cx));
    let category = item.category();
    let title = item.title().cloned();
    // The time belongs at the trailing edge whether or not the report has a title, so the
    // two halves of the line are laid out from its two ends rather than from the badge.
    let meta = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        .w_full()
        .min_w_0()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .flex_1()
                .min_w_0()
                .child(
                    Badge::new()
                        .outline()
                        .small()
                        .child(category_label(category, cx)),
                )
                .children(title.map(|title| {
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(theme.roles.fg.default.hsla)
                        .child(title)
                })),
        )
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(theme.roles.fg.muted.hsla)
                .child(display.time.clone()),
        );

    let row = div()
        .id(ElementId::NamedInteger(
            SharedString::new_static("NewsRow"),
            index as u64,
        ))
        .debug_selector(|| "TEST_NEWS_ROW".into())
        .role(Role::ListItem)
        .aria_label(i18n::t!(
            cx,
            "widget.news.item",
            category = category_label(category, cx),
            time = display.time.clone()
        ))
        .flex()
        .flex_col()
        .gap_2()
        .w_full()
        .min_w_0()
        .px_2()
        .py_2();

    row.child(meta)
        .child(
            div()
                .min_w_0()
                .whitespace_normal()
                .text_color(theme.roles.fg.default.hsla)
                .child(item.content().clone()),
        )
        .into_any_element()
}

/// The name of a kind of news.
fn category_label(category: NewsCategory, cx: &App) -> SharedString {
    match category {
        NewsCategory::General => t!(cx, "widget.news.general"),
        NewsCategory::Crypto => t!(cx, "widget.news.crypto"),
        NewsCategory::Political => t!(cx, "widget.news.political"),
    }
}

/// The recovery an account without news access is offered.
///
/// The subscription is what is missing, so the control goes where one is bought.
fn view_plans() -> WidgetAction {
    WidgetAction::new("widget.news.view_plans", |cx| {
        cx.open_url(ACCOUNT_ORIGIN);
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use app_config::{
        settings::{AccountConfig, AccountWalletKind, Config},
        widget::{VersionedWidgetConfig, decode_or_default},
    };
    use gpui_kit::{
        AppContext, BorrowAppContext, Entity, IntoElement, Pixels, Render, TestAppContext,
        VisualTestContext, Window, div, point, px, size,
    };
    use i18n::language::codes::LanguageCode;
    use kytrix_service_provider::service::engine::KytrixServiceError;
    use network::transport::error::TransportError;
    use tokio::sync::watch;
    use widget::protocol::RequestId;

    use super::*;
    use crate::{
        service::NewsStreamError,
        sound::{MINIMUM_INTERVAL, NotificationSound},
        test_support::{ACCOUNT, FakeFeed, FakeSource, RecordedSound, event, settle_until},
    };

    /// Settings holding `account`, or holding nobody when it is `None`.
    fn settings(account: Option<&str>) -> Config {
        let mut config = Config::default();
        config.account = account.map(|user_id| AccountConfig {
            user_id: user_id.to_owned(),
            wallet_address: "0x0000000000000000000000000000000000000001".to_owned(),
            wallet_kind: AccountWalletKind::Ethereum,
        });
        config
    }

    /// Installs everything a pane reads, with `account` signed in or nobody.
    fn install(cx: &mut App, account: Option<&str>) {
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
        global::install(cx).expect("the test Tokio runtime should build");
        let manager = ConfigManager::in_memory(settings(account), cx);
        cx.set_global(manager);
    }

    /// A pane the framework would have built, driven by a fake session and a fake sound.
    ///
    /// The fake answers nothing unless a test queues it an answer: a source that
    /// answered on its own would race the tests, because its service publishes on a task
    /// of its own.
    fn pane_with(
        sound: Box<dyn NotificationSound>,
        source: FakeSource,
        cx: &mut Context<NewsWidget>,
    ) -> NewsWidget {
        let status = cx.new(|_| WidgetStatus::new());
        let weak = cx.weak_entity();
        let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
            let _ = weak.update(cx, |pane, cx| pane.load(cx));
        });
        NewsWidget::new_with_sound(
            NewsConfig::default(),
            source,
            sound,
            status,
            WidgetConfigChange::default(),
            recovery,
            cx,
        )
    }

    /// A pane created the way the framework creates one: state, then one load.
    fn pane(cx: &mut TestAppContext, source: FakeSource) -> Entity<NewsWidget> {
        cx.executor().allow_parking();
        let pane = cx.update(|cx| {
            install(cx, Some(ACCOUNT));
            cx.new(|cx| pane_with(Box::new(Sound::default()), source, cx))
        });
        pane.update(cx, |pane, cx| pane.load(cx));
        pane
    }

    /// A pane whose sound a test can read back.
    fn pane_with_sound(cx: &mut TestAppContext, source: FakeSource) -> (Entity<NewsWidget>, Sound) {
        let sound = Sound::default();
        cx.executor().allow_parking();
        let pane = cx.update(|cx| {
            install(cx, Some(ACCOUNT));
            cx.new(|cx| pane_with(Box::new(sound.clone()), source, cx))
        });
        pane.update(cx, |pane, cx| pane.load(cx));
        (pane, sound)
    }

    /// A pane nobody is signed in for.
    fn signed_out_pane(cx: &mut TestAppContext, source: FakeSource) -> Entity<NewsWidget> {
        cx.executor().allow_parking();
        let pane = cx.update(|cx| {
            install(cx, None);
            cx.new(|cx| pane_with(Box::new(Sound::default()), source, cx))
        });
        pane.update(cx, |pane, cx| pane.load(cx));
        pane
    }

    /// The sound a pane was built with, as a test drives it.
    ///
    /// It records what it was asked for and nothing else: whether a clip is audible is the
    /// device's business, and what a pane decides is whether to ask. What it holds of the
    /// device is what the pane reads to know whether it may offer the sound at all.
    #[derive(Clone, Default)]
    struct Sound {
        recorded: Arc<RecordedSound>,
        /// The device, which a test opens and closes the way the sound's own thread does.
        device: watch::Sender<bool>,
    }

    impl Sound {
        /// One entry per play the pane asked for.
        fn plays(&self) -> usize {
            self.recorded.plays()
        }

        /// Says whether this sound is one a device could be heard on.
        fn set_available(&self, available: bool) {
            self.device.send_replace(available);
        }
    }

    impl NotificationSound for Sound {
        fn play(&self) {
            NotificationSound::play(&self.recorded);
        }

        fn is_available(&self) -> bool {
            *self.device.borrow()
        }

        /// The device a test opened is one a pane may be woken for; a test that has not
        /// opened one yet leaves the wait where it is, which is what a real machine does too.
        fn ready(&self) -> watch::Receiver<bool> {
            self.device.subscribe()
        }
    }

    /// Reports a feed as the stream would.
    ///
    /// The binding is cancelled first, which is what makes the reported message the only
    /// input the pane has.
    fn report_feed(pane: &Entity<NewsWidget>, feed: NewsSnapshot, cx: &mut App) -> bool {
        let scope = pane
            .read(cx)
            .current_scope()
            .expect("a loaded pane follows an account");
        pane.update(cx, |pane, cx| {
            pane.binding.cancel();
            pane.apply_latest(scope, Arc::new(NewsStreamMessage::Snapshot(feed)), cx)
        })
    }

    /// Reports a status as the stream would.
    fn report_status(pane: &Entity<NewsWidget>, status: NewsStatus, cx: &mut App) -> bool {
        let scope = pane
            .read(cx)
            .current_scope()
            .expect("a loaded pane follows an account");
        pane.update(cx, |pane, cx| {
            pane.apply_reliable(scope, Arc::new(NewsStreamMessage::Status(status)), cx)
        })
    }

    /// The text of a failure the pane reported.
    fn failure_of(pane: &Entity<NewsWidget>, cx: &App) -> SharedString {
        pane.read(cx)
            .status
            .state(cx)
            .failure()
            .expect("the pane should have reported a failure")
            .text(cx)
    }

    /// What the reports the pane holds say, which is what its list is drawn from.
    ///
    /// The list is not read: what it holds is the list's own, and what a reader sees of it is
    /// asserted by drawing the pane.
    fn snapshot_contents_of(pane: &NewsWidget) -> Vec<String> {
        pane.snapshot
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

    /// Draws a pane in a frame of its own, at a size a feed fits in.
    ///
    /// The frame is drawn with every cache missed, so a debug selector read after it reports what
    /// this frame painted rather than what an earlier frame left behind (AGENTS.md §10):
    /// the rows of the feed are drawn inside the body, which is the one cached layer.
    fn draw_pane(window: &mut VisualTestContext, pane: &Entity<NewsWidget>) {
        window.update(|window, _| window.refresh());
        let pane = pane.clone();
        window.draw(
            point(Pixels::ZERO, Pixels::ZERO),
            size(px(400.0), px(400.0)),
            move |_, _| pane.into_any_element(),
        );
    }

    /// A missing key is an empty string on screen rather than an error, so a gap in the
    /// interface is what a typo looks like: this is the test that turns it into a failure.
    #[gpui_kit::test]
    fn every_message_this_pane_shows_should_resolve_to_text(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(cx, Some(ACCOUNT));

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
                assert!(!t!(cx, key).is_empty(), "{key} has no copy");
            }
            assert!(
                !t!(
                    cx,
                    "widget.news.item",
                    category = t!(cx, "widget.news.general"),
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
                    !failure.localized_message(cx).is_empty(),
                    "{failure:?} has no copy"
                );
            }
        });
    }

    #[test]
    fn the_widget_should_have_a_stable_identity() {
        assert_eq!(NewsWidget::ID.as_str(), "kytrix.news");
        assert_eq!(NewsWidget::DISPLAY_NAME, "widget.names.news");
        assert_eq!(NewsConfig::SCHEMA_VERSION, WidgetConfigSchemaVersion::V1);
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

    #[gpui_kit::test]
    fn a_signed_out_pane_should_say_what_it_is_waiting_for(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let pane = signed_out_pane(cx, source.clone());

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(!pane.has_content());
            assert_eq!(pane.current_scope(), None);
            assert_eq!(
                pane.status.state(cx).notice(),
                Some(&Notice::message("widget.news.logged_out")),
                "a pane nobody signed in for is not a failure"
            );
            assert!(!pane.status.state(cx).is_failed());
        });
        assert_eq!(
            source.subscriptions(),
            0,
            "a pane nobody reads for subscribes to nothing"
        );
    }

    /// The words a signed-out pane waits with are the ones the window's language resolves them
    /// in: the pane hands over a reference into the catalogue, and the sentence is resolved
    /// where it is drawn, so setting the language global is the whole of the work — nothing
    /// loads again, and the pane reports nothing again.
    #[gpui_kit::test]
    fn a_signed_out_pane_should_follow_the_language(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let pane = signed_out_pane(cx, source);

        cx.update(|cx| {
            let notice = pane
                .read(cx)
                .status
                .state(cx)
                .notice()
                .cloned()
                .expect("a pane nobody signed in for waits");
            assert_eq!(notice, Notice::message("widget.news.logged_out"));
            let copy = notice.resolve(cx);
            assert_eq!(copy.as_ref(), t!(cx, "widget.news.logged_out").as_ref());
            assert!(
                copy.contains("log in"),
                "the notice shows the language that was installed: {copy}"
            );
        });

        cx.update(|cx| cx.set_global(LanguageCode::ZhCn));

        cx.update(|cx| {
            let notice = pane
                .read(cx)
                .status
                .state(cx)
                .notice()
                .cloned()
                .expect("a pane nobody signed in for still waits");
            assert_eq!(notice, Notice::message("widget.news.logged_out"));
            let copy = notice.resolve(cx);
            assert_eq!(copy.as_ref(), t!(cx, "widget.news.logged_out").as_ref());
            assert!(
                copy.contains("登陆"),
                "the same reference resolves in the language showing now: {copy}"
            );
        });
    }

    #[gpui_kit::test]
    fn a_feed_should_become_content_and_readiness_should_follow_it(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source);
        let mut feed = FakeFeed::new();

        cx.update(|cx| {
            assert!(!pane.read(cx).has_content());
            assert!(
                pane.read(cx).status.is_busy(cx),
                "a pane that has read nothing is still connecting"
            );
            assert!(
                report_feed(&pane, feed.push(2), cx),
                "a feed is what the pane draws"
            );
            assert!(
                pane.read(cx).has_content(),
                "content is the feed, not the subscription's own report"
            );
            assert!(
                pane.read(cx).status.is_busy(cx),
                "readiness is not the feed's to decide"
            );

            report_status(&pane, NewsStatus::Ready, cx);
            let read = pane.read(cx);
            assert!(!read.status.is_busy(cx), "a live subscription is done");
            assert_eq!(snapshot_contents_of(read), ["News 2"]);
        });
    }

    #[gpui_kit::test]
    fn a_live_subscription_without_a_feed_should_still_be_reading(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source);

        cx.update(|cx| {
            report_status(&pane, NewsStatus::Ready, cx);

            let read = pane.read(cx);
            assert!(!read.has_content());
            assert_eq!(
                read.status.state(cx),
                WidgetState::Loading,
                "a live subscription with nothing to show is still reading"
            );
            assert!(!read.status.state(cx).is_failed());
        });
    }

    #[gpui_kit::test]
    fn an_empty_feed_should_still_be_content(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source);

        cx.update(|cx| {
            report_feed(&pane, FakeFeed::new().snapshot(), cx);
            report_status(&pane, NewsStatus::Ready, cx);

            let pane = pane.read(cx);
            assert!(pane.has_content(), "a feed with nothing in it is a feed");
            assert!(
                pane.status.state(cx).notice().is_none(),
                "the feed says it holds no news itself rather than through a placeholder"
            );
            assert!(!pane.status.is_busy(cx));
        });
    }

    /// A reader who has scrolled into the feed is reading a row, and a report that arrives
    /// above them belongs above them: the list is told the reports it has not been shown, so
    /// the row at the top of the viewport is still the one they were reading.
    #[gpui_kit::test]
    fn a_feed_nothing_of_which_survives_should_be_replaced(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source);
        let mut feed = FakeFeed::new();

        cx.update(|cx| {
            report_feed(&pane, feed.push(1), cx);
            // A feed that shares no report with what the pane holds is not one it can be
            // extended into: every row on screen is news the feed no longer has.
            report_feed(&pane, FakeFeed::new().push(9), cx);

            let pane = pane.read(cx);
            assert_eq!(snapshot_contents_of(pane), ["News 9"]);
        });
    }

    #[gpui_kit::test]
    fn a_report_for_an_account_the_pane_left_should_be_ignored(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source);
        let mut feed = FakeFeed::new();

        cx.update(|cx| {
            report_feed(&pane, feed.push(1), cx);
            let other = NewsScope::new(Some(&SmolStr::new_static("someone-else")))
                .expect("another account is another scope");
            pane.update(cx, |pane, cx| {
                assert!(
                    !pane.apply_latest(
                        other.clone(),
                        Arc::new(NewsStreamMessage::Snapshot(feed.push(2))),
                        cx
                    ),
                    "a feed published for another account is not this pane's to draw"
                );
                assert!(!pane.apply_reliable(
                    other,
                    Arc::new(NewsStreamMessage::Status(NewsStatus::Ready)),
                    cx
                ));
            });

            let pane = pane.read(cx);
            assert_eq!(snapshot_contents_of(pane), ["News 1"]);
            assert_eq!(
                pane.snapshot
                    .as_ref()
                    .map(NewsSnapshot::items)
                    .map(<[_]>::len),
                Some(1)
            );
        });
    }

    #[gpui_kit::test]
    fn a_failed_pane_should_ignore_late_reports_until_it_is_loaded_again(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source);
        let mut feed = FakeFeed::new();

        cx.update(|cx| {
            report_feed(&pane, feed.push(1), cx);
            report_status(&pane, NewsStatus::Ready, cx);
            report_status(
                &pane,
                NewsStatus::Failed(NewsStreamError::Transport(TransportError::Closed)),
                cx,
            );
            assert!(pane.read(cx).status.is_failed(cx));

            assert!(
                !report_feed(&pane, feed.push(2), cx),
                "a pane that has failed is finished with the feed it was reading"
            );
            assert_eq!(snapshot_contents_of(pane.read(cx)), ["News 1"]);

            // A recovery keeps the news the reader has, so a reconnect does not blank the
            // screen they were reading.
            pane.update(cx, |pane, cx| pane.load(cx));
            assert!(!pane.read(cx).status.is_failed(cx));
            assert!(
                pane.read(cx).has_content(),
                "a recovery reads again without dropping what was read"
            );
            assert_eq!(snapshot_contents_of(pane.read(cx)), ["News 1"]);
        });
    }

    /// The recovery an account without news access is offered is the subscription it is
    /// missing, not a retry of the question the service already refused.
    #[gpui_kit::test]
    fn a_missing_entitlement_should_offer_the_subscription_instead_of_a_retry(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source);

        cx.update(|cx| {
            report_status(
                &pane,
                NewsStatus::Failed(NewsStreamError::EntitlementRequired),
                cx,
            );
            assert_eq!(
                failure_of(&pane, cx).as_ref(),
                t!(cx, "error.news.entitlement_required").as_ref()
            );
        });

        let action = cx
            .update(|cx| {
                pane.read(cx)
                    .status
                    .state(cx)
                    .failure()
                    .map(|failure| failure.action().clone())
            })
            .expect("the refusal should offer a recovery");
        let expected = cx.update(|cx| t!(cx, "widget.news.view_plans").to_string());
        assert_eq!(
            cx.update(|cx| action.label(cx)).as_ref(),
            expected,
            "the control is where a subscription is bought"
        );

        cx.update(|cx| action.run(cx));
        assert_eq!(cx.opened_url().as_deref(), Some(ACCOUNT_ORIGIN));
    }

    #[gpui_kit::test]
    fn an_account_change_should_read_the_new_account(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let first = source.subscribe();
        let _second = source.subscribe();
        let pane = pane(cx, source.clone());

        settle_until(cx, "the pane to subscribe", |_| source.subscriptions() == 1);
        // The feed is pushed through the live subscription rather than reported by hand:
        // the pane has to keep its service for the account change to reach it.
        let handle = cx.update(|cx| global::handle(cx).handle().clone());
        handle.block_on(async {
            first
                .events
                .send(Ok(event(1)))
                .await
                .expect("the live subscription should accept news");
        });
        settle_until(cx, "the feed to reach the pane", |cx| {
            snapshot_contents_of(pane.read(cx)) == ["News 1"]
        });

        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(
                    |config| {
                        config.account = Some(AccountConfig {
                            user_id: "someone-else".to_owned(),
                            wallet_address: "0x0000000000000000000000000000000000000002".to_owned(),
                            wallet_kind: AccountWalletKind::Ethereum,
                        });
                    },
                    cx,
                );
            });
        });

        settle_until(cx, "the replacement subscription", |_| {
            source.subscriptions() == 2
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let read = pane.read(cx);
            assert_eq!(read.account_id.as_deref(), Some("someone-else"));
            assert!(
                snapshot_contents_of(read).is_empty(),
                "another account is another feed, so what was read is dropped"
            );
        });
        settle_until(cx, "the first subscription to be let go", |_| {
            first.drops.load(Ordering::SeqCst) == 1
        });
    }

    #[gpui_kit::test]
    fn signing_out_should_take_the_feed_with_it(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let control = source.subscribe();
        let pane = pane(cx, source);
        let mut feed = FakeFeed::new();

        cx.update(|cx| {
            report_feed(&pane, feed.push(1), cx);
            pane.update(cx, |pane, cx| pane.reconcile_account(None, cx));

            let pane = pane.read(cx);
            assert!(!pane.has_content());
            assert_eq!(pane.current_scope(), None);
            assert_eq!(
                pane.status.state(cx).notice(),
                Some(&Notice::message("widget.news.logged_out"))
            );
        });
        settle_until(cx, "the subscription to be released", |_| {
            control.drops.load(Ordering::SeqCst) == 1
        });
    }

    #[gpui_kit::test]
    fn the_feed_should_reach_a_live_pane_end_to_end(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let control = source.subscribe();
        let pane = pane(cx, source.clone());

        settle_until(cx, "the pane to subscribe", |_| source.subscriptions() == 1);
        let handle = cx.update(|cx| global::handle(cx).handle().clone());
        handle.block_on(async {
            for timestamp in [1, 2, 3] {
                control
                    .events
                    .send(Ok(event(timestamp)))
                    .await
                    .expect("the live subscription should accept news");
            }
        });

        settle_until(cx, "the feed to reach the pane", |cx| {
            snapshot_contents_of(pane.read(cx)) == ["News 3", "News 2", "News 1"]
        });
        settle_until(cx, "the subscription to report itself live", |cx| {
            !pane.read(cx).status.is_busy(cx)
        });
        cx.update(|cx| assert!(pane.read(cx).has_content()));
    }

    #[gpui_kit::test]
    fn the_body_should_draw_the_rows_of_the_feed(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source);
        let mut feed = FakeFeed::new();
        let window = cx.add_empty_window();

        draw_pane(window, &pane);
        assert!(
            window.debug_bounds("TEST_NEWS_LIST").is_some(),
            "the list is what a feed is drawn in"
        );
        assert!(
            window.debug_bounds("TEST_NEWS_ROW").is_none(),
            "a feed with nothing in it draws no rows"
        );

        window.update(|_, cx| {
            report_feed(&pane, feed.push(2), cx);
            report_feed(&pane, feed.push(1), cx);
        });
        draw_pane(window, &pane);

        let list = window
            .debug_bounds("TEST_NEWS_LIST")
            .expect("the list should be drawn");
        let row = window
            .debug_bounds("TEST_NEWS_ROW")
            .expect("the feed should draw its rows");
        assert!(row.size.height > px(0.0), "a row is as tall as its text");
        assert!(
            row.top() >= list.top() && row.bottom() <= list.bottom(),
            "a row is drawn inside the list that holds it"
        );
    }

    /// A feed nothing the list held survives in is one the list is built again for: a list holds
    /// the reports it was given, so the feed a pane draws is the one it has now rather than the
    /// rows of the one before it.
    #[gpui_kit::test]
    fn a_feed_nothing_of_which_survives_should_be_held_by_a_list_built_again(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source);
        let mut feed = FakeFeed::new();

        let held = cx.update(|cx| {
            report_feed(&pane, feed.push(1), cx);
            report_feed(&pane, feed.push(2), cx);
            pane.read(cx).list.entity_id()
        });

        let built = cx.update(|cx| {
            report_feed(&pane, FakeFeed::new().push(9), cx);

            assert_eq!(
                snapshot_contents_of(pane.read(cx)),
                ["News 9"],
                "the pane holds the feed it was given"
            );

            pane.read(cx).list.entity_id()
        });

        assert_ne!(
            held, built,
            "the list is built again for a feed none of what it held survives in"
        );
    }

    /// A pane that has just opened is not being told news: everything in the feed it is
    /// handed happened before the reader was looking, so nothing there is an arrival.
    #[gpui_kit::test]
    fn opening_a_pane_on_a_feed_should_not_make_a_sound(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let (pane, sound) = pane_with_sound(cx, source);
        let mut feed = FakeFeed::new();

        cx.update(|cx| {
            report_feed(&pane, feed.push(3), cx);
            report_status(&pane, NewsStatus::Ready, cx);
        });

        assert_eq!(
            sound.plays(),
            0,
            "the feed a pane opens on is what the feed is, not what arrived"
        );
    }

    /// A subscription that is live before it holds anything publishes the empty feed it
    /// holds, which is what a pane normally starts from: the report that fills it is the
    /// first news that pane ever has, and the one a reader most needs telling about.
    #[gpui_kit::test]
    fn the_first_report_a_live_feed_publishes_should_make_a_sound(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let (pane, sound) = pane_with_sound(cx, source);
        let mut feed = FakeFeed::new();

        cx.update(|cx| {
            report_feed(&pane, feed.snapshot(), cx);
            report_status(&pane, NewsStatus::Ready, cx);
            assert_eq!(sound.plays(), 0, "an empty feed has nothing to announce");

            report_feed(&pane, feed.push(1), cx);
        });

        assert_eq!(
            sound.plays(),
            1,
            "the report that filled a live feed is news a reader has not seen"
        );
    }

    #[gpui_kit::test]
    fn a_report_should_be_announced_with_the_notification_sound(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let (pane, sound) = pane_with_sound(cx, source);
        let mut feed = FakeFeed::new();

        cx.update(|cx| {
            report_feed(&pane, feed.push(1), cx);
            report_status(&pane, NewsStatus::Ready, cx);
            report_feed(&pane, feed.push(2), cx);
        });

        assert_eq!(
            sound.plays(),
            1,
            "the report a reader has not seen is heard"
        );
    }

    /// The service publishes a feed whenever it moves, and only one of those publications is
    /// news: the feed a pane already holds is the one that was published again.
    #[gpui_kit::test]
    fn a_feed_that_did_not_move_should_not_be_announced(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let (pane, sound) = pane_with_sound(cx, source);
        let mut feed = FakeFeed::new();

        cx.update(|cx| {
            let grown = feed.push(1);
            report_feed(&pane, grown.clone(), cx);
            // The feed the pane is showing, published once more before anything was added
            // to it: the same reports on a feed that has not moved.
            report_feed(&pane, grown, cx);

            let added = feed.push(2);
            report_feed(&pane, added.clone(), cx);
            // And again, once the feed it moved to is the one being published.
            report_feed(&pane, added, cx);
        });

        assert_eq!(
            sound.plays(),
            1,
            "the publication that moved the feed is the one a reader is told about"
        );
    }

    /// A service that publishes a run of reports is a reader who is told about the run
    /// rather than about each report in it.
    #[gpui_kit::test]
    fn a_run_of_reports_should_be_announced_once(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let (pane, sound) = pane_with_sound(cx, source);
        let mut feed = FakeFeed::new();

        cx.update(|cx| {
            report_feed(&pane, feed.push(1), cx);
            for timestamp in 2..=6 {
                report_feed(&pane, feed.push(timestamp), cx);
            }
        });

        assert_eq!(
            sound.plays(),
            1,
            "five reports inside one window are one sound"
        );
    }

    /// The window belongs to the sound that was heard: a run that starts again after it is
    /// news again, which is what keeps the limit from silencing a feed for good.
    #[gpui_kit::test]
    fn the_sound_should_be_heard_again_a_window_later(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let (pane, sound) = pane_with_sound(cx, source);
        let mut feed = FakeFeed::new();

        cx.update(|cx| {
            report_feed(&pane, feed.push(1), cx);
            report_feed(&pane, feed.push(2), cx);
            // The clock is the pane's to read, so a window that has passed is one a test
            // can say has passed: the sound is what is being tested, not the waiting.
            pane.update(cx, |pane, _| {
                pane.throttle = Throttle::heard_at(MINIMUM_INTERVAL);
            });
            report_feed(&pane, feed.push(3), cx);
        });

        assert_eq!(
            sound.plays(),
            2,
            "a report a window after the last sound is one a reader hears"
        );
    }

    /// What a reader turns off is the sound, not the news: a pane that is quiet still holds
    /// the feed, and it hears the report that arrives after it is turned back on.
    #[gpui_kit::test]
    fn a_silent_pane_should_not_be_heard_and_should_hear_again_when_it_is_turned_back_on(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let (pane, sound) = pane_with_sound(cx, source);
        let mut feed = FakeFeed::new();

        cx.update(|cx| {
            report_feed(&pane, feed.push(1), cx);
            pane.update(cx, |pane, cx| pane.toggle_sound(cx));
            assert!(
                !pane.read(cx).config.sound,
                "the control is what writes the option"
            );
            report_feed(&pane, feed.push(2), cx);
            assert_eq!(
                sound.plays(),
                0,
                "a reader who turned the sound off hears nothing"
            );

            pane.update(cx, |pane, cx| pane.toggle_sound(cx));
            report_feed(&pane, feed.push(3), cx);
        });

        assert_eq!(
            sound.plays(),
            1,
            "the report after the sound is turned back on is heard"
        );
    }

    /// The control is offered for the sound the pane was built with, and only when there is
    /// a device to hear it on: a machine with none is a pane whose control is not one that
    /// could do anything.
    ///
    /// What says it cannot be taken is the same fact that keeps its click off it — the
    /// control is given a handler only while the sound is one a device could play — and the
    /// click itself is not asserted here: a press on this control puts the pointer over the
    /// tooltip it carries, and that overlay fades on a clock a test never advances.
    #[gpui_kit::test]
    fn the_sound_control_should_follow_the_device_and_the_option(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let (pane, sound) = pane_with_sound(cx, source);

        assert!(
            cx.read(|cx| pane.read(cx).config().expect("persisted").sound),
            "a pane starts as one that may be heard"
        );

        sound.set_available(false);
        let window = header_window(cx, &pane);
        assert!(
            window.debug_bounds("TEST_NEWS_SOUND").is_some(),
            "the control is in the header's right group whether or not the sound can be heard"
        );
        assert!(
            !cx.read(|cx| pane.read(cx).sound.is_available()),
            "a pane whose device has not opened reads its sound as one that cannot be heard"
        );

        sound.set_available(true);
        assert!(
            cx.read(|cx| pane.read(cx).sound.is_available()),
            "the device the sound thread opened is the one the pane reads"
        );
    }

    /// What taking the control does: it writes the option the pane hands back, and the words
    /// it offers are then the other way round.
    #[gpui_kit::test]
    fn taking_the_sound_control_should_take_the_sound_the_other_way(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let (pane, sound) = pane_with_sound(cx, source);
        sound.set_available(true);
        let off = cx.update(|cx| t!(cx, "widget.news.disable_sound"));
        let on = cx.update(|cx| t!(cx, "widget.news.enable_sound"));
        let label = |cx: &mut App| pane.update(cx, |pane, cx| pane.sound_label(cx));

        assert_eq!(
            cx.update(label).as_ref(),
            off.as_ref(),
            "an audible pane offers to turn its sound off"
        );

        cx.update(|cx| pane.update(cx, |pane, cx| pane.toggle_sound(cx)));
        assert!(
            !cx.read(|cx| pane.read(cx).config().expect("persisted").sound),
            "taking the control is what writes the option the pane hands back"
        );
        assert_eq!(
            cx.update(label).as_ref(),
            on.as_ref(),
            "a silent pane offers to turn its sound on"
        );
    }

    /// A window whose root is the pane's own header group.
    ///
    /// A header is the pane's to contribute and the framework's to place, so what a test can
    /// draw is the group the pane put there: nothing outside a frame can call `header`.
    fn header_window<'a>(
        cx: &'a mut TestAppContext,
        pane: &Entity<NewsWidget>,
    ) -> &'a mut VisualTestContext {
        let pane = pane.clone();
        let (_, window) = cx.add_window_view(move |_, _| HeaderOf { pane });
        window.simulate_resize(size(px(160.0), px(40.0)));
        window.run_until_parked();

        window
    }

    /// The pane's header group, which is where its control is drawn.
    ///
    /// The control is the pane's own — this is the frame it is drawn in, not a copy of it —
    /// and it is drawn as a view because that is what a control that opens a tooltip needs:
    /// the tooltip reads the view it is drawn for, and an element with no view behind it has
    /// no frame to be drawn in.
    struct HeaderOf {
        pane: Entity<NewsWidget>,
    }

    impl Render for HeaderOf {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .children(
                    self.pane
                        .update(cx, |pane, cx| pane.header(window, cx))
                        .right,
                )
        }
    }
}

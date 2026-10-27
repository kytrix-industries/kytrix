//! The tweet-monitor pane: the timeline it follows, and the accounts it manages.
//!
//! # What a pane is scoped to
//!
//! One account. The account is the pane's scope and the stream key at once, so the pane never
//! has to decide whether a tweet belongs to the account on screen: a message published for an
//! account the pane has left is a message the framework does not deliver, and one that arrives
//! by hand is refused by comparing the key.
//!
//! # The pane owns the accounts, the service owns the session
//!
//! Following a timeline and managing the accounts it comes from are two things the same
//! account is asked for, and the same service answers both with the same session. So the pane
//! is what waits for both, and the accounts the dialog draws are the pane's rather than the
//! dialog's: they outlive any one dialog, and a dialog that held them would lose them on the
//! next open.
//!
//! What a dialog does hold is the search of X it runs for one more account. A search is a
//! procedure of that dialog's own page rather than a change to the accounts, so what it found
//! belongs to the dialog that asked and dies with it — which is the opposite of what the
//! accounts need, and the reason the two are kept apart.
//!
//! # The binding carries one call at a time
//!
//! A request reaches the service through the pane's binding, and the binding replaces the call
//! it holds with the next one. A search and a change therefore cannot both be out, and it is
//! the pane that refuses whichever is asked for second — while a call the stream itself asks
//! for (a read as the stream comes up, an avatar watched for a dialog that opened) is not the
//! dialog's to refuse, and closes out the search it displaces instead of leaving a page waiting
//! for an answer that can no longer arrive.
//!
//! # What a change the pane stopped waiting for means
//!
//! A request the pane stops waiting for — because the user retried, or because the account
//! changed under it — is dropped rather than decided: whether the service made the change is
//! not something the pane can know, and it does not guess. What settles it is the next read,
//! which is why a dialog that is open reads the accounts again as soon as the stream that
//! answers reads is live.
//!
//! # Why the timeline is a list of measured rows
//!
//! A tweet is a paragraph, a gallery and a quoted tweet, so its height is the sum of things
//! only the text system can measure. The body is therefore a bounded list of rows of their own
//! height which holds the newest tweets and remembers which row a reader is looking at. A newer
//! timeline is reported to it as the tweets it has not been shown rather than as a new set of
//! rows: a list rebuilt from scratch would re-measure every row on screen and, worse, move a
//! reader who had scrolled into the timeline back to the top. The one case that does move them
//! is a tweet that arrives while the newest row is the one they are reading — the newest tweet
//! belongs on screen, and that is exactly where they are looking.
//!
//! # Pictures are drawn from what the stream said
//!
//! A row draws a picture from the state the service published with the tweets it belongs to,
//! never by fetching anything itself: what a frame shows is one message, and a picture whose
//! bytes are not on disk yet is a tile that says so. The one thing a row can ask for is that a
//! picture which failed be fetched again, which is the same request the service already knows
//! how to answer.

use std::sync::Arc;

use ahash::AHashMap as HashMap;
use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use app_config_provider::{events::AccountChanged, manager::ConfigManager};
use base_ui::{
    button::Button,
    deque_unsized_list::DequeUnsizedList,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    separator::Separator,
    size::UnifiedSizable,
    style::theme::{Theme, ThemeExt},
    tooltip::{Tooltip, TooltipExt},
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, ElementId, Entity, FontWeight, Hsla, InteractiveElement,
    InteractiveText, IntoElement, ListAlignment, ObjectFit, ParentElement, Render, Role,
    SharedString, StatefulInteractiveElement, Styled, StyledImage, StyledText, Subscription,
    WeakEntity, Window, base::Disableable, div, img, px,
};
use i18n::t;
use kytrix_service_provider::service::{
    connector::KytrixServiceConnector,
    x::{
        XAccountMonitoringApiError, XAccountMonitoringMutationOutcome,
        XAccountMonitoringMutationUnknown, XAccountMonitoringSelectionStatus,
    },
};
use network::transport::hub::{TransportHub, TransportPolicy};
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use time::OffsetDateTime;
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
    dialog::{DialogState, SearchAnswer, TweetDialog},
    media::{MediaDownloader, SharedTransport},
    model::{
        MAX_TWEETS, MediaState, MediaStates, SelectionData, TweetAge, TweetItem, TweetMediaItem,
        TweetReferenceItem, TweetSnapshot, retained_hints, unshown,
    },
    service::{
        TweetProtocol, TweetRequest, TweetResponse, TweetScope, TweetService, TweetSource,
        TweetStatus, TweetStreamError, TweetStreamMessage, TweetUpdate,
    },
};

/// How far above and below the viewport the list measures rows it cannot see, which is what
/// keeps a row from appearing a moment after it was scrolled to. A tweet is a paragraph, a
/// gallery and sometimes a quoted tweet, so a row is tall and expensive to measure: eight
/// hundred is the two or three rows around the viewport rather than a screenful of them.
const LIST_OVERDRAW: f32 = 800.0;

/// The size of the face a timeline row is drawn with.
const AVATAR_SIZE: f32 = 40.0;

/// The size of the face a quoted tweet is drawn with.
const QUOTE_AVATAR_SIZE: f32 = 28.0;

/// How wide a gallery of several pictures is laid out, and how tall.
const GALLERY_WIDE_WIDTH: f32 = 510.0;
const GALLERY_WIDE_HEIGHT: f32 = 510.0;
const GALLERY_COMPACT_WIDTH: f32 = 420.0;
const GALLERY_COMPACT_HEIGHT: f32 = 320.0;

/// A tweet monitor carries no options.
///
/// What a pane follows is the account's own list of monitored accounts, which is read from the
/// service rather than configured here, so there is nothing to persist but the schema version.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TweetMonitorConfig;

impl WidgetConfig for TweetMonitorConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// A failure a pane reports.
#[derive(Clone, Debug, Eq, PartialEq)]
enum TweetMonitorFailure {
    Stream(TweetStreamError),
    Rpc(RpcCallError<XAccountMonitoringApiError>),
}

impl LocalizedError for TweetMonitorFailure {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Stream(error) => error.localized_message(cx),
            Self::Rpc(error) => error.localized_message(cx),
        }
    }
}

/// What the dialog is doing, or waiting for.
///
/// A search is not here: it is a procedure of the dialog's own page rather than something the
/// accounts are waiting for, and it is carried by the pane apart from what it waits on the
/// accounts with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SelectionOperation {
    /// The accounts are being read for a dialog that has none to show.
    Loading,
    /// The accounts are being read for a dialog that is already showing some.
    Refreshing,
    /// An account is being monitored.
    Adding,
    /// An account is being left.
    Deleting,
}

impl SelectionOperation {
    /// Whether this operation is a read rather than a search or a change.
    pub(super) const fn is_read(self) -> bool {
        matches!(self, Self::Loading | Self::Refreshing)
    }
}

/// What the dialog is waiting for.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SelectionPending {
    operation: SelectionOperation,
    /// What the operation is about: the username being monitored or the id being left, which
    /// is what a confirmation is projected from.
    target: Option<SmolStr>,
    /// Whether the request is out.
    ///
    /// A read asked for before the stream is live is held here instead: there is no session
    /// behind the account yet, and the readiness that arrives later is what makes the read
    /// possible.
    sent: bool,
}

/// What the last change said.
///
/// The dialog reports it in an alert over itself, and holds the report it has already made, so
/// what a change says is read by the user where they asked for it rather than in a line of the
/// dialog's own.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SelectionNotice {
    /// The account is now being monitored.
    Added,
    /// Why the last change did not happen, or did not happen for certain.
    Problem(SelectionFailure),
}

/// Why a change to the monitored accounts did not happen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SelectionFailure {
    /// The service refused the change.
    Rejected(XAccountMonitoringApiError),
    /// The change may or may not have been made, which is what the service answers when it
    /// could not tell.
    Unknown(XAccountMonitoringMutationUnknown),
    /// The call itself failed.
    Rpc(RpcCallError<XAccountMonitoringApiError>),
}

impl LocalizedError for SelectionFailure {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Rejected(error) => error.localized_message(cx),
            Self::Unknown(error) => error.localized_message(cx),
            Self::Rpc(error) => error.localized_message(cx),
        }
    }
}

/// One pane: the timeline it holds, and the accounts it manages.
pub struct XAccountMonitoringWidget {
    binding: WidgetBinding<TweetProtocol>,
    status: Entity<WidgetStatus>,
    recovery: WidgetAction,
    /// The account the pane reads for, which is its whole scope.
    account_id: Option<SmolStr>,
    /// The tweets the pane is showing, once a stream has published them.
    feed: Option<TweetSnapshot>,
    /// What is known about every picture the feed wants.
    media: MediaStates,
    /// How many updates the stream this feed came from has published.
    revision: u64,
    /// Whether the subscription has reported itself live.
    ///
    /// It is what keeps a read of the monitored accounts from being sent before there is a
    /// session to answer it, and a change from being sent while the stream carrying the
    /// account is down.
    ready: bool,
    /// The list itself, which holds the newest tweets, measures their rows and remembers where
    /// a reader is.
    list: Entity<DequeUnsizedList<Arc<TweetItem>>>,
    /// The accounts the account monitors, as the service last answered them.
    selections: Option<SelectionData>,
    /// The query the binding is carrying a search of X for, if it is carrying one.
    ///
    /// A search is the dialog's page's, and what it found is delivered there; what is here is
    /// the one fact only the pane can know, because the binding is the pane's: which call it is
    /// carrying. That is what keeps a search and a change from displacing each other, and what
    /// lets a search that another call displaced be closed out rather than left as a page
    /// waiting for an answer nothing will bring.
    searching: Option<SmolStr>,
    /// What the dialog is waiting for, if anything.
    pending: Option<SelectionPending>,
    /// What the dialog says about the last change.
    notice: Option<SelectionNotice>,
    /// The username of every monitored account that came back without a profile, so a row
    /// waiting to be activated is named rather than numbered.
    ///
    /// The rows of the dialog's list are drawn from it, one frame at a time, so the map is
    /// shared with them rather than copied into each row's state. It is a memo beside the rows
    /// rather than a second list of accounts: it is born where the pane is told a name, and it
    /// is pruned against the set every time that set is read, so it cannot outlive the rows it
    /// names. What it is *not* is dropped by a read: the accounts are read again every time a
    /// dialog opens, and a row that is still waiting to be activated comes back without the
    /// name the user added it under.
    hints: Arc<HashMap<SmolStr, SharedString>>,
    /// The open dialog, which is the pane's only way of managing accounts.
    dialog: Option<WeakEntity<TweetDialog>>,
    _account: Subscription,
    /// Wakes the pane when the zone its timestamps are read in moves.
    _time_zone: Option<Subscription>,
}

impl WidgetView for XAccountMonitoringWidget {
    type Config = TweetMonitorConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.market.x_account_monitoring");
    const DISPLAY_NAME: &'static str = "widget.names.x_account_monitoring";
    const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;
    const DESCRIPTION: &'static str = "Live posts from monitored X accounts";
    const ICON: Option<&'static str> = Some("widgets/x_account_monitoring.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: Entity<WidgetStatus>,
        _reload: impl Fn(&mut App) + 'static,
        _config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = cx.global::<KytrixServiceConnector>().clone();
        // Pictures are fetched by this pane rather than by the service, and the transport they
        // are fetched over is the application's own build policy: the two X image origins are
        // the only hosts a picture URL may name, and they are always reached over HTTPS.
        let transport = SharedTransport::new(
            Arc::new(TransportHub::new(TransportPolicy::default())),
            source.network_notification(),
            global::handle(cx),
        );
        Self::new_with_source(
            config,
            source,
            MediaDownloader::new(transport),
            status,
            WidgetConfigChange::default(),
            recovery,
            cx,
        )
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let retry = self.status.is_failed(cx);
        self.account_id = account_of(&cx.global::<ConfigManager>().read());
        self.start(retry, cx);
    }

    /// A pane is content once a stream has published a timeline for it.
    ///
    /// An empty timeline is not content: an account whose monitored accounts have not posted
    /// is an account the pane has nothing to show for, and what it has instead is the
    /// framework's own "waiting" placeholder, which is where a neutral sentence belongs.
    fn has_content(&self) -> bool {
        self.feed.as_ref().is_some_and(|feed| !feed.is_empty())
    }

    /// The control that opens the monitored-accounts dialog.
    fn header(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> WidgetHeader {
        WidgetHeader::empty().left(self.manage_button(cx))
    }
}

impl XAccountMonitoringWidget {
    /// The seam a test can answer through: `new` only picks the production source and
    /// transport.
    fn new_with_source<S, T>(
        _config: TweetMonitorConfig,
        source: S,
        downloader: MediaDownloader<T>,
        status: Entity<WidgetStatus>,
        _config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: TweetSource,
        T: super::media::MediaTransport,
    {
        let binding = WidgetBinding::spawn_conditionally(
            global::handle(cx).handle(),
            TweetService::new(source, downloader),
            cx,
            Self::apply_reliable,
            Self::apply_latest,
        );
        // The account is the pane's scope, and the user can change it while the pane is open:
        // signing out has to take the timeline with it, and signing in as someone else has to
        // read again.
        let events = cx.global::<ConfigManager>().events().clone();
        let account = cx.subscribe(&events, |this, _, _: &AccountChanged, cx| {
            this.reconcile_account(account_of(&cx.global::<ConfigManager>().read()), cx);
        });

        // The list holds the timeline itself, so what a reader is reading is the list's to keep:
        // it is told about the tweets a feed added rather than about the feed.
        let widget = cx.weak_entity();
        let list = cx.new(|_| feed_list(widget));

        // What this pane waits with is a reference into the catalogue, resolved by the
        // framework on the frame that draws it, so nothing here has to be re-reported when the
        // language moves.
        Self {
            binding,
            status,
            recovery,
            // The pane records the account it was built for, but it starts nothing: the
            // framework calls `load` once construction is done.
            account_id: account_of(&cx.global::<ConfigManager>().read()),
            feed: None,
            media: Arc::new(HashMap::new()),
            revision: 0,
            ready: false,
            list,
            selections: None,
            searching: None,
            pending: None,
            notice: None,
            hints: Arc::new(HashMap::new()),
            dialog: None,
            _account: account,
            // The rows are built by the list entity, which keeps what it drew until it
            // is told otherwise: moving the display zone has to wake the list itself.
            _time_zone: on_data_time_zone_change(cx, |this, _, cx| {
                this.list.update(cx, |_, cx| cx.notify());
                cx.notify();
            }),
        }
    }

    /// The scope this pane follows, which is the account it reads as.
    fn current_scope(&self) -> Option<TweetScope> {
        TweetScope::new(self.account_id.as_ref())
    }

    /// Follows the account the settings now hold.
    ///
    /// Another account is another timeline and another list of accounts, so what the pane is
    /// showing is dropped rather than kept under a new name — and a change the pane was
    /// waiting for is dropped with it, because whether that change was made is not something
    /// the new account's list can answer.
    fn reconcile_account(&mut self, account: Option<SmolStr>, cx: &mut Context<Self>) {
        if self.account_id == account {
            return;
        }
        self.account_id = account;
        self.start(false, cx);
    }

    /// Reports the notice that stands in while the pane has nothing to show.
    ///
    /// Nobody signed in and a live stream whose accounts have not posted are the two states
    /// this pane waits in, and each is a reference into the catalogue rather than a sentence:
    /// the timeline, the accounts and the dialog belong to `start` and `report_ready`, and
    /// saying the same reference again is not a change and schedules no frame.
    fn report_no_content_notice(&self, cx: &mut Context<Self>) {
        let notice = if self.current_scope().is_none() {
            // Nobody is signed in: what stands in is the sentence that sends the reader to
            // the account settings, which is the one `start` reports for the same state.
            Notice::message("widget.x_account_monitoring.logged_out")
        } else {
            Notice::message("widget.x_account_monitoring.waiting")
        };
        self.status.report(WidgetState::waiting(notice), cx);
    }

    /// Starts reading, or restarts for a pane the user is recovering.
    ///
    /// A recovery keeps the timeline the user is still reading — blanking a feed to reconnect
    /// would flash it away, and the accounts are read again once the stream this pane is
    /// recovering is live — while an account change starts over, because the pane cannot
    /// vouch for what it was showing.
    fn start(&mut self, retry: bool, cx: &mut Context<Self>) {
        let Some(scope) = self.current_scope() else {
            self.clear(cx);
            self.report_no_content_notice(cx);
            // Nothing is followed, so nothing should stay subscribed on this pane's behalf: an
            // account that signed out must not keep a seat it is not using.
            self.hand_over(TweetRequest::Stop, cx, |_, _, _| {});
            cx.notify();
            return;
        };
        self.ready = false;
        // Every stream numbers its own updates from the beginning, so what this pane last
        // applied belongs to the stream being replaced rather than to the one starting here.
        // A stale update cannot arrive by hand either: the framework retires the generation a
        // replaced stream published under.
        self.revision = 0;
        if !retry {
            self.clear(cx);
        }
        // Starting a stream replaces whatever the binding was awaiting, so a change that was in
        // flight is dropped rather than remembered: whether it was made is not something this
        // pane knows, and what settles it is the read that follows the stream coming back.
        self.pending = None;
        self.status.report(WidgetState::Connecting, cx);
        self.hand_over(
            if retry {
                TweetRequest::Retry(scope)
            } else {
                TweetRequest::Load(scope)
            },
            cx,
            Self::apply_load_result,
        );
        cx.notify();
    }

    /// Drops what a pane is leaving behind.
    ///
    /// A list holds the tweets it was given and can only take more of them, so the timeline it
    /// held is left behind by building the list again rather than by telling it about a feed it
    /// no longer has. What is remembered about the accounts goes with them: this is a pane
    /// starting over on another account or on no account at all, and a name remembered for one
    /// account's row is not a name for another account's.
    fn clear(&mut self, cx: &mut Context<Self>) {
        self.feed = None;
        self.media = Arc::new(HashMap::new());
        self.revision = 0;
        self.ready = false;
        let widget = cx.weak_entity();
        self.list = cx.new(|_| feed_list(widget));
        self.selections = None;
        self.searching = None;
        self.pending = None;
        self.notice = None;
        self.hints = Arc::new(HashMap::new());
        self.notify_dialog(cx);
    }

    fn apply_load_result(
        &mut self,
        result: WidgetRpcResult<TweetProtocol>,
        cx: &mut Context<Self>,
    ) {
        // The answer says the service took the account; what the subscription then reports
        // about itself arrives on the stream, which is where a failure to subscribe is drawn
        // from.
        if let Err(error) = result {
            self.ready = false;
            // A read the dialog is waiting for is answered by the account being taken, so it
            // does not outlive the failure of the request that would have taken it.
            self.pending = None;
            self.notify_dialog(cx);
            let recovery = self.recovery.clone();
            self.status.report(
                WidgetState::failed(Arc::new(TweetMonitorFailure::Rpc(error)), recovery),
                cx,
            );
        }
    }

    /// Applies the timeline a subscription published.
    ///
    /// Returns whether what a frame draws changed, which for a feed is always: the service
    /// publishes it when a tweet or a picture moved it.
    fn apply_latest(
        &mut self,
        key: TweetScope,
        message: Arc<TweetStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_scope().as_ref() != Some(&key) || self.status.is_failed(cx) {
            return false;
        }
        let TweetStreamMessage::Update(update) = message.as_ref() else {
            return false;
        };
        // The latest channel coalesces, so an update can arrive after a newer one already has:
        // what a stream published before the last thing this pane applied is not a timeline
        // any more.
        if update.revision() <= self.revision {
            return false;
        }

        self.accept(update, cx);
        if self.ready {
            self.report_ready(cx);
        }
        true
    }

    /// Applies a status the stream reported.
    ///
    /// Returns whether what a frame draws changed, which a status never does on its own: the
    /// framework draws the state around the body, and a row of tweets says nothing about the
    /// subscription it arrived on.
    fn apply_reliable(
        &mut self,
        key: TweetScope,
        message: Arc<TweetStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_scope().as_ref() != Some(&key) || self.status.is_failed(cx) {
            return false;
        }
        let TweetStreamMessage::Status(status) = message.as_ref() else {
            return false;
        };

        match status {
            TweetStatus::Connecting => {
                self.ready = false;
                // The timeline already read stays: a reconnecting feed is not a reason to blank
                // what a reader was looking at.
                self.status.report(WidgetState::Connecting, cx);
                self.notify_dialog(cx);
            }
            TweetStatus::Ready => {
                self.ready = true;
                self.report_ready(cx);
                // What a pane shows is only as true as the read it came from, and a change the
                // pane stopped waiting for was never read back. The stream coming up is what
                // makes reading the accounts possible again, so it is what reads them.
                if self.pending.is_some() {
                    self.send_pending_selections(cx);
                }
                if self.dialog_open() {
                    // A search the dialog had out is displaced by the read this asks for: the
                    // stream coming back settles what the accounts are, and a page waiting for
                    // an answer that the read took the place of would wait forever.
                    self.watch_selections(true, cx);
                    if self.pending.is_none() {
                        self.load_selections(cx);
                    }
                }
            }
            TweetStatus::Failed(error) => {
                self.ready = false;
                // A read that never went out has nothing left to wait for: the stream it was
                // waiting for failed, and the dialog says so rather than spinning.
                if self.pending.as_ref().is_some_and(|pending| !pending.sent) {
                    self.pending = None;
                }
                self.notify_dialog(cx);
                self.fail(error, cx);
            }
        }

        false
    }

    /// Reports the state of a pane whose stream is live.
    ///
    /// A live stream with nothing to show is not a failure and not a load: it is a pane
    /// waiting for the accounts it monitors to post, which is a sentence the framework's
    /// placeholder draws.
    fn report_ready(&mut self, cx: &mut Context<Self>) {
        if self.has_content() {
            self.status.report(WidgetState::Ready, cx);
        } else {
            self.report_no_content_notice(cx);
        }
    }

    /// Reports why a subscription stopped.
    fn fail(&mut self, error: &TweetStreamError, cx: &mut Context<Self>) {
        let failure = TweetMonitorFailure::Stream(error.clone());
        let recovery = self.recovery.clone();
        self.status
            .report(WidgetState::failed(Arc::new(failure), recovery), cx);
    }

    /// Tells the list about the tweets this feed added.
    ///
    /// The list holds the newest tweets and remembers the one a reader is looking at, so it is
    /// told the tweets it has not been shown rather than the feed itself: a list that keeps the
    /// tweets it holds where they are is what keeps a reader on the tweet they are reading. The
    /// one tweet that does move them is one that arrives while the newest tweet is the one they
    /// are reading, which is where the newest tweet belongs — and a feed the list cannot be
    /// extended into is one it holds no tweet of, which is built for instead.
    fn accept(&mut self, update: &TweetUpdate, cx: &mut Context<Self>) {
        let unshown = unshown(
            self.feed.as_ref().map_or(&[], TweetSnapshot::items),
            update.feed().items(),
        );
        self.feed = Some(update.feed().clone());
        self.media = update.media().clone();
        self.revision = update.revision();

        match unshown {
            // The oldest of the tweets it has not been shown goes in first, so the newest of
            // them ends up where a list that draws its newest tweet first draws it.
            Some(count) => {
                self.list.update(cx, |list, cx| {
                    for tweet in update.feed().items()[..count].iter().rev() {
                        list.push(tweet.clone());
                    }
                    // What a row draws is not only the tweet it holds: a picture can be fetched,
                    // or fail, while the timeline does not move at all.
                    cx.notify();
                });
            }
            // Nothing the list holds survives, so there is no edit it can be told: every tweet
            // on screen is one the feed no longer has. A list holds the tweets it was given and
            // can only take more of them, so the one this pane draws is built again for the feed
            // it has now, and the reader starts over on the tweet that feed draws first.
            None => {
                let widget = cx.weak_entity();
                let tweets: Vec<Arc<TweetItem>> = update.feed().items().to_vec();
                self.list = cx.new(move |_| {
                    let mut list = feed_list(widget);
                    for tweet in tweets.iter().rev() {
                        list.push(tweet.clone());
                    }
                    list
                });
                // The rows are drawn by the list built here rather than the one it replaced, so
                // the pane is what is told about it.
                cx.notify();
            }
        }
    }

    /// The list, which draws the tweets the pane holds.
    fn list_body(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .id(ElementId::Name(SharedString::new_static(
                "TweetMonitorFrame",
            )))
            .debug_selector(|| "TEST_TWEET_LIST".into())
            .aria_label(t!(cx, "widget.x_account_monitoring.scroll_tweets"))
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(self.list.clone())
            .into_any_element()
    }

    /// The control that opens the monitored-accounts dialog.
    ///
    /// It is disabled without an account: the accounts belong to one, so a reader the service
    /// does not know has nothing to manage — and a live control that can only fail reads as if
    /// the pane were broken.
    fn manage_button(&self, cx: &Context<Self>) -> AnyElement {
        let widget = cx.entity().downgrade();

        div()
            .debug_selector(|| "TEST_TWEET_MONITOR_MANAGE".into())
            .flex_none()
            // A control in the header row has to take its own clicks: the row is the pane's
            // drag area as well.
            .occlude()
            .child(
                Button::new(ElementId::Name(SharedString::new_static(
                    "TweetMonitorManage",
                )))
                .filled()
                .small()
                .disabled(self.account_id.is_none())
                .aria_label(t!(cx, "widget.x_account_monitoring.manage"))
                .with_tooltip(t!(cx, "widget.x_account_monitoring.manage"))
                .on_click(move |_, window, cx| {
                    TweetDialog::show(widget.clone(), window, cx);
                })
                .child(Icon::new(IconKind::Faders).size_3()),
            )
            .into_any_element()
    }

    /// Whether the monitored-accounts dialog is open.
    pub(super) fn dialog_open(&self) -> bool {
        self.dialog
            .as_ref()
            .is_some_and(|dialog| dialog.upgrade().is_some())
    }

    /// The pane's own state, read for the frame the dialog draws it in.
    ///
    /// The dialog's list reads it again for each row it draws, so everything in it is shared
    /// rather than copied: a row is drawn on every frame of the dialog, and rebuilding what it
    /// reads would be work per row rather than per frame.
    pub(super) fn dialog_state(&self) -> DialogState {
        DialogState::new(
            self.account_id.is_some(),
            self.ready,
            self.selections.clone(),
            self.searching.clone(),
            self.pending.as_ref().map(|pending| pending.operation),
            self.notice.clone(),
            self.hints.clone(),
            self.media.clone(),
            self.recovery.clone(),
        )
    }

    /// Takes over the dialog that was just opened over this pane.
    ///
    /// A dialog is showing the accounts, so opening one is what makes their faces worth
    /// fetching, and it is what asks for them: the service answers once the stream carrying
    /// the account is live, which is where a read waits.
    pub(super) fn attach_dialog(
        &mut self,
        dialog: WeakEntity<TweetDialog>,
        cx: &mut Context<Self>,
    ) {
        self.dialog = Some(dialog);
        self.watch_selections(true, cx);
        self.load_selections(cx);
    }

    /// Stops asking for the avatars a dialog was showing.
    ///
    /// A dismissed dialog is a pane with no row that draws a face, so what it was fetching is
    /// no longer worth a request. A search of X it had out is that dialog's too, and what it
    /// would have answered has nowhere left to be drawn: the pane stops waiting for it rather
    /// than carrying a search whose answer is nobody's.
    ///
    /// The search is taken here rather than left to the call that stops the watch, which closes
    /// out what it displaces: that call is only made for a pane following an account on a live
    /// stream, while a search that is out is a search the stream can go down under — nothing
    /// displaces it there, nothing would take it away, and a pane waiting for an answer it will
    /// not be given is a pane that refuses every change.
    pub(super) fn on_dialog_closed(&mut self, cx: &mut Context<Self>) {
        self.dialog = None;
        self.searching = None;
        self.watch_selections(false, cx);
    }

    /// Repaints the dialog, which draws the state this pane holds.
    ///
    /// The dialog is told that it changed rather than asked to change: a pane is what answers
    /// the dialog's own requests, so this runs inside a frame that dialog is already being
    /// updated for — and an entity cannot be updated while it is being updated. Notifying it
    /// by id is what says the same thing without taking it.
    fn notify_dialog(&self, cx: &mut Context<Self>) {
        if let Some(dialog) = self.dialog.as_ref().and_then(WeakEntity::upgrade) {
            App::notify(cx, dialog.entity_id());
        }
    }

    /// Hands one request to the service, of which the binding carries one at a time.
    ///
    /// The requests that come here are the ones the pane is not waiting for an answer to — a
    /// stream watched for a dialog that opened, a picture fetched again, a stream started or
    /// stopped — so what the binding was carrying is what this takes the place of, and it is
    /// closed out first: see [`Self::drop_displaced`]. The requests the pane *does* wait on go
    /// to the binding directly, because each of them is the call the pane's guards keep the
    /// others out of the way of.
    fn hand_over(
        &mut self,
        request: TweetRequest,
        cx: &mut Context<Self>,
        on_result: fn(&mut Self, WidgetRpcResult<TweetProtocol>, &mut Context<Self>),
    ) {
        self.drop_displaced(cx);
        self.binding.call(request, cx, on_result);
    }

    /// Closes out what a call the pane is not waiting on takes the place of.
    ///
    /// The binding carries one call at a time and replaces it with the next, so a call that was
    /// already out is one whose answer can no longer arrive. Waiting for it anyway is what
    /// leaves a dialog spinning and its controls disabled forever, so what is left behind is
    /// dropped here rather than guessed at: a search dies with the page that asked for it, and a
    /// change is dropped rather than decided — whether it was made is not something this pane
    /// knows, and the read that follows the call which displaced it is what settles the list.
    ///
    /// A read that has not gone out yet is left alone: it is not a call the binding is carrying,
    /// so a call that takes the place of nothing leaves it waiting for the stream that will send
    /// it.
    fn drop_displaced(&mut self, cx: &mut Context<Self>) {
        let search = self.searching.take().is_some();
        let sent = self.pending.as_ref().is_some_and(|pending| pending.sent);
        if sent {
            self.pending = None;
        }
        if search || sent {
            self.notify_dialog(cx);
        }
    }

    /// Says whether a dialog is showing the monitored accounts.
    ///
    /// Their avatars are only fetched while one is: a pane whose dialog is closed has no row
    /// that draws them, and fetching thousands of faces nobody is looking at is not what a
    /// timeline should spend its requests on.
    pub(super) fn watch_selections(&mut self, watched: bool, cx: &mut Context<Self>) {
        let Some(scope) = self.current_scope() else {
            return;
        };
        if !self.ready {
            return;
        }
        self.hand_over(
            TweetRequest::WatchSelections { scope, watched },
            cx,
            |_, _, _| {},
        );
    }

    /// Reads the accounts the account monitors.
    ///
    /// A read goes over a live stream, because a live stream is what says the service is
    /// holding the account: before it there is no session to answer one. A read asked for too
    /// early waits here instead, and the readiness that arrives later is what sends it.
    pub(super) fn load_selections(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(scope) = self.current_scope() else {
            return false;
        };
        if self.pending.is_some() {
            return false;
        }
        self.pending = Some(SelectionPending {
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
            self.send_selections(scope, cx);
        }
        self.notify_dialog(cx);
        true
    }

    /// Sends the read a dialog is waiting for.
    fn send_pending_selections(&mut self, cx: &mut Context<Self>) {
        let Some(scope) = self.current_scope() else {
            return;
        };
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| !pending.sent && pending.operation.is_read())
        {
            self.send_selections(scope, cx);
            self.notify_dialog(cx);
        }
    }

    fn send_selections(&mut self, scope: TweetScope, cx: &mut Context<Self>) {
        if let Some(pending) = self.pending.as_mut() {
            pending.sent = true;
        }
        // A read is a call the pane waits on, so it goes to the binding rather than through the
        // hand-over that closes out what a call displaces: what it displaces is a watch or a
        // picture fetched again, neither of which the pane is waiting for.
        self.binding.call(
            TweetRequest::FetchSelections(scope),
            cx,
            Self::apply_selections_result,
        );
    }

    /// Searches X for an account to monitor.
    ///
    /// The request is the pane's to send — the binding is the pane's, and it carries one call at
    /// a time — while what the search is *for* is the dialog's page: the query is typed there
    /// and what comes back is handed to it. So the pane waits on nothing here beyond the call
    /// itself: a search is not an operation the accounts are waiting for, and the dialog that
    /// asked for it is what draws one being out.
    pub(super) fn search_users(&mut self, query: SmolStr, cx: &mut Context<Self>) -> bool {
        let Some(scope) = self.current_scope() else {
            return false;
        };
        // The control that sends a search is disabled for one that could not be answered, but
        // the field also answers Enter, which reaches here without a control to have been
        // disabled: what a change and a read are waiting for is refused the same way here.
        if !self.may_change(cx) || !valid_search_query(&query) {
            return false;
        }
        self.searching = Some(query.clone());
        // What a search being out looks like is drawn by the page that asked for it, and it is
        // read from here: a search that goes out without telling the dialog leaves that page
        // showing the answer before it — or nothing at all — while the pane turns the next one
        // away, with nothing on screen to say why.
        self.notify_dialog(cx);
        self.binding.call(
            TweetRequest::SearchUsers { scope, query },
            cx,
            Self::apply_search_result,
        );
        true
    }

    /// Monitors one more account.
    pub(super) fn add_selection(&mut self, username: SmolStr, cx: &mut Context<Self>) -> bool {
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
        self.begin_change(SelectionOperation::Adding, Some(username.clone()), cx);
        // A change is a call the pane waits on, and one the guards keep from being asked for
        // while a read or a search is out: it goes to the binding directly.
        self.binding.call(
            TweetRequest::AddSelection { scope, username },
            cx,
            Self::apply_add_result,
        );
        true
    }

    /// Stops monitoring one account.
    pub(super) fn delete_selection(&mut self, x_user_id: SmolStr, cx: &mut Context<Self>) -> bool {
        let Some(scope) = self.current_scope() else {
            return false;
        };
        if !self.may_change(cx) {
            return false;
        }
        self.begin_change(SelectionOperation::Deleting, Some(x_user_id.clone()), cx);
        self.binding.call(
            TweetRequest::DeleteSelection { scope, x_user_id },
            cx,
            Self::apply_delete_result,
        );
        true
    }

    /// Fetches one picture again.
    ///
    /// The picture is the service's to describe, so this only says which one: what comes of it
    /// is published on the stream like every other picture state.
    pub(super) fn retry_media(&mut self, url: SmolStr, cx: &mut Context<Self>) {
        let Some(scope) = self.current_scope() else {
            return;
        };
        self.hand_over(TweetRequest::RetryMedia { scope, url }, cx, |_, _, _| {});
    }

    /// Whether the pane is in a position to ask the service about the monitored accounts.
    ///
    /// A change is sent over the same session the stream was opened with, so a pane whose
    /// stream is not live has no session to change anything through. A search and a change are
    /// carried by the binding one at a time, so neither may be asked for while the other is
    /// out: the two would otherwise take each other's place, and whichever was displaced would
    /// never be answered.
    fn may_change(&self, cx: &Context<Self>) -> bool {
        self.ready
            && self.selections.is_some()
            && self.pending.is_none()
            && self.searching.is_none()
            && !self.status.is_failed(cx)
    }

    fn begin_change(
        &mut self,
        operation: SelectionOperation,
        target: Option<SmolStr>,
        cx: &mut Context<Self>,
    ) {
        self.pending = Some(SelectionPending {
            operation,
            target,
            sent: true,
        });
        self.notice = None;
        self.notify_dialog(cx);
    }

    fn apply_selections_result(
        &mut self,
        result: WidgetRpcResult<TweetProtocol>,
        cx: &mut Context<Self>,
    ) {
        self.pending = None;
        match result {
            Ok(TweetResponse::Selections(data)) => {
                // What is remembered is kept for the rows this read still has and has no
                // profile for, and for no others: a hint is a name for a row that is monitored
                // and unnamed, so what the read does not hold cannot be named by it.
                self.hints = Arc::new(retained_hints(&self.hints, data.selections()));
                self.selections = Some(data);
                self.notice = None;
            }
            // The protocol types the answer, so anything else is a service that answered the
            // wrong question rather than a shape a reader could describe: nothing changes.
            Ok(_) => {}
            Err(error) => {
                self.notice = Some(SelectionNotice::Problem(SelectionFailure::Rpc(error)));
            }
        }
        self.notify_dialog(cx);
    }

    /// Hands what a search answered to the dialog that asked for it.
    ///
    /// A search is the dialog's page's, so the answer is delivered there rather than kept here:
    /// a dialog is the only thing that draws it, and an answer that outlived the dialog would be
    /// drawn by the next one under a query it does not stand for.
    ///
    /// The answer is delivered in a later effect rather than in this update, because the dialog
    /// asks for a search from inside its own update and an entity cannot be updated while it is
    /// being updated. A dialog that is gone by then is a dialog with nowhere to draw it, and the
    /// answer is dropped rather than cached: what a search found belongs to the query that asked
    /// for it, and the query went with the dialog.
    fn apply_search_result(
        &mut self,
        result: WidgetRpcResult<TweetProtocol>,
        cx: &mut Context<Self>,
    ) {
        // An answer arrives only for the call the binding was carrying, so a search the pane is
        // no longer waiting for — the dialog was dismissed while it was out — has none to hand
        // over.
        let Some(query) = self.searching.take() else {
            return;
        };
        self.notify_dialog(cx);
        let Some(dialog) = self.dialog.clone() else {
            return;
        };
        let answer = match result {
            Ok(TweetResponse::SearchResults(profiles)) => SearchAnswer::Found(profiles),
            // The protocol types the answer, so anything else is a service that answered the
            // wrong question rather than one a page could draw: the search is over, and the page
            // is left with nothing to show for it.
            Ok(_) => return,
            Err(error) => SearchAnswer::Failed(error),
        };
        cx.defer(move |cx| {
            // The dialog is taken where the answer is delivered rather than where it arrived: a
            // dialog that is gone by then is one with nothing to draw the answer, and the answer
            // is dropped rather than kept for a dialog that never asked for it.
            let Some(dialog) = dialog.upgrade() else {
                return;
            };
            dialog.update(cx, |dialog, cx| {
                dialog.answered_search(&query, answer, cx);
            });
        });
    }

    fn apply_add_result(&mut self, result: WidgetRpcResult<TweetProtocol>, cx: &mut Context<Self>) {
        let requested = self.pending.take().and_then(|pending| pending.target);
        let outcome = match result {
            Ok(TweetResponse::Added(outcome)) => outcome,
            Ok(_) => return,
            Err(error) => {
                self.notice = Some(SelectionNotice::Problem(SelectionFailure::Rpc(error)));
                self.notify_dialog(cx);
                return;
            }
        };
        self.notice = match outcome {
            // What the list becomes is the confirmation applied to it, and an account that
            // came back without a profile is named by what was asked for rather than numbered.
            XAccountMonitoringMutationOutcome::Confirmed(selection) => {
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
                                .insert(selection.x_user_id, SharedString::new(username.as_str()));
                        }
                        Some(SelectionNotice::Added)
                    }
                    None => None,
                }
            }
            XAccountMonitoringMutationOutcome::Rejected(error) => {
                Some(SelectionNotice::Problem(SelectionFailure::Rejected(error)))
            }
            // The change may or may not have been made, so the list the pane holds is not one
            // it can change: it is left as it was, and the next read settles it.
            XAccountMonitoringMutationOutcome::Unknown(unknown) => {
                Some(SelectionNotice::Problem(SelectionFailure::Unknown(unknown)))
            }
        };
        self.notify_dialog(cx);
    }

    fn apply_delete_result(
        &mut self,
        result: WidgetRpcResult<TweetProtocol>,
        cx: &mut Context<Self>,
    ) {
        let requested = self.pending.take().and_then(|pending| pending.target);
        let outcome = match result {
            Ok(TweetResponse::Deleted(outcome)) => outcome,
            Ok(_) => return,
            Err(error) => {
                self.notice = Some(SelectionNotice::Problem(SelectionFailure::Rpc(error)));
                self.notify_dialog(cx);
                return;
            }
        };
        self.notice = match outcome {
            XAccountMonitoringMutationOutcome::Confirmed(()) => match requested {
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
            XAccountMonitoringMutationOutcome::Rejected(error) => {
                Some(SelectionNotice::Problem(SelectionFailure::Rejected(error)))
            }
            XAccountMonitoringMutationOutcome::Unknown(unknown) => {
                Some(SelectionNotice::Problem(SelectionFailure::Unknown(unknown)))
            }
        };
        self.notify_dialog(cx);
    }
}

impl Render for XAccountMonitoringWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(self.list_body(cx))
    }
}

/// The list a pane draws its timeline with.
///
/// The list holds the timeline itself, so what a reader is reading is the list's to keep: it is
/// told about the tweets a feed added rather than about the feed. What it was given is what it
/// holds, which is why a feed that is not one the list it holds can be extended into is a list
/// built again rather than a list told about it.
///
/// The row it draws is built once here while what it shows is rebuilt for every frame, which is
/// why the pane is read inside the closure rather than captured with it.
fn feed_list(widget: WeakEntity<XAccountMonitoringWidget>) -> DequeUnsizedList<Arc<TweetItem>> {
    DequeUnsizedList::new(
        ElementId::Name(SharedString::new_static("TweetList")),
        ListAlignment::Top,
        MAX_TWEETS,
        px(LIST_OVERDRAW),
        move |index, tweet: &Arc<TweetItem>, _window, cx| {
            let Some(pane_entity) = widget.upgrade() else {
                return div().into_any_element();
            };
            let pane = pane_entity.read(cx);
            let Some(feed) = pane.feed.as_ref() else {
                return div().into_any_element();
            };
            let retry = pane.current_scope().map(|_| MediaRetry {
                widget: pane_entity.clone(),
            });
            tweet_row(index, tweet, feed, &pane.media, retry.as_ref(), cx)
        },
    )
    .separator(|_, _| Separator::horizontal().into_any_element())
    .render_reverse()
    .keep_on_newest()
    // The row after the last tweet says there are no more, so it is the list's own footer rather
    // than part of the feed: it is never one of the tweets a push drops.
    .footer(move |_window, cx| timeline_end(cx))
}

/// Whether one search string is one X could be asked about.
///
/// The bound is the service's own, and it counts bytes: a query that is longer is one the
/// service would refuse, so it is refused here rather than sent.
pub(super) fn valid_search_query(query: &str) -> bool {
    let query = query.trim();
    !query.is_empty()
        && query.len() <= MAX_SEARCH_QUERY_BYTES
        && !query.chars().any(char::is_control)
}

/// The longest search string the service accepts.
const MAX_SEARCH_QUERY_BYTES: usize = 100;

/// What a row needs to fetch one of its pictures again.
///
/// The picture is the service's to describe, so the request only names it: which stream the
/// picture belongs to is the pane's current scope, which is what the pane sends it with.
struct MediaRetry {
    widget: Entity<XAccountMonitoringWidget>,
}

/// One row of the timeline: one tweet, and everything it carries.
fn tweet_row(
    index: usize,
    item: &Arc<TweetItem>,
    feed: &TweetSnapshot,
    media: &MediaStates,
    retry: Option<&MediaRetry>,
    cx: &App,
) -> AnyElement {
    let (tweet, reposted) = item.displayed();
    let outer = feed.author(&item.x_user_id);
    let author = feed.author(&tweet.x_user_id);
    // A repost row shows the tweet that was reposted and opens *that* tweet, while the line
    // above it names who reposted it.
    let url = tweet_url(author.username.as_ref(), tweet.tweet_id());
    let age = tweet.age(OffsetDateTime::now_utc(), data_time_zone(cx));

    let mut row = div()
        .id(ElementId::NamedInteger(
            SharedString::new_static("TweetMonitorRow"),
            index as u64,
        ))
        .debug_selector(|| "TEST_TWEET_ROW".into())
        .role(Role::ListItem)
        .aria_label(t!(
            cx,
            "widget.x_account_monitoring.tweet",
            author = author.username,
            time = age_text(age, cx)
        ))
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .px_3()
        .py_3()
        .cursor_pointer()
        .on_click(move |_, _, cx| cx.open_url(&url));
    if reposted {
        row = row.child(repost_line(&outer, cx));
    }

    row.child(tweet_content(tweet, feed, media, reposted, retry, cx))
        .into_any_element()
}

/// The line above a repost that says who reposted it.
fn repost_line(author: &super::model::TweetAuthor, cx: &App) -> AnyElement {
    let theme = cx.theme();
    div()
        .min_w_0()
        .pl(px(AVATAR_SIZE + 12.0))
        .flex()
        .items_center()
        .gap_1()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.roles.fg.muted.hsla)
        .child(
            Icon::new(IconKind::Repeat)
                .size_3()
                .text_color(theme.roles.fg.muted.hsla),
        )
        .child(div().min_w_0().truncate().child(t!(
            cx,
            "widget.x_account_monitoring.reposted",
            name = author.display_name.clone()
        )))
        .into_any_element()
}

/// One tweet's own content: its face, its author, its text, its pictures and what it quotes.
fn tweet_content(
    tweet: &TweetItem,
    feed: &TweetSnapshot,
    media: &MediaStates,
    reposted: bool,
    retry: Option<&MediaRetry>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let author = feed.author(&tweet.x_user_id);
    let mut column = div().min_w_0().flex_1().flex().flex_col().gap_2();
    column = column.child(tweet_header(tweet, &author, cx));
    if !tweet.text.is_empty() {
        column = column.child(
            div()
                .min_w_0()
                .text_sm()
                .whitespace_normal()
                .debug_selector(|| "TEST_TWEET_TEXT".into())
                .child(interactive_text(tweet, false, cx)),
        );
    }
    if !tweet.media.is_empty() {
        column = column.child(media_gallery(tweet, media, false, retry, cx));
    }
    if let Some(TweetReferenceItem::Quote(quoted)) = tweet.reference.as_ref() {
        column = column.child(quote_block(quoted, feed, media, retry, cx));
    }

    let content = div()
        .min_w_0()
        .flex()
        .items_start()
        .gap_3()
        .child(avatar(
            author.avatar_url.as_ref(),
            media,
            format_id("tweet-avatar", tweet.tweet_id()),
            None,
            theme.roles.surface.raised.hsla,
            theme.roles.fg.muted.hsla,
            px(AVATAR_SIZE),
        ))
        .child(column);

    if !reposted {
        return content.into_any_element();
    }
    // A repost's inner tweet is a control of its own, so a click on it opens the tweet that was
    // reposted rather than the repost.
    let url = tweet_url(author.username.as_ref(), tweet.tweet_id());
    div()
        .id(format_id("tweet-repost", tweet.tweet_id()))
        .w_full()
        .min_w_0()
        .cursor_pointer()
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            cx.open_url(&url);
        })
        .child(content)
        .into_any_element()
}

/// Who wrote one tweet, when, and whether X vouches for them.
fn tweet_header(tweet: &TweetItem, author: &super::model::TweetAuthor, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let muted = theme.roles.fg.muted.hsla;
    let age = tweet.age(OffsetDateTime::now_utc(), data_time_zone(cx));
    let mut name_row = div()
        .min_w_0()
        .flex()
        .items_center()
        .gap_1()
        .text_sm()
        .child(
            div()
                .min_w_0()
                .truncate()
                .font_weight(FontWeight::SEMIBOLD)
                .debug_selector(|| "TEST_TWEET_DISPLAY_NAME".into())
                .child(author.display_name.clone()),
        );
    if author.verified {
        name_row = name_row.child(
            div()
                .flex_none()
                .debug_selector(|| "TEST_TWEET_VERIFIED".into())
                .child(
                    Icon::new(IconKind::CheckCircle)
                        .size_4()
                        .text_color(theme.roles.brand.base.hsla),
                ),
        );
    }
    name_row = name_row
        .child(div().flex_none().text_color(muted).child("·"))
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(muted)
                .debug_selector(|| "TEST_TWEET_TIME".into())
                .child(
                    Tooltip::new(
                        format_id("tweet-time", tweet.tweet_id()),
                        div().flex_none().child(age_text(age, cx)),
                    )
                    .content(tweet.published_at(data_time_zone(cx))),
                ),
        );

    div()
        .min_w_0()
        .flex()
        .flex_col()
        .child(name_row)
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(muted)
                .child(SharedString::new(format_smolstr!(
                    "@{}",
                    author.username.as_ref()
                ))),
        )
        .into_any_element()
}

/// One tweet's text, with the runs that are links made clickable.
fn interactive_text(tweet: &TweetItem, compact: bool, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let link_color = theme.roles.brand.base.hsla;
    let ranges = tweet
        .text
        .links()
        .iter()
        .map(|link| link.range.clone())
        .collect::<Vec<_>>();
    let urls = tweet
        .text
        .links()
        .iter()
        .map(|link| link.url.clone())
        .collect::<Vec<_>>();
    let highlights = tweet
        .text
        .links()
        .iter()
        .map(|link| {
            (
                link.range.clone(),
                gpui_kit::HighlightStyle {
                    color: Some(link_color),
                    ..Default::default()
                },
            )
        })
        .collect::<Vec<_>>();
    let text = StyledText::new(tweet.text.shared()).with_highlights(highlights);

    InteractiveText::new(
        ElementId::Name(SharedString::new(format_smolstr!(
            "tweet-text-{}-{compact}",
            tweet.tweet_id()
        ))),
        text,
    )
    .on_click(ranges, move |index, _, cx| {
        let Some(url) = urls.get(index) else {
            return;
        };
        cx.stop_propagation();
        cx.open_url(url.as_ref());
    })
    .into_any_element()
}

/// A tweet that is quoted under the tweet quoting it.
fn quote_block(
    tweet: &TweetItem,
    feed: &TweetSnapshot,
    media: &MediaStates,
    retry: Option<&MediaRetry>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let author = feed.author(&tweet.x_user_id);
    let url = tweet_url(author.username.as_ref(), tweet.tweet_id());
    let mut column = div()
        .min_w_0()
        .flex_1()
        .flex()
        .flex_col()
        .gap_1()
        .child(tweet_header(tweet, &author, cx));
    if !tweet.text.is_empty() {
        column = column.child(
            div()
                .min_w_0()
                .text_xs()
                .whitespace_normal()
                .debug_selector(|| "TEST_TWEET_QUOTE_TEXT".into())
                .child(interactive_text(tweet, true, cx)),
        );
    }
    if !tweet.media.is_empty() {
        column = column.child(media_gallery(tweet, media, true, retry, cx));
    }

    div()
        .id(format_id("tweet-quote", tweet.tweet_id()))
        .min_w_0()
        .flex()
        .gap_2()
        .rounded_sm()
        .border_1()
        .border_color(theme.roles.line.divider.hsla)
        .px_2()
        .py_2()
        .cursor_pointer()
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            cx.open_url(&url);
        })
        .child(avatar(
            author.avatar_url.as_ref(),
            media,
            format_id("tweet-quote-avatar", tweet.tweet_id()),
            None,
            theme.roles.surface.raised.hsla,
            theme.roles.fg.muted.hsla,
            px(QUOTE_AVATAR_SIZE),
        ))
        .child(column)
        .into_any_element()
}

/// Every picture one tweet carries, laid out as one gallery.
fn media_gallery(
    tweet: &TweetItem,
    media: &MediaStates,
    compact: bool,
    retry: Option<&MediaRetry>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let (count, ratio, fitted_width, max_height) = gallery_geometry(&tweet.media, compact);
    let columns = if count == 1 { 1 } else { 2 };
    let rows = if count >= 3 { 2 } else { 1 };

    let mut grid = div()
        .w_full()
        .max_w(px(fitted_width))
        .max_h(px(max_height))
        .min_w_0()
        .grid()
        .grid_cols(columns)
        .grid_rows(rows)
        .gap_0p5()
        .aspect_ratio(ratio)
        .rounded_sm()
        .overflow_hidden()
        // The gallery's own backdrop is a surface, not an edge: it is what shows between and
        // behind the tiles while they load, and it matches the surfaces the tiles themselves
        // fall back to.
        .bg(theme.roles.surface.raised.hsla);

    for (index, item) in tweet.media.iter().take(count).enumerate() {
        let mut tile = div().size_full().min_h_0().overflow_hidden();
        if count == 3 && index == 0 {
            tile = tile.row_span(2);
        }
        grid = grid.child(tile.child(media_tile(tweet, item, media, index, retry, cx)));
    }

    grid.into_any_element()
}

/// How tall one gallery is, and how wide: the numbers X's own layout is closest to.
fn gallery_geometry(media: &[TweetMediaItem], compact: bool) -> (usize, f32, f32, f32) {
    let count = media.len().min(4);
    let ratio = match media.first() {
        Some(first) if count == 1 => {
            let ratio = first.aspect_ratio();
            if compact {
                ratio.clamp(1.5, 2.4)
            } else {
                ratio.clamp(0.75, 2.0)
            }
        }
        _ if compact => 2.0,
        _ => 16.0 / 9.0,
    };
    let (max_width, max_height) = if compact {
        (GALLERY_COMPACT_WIDTH, GALLERY_COMPACT_HEIGHT)
    } else {
        (GALLERY_WIDE_WIDTH, GALLERY_WIDE_HEIGHT)
    };
    (count, ratio, max_width.min(max_height * ratio), max_height)
}

/// One picture: what is known about it, or the way to ask for it again.
fn media_tile(
    tweet: &TweetItem,
    item: &TweetMediaItem,
    media: &MediaStates,
    index: usize,
    retry: Option<&MediaRetry>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let url = item.url().clone();
    let id = SharedString::new(format_smolstr!("tweet-media-{}-{index}", tweet.tweet_id()));

    match media.get(url.as_ref()) {
        Some(MediaState::Ready(path)) => media_ready(item, path, id, cx),
        Some(MediaState::Failed) => media_failed(theme, id, url, retry, cx),
        // Nothing known about a picture and a picture being fetched look the same, because
        // from here they are the same: neither has bytes on disk yet.
        Some(MediaState::Loading) | None => media_loading(theme),
    }
}

/// A picture whose bytes are on disk.
fn media_ready(
    item: &TweetMediaItem,
    path: &std::path::Path,
    id: SharedString,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let mut tile = div()
        .relative()
        .size_full()
        .rounded_sm()
        .bg(theme.roles.surface.raised.hsla)
        .child(
            img(path.to_path_buf())
                .id(ElementId::Name(id))
                .size_full()
                .rounded_sm()
                .object_fit(ObjectFit::Cover),
        );
    match item {
        TweetMediaItem::Video { .. } => {
            tile = tile.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .size(px(32.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(theme.roles.brand.base.hsla)
                            .child(
                                Icon::new(IconKind::Play)
                                    .size_4()
                                    .text_color(theme.roles.fg.on_brand.hsla),
                            ),
                    ),
            );
        }
        // A GIF whose still frame is what was fetched says so, because what is on screen is
        // not what the tweet carries.
        TweetMediaItem::Gif {
            poster_only: true, ..
        } => {
            tile = tile.child(
                div()
                    .absolute()
                    .left(px(6.0))
                    .bottom(px(6.0))
                    .h(px(18.0))
                    .px_1()
                    .flex()
                    .items_center()
                    .rounded_sm()
                    .bg(theme.roles.brand.base.hsla)
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.roles.fg.on_brand.hsla)
                    .child(SharedString::new_static("GIF")),
            );
        }
        TweetMediaItem::Image { .. } | TweetMediaItem::Gif { .. } => {}
    }
    tile.into_any_element()
}

/// A picture that is being fetched.
///
/// The frame is not animated: the body this tile is drawn in is cached, and an animation
/// inside a cached subtree would freeze rather than spin.
fn media_loading(theme: &Theme) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .bg(theme.roles.surface.raised.hsla)
        .child(
            Icon::new(IconKind::CircleNotch)
                .size_5()
                .text_color(theme.roles.fg.muted.hsla),
        )
        .into_any_element()
}

/// A picture that could not be fetched, and the way to ask for it again.
fn media_failed(
    theme: &Theme,
    id: SharedString,
    url: SharedString,
    retry: Option<&MediaRetry>,
    cx: &App,
) -> AnyElement {
    let muted = theme.roles.fg.muted.hsla;
    let mut tile = div()
        .size_full()
        .min_h_0()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .px_2()
        .rounded_sm()
        .bg(theme.roles.surface.raised.hsla)
        .text_center()
        .text_xs()
        .text_color(muted)
        .child(
            Icon::new(IconKind::WarningCircle)
                .size_5()
                .text_color(muted),
        )
        .child(t!(cx, "widget.x_account_monitoring.media_load_failed"));
    if let Some(retry) = retry {
        let widget = retry.widget.clone();
        tile = tile.child(
            Button::new(ElementId::Name(SharedString::new(format_smolstr!(
                "{id}-retry"
            ))))
            .ghost()
            .small()
            .aria_label(t!(cx, "common.retry"))
            .with_tooltip(t!(cx, "common.retry"))
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                let url = SmolStr::new(url.as_str());
                widget.update(cx, |widget, cx| widget.retry_media(url, cx));
            })
            .child(Icon::new(IconKind::ArrowsClockwise).size_3()),
        );
    }
    tile.into_any_element()
}

/// The row that says the timeline has no more tweets to show.
fn timeline_end(cx: &App) -> AnyElement {
    div()
        .id(ElementId::Name(SharedString::new_static(
            "TweetMonitorTimelineEnd",
        )))
        .debug_selector(|| "TEST_TWEET_TIMELINE_END".into())
        .role(Role::ListItem)
        .w_full()
        .min_w_0()
        .py_2()
        .text_center()
        .text_xs()
        .text_color(cx.theme().roles.fg.muted.hsla)
        .child(t!(cx, "widget.x_account_monitoring.timeline_end"))
        .into_any_element()
}

/// A face, or the stand-in that says there is one to come.
///
/// `selector` names the place the face is drawn in, which is what a test measures a row by: a
/// face is the tallest thing a dialog's row holds, so where it sits is where the row's own
/// height shows.
pub(super) fn avatar(
    url: &str,
    media: &MediaStates,
    id: ElementId,
    selector: Option<&'static str>,
    background: Hsla,
    foreground: Hsla,
    size: gpui_kit::Pixels,
) -> AnyElement {
    let face = div()
        .size(size)
        .flex_none()
        .debug_selector(move || selector.map_or_else(String::new, String::from));
    match media.get(url) {
        Some(MediaState::Ready(path)) => face
            .rounded_full()
            .overflow_hidden()
            .bg(background)
            .child(
                img(path.clone())
                    .id(id)
                    .size_full()
                    .rounded_full()
                    .object_fit(ObjectFit::Cover),
            )
            .into_any_element(),
        Some(MediaState::Loading) | Some(MediaState::Failed) | None => face
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(background)
            .text_sm()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(foreground)
            .child(SharedString::new_static("@"))
            .into_any_element(),
    }
}

/// A message in the middle of a panel.
pub(super) fn centered_message(message: SharedString, color: Hsla) -> AnyElement {
    div()
        .size_full()
        .min_h_0()
        .flex()
        .items_center()
        .justify_center()
        .px_4()
        .text_center()
        .text_sm()
        .text_color(color)
        .child(message)
        .into_any_element()
}

/// A message and the control that offers a way out of it.
pub(super) fn message_with_action(message: SharedString, action: AnyElement) -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .px_4()
        .text_center()
        .text_xs()
        .child(message)
        .child(div().flex_none().child(action))
        .into_any_element()
}

/// The recovery the pane itself offers, as a dialog's way out of having no stream.
pub(super) fn recovery_button(action: &WidgetAction, cx: &App) -> AnyElement {
    Button::new("tweet-monitor-recover")
        .outline()
        .small()
        .disabled(!action.is_enabled())
        .on_click({
            let action = action.clone();
            move |_, _, cx| action.run(cx)
        })
        .child(action.label(cx))
        .into_any_element()
}

pub(super) fn status_label(status: XAccountMonitoringSelectionStatus, cx: &App) -> SharedString {
    match status {
        XAccountMonitoringSelectionStatus::Active => t!(cx, "widget.x_account_monitoring.active"),
        XAccountMonitoringSelectionStatus::Paused => t!(cx, "widget.x_account_monitoring.paused"),
        XAccountMonitoringSelectionStatus::Pending => t!(cx, "widget.x_account_monitoring.pending"),
    }
}

/// The address of one tweet on X, which is what a row opens.
fn tweet_url(username: &str, tweet_id: &str) -> String {
    format!("https://x.com/{username}/status/{tweet_id}")
}

/// How long ago a tweet was published, in the reader's language.
fn age_text(age: TweetAge, cx: &App) -> SharedString {
    match age {
        TweetAge::Seconds(seconds) => {
            t!(
                cx,
                "widget.x_account_monitoring.age_seconds",
                count = seconds
            )
        }
        TweetAge::Minutes(minutes) => {
            t!(
                cx,
                "widget.x_account_monitoring.age_minutes",
                count = minutes
            )
        }
        TweetAge::Hours(hours) => t!(cx, "widget.x_account_monitoring.age_hours", count = hours),
        TweetAge::Date { month, day } => t!(
            cx,
            "widget.x_account_monitoring.age_date",
            month = month,
            day = day
        ),
        TweetAge::FullDate { year, month, day } => t!(
            cx,
            "widget.x_account_monitoring.age_full_date",
            year = year,
            month = month,
            day = day
        ),
        TweetAge::Unknown => SharedString::new_static("--"),
    }
}

/// The element id of one of a row's own parts, which is unique within the row it is drawn in.
fn format_id(prefix: &str, tweet_id: &str) -> ElementId {
    ElementId::Name(SharedString::new(format_smolstr!("{prefix}-{tweet_id}")))
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc, time::Duration};

    use app_config::{
        settings::{AccountConfig, AccountWalletKind, Config},
        widget::{VersionedWidgetConfig, decode_or_default},
    };
    use base_ui::{input::InputStateBuilder, layer::Layer};
    use gpui_kit::{
        AppContext, BorrowAppContext, Bounds, IntoElement, Modifiers, Pixels, TestAppContext,
        VisualTestContext, px,
    };
    use i18n::language::codes::LanguageCode;
    use kytrix_service_provider::service::{
        engine::{KytrixServiceError, XAccountMonitoringTweetEvent},
        x::XAccountMonitoringMutationUnknown,
    };
    use network::transport::error::TransportError;
    use widget::protocol::RequestId;

    use super::*;
    use crate::{
        service::{TweetStreamMessage, TweetUpdate},
        test_support::{
            ACCOUNT, FakeFeed, FakeSession, FakeSource, FakeTransport, media_cache, media_url,
            profile, search_profile, seed_picture, selection, selections, settle_until, tweet,
            tweet_with_media,
        },
    };

    const OTHER_ACCOUNT: &str = "another-account";

    /// Settings holding `account`, or holding nobody when it is `None`.
    fn settings(account: Option<&str>) -> Config {
        let mut config = Config::default();
        config.account = account.map(|user_id| AccountConfig {
            user_id: user_id.to_owned(),
            wallet_address: "0x1111111111111111111111111111111111111111".to_owned(),
            wallet_kind: AccountWalletKind::Ethereum,
        });
        config
    }

    /// Installs everything a pane reads, with `account` signed in or nobody.
    ///
    /// The base layer's own state is installed first, because the dialog's fields are built on
    /// it: a field that is typed into reads the state `gpui_kit::base::init` installs.
    fn install(cx: &mut App, account: Option<&str>) {
        gpui_kit::base::init(cx);
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
        global::install(cx).expect("the test Tokio runtime should build");
        let manager = ConfigManager::in_memory(settings(account), cx);
        cx.set_global(manager);
    }

    /// A pane the framework would have built, driven by a fake account and a fake transport.
    fn pane_with_source<S, T>(
        source: S,
        downloader: MediaDownloader<T>,
        cx: &mut Context<XAccountMonitoringWidget>,
    ) -> XAccountMonitoringWidget
    where
        S: TweetSource,
        T: crate::media::MediaTransport,
    {
        let status = cx.new(|_| WidgetStatus::new());
        let weak = cx.weak_entity();
        let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
            let _ = weak.update(cx, |pane, cx| pane.load(cx));
        });
        XAccountMonitoringWidget::new_with_source(
            TweetMonitorConfig,
            source,
            downloader,
            status,
            WidgetConfigChange::default(),
            recovery,
            cx,
        )
    }

    /// A pane a test drives by hand, with the production stream left where it is.
    fn pane_with(
        cx: &mut TestAppContext,
        account: Option<&str>,
        source: FakeSource,
        transport: FakeTransport,
        cache: crate::media::MediaCache,
    ) -> Entity<XAccountMonitoringWidget> {
        cx.executor().allow_parking();
        cx.update(|cx| {
            install(cx, account);
            cx.new(|cx| pane_with_source(source, MediaDownloader::with_cache(transport, cache), cx))
        })
    }

    /// A pane holding an account with nothing started, so what a test reports to it is the
    /// only input it has.
    ///
    /// The cache directory is handed back with the pane because a pane that fetches a picture
    /// creates it: the caller holds the handle, so the directory goes away with the test rather
    /// than outliving it as an empty directory nothing owns.
    fn quiet_pane(
        cx: &mut TestAppContext,
        source: FakeSource,
    ) -> (Entity<XAccountMonitoringWidget>, tempfile::TempDir) {
        let (cache, root) = media_cache();
        let pane = pane_with(cx, Some(ACCOUNT), source, FakeTransport::failing(), cache);
        pane.update(cx, |pane, _| {
            pane.account_id = Some(SmolStr::new_static(ACCOUNT));
        });
        (pane, root)
    }

    /// A pane nobody is signed in for.
    fn signed_out_pane(
        cx: &mut TestAppContext,
        source: FakeSource,
    ) -> (Entity<XAccountMonitoringWidget>, tempfile::TempDir) {
        let (cache, root) = media_cache();
        let pane = pane_with(cx, None, source, FakeTransport::failing(), cache);
        pane.update(cx, |pane, cx| pane.load(cx));
        (pane, root)
    }

    /// A pane reading an account, with the stream the framework starts left running.
    fn pane(
        cx: &mut TestAppContext,
        source: &FakeSource,
    ) -> (Entity<XAccountMonitoringWidget>, tempfile::TempDir) {
        let (cache, root) = media_cache();
        let pane = pane_with(
            cx,
            Some(ACCOUNT),
            source.clone(),
            FakeTransport::failing(),
            cache,
        );
        pane.update(cx, |pane, cx| pane.load(cx));
        (pane, root)
    }

    /// A pane whose stream is live and whose timeline is the one it read.
    fn live_pane(
        cx: &mut TestAppContext,
        source: &FakeSource,
        session: &FakeSession,
        tweets: Vec<XAccountMonitoringTweetEvent>,
    ) -> (Entity<XAccountMonitoringWidget>, tempfile::TempDir) {
        let (cache, root) = media_cache();
        live_pane_over(cx, source, session, tweets, cache, root)
    }

    /// A live pane over a cache of the caller's choosing, which is what a test that wants a
    /// picture on disk before the stream reaches it holds.
    fn live_pane_over(
        cx: &mut TestAppContext,
        source: &FakeSource,
        session: &FakeSession,
        tweets: Vec<XAccountMonitoringTweetEvent>,
        cache: crate::media::MediaCache,
        root: tempfile::TempDir,
    ) -> (Entity<XAccountMonitoringWidget>, tempfile::TempDir) {
        session.answer_history(tweets, vec![profile("42", "kytrix")]);
        let pane = pane_with(
            cx,
            Some(ACCOUNT),
            source.clone(),
            FakeTransport::failing(),
            cache,
        );
        pane.update(cx, |pane, cx| pane.load(cx));
        settle_until(cx, "the stream to come up", |cx| pane.read(cx).ready);
        settle_until(cx, "the timeline to arrive", |cx| {
            pane.read(cx).feed.is_some()
        });

        (pane, root)
    }

    /// A live pane holding a timeline of `count` tweets.
    fn live_pane_with(
        cx: &mut TestAppContext,
        source: &FakeSource,
        session: &FakeSession,
        count: usize,
    ) -> (Entity<XAccountMonitoringWidget>, tempfile::TempDir) {
        let tweets = (0..count)
            .map(|index| tweet(&index.to_string(), index as i64))
            .collect();
        live_pane(cx, source, session, tweets)
    }

    /// A live pane with the monitored accounts already read.
    fn live_pane_reading(
        cx: &mut TestAppContext,
        source: &FakeSource,
        session: &FakeSession,
        accounts: Vec<kytrix_service_provider::service::x::XAccountMonitoringSelection>,
        limit: usize,
    ) -> (Entity<XAccountMonitoringWidget>, tempfile::TempDir) {
        let (pane, root) = live_pane(cx, source, session, vec![tweet("1", 1)]);
        session.answer_selections(selections(accounts, limit));
        pane.update(cx, |pane, cx| {
            pane.load_selections(cx);
        });
        settle_until(cx, "the accounts to arrive", |cx| {
            pane.read(cx).selections.is_some()
        });

        (pane, root)
    }

    /// The dialog a pane opens, as the pane holds it.
    ///
    /// `show` itself needs the window layer the application installs, so a test attaches the
    /// same dialog the way the pane would hold it.
    fn attach_dialog(
        pane: &Entity<XAccountMonitoringWidget>,
        cx: &mut TestAppContext,
    ) -> Entity<TweetDialog> {
        let window = cx.add_empty_window();
        let dialog = window.update(|window, cx| {
            let filter = cx.new(|cx| InputStateBuilder::new("tweet-filter").build(window, cx));
            let query = cx.new(|cx| InputStateBuilder::new("tweet-query").build(window, cx));
            cx.new(|cx| TweetDialog::new(pane.downgrade(), filter, query, cx))
        });
        pane.update(cx, |pane, _| pane.dialog = Some(dialog.downgrade()));
        dialog
    }

    /// Opens the page that searches X and runs one search from it, the way a reader does: the
    /// query is typed into the page's own field and the control beside it is clicked.
    ///
    /// The page is what a search belongs to, so this is the whole way in — and a query is typed
    /// rather than set because an answer belongs to the query that asked for it.
    fn search_from_page(
        pane: &Entity<XAccountMonitoringWidget>,
        dialog: &Entity<TweetDialog>,
        typed: &str,
        window: &mut VisualTestContext,
    ) {
        dialog.update(&mut window.cx, |dialog, cx| dialog.open_add(cx));
        settle_pages(window);
        let field = window
            .debug_bounds("TEST_TWEET_QUERY_FIELD")
            .expect("the search page should draw the field X is searched with")
            .center();
        window.simulate_click(field, Modifiers::none());
        window.simulate_input(typed);
        settle_pages(window);
        let search = window
            .debug_bounds("TEST_TWEET_SEARCH")
            .expect("the search page should draw the control that sends the search")
            .center();
        window.simulate_click(search, Modifiers::none());
        settle_until(&mut window.cx, "the search to be answered", |cx| {
            pane.read(cx).searching.is_none()
        });
        window.update(|window, _| window.refresh());
        window.run_until_parked();
    }

    /// Reports one timeline as a stream would, at the revision after the last one.
    fn report_update(
        pane: &Entity<XAccountMonitoringWidget>,
        feed: TweetSnapshot,
        media: MediaStates,
        cx: &mut TestAppContext,
    ) -> bool {
        cx.update(|cx| {
            let scope = pane
                .read(cx)
                .current_scope()
                .expect("the pane follows an account");
            let revision = pane.read(cx).revision + 1;
            pane.update(cx, |pane, cx| {
                pane.apply_latest(
                    scope,
                    Arc::new(TweetStreamMessage::Update(TweetUpdate::published(
                        revision, feed, media,
                    ))),
                    cx,
                )
            })
        })
    }

    /// Reports a status as a stream would.
    fn report_status(
        pane: &Entity<XAccountMonitoringWidget>,
        status: TweetStatus,
        cx: &mut TestAppContext,
    ) -> bool {
        cx.update(|cx| {
            let scope = pane
                .read(cx)
                .current_scope()
                .expect("the pane follows an account");
            pane.update(cx, |pane, cx| {
                pane.apply_reliable(scope, Arc::new(TweetStreamMessage::Status(status)), cx)
            })
        })
    }

    /// What the pane reports, as a frame would read it.
    fn state_of(pane: &Entity<XAccountMonitoringWidget>, cx: &mut TestAppContext) -> WidgetState {
        cx.update(|cx| pane.read(cx).status.state(cx))
    }

    /// The notice the pane is standing in with.
    fn notice_of(
        pane: &Entity<XAccountMonitoringWidget>,
        cx: &mut TestAppContext,
    ) -> Option<Notice> {
        cx.update(|cx| pane.read(cx).status.state(cx).notice().cloned())
    }

    /// Whether the pane has a body to draw.
    fn has_content(pane: &Entity<XAccountMonitoringWidget>, cx: &mut TestAppContext) -> bool {
        cx.update(|cx| pane.read(cx).has_content())
    }

    /// The tweet ids the pane holds, which is what its list is drawn from.
    ///
    /// The list is not read: what it holds is the list's own, and what a reader sees of it is
    /// asserted by drawing the pane.
    fn feed_ids(pane: &XAccountMonitoringWidget) -> Vec<String> {
        pane.feed
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .items()
                    .iter()
                    .map(|item| item.tweet_id().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The tweet ids the pane a test is holding has.
    fn ids_of(pane: &Entity<XAccountMonitoringWidget>, cx: &mut TestAppContext) -> Vec<String> {
        cx.update(|cx| feed_ids(pane.read(cx)))
    }

    /// One row the service has not activated yet, which by contract carries no profile.
    fn pending(
        x_user_id: &str,
    ) -> kytrix_service_provider::service::x::XAccountMonitoringSelection {
        let mut selection = selection(x_user_id, "");
        selection.status = XAccountMonitoringSelectionStatus::Pending;
        selection.profile = None;
        selection
    }

    /// What the pane remembers about the names of the rows it holds, which is what a row
    /// waiting to be activated is drawn with.
    fn remembered_names(
        pane: &Entity<XAccountMonitoringWidget>,
        cx: &mut TestAppContext,
    ) -> HashMap<SmolStr, SharedString> {
        cx.update(|cx| (*pane.read(cx).hints).clone())
    }

    /// The usernames of the accounts the pane holds.
    fn usernames_of(
        pane: &Entity<XAccountMonitoringWidget>,
        cx: &mut TestAppContext,
    ) -> Vec<String> {
        cx.update(|cx| {
            pane.read(cx)
                .selections
                .as_ref()
                .map_or_else(Vec::new, |data| {
                    data.selections()
                        .iter()
                        .map(|selection| {
                            selection.profile.as_ref().map_or_else(
                                || selection.x_user_id.to_string(),
                                |profile| profile.username.to_string(),
                            )
                        })
                        .collect()
                })
        })
    }

    #[test]
    fn the_widget_should_have_a_stable_identity() {
        assert_eq!(
            XAccountMonitoringWidget::ID.as_str(),
            "kytrix.market.x_account_monitoring"
        );
        assert_eq!(
            XAccountMonitoringWidget::DISPLAY_NAME,
            "widget.names.x_account_monitoring"
        );
        assert_eq!(
            TweetMonitorConfig::SCHEMA_VERSION,
            WidgetConfigSchemaVersion::V1
        );
    }

    #[test]
    fn the_config_should_round_trip_through_its_envelope() {
        let stored = VersionedWidgetConfig::encode(&TweetMonitorConfig)
            .expect("a tweet monitor config should encode");

        assert_eq!(stored.schema_version(), WidgetConfigSchemaVersion::V1);
        assert_eq!(stored.decode::<TweetMonitorConfig>(), TweetMonitorConfig);
        assert_eq!(
            decode_or_default::<TweetMonitorConfig>(None),
            TweetMonitorConfig
        );
    }

    /// A missing key is an empty string on screen rather than an error, so a gap in the
    /// interface is what a typo looks like: this is the test that turns it into a failure.
    #[gpui_kit::test]
    fn every_message_this_pane_shows_should_resolve_to_text(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(cx, Some(ACCOUNT));

            for key in [
                "widget.names.x_account_monitoring",
                "widget.x_account_monitoring.account_id",
                "widget.x_account_monitoring.active",
                "widget.x_account_monitoring.add_account",
                "widget.x_account_monitoring.add_search_result",
                "widget.x_account_monitoring.add_success",
                "widget.x_account_monitoring.age_date",
                "widget.x_account_monitoring.age_full_date",
                "widget.x_account_monitoring.age_hours",
                "widget.x_account_monitoring.age_minutes",
                "widget.x_account_monitoring.age_seconds",
                "widget.x_account_monitoring.already_monitored",
                "widget.x_account_monitoring.back",
                "widget.x_account_monitoring.delete_account",
                "widget.x_account_monitoring.dialog_title",
                "widget.x_account_monitoring.empty_accounts",
                "widget.x_account_monitoring.error_title",
                "widget.x_account_monitoring.filter_placeholder",
                "widget.x_account_monitoring.followers_following",
                "widget.x_account_monitoring.load_failed",
                "widget.x_account_monitoring.loading_accounts",
                "widget.x_account_monitoring.logged_out",
                "widget.x_account_monitoring.manage",
                "widget.x_account_monitoring.media_load_failed",
                "widget.x_account_monitoring.monitored_accounts",
                "widget.x_account_monitoring.no_search_results",
                "widget.x_account_monitoring.paused",
                "widget.x_account_monitoring.pending",
                "widget.x_account_monitoring.protected",
                "widget.x_account_monitoring.refresh",
                "widget.x_account_monitoring.reposted",
                "widget.x_account_monitoring.scroll_tweets",
                "widget.x_account_monitoring.search_placeholder",
                "widget.x_account_monitoring.search_users",
                "widget.x_account_monitoring.searching_users",
                "widget.x_account_monitoring.stream_unavailable",
                "widget.x_account_monitoring.success_title",
                "widget.x_account_monitoring.timeline_end",
                "widget.x_account_monitoring.tweet",
                "widget.x_account_monitoring.waiting",
            ] {
                assert!(!t!(cx, key).is_empty(), "{key} has no copy");
            }

            for failure in [
                SelectionFailure::Rejected(XAccountMonitoringApiError::Authentication),
                SelectionFailure::Unknown(XAccountMonitoringMutationUnknown::Timeout),
                SelectionFailure::Rpc(RpcCallError::DeadlineExceeded {
                    request_id: RequestId::new(2),
                }),
            ] {
                assert!(!failure.localized_message(cx).is_empty());
            }
            // A search the service refused is drawn on the page that asked for it in the words
            // the failure itself gives, so those words have to be there: a key a locale does not
            // define translates to nothing, and a search that failed would say so with a blank
            // panel.
            for failure in [
                RpcCallError::Service {
                    request_id: RequestId::new(3),
                    source: XAccountMonitoringApiError::NotReady,
                },
                RpcCallError::ServiceClosed {
                    request_id: RequestId::new(4),
                },
                RpcCallError::ResponseDropped {
                    request_id: RequestId::new(5),
                },
            ] {
                assert!(!failure.localized_message(cx).is_empty());
            }
            assert!(
                !TweetMonitorFailure::Stream(TweetStreamError::Transport(TransportError::Closed))
                    .localized_message(cx)
                    .is_empty()
            );
            assert!(
                !TweetMonitorFailure::Stream(TweetStreamError::Service(
                    KytrixServiceError::Timeout
                ))
                .localized_message(cx)
                .is_empty()
            );
        });
    }

    #[gpui_kit::test]
    fn a_pane_nobody_is_signed_in_for_should_wait_and_follow_nothing(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let (pane, _root) = signed_out_pane(cx, source.clone());

        settle_until(cx, "the pane to report waiting", |cx| {
            pane.read(cx).status.state(cx).notice().is_some()
        });
        assert!(!has_content(&pane, cx));
        assert_eq!(
            state_of(&pane, cx),
            WidgetState::waiting(Notice::message("widget.x_account_monitoring.logged_out")),
            "a pane nobody is signed in for waits rather than working"
        );
        // A sign-out takes the subscription with it: nothing is followed for an account the
        // pane is not reading as, which is what an acquisition that never happened says.
        assert!(source.requested().is_empty());
    }

    #[gpui_kit::test]
    fn a_pane_whose_account_could_not_be_taken_should_fail_and_offer_a_way_out(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        source.refuse(KytrixServiceError::Authentication(SmolStr::new_static(
            "the test credential is gone",
        )));
        let (pane, _root) = pane(cx, &source);

        settle_until(cx, "the failure to be reported", |cx| {
            pane.read(cx).status.is_failed(cx)
        });
        let state = cx.update(|cx| pane.read(cx).status.state(cx));
        let failure = state
            .failure()
            .expect("an account that could not be taken is reported");
        assert_eq!(
            failure.action().label_key(),
            "common.retry",
            "every failure offers the way out the framework wired"
        );
    }

    #[gpui_kit::test]
    fn a_pane_should_be_content_only_once_a_timeline_arrives(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) = live_pane(cx, &source, &session, vec![tweet("1", 1)]);

        assert!(has_content(&pane, cx));
        assert_eq!(ids_of(&pane, cx), ["1"]);
        assert_eq!(state_of(&pane, cx), WidgetState::Ready);
    }

    #[gpui_kit::test]
    fn an_empty_timeline_should_be_a_pane_waiting_rather_than_content(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) = live_pane(cx, &source, &session, Vec::new());

        assert!(!has_content(&pane, cx));
        assert_eq!(
            notice_of(&pane, cx),
            Some(Notice::message("widget.x_account_monitoring.waiting")),
            "a live stream with nothing to show says so"
        );
    }

    /// A waiting pane follows the language without being reloaded: the notice is a reference
    /// into the catalogue, resolved on the frame that draws it, so the language moving writes
    /// no state at all.
    ///
    /// The same test pins the other half of that contract, which is what keeps it cheap: the
    /// pane reports its notice on every accepted timeline update, so the reference is compared
    /// where it is written — the same one again is not a write and schedules no frame.
    #[gpui_kit::test]
    fn a_language_change_should_relabel_the_sentence_a_waiting_pane_shows(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) = live_pane(cx, &source, &session, Vec::new());

        let notice = notice_of(&pane, cx).expect("a live stream with nothing to show stands in");
        assert_eq!(
            notice,
            Notice::message("widget.x_account_monitoring.waiting")
        );
        let english = cx.update(|cx| notice.resolve(cx));
        assert_eq!(
            english,
            cx.update(|cx| t!(cx, "widget.x_account_monitoring.waiting")),
            "the pane starts out in the language showing now"
        );

        let cell = cx.update(|cx| pane.read(cx).status.clone());
        let reports = Rc::new(Cell::new(0));
        let _observation = cx.update({
            let reports = reports.clone();
            move |cx| {
                cx.observe(&cell, move |_, _| {
                    reports.set(reports.get() + 1);
                })
            }
        });

        // The hot path: a timeline update reports the notice that is already there.
        pane.update(cx, |pane, cx| pane.report_no_content_notice(cx));
        assert_eq!(reports.get(), 0, "the same notice again is not a write");

        cx.update(|cx| cx.set_global(LanguageCode::ZhCn));

        assert_eq!(
            reports.get(),
            0,
            "a language change is not a write either: the sentence is resolved where it is drawn"
        );
        let chinese = cx.update(|cx| notice.resolve(cx));
        assert_ne!(chinese, english, "the two locales differ");
        assert_eq!(
            notice_of(&pane, cx),
            Some(Notice::message("widget.x_account_monitoring.waiting")),
            "and the pane still stands in with the same reference"
        );
        assert_eq!(
            chinese,
            cx.update(|cx| t!(cx, "widget.x_account_monitoring.waiting")),
            "which is what the pane says in the language showing now"
        );
    }

    /// A pane nobody is signed in for is waiting too, with the notice that sends the reader to
    /// the account settings: that one follows the language the same way, because it too is a
    /// reference resolved where it is drawn.
    #[gpui_kit::test]
    fn a_language_change_should_relabel_the_sentence_of_a_signed_out_pane(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let (pane, _root) = signed_out_pane(cx, source);

        settle_until(cx, "the pane to report waiting", |cx| {
            pane.read(cx).status.state(cx).notice().is_some()
        });
        let notice = notice_of(&pane, cx).expect("a pane nobody is signed in for stands in");
        assert_eq!(
            notice,
            Notice::message("widget.x_account_monitoring.logged_out")
        );
        let logged_out = cx.update(|cx| notice.resolve(cx));
        assert_eq!(
            logged_out,
            cx.update(|cx| t!(cx, "widget.x_account_monitoring.logged_out")),
            "a pane nobody is signed in for says so"
        );

        cx.update(|cx| cx.set_global(LanguageCode::ZhCn));

        let signed_out_now = cx.update(|cx| notice.resolve(cx));
        assert_ne!(signed_out_now, logged_out, "the two locales differ");
        assert_eq!(
            signed_out_now,
            cx.update(|cx| t!(cx, "widget.x_account_monitoring.logged_out")),
            "and the same reference resolves in the language showing now"
        );
    }

    #[gpui_kit::test]
    fn a_timeline_from_another_account_should_not_be_shown(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) = live_pane(cx, &source, &session, vec![tweet("1", 1)]);
        let mut feed = FakeFeed::new();
        let snapshot = feed.push("9", 9);

        let applied = pane.update(cx, |pane, cx| {
            pane.apply_latest(
                TweetScope::new(Some(&SmolStr::new_static(OTHER_ACCOUNT)))
                    .expect("another account is a scope"),
                Arc::new(TweetStreamMessage::Update(TweetUpdate::published(
                    9,
                    snapshot,
                    Arc::new(HashMap::new()),
                ))),
                cx,
            )
        });

        assert!(!applied);
        assert_eq!(ids_of(&pane, cx), ["1"]);
    }

    #[gpui_kit::test]
    fn an_update_older_than_the_one_on_screen_should_not_be_shown(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) = live_pane(cx, &source, &session, vec![tweet("1", 1)]);
        let mut feed = FakeFeed::new();
        let snapshot = feed.push("2", 2);
        let revision = cx.update(|cx| pane.read(cx).revision);

        // The same revision the pane already applied is not a newer timeline: the latest
        // channel coalesces, so an update can arrive after one that already replaced it.
        let applied = cx.update(|cx| {
            let scope = pane
                .read(cx)
                .current_scope()
                .expect("the pane follows an account");
            pane.update(cx, |pane, cx| {
                pane.apply_latest(
                    scope,
                    Arc::new(TweetStreamMessage::Update(TweetUpdate::published(
                        revision,
                        snapshot,
                        Arc::new(HashMap::new()),
                    ))),
                    cx,
                )
            })
        });

        assert!(!applied);
        assert_eq!(ids_of(&pane, cx), ["1"]);
    }

    #[gpui_kit::test]
    fn a_failure_should_keep_the_timeline_and_offer_a_way_out(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) = live_pane(cx, &source, &session, vec![tweet("1", 1), tweet("2", 2)]);

        report_status(
            &pane,
            TweetStatus::Failed(TweetStreamError::Transport(TransportError::Closed)),
            cx,
        );

        let state = cx.update(|cx| pane.read(cx).status.state(cx));
        assert!(state.is_failed());
        let failure = state.failure().expect("a failure is what the pane reports");
        assert!(
            !cx.update(|cx| failure.text(cx)).is_empty(),
            "a failure has to say what went wrong"
        );
        assert_eq!(
            failure.action().label_key(),
            "common.retry",
            "every failure offers the way out the framework wired"
        );
        // What a reader was looking at is not blanked by a stream that ended.
        assert_eq!(ids_of(&pane, cx), ["2", "1"]);
        assert!(has_content(&pane, cx));
    }

    #[gpui_kit::test]
    fn a_message_after_a_failure_should_not_be_applied(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) = live_pane(cx, &source, &session, vec![tweet("1", 1)]);
        report_status(
            &pane,
            TweetStatus::Failed(TweetStreamError::Service(KytrixServiceError::Closed)),
            cx,
        );
        let mut feed = FakeFeed::new();
        let snapshot = feed.push("9", 9);

        let applied = report_update(&pane, snapshot, Arc::new(HashMap::new()), cx);

        assert!(!applied);
        assert_eq!(ids_of(&pane, cx), ["1"]);
    }

    #[gpui_kit::test]
    fn a_recovery_should_keep_the_timeline_and_another_account_should_drop_it(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) = live_pane(cx, &source, &session, vec![tweet("1", 1)]);

        let replacement = source.session(ACCOUNT);
        replacement.answer_history(vec![tweet("2", 2)], vec![profile("42", "kytrix")]);
        let _control = replacement.subscribe();
        pane.update(cx, |pane, cx| pane.start(true, cx));
        assert_eq!(
            ids_of(&pane, cx),
            ["1"],
            "a recovery keeps what a reader was looking at"
        );
        settle_until(cx, "the recovered stream to come up", |cx| {
            pane.read(cx).ready
        });
        settle_until(cx, "the recovered timeline to arrive", |cx| {
            !feed_ids(pane.read(cx)).is_empty()
        });
        settle_until(cx, "the new tweet to be the newest row", |cx| {
            feed_ids(pane.read(cx)) == ["2"]
        });

        // Another account is another timeline, and the pane cannot vouch for the one it has.
        let _other = source.session(OTHER_ACCOUNT).subscribe();
        pane.update(cx, |pane, cx| {
            pane.reconcile_account(Some(SmolStr::new_static(OTHER_ACCOUNT)), cx);
        });
        assert!(ids_of(&pane, cx).is_empty());
        assert!(!has_content(&pane, cx));
    }

    /// Another account is another timeline, and the pane hears about it from the settings
    /// rather than from the framework: what it follows and what it was showing move together.
    #[gpui_kit::test]
    fn an_account_change_should_start_the_pane_over_on_the_new_account(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) = live_pane(cx, &source, &session, vec![tweet("1", 1)]);
        assert_eq!(ids_of(&pane, cx), ["1"]);

        // The stream the new account is read through is queued first, so what the pane starts
        // over with is the account it was told about rather than a refusal to report.
        let _other = source.session(OTHER_ACCOUNT).subscribe();

        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(
                    |config| {
                        config.account = Some(AccountConfig {
                            user_id: OTHER_ACCOUNT.to_owned(),
                            wallet_address: "0x0000000000000000000000000000000000000002".to_owned(),
                            wallet_kind: AccountWalletKind::Ethereum,
                        });
                    },
                    cx,
                );
            });
        });

        settle_until(cx, "the pane to follow the new account", |cx| {
            pane.read(cx).account_id.as_deref() == Some(OTHER_ACCOUNT)
        });
        assert!(
            ids_of(&pane, cx).is_empty(),
            "another account is another timeline, so the one it left is dropped"
        );
        assert!(!has_content(&pane, cx));
    }

    #[gpui_kit::test]
    fn a_read_should_wait_for_a_live_stream(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_history(vec![tweet("1", 1)], vec![profile("42", "kytrix")]);
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        let (pane, _root) = quiet_pane(cx, source);
        pane.update(cx, |pane, cx| pane.load(cx));

        // The stream is not live yet, so a read has nothing to answer it and is held.
        pane.update(cx, |pane, cx| {
            pane.load_selections(cx);
        });
        assert_eq!(
            session.state().fetches,
            0,
            "a read asked for before the stream is live is not sent"
        );

        settle_until(cx, "the read to go out", |cx| {
            let _ = cx;
            session.state().fetches == 1
        });
        pane.update(cx, |pane, cx| {
            pane.load_selections(cx);
        });
        settle_until(cx, "the accounts to arrive", |cx| {
            pane.read(cx).selections.is_some()
        });
        assert_eq!(usernames_of(&pane, cx), ["seven"]);
    }

    #[gpui_kit::test]
    fn a_dialog_should_ask_for_the_accounts_and_for_their_avatars(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_history(vec![tweet("1", 1)], vec![profile("42", "kytrix")]);
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        let (pane, _root) = live_pane(cx, &source, &session, vec![tweet("1", 1)]);

        let _dialog = attach_dialog(&pane, cx);
        pane.update(cx, |pane, cx| {
            pane.watch_selections(true, cx);
            pane.load_selections(cx);
        });

        settle_until(cx, "the accounts to arrive", |cx| {
            pane.read(cx).selections.is_some()
        });
        assert!(cx.update(|cx| pane.read(cx).dialog_open()));

        // A dismissed dialog is a pane with nothing drawing a face, and the pane stops asking
        // for the ones it was showing.
        pane.update(cx, |pane, cx| pane.on_dialog_closed(cx));
        assert!(!cx.update(|cx| pane.read(cx).dialog_open()));
    }

    /// The dialog draws the page that is showing, and each page is a list of its own: what a
    /// pane holds for one of them is not what the other one draws, and a page shown again is
    /// the page a reader left rather than one starting over.
    ///
    /// The rows are drawn from the pane while the list draws them, so a frame is the only thing
    /// that puts those two in an order.
    #[gpui_kit::test]
    fn a_dialog_should_draw_the_list_of_the_page_that_is_showing(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_history(vec![tweet("1", 1)], vec![profile("42", "kytrix")]);
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        session.answer_search(vec![search_profile("9", "nine")]);
        let (pane, _root) = live_pane(cx, &source, &session, vec![tweet("1", 1)]);

        pane.update(cx, |pane, cx| {
            pane.watch_selections(true, cx);
            pane.load_selections(cx);
        });
        settle_until(cx, "the accounts to arrive", |cx| {
            pane.read(cx).selections.is_some()
        });

        let (dialog, window) = draw_dialog(cx, &pane);
        assert!(
            window.debug_bounds("TEST_TWEET_ACCOUNT_ROW").is_some(),
            "the monitored page draws the accounts the pane holds"
        );
        assert!(
            window.debug_bounds("TEST_TWEET_SEARCH_LIST").is_none(),
            "what one search found belongs to the other page"
        );

        search_from_page(&pane, &dialog, "nine", window);
        assert!(
            window.debug_bounds("TEST_TWEET_SEARCH_ROW").is_some(),
            "the search page draws what the search found"
        );
        assert!(
            window.debug_bounds("TEST_TWEET_ACCOUNT_LIST").is_none(),
            "the accounts belong to the page that draws them"
        );
        // The page that searches X is a page over the one that manages the accounts: what it
        // adds is added to that list, so the controls that belong to it are not drawn here.
        assert!(
            window.debug_bounds("TEST_TWEET_REFRESH").is_none()
                && window.debug_bounds("TEST_TWEET_ADD_ACCOUNT").is_none(),
            "the controls the accounts page offers belong to the accounts page"
        );

        dialog.update(&mut window.cx, |dialog, cx| dialog.go_back(cx));
        settle_pages(window);
        assert!(
            window.debug_bounds("TEST_TWEET_ACCOUNT_ROW").is_some(),
            "a page popped back to is the page the pane still holds"
        );
        assert!(
            window.debug_bounds("TEST_TWEET_ADD_PAGE").is_none(),
            "the page that was popped is off the stack rather than drawn under the one on top"
        );
    }

    /// What one dialog searched for is not what the next one draws: a search is a procedure of
    /// the page that runs it, so its answer belongs to the dialog that asked for it and goes
    /// with it.
    ///
    /// The pane keeps nothing of a search, which is why the answer cannot be drawn again — and
    /// the dialog that opens next draws an empty query, an empty list and no spinner.
    #[gpui_kit::test]
    fn a_search_should_not_be_drawn_by_the_dialog_that_opens_after_it(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        session.answer_search(vec![search_profile("9", "nine")]);
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);
        let window = dialog_window(cx, &pane);

        // A search run and answered from the dialog's own page, drawn by the dialog that ran it.
        let dialog = open_dialog(&pane, window);
        settle_until(&mut window.cx, "the accounts to be read", |cx| {
            pane.read(cx).pending.is_none()
        });
        window.update(|window, _| window.refresh());
        search_from_page(&pane, &dialog, "nine", window);
        assert!(
            window.debug_bounds("TEST_TWEET_SEARCH_ROW").is_some(),
            "the dialog that asked for the search should draw what it found"
        );

        // The dialog is closed and another one opens over the same pane. The handle this test
        // holds goes with the close, because a dialog is released once the layer stops drawing it
        // and nothing else holds it — which is what tells the pane that it is gone.
        close_dialog_from_its_footer(dialog, &pane, window);
        assert!(
            !window.cx.update(|cx| pane.read(cx).dialog_open()),
            "the closed dialog should leave the pane holding none"
        );

        let second = open_dialog(&pane, window);
        settle_until(&mut window.cx, "the accounts to be read", |cx| {
            pane.read(cx).pending.is_none()
        });
        second.update(&mut window.cx, |dialog, cx| dialog.open_add(cx));
        settle_pages(window);

        assert!(
            window.debug_bounds("TEST_TWEET_SEARCH_ROW").is_none(),
            "what the last dialog searched for is not this dialog's to draw"
        );
        assert!(
            window.debug_bounds("TEST_TWEET_SEARCH_LIST").is_none(),
            "nor is the list it was drawn in"
        );
        assert!(
            window.debug_bounds("TEST_TWEET_SEARCHING").is_none(),
            "a dialog just opened is not waiting for a search it never asked for"
        );
    }

    /// A search is not something the accounts are waiting for, so a dialog dismissed while one
    /// is out leaves the pane able to read the accounts and to change them: what it must not do
    /// is keep waiting for an answer that the dismissal took the place of.
    ///
    /// Nothing answers the search this is asserted with, so the pane is still waiting for it when
    /// the dialog goes: what stops it waiting is the dismissal, and a pane that a dismissal left
    /// waiting would show it here rather than in the change it refuses next.
    #[gpui_kit::test]
    fn a_search_that_is_out_when_the_dialog_closes_should_leave_nothing_waiting(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        session.answer_add(XAccountMonitoringMutationOutcome::Confirmed(selection(
            "8", "eight",
        )));
        // The search this dialog is dismissed over is one the service never answers, so it is
        // still out for as long as the pane is waiting for it.
        let _held = session.block_search();
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);
        let window = dialog_window(cx, &pane);

        let dialog = open_dialog(&pane, window);
        settle_until(&mut window.cx, "the accounts to be read", |cx| {
            pane.read(cx).pending.is_none()
        });

        // The search is asked for with nothing pumped in between, so it is still out when the
        // dialog is dismissed: what stops the pane waiting for it is the dismissal, because the
        // answer this one is waiting for is never given.
        pane.update(&mut window.cx, |pane, cx| {
            assert!(pane.search_users(SmolStr::new_static("nine"), cx));
        });
        settle_until(
            &mut window.cx,
            "the held search to reach the service",
            |cx| {
                let _ = cx;
                session.state().searches_made == 1
            },
        );
        assert!(
            window.cx.update(|cx| pane.read(cx).searching.is_some()),
            "the search should be out when the dialog is dismissed"
        );
        assert!(
            window.cx.update(|cx| pane.read(cx).pending.is_none()),
            "a search is not an operation the accounts are waiting for"
        );

        close_dialog_from_its_footer(dialog, &pane, window);
        assert!(!window.cx.update(|cx| pane.read(cx).dialog_open()));
        assert!(
            window.cx.update(|cx| pane.read(cx).searching.is_none()),
            "the pane should stop waiting for the search of the closed dialog"
        );
        assert!(
            window.cx.update(|cx| pane.read(cx).pending.is_none()),
            "nothing should be left waiting for a search that will not be answered"
        );

        // The pane is still able to read the accounts, and still able to change them.
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        pane.update(&mut window.cx, |pane, cx| {
            assert!(
                pane.load_selections(cx),
                "the accounts should be readable again"
            );
        });
        settle_until(&mut window.cx, "the accounts to arrive", |cx| {
            pane.read(cx).selections.is_some() && pane.read(cx).pending.is_none()
        });
        pane.update(&mut window.cx, |pane, cx| {
            assert!(
                pane.add_selection(SmolStr::new_static("eight"), cx),
                "an account should be addable again"
            );
        });
        settle_until(&mut window.cx, "the account to join the list", |cx| {
            pane.read(cx)
                .selections
                .as_ref()
                .is_some_and(|data| data.selections().len() == 2)
        });
        assert_eq!(usernames_of(&pane, &mut window.cx), ["seven", "eight"]);

        // And the dialog that opens next is not waiting for a search either.
        let _second = open_dialog(&pane, window);
        settle_until(&mut window.cx, "the accounts to be read", |cx| {
            pane.read(cx).pending.is_none()
        });
        window.update(|window, _| window.refresh());
        assert!(
            window.debug_bounds("TEST_TWEET_SEARCHING").is_none(),
            "a dialog that has just opened should not be showing a search that is over"
        );
    }

    /// A search that has just gone out is what the page that sent it draws.
    ///
    /// What a search being out looks like is read from the pane, so the pane is what has to tell
    /// the dialog that the page changed: a page that is not told goes on drawing the answer
    /// before the search — or nothing at all — while the pane turns the next one away, with
    /// nothing on screen to say why.
    ///
    /// The search is sent here the way this file's own pane tests send one rather than from the
    /// page's control, because the control cannot assert this: it keeps its own hover and press
    /// state in the window, and a change to the state a control keeps redraws the page it is
    /// drawn on — which redraws the dialog above it — so a frame drawn for a click shows the
    /// search being out even for a pane that told the dialog nothing at all.
    #[gpui_kit::test]
    fn a_search_that_has_just_been_sent_should_be_drawn_as_out(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        // Nothing answers this search, so what the page draws after it is sent is the search
        // being out rather than whatever an answer would have put in its place.
        let _held = session.block_search();
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);
        let (dialog, window) = draw_dialog(cx, &pane);
        pane.update(&mut window.cx, |pane, cx| {
            pane.watch_selections(true, cx);
        });
        dialog.update(&mut window.cx, |dialog, cx| dialog.open_add(cx));
        settle_pages(window);
        assert!(
            window.debug_bounds("TEST_TWEET_SEARCHING").is_none(),
            "a page nothing has been searched from is not waiting for a search"
        );

        // The search goes out with nothing else changing in the window, so the frame the pane
        // asks for is the only thing that can draw it: a pane that asks for none leaves the page
        // showing what it drew while no search was out.
        window.cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                assert!(pane.search_users(SmolStr::new_static("nine"), cx));
            });
        });

        assert!(
            window.cx.update(|cx| pane.read(cx).searching.is_some()),
            "the search should be out"
        );
        assert!(
            window.debug_bounds("TEST_TWEET_SEARCHING").is_some(),
            "the page that sent the search should draw it as out on the frame it went out on"
        );
    }

    /// A dialog dismissed over a pane whose stream is down takes its search with it as well.
    ///
    /// A search that is out is a search the stream can go down under, and the call that stops the
    /// watch a dialog asked for is one a pane without a live stream does not make: there is
    /// nothing to displace the search, so the dismissal is what has to take it — or the pane sits
    /// on a search whose page is gone and turns every change away.
    #[gpui_kit::test]
    fn a_search_should_go_with_a_dialog_dismissed_over_a_pane_whose_stream_went_down(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        let _held = session.block_search();
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);
        let window = dialog_window(cx, &pane);

        let dialog = open_dialog(&pane, window);
        settle_until(&mut window.cx, "the accounts to be read", |cx| {
            pane.read(cx).pending.is_none()
        });
        pane.update(&mut window.cx, |pane, cx| {
            assert!(pane.search_users(SmolStr::new_static("nine"), cx));
        });
        settle_until(
            &mut window.cx,
            "the held search to reach the service",
            |cx| {
                let _ = cx;
                session.state().searches_made == 1
            },
        );

        // The stream reports that it is connecting again, which is the pane still following the
        // account with nothing live behind it: a search out here is one the dismissal has to
        // answer for.
        report_status(&pane, TweetStatus::Connecting, &mut window.cx);
        assert!(
            !window.cx.update(|cx| pane.read(cx).ready),
            "the pane should not be reading over a stream that is not live"
        );

        close_dialog_from_its_footer(dialog, &pane, window);
        assert!(!window.cx.update(|cx| pane.read(cx).dialog_open()));
        assert!(
            window.cx.update(|cx| pane.read(cx).searching.is_none()),
            "closing the dialog should take the search of a pane whose stream is down"
        );
    }

    /// The two pages are pushed and popped, and each control that does it is the one in the
    /// header of the page it belongs to.
    #[gpui_kit::test]
    fn a_dialog_should_push_and_pop_its_search_page_with_the_controls_in_its_headers(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_history(vec![tweet("1", 1)], vec![profile("42", "kytrix")]);
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        let (pane, _root) = live_pane(cx, &source, &session, vec![tweet("1", 1)]);
        let (_, window) = draw_dialog(cx, &pane);

        // The accounts are read, so that the page the search is pushed over has rows to draw and
        // the pop below has something to come back to.
        pane.update(&mut window.cx, |pane, cx| {
            pane.watch_selections(true, cx);
            pane.load_selections(cx);
        });
        settle_until(&mut window.cx, "the accounts to arrive", |cx| {
            pane.read(cx).selections.is_some()
        });
        window.update(|window, _| window.refresh());

        // The page the stack opens on is the accounts, and the control that opens the search is
        // the add button in its header.
        let add = window
            .debug_bounds("TEST_TWEET_ADD_ACCOUNT")
            .expect("the accounts page should draw the control that searches X")
            .center();
        window.simulate_click(add, Modifiers::none());
        settle_pages(window);
        assert!(
            window.debug_bounds("TEST_TWEET_ADD_PAGE").is_some(),
            "the control in the accounts header should push the search page"
        );

        // The control at the top of the search page's own header is what pops it again.
        let back = window
            .debug_bounds("TEST_TWEET_BACK")
            .expect("the search page should draw the control that goes back")
            .center();
        window.simulate_click(back, Modifiers::none());
        settle_pages(window);
        assert!(
            window.debug_bounds("TEST_TWEET_ADD_PAGE").is_none(),
            "the control at the top of the search page should pop it off the stack"
        );
        assert!(
            window.debug_bounds("TEST_TWEET_ACCOUNT_ROW").is_some(),
            "the page the search was pushed over should be the one it is popped back to"
        );
    }

    /// The control at the foot of the dialog closes it by emitting the event the layer listens for,
    /// rather than by asking the window to dismiss whatever is on top.
    ///
    /// What tells the two apart is that emitting closes *this* dialog wherever it sits, so what the
    /// test reads is that the dialog leaves the layer once its exit transition has settled — the
    /// same thing the footer's control used to ask the window for.
    #[gpui_kit::test]
    fn a_control_at_the_foot_of_the_dialog_should_close_it(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);
        let window = dialog_window(cx, &pane);

        let dialog = open_dialog(&pane, window);
        settle_until(&mut window.cx, "the accounts to be read", |cx| {
            pane.read(cx).pending.is_none()
        });
        window.update(|window, _| window.refresh());
        assert!(
            window.debug_bounds("TEST_TWEET_CLOSE").is_some(),
            "the dialog should draw the control at its foot"
        );

        close_dialog_from_its_footer(dialog, &pane, window);

        assert!(
            !window.cx.update(|cx| pane.read(cx).dialog_open()),
            "the control at the foot of the dialog should have closed it"
        );
        window.update(|window, _| window.refresh());
        assert!(
            window.debug_bounds("TEST_TWEET_CLOSE").is_none(),
            "the dialog the control closed should be off the layer once its exit transition is over"
        );
    }

    /// A dialog closed by the control at its foot still runs the pane's cleanup, which is what
    /// stops it asking for avatars nobody is drawing and what takes a search that is out with it.
    ///
    /// The cleanup is hung on the dialog *entity* rather than on the event the control emits,
    /// because that is the only hook that runs on every way a dialog can go. What it leaves behind
    /// is what this reads: a search still held by the pane is a pane waiting for an answer to a
    /// page that is gone, and it is what turns the next change away.
    #[gpui_kit::test]
    fn a_dialog_closed_by_its_own_control_should_still_be_cleaned_up_after(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        // Nothing answers this search, so it is still out when the dialog is closed: what takes it
        // away is the cleanup, and a pane left holding it would show it here.
        let _held = session.block_search();
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);
        let window = dialog_window(cx, &pane);

        let dialog = open_dialog(&pane, window);
        settle_until(&mut window.cx, "the accounts to be read", |cx| {
            pane.read(cx).pending.is_none()
        });
        pane.update(&mut window.cx, |pane, cx| {
            assert!(pane.search_users(SmolStr::new_static("nine"), cx));
        });
        settle_until(
            &mut window.cx,
            "the held search to reach the service",
            |cx| {
                let _ = cx;
                session.state().searches_made == 1
            },
        );
        assert!(
            window.cx.update(|cx| pane.read(cx).searching.is_some()),
            "the search should be out when the dialog is closed"
        );

        close_dialog_from_its_footer(dialog, &pane, window);

        assert!(
            !window.cx.update(|cx| pane.read(cx).dialog_open()),
            "the dialog the control closed should have left the pane holding none"
        );
        assert!(
            window.cx.update(|cx| pane.read(cx).searching.is_none()),
            "the pane should stop waiting for the search of a dialog its own control closed"
        );
        assert!(
            window.cx.update(|cx| pane.read(cx).pending.is_none()),
            "nothing should be left waiting for a search that will not be answered"
        );
    }

    /// Lets one page finish sliding over the other.
    ///
    /// The page on top of a change is the page showing once the transition is over; until then
    /// both of them are mounted, which is what a frame drawn right after the push would see.
    fn settle_pages(window: &mut VisualTestContext) {
        window.executor().advance_clock(Duration::from_millis(400));
        window.update(|window, _| window.refresh());
        window.run_until_parked();
    }

    /// The dialog is one panel of a fixed height, so what its pages are drawn in is what is left
    /// of that height once the way out of it is laid out at the foot.
    ///
    /// A page that took the whole height rather than what is left would push the control at the
    /// foot of the dialog past the edge of the panel it is drawn in.
    #[gpui_kit::test]
    fn a_dialogs_pages_should_not_push_the_way_out_of_it_past_its_panel(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_history(vec![tweet("1", 1)], vec![profile("42", "kytrix")]);
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        let (pane, _root) = live_pane(cx, &source, &session, vec![tweet("1", 1)]);
        let (_, window) = draw_dialog(cx, &pane);
        pane.update(&mut window.cx, |pane, cx| {
            pane.watch_selections(true, cx);
            pane.load_selections(cx);
        });
        settle_until(&mut window.cx, "the accounts to arrive", |cx| {
            pane.read(cx).selections.is_some()
        });
        window.update(|window, _| window.refresh());

        let close = drawn(window, "TEST_TWEET_CLOSE");
        let list = drawn(window, "TEST_TWEET_ACCOUNT_LIST");

        assert!(
            list.bottom() <= close.top(),
            "the way out of the dialog is drawn under the page rather than over it"
        );
    }

    /// The page a change puts on top covers the page it is over, and both keep the box a page is
    /// drawn in while they travel.
    ///
    /// Two pages are drawn at once for the length of a change, which is the only time either of
    /// them is not the whole of what the panel holds. So the page on top has to be as wide as that
    /// box wherever it is in its journey — a page squeezed between the two edges it is placed by
    /// would be a page that reflows as it moves — and it has to carry the surface it is drawn on,
    /// or what is under it reads through the header and the field of the page over it.
    ///
    /// What the surface is *is* the panel's own token (`components.dialog.background`, which
    /// is what `base_ui::dialog::Dialog` paints the panel with), so what this pins is that a page
    /// paints that token and nothing else: the panel itself is not in this window — the dialog is
    /// attached to the pane rather than opened through the layer — so a test cannot read the two
    /// off one frame and compare them.
    #[gpui_kit::test]
    fn a_page_on_its_way_in_should_cover_the_page_it_is_over(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_history(vec![tweet("1", 1)], vec![profile("42", "kytrix")]);
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        let (pane, _root) = live_pane(cx, &source, &session, vec![tweet("1", 1)]);
        let (dialog, window) = draw_dialog(cx, &pane);
        pane.update(&mut window.cx, |pane, cx| {
            pane.watch_selections(true, cx);
            pane.load_selections(cx);
        });
        settle_until(&mut window.cx, "the accounts to arrive", |cx| {
            pane.read(cx).selections.is_some()
        });
        window.update(|window, _| window.refresh());

        // The box a page fills, read off the page that is showing before the change starts.
        let page_box = drawn(window, "TEST_TWEET_MONITORED_PAGE");
        let surface = window
            .cx
            .update(|cx| cx.theme().components.dialog.background.hsla);

        dialog.update(&mut window.cx, |dialog, cx| dialog.open_add(cx));
        window.run_until_parked();
        // Halfway through: both pages are inside the panel, and the one arriving is over the one
        // it is covering rather than beside it.
        window
            .executor()
            .advance_clock(crate::dialog::PAGE_TRANSITION / 2);
        window.update(|window, _| window.refresh());
        window.run_until_parked();

        let covered = drawn(window, "TEST_TWEET_MONITORED_PAGE");
        let arriving = drawn(window, "TEST_TWEET_ADD_PAGE");
        assert_eq!(
            (arriving.size, covered.size),
            (page_box.size, page_box.size),
            "a page is the size of the box it is drawn in, wherever it is on its way in from: \
             {arriving:?} and {covered:?} against {page_box:?}"
        );
        assert!(
            arriving.left() > covered.left(),
            "the page arriving is the page over the one it covers: {arriving:?} against {covered:?}"
        );
        assert_eq!(
            painted_surface(window, "TEST_TWEET_ADD_PAGE"),
            Some(surface),
            "the page on top carries the panel's own surface"
        );
        assert_eq!(
            painted_surface(window, "TEST_TWEET_MONITORED_PAGE"),
            Some(surface),
            "and so does the page it is covering"
        );
    }

    /// Every place of a list is laid out at the height of the first row it measures, so a row
    /// that is not the height its content needs is a row the places after it are drawn out of
    /// step with.
    ///
    /// What a row used to be given was the height of a control — one line — while a dialog's row
    /// holds a face and the lines beside it, so the row is given a height of its own here and
    /// measured where it is drawn.
    #[gpui_kit::test]
    fn a_dialogs_rows_should_be_drawn_at_a_height_their_content_fits_in(cx: &mut TestAppContext) {
        /// The height one row of either of the dialog's lists is given.
        const ROW: f32 = 72.0;

        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_history(vec![tweet("1", 1)], vec![profile("42", "kytrix")]);
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        session.answer_search(vec![search_profile("9", "nine")]);
        let (pane, _root) = live_pane(cx, &source, &session, vec![tweet("1", 1)]);
        let (dialog, window) = draw_dialog(cx, &pane);

        // The pane is driven through the window's own context: the dialog and the pane are read
        // in one application, and the window is what the tests below measure a frame of.
        pane.update(&mut window.cx, |pane, cx| {
            pane.watch_selections(true, cx);
            pane.load_selections(cx);
        });
        settle_until(&mut window.cx, "the accounts to arrive", |cx| {
            pane.read(cx).selections.is_some()
        });
        window.update(|window, _| window.refresh());

        assert_eq!(
            drawn(window, "TEST_TWEET_ACCOUNT_ROW").size.height,
            px(ROW),
            "a monitored account is drawn at the height its list lays its places out at"
        );

        // A face is the tallest thing a row holds, and it has to be drawn at its own size and
        // inside the row rather than over the edge of it: a row drawn the height of a control
        // clips exactly this.
        let face = drawn(window, "TEST_TWEET_ACCOUNT_AVATAR");
        let row = drawn(window, "TEST_TWEET_ACCOUNT_ROW");
        assert_eq!(
            face.size.height,
            px(42.0),
            "a row draws the face it was given at the size it was given"
        );
        assert!(
            face.top() >= row.top() && face.bottom() <= row.bottom(),
            "the face of a row sits inside the row rather than over the edge of it"
        );

        // The page that searches X draws rows of the same size.
        search_from_page(&pane, &dialog, "nine", window);
        assert_eq!(
            drawn(window, "TEST_TWEET_SEARCH_ROW").size.height,
            px(ROW),
            "an account one search found is drawn at the height its list lays its places out at"
        );
        let face = drawn(window, "TEST_TWEET_SEARCH_AVATAR");
        let row = drawn(window, "TEST_TWEET_SEARCH_ROW");
        assert!(
            face.top() >= row.top() && face.bottom() <= row.bottom(),
            "the face of a row of either list sits inside the row"
        );
    }

    /// The dialog's own controls are the sizes the dialog asks for: a field as tall as a field,
    /// a control as tall as a control, and the one at the foot of the dialog a way out of it.
    ///
    /// Nothing reads a control's size back off a style, so what each control was given is what
    /// the frame drew it at.
    #[gpui_kit::test]
    fn a_dialogs_controls_should_be_drawn_at_the_sizes_the_dialog_asks_for(
        cx: &mut TestAppContext,
    ) {
        /// The height a control is drawn at when the dialog asks for the medium size.
        const MEDIUM: f32 = 30.0;

        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_history(vec![tweet("1", 1)], vec![profile("42", "kytrix")]);
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        let (pane, _root) = live_pane(cx, &source, &session, vec![tweet("1", 1)]);
        let (dialog, window) = draw_dialog(cx, &pane);

        // The field the accounts are filtered by, and the way out at the foot of the dialog.
        for field in ["TEST_TWEET_FILTER_FIELD", "TEST_TWEET_CLOSE"] {
            assert_eq!(
                drawn(window, field).size.height,
                px(MEDIUM),
                "{field} is a control the dialog asks the medium size for"
            );
        }

        // The header's controls are medium, and the row they are laid out in reads the way to
        // add an account before the way to read the same accounts again.
        for control in ["TEST_TWEET_REFRESH", "TEST_TWEET_ADD_ACCOUNT"] {
            assert_eq!(
                drawn(window, control).size.height,
                px(MEDIUM),
                "{control} is a control the dialog asks the medium size for"
            );
        }
        assert!(
            drawn(window, "TEST_TWEET_ADD_ACCOUNT").left()
                < drawn(window, "TEST_TWEET_REFRESH").left(),
            "the way to add an account comes before the way to read the accounts again, which \
             is the order the two controls were exchanged for"
        );

        // The page that searches X has a field of its own, drawn at the same size.
        dialog.update(&mut window.cx, |dialog, cx| dialog.open_add(cx));
        settle_pages(window);
        assert_eq!(
            drawn(window, "TEST_TWEET_QUERY_FIELD").size.height,
            px(MEDIUM)
        );
    }

    /// The frame the dialog is drawn in, inside the window layer that holds it.
    struct DialogHarness {
        dialog: Entity<TweetDialog>,
    }

    impl Render for DialogHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.dialog.clone())
        }
    }

    /// A window with a frame of its own for the dialog a pane holds.
    ///
    /// `show` itself needs the window layer the application installs, so what a test attaches is
    /// the same dialog the pane would hold, drawn in a window whose root is that layer: it is
    /// also where an alert a change is reported in is opened, over the dialog it is about.
    fn draw_dialog<'a>(
        cx: &'a mut TestAppContext,
        pane: &Entity<XAccountMonitoringWidget>,
    ) -> (Entity<TweetDialog>, &'a mut VisualTestContext) {
        let dialog = attach_dialog(pane, cx);
        let (_, window) = cx.add_window_view({
            let dialog = dialog.clone();
            move |_, cx| Layer::new(cx.new(move |_| DialogHarness { dialog }))
        });
        window.run_until_parked();

        (dialog, window)
    }

    /// A window whose root is the application's dialog layer, with the pane drawn in it.
    ///
    /// This is the window a dialog is really opened over and dismissed from: the pane's own
    /// control opens it, the layer holds it while it is open, and the layer is what drops it
    /// once it has left — which is the only thing that tells the pane its dialog is gone.
    fn dialog_window<'a>(
        cx: &'a mut TestAppContext,
        pane: &Entity<XAccountMonitoringWidget>,
    ) -> &'a mut VisualTestContext {
        let pane = pane.clone();
        let (_, window) =
            cx.add_window_view(move |_, cx| Layer::new(cx.new(move |_| PaneHarness { pane })));
        window.run_until_parked();

        window
    }

    /// Opens the pane's dialog over the layer, as the pane's own control does.
    fn open_dialog(
        pane: &Entity<XAccountMonitoringWidget>,
        window: &mut VisualTestContext,
    ) -> Entity<TweetDialog> {
        window.update(|window, cx| TweetDialog::show(pane.downgrade(), window, cx));
        settle_pages(window);

        window.update(|_, cx| {
            pane.read(cx)
                .dialog
                .as_ref()
                .and_then(WeakEntity::upgrade)
                .expect("the pane should hold the dialog it opened")
        })
    }

    /// Closes the dialog by the control at the foot of it, which is the way a reader closes it.
    ///
    /// This is the path the dialog closes itself by: the control emits the event the layer reads,
    /// rather than asking the window to close whatever happens to be on top. The dialog is closed
    /// here and nowhere else — there is no way to close one from outside it — so this is what every
    /// test that needs a dialog gone goes through.
    ///
    /// The test's own handle on the dialog goes once the click has been made, because the layer has
    /// to be the last thing holding it from then on: a handle kept here would keep the dialog's
    /// state alive after it has left, and the release below — which is the only thing that tells the
    /// pane it is gone — would never run.
    fn close_dialog_from_its_footer(
        dialog: Entity<TweetDialog>,
        pane: &Entity<XAccountMonitoringWidget>,
        window: &mut VisualTestContext,
    ) {
        let close = window
            .debug_bounds("TEST_TWEET_CLOSE")
            .expect("the dialog should draw the control at its foot")
            .center();

        window.simulate_click(close, Modifiers::none());
        drop(dialog);

        for _ in 0..4 {
            settle_pages(window);
            if !window.cx.update(|cx| pane.read(cx).dialog_open()) {
                return;
            }
        }
        panic!("the control at the foot of the dialog should have closed it");
    }

    /// Where one selector is drawn, which is the only thing a test can read a control's own size
    /// or place back from: there is no style to read off an element once it is a frame.
    fn drawn(window: &mut VisualTestContext, selector: &'static str) -> Bounds<Pixels> {
        window
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the dialog should draw {selector}"))
    }

    /// What one page is painted on: the colour of the quad the page's own box was painted with.
    ///
    /// Where a page is drawn is read off the frame's selectors, what it is drawn *on* is read off
    /// the frame's quads — the one thing a page can cover the page under it with. The bounds are
    /// compared in device pixels, which is the unit both are recorded in: a frame's quads are the
    /// layout snapped to them, so the two agree exactly at any whole scale factor — which is what
    /// the windows tests are drawn in.
    fn painted_surface(window: &mut VisualTestContext, selector: &'static str) -> Option<Hsla> {
        let bounds = window.debug_bounds(selector)?;
        let scale = window.update(|window, _| window.scale_factor());
        window
            .update(|window, _| window.painted_quads())
            .into_iter()
            .find(|quad| quad.bounds == bounds.scale(scale))
            .and_then(|quad| quad.background.as_solid())
    }

    /// Lets the alert a change is reported in arrive and be drawn.
    ///
    /// The alert is opened at the end of the effect cycle the frame that read the report was
    /// drawn in, so the frame that draws the dialog is not the frame that draws the alert.
    fn settle_alert(window: &mut VisualTestContext) {
        window.executor().advance_clock(Duration::from_millis(400));
        window.update(|window, _| window.refresh());
        window.run_until_parked();
    }

    /// A change reports itself in an alert over the dialog, rather than in a line of the page
    /// that asked for it.
    ///
    /// The report is not a question, so it is one alert with the one button that says it was
    /// read rather than one with a way of saying no.
    #[gpui_kit::test]
    fn a_change_that_was_refused_should_be_reported_in_an_alert(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);
        let (_, window) = draw_dialog(cx, &pane);

        session.answer_add(XAccountMonitoringMutationOutcome::Rejected(
            XAccountMonitoringApiError::QuotaExceeded,
        ));
        pane.update(&mut window.cx, |pane, cx| {
            pane.add_selection(SmolStr::new_static("eight"), cx);
        });
        settle_until(&mut window.cx, "the refusal to be reported", |cx| {
            pane.read(cx).notice.is_some()
        });
        settle_alert(window);

        assert!(
            window.debug_bounds("BASE_UI_ALERT").is_some(),
            "a change that was refused should be reported over the dialog"
        );
        assert!(
            window.debug_bounds("BASE_UI_ALERT_OK").is_some(),
            "the report should offer the button that says it was read"
        );
        assert!(
            window.debug_bounds("BASE_UI_ALERT_CANCEL").is_none(),
            "a report asks nothing, so it should not offer a way of saying no"
        );
        assert!(
            window.debug_bounds("TEST_TWEET_ACCOUNT_LIST").is_some(),
            "the dialog the change was asked for in should still be drawn under the report"
        );
    }

    /// An account that joined the list is reported in the same alert, and what a change said is
    /// reported once: the frames after the alert are not the change reported again.
    #[gpui_kit::test]
    fn a_change_should_be_reported_once(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);
        let (_, window) = draw_dialog(cx, &pane);

        session.answer_add(XAccountMonitoringMutationOutcome::Confirmed(selection(
            "8", "eight",
        )));
        pane.update(&mut window.cx, |pane, cx| {
            pane.add_selection(SmolStr::new_static("eight"), cx);
        });
        settle_until(&mut window.cx, "the account to join the list", |cx| {
            pane.read(cx)
                .selections
                .as_ref()
                .is_some_and(|data| data.selections().len() == 2)
        });
        settle_alert(window);
        assert!(
            window.debug_bounds("BASE_UI_ALERT").is_some(),
            "an account that joined the list should be reported too"
        );

        // The report is read, which is the whole of answering it.
        let ok = window
            .debug_bounds("BASE_UI_ALERT_OK")
            .expect("the report should offer the button that says it was read")
            .center();
        window.simulate_click(ok, Modifiers::none());
        settle_alert(window);
        assert!(
            window.debug_bounds("BASE_UI_ALERT").is_none(),
            "reading the report should leave nothing raised"
        );

        // The pane still holds what the change said, so what the frames after it draw is what
        // says whether the dialog reports the change again or leaves it reported.
        window.update(|window, _| window.refresh());
        window.run_until_parked();
        assert!(
            window.debug_bounds("BASE_UI_ALERT").is_none(),
            "a change should be one alert rather than one per frame the dialog is drawn in"
        );
    }

    /// A report the pane was already holding when a dialog opened was not made to that dialog:
    /// what a dialog raises is what a change says while it is there to hear it.
    ///
    /// The pane holds what a change said until another change replaces it, so a dialog that
    /// started out with nothing reported would read a report from before it opened as one it had
    /// not raised — and raise the alert of a session that is over.
    #[gpui_kit::test]
    fn a_notice_the_pane_was_already_holding_should_not_be_raised_by_a_new_dialog(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);

        // A change that was answered while no dialog was open: the pane holds what it said, and
        // nobody was there to be told.
        session.answer_add(XAccountMonitoringMutationOutcome::Confirmed(selection(
            "8", "eight",
        )));
        pane.update(cx, |pane, cx| {
            assert!(pane.add_selection(SmolStr::new_static("eight"), cx));
        });
        settle_until(cx, "the change to be answered", |cx| {
            pane.read(cx).notice.is_some()
        });

        let (_, window) = draw_dialog(cx, &pane);
        settle_alert(window);
        assert!(
            window.debug_bounds("BASE_UI_ALERT").is_none(),
            "a change the dialog was not there to hear is not one it raises"
        );
    }

    /// A dialog that opens over a change a previous one made does not raise it again: the alert
    /// belongs to the session that asked for the change, and reopening the dialog is a new one.
    #[gpui_kit::test]
    fn a_change_a_previous_dialog_reported_should_not_be_raised_again(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);
        let window = dialog_window(cx, &pane);

        // A change made from the dialog, reported in the alert over it and read there.
        let dialog = open_dialog(&pane, window);
        settle_until(&mut window.cx, "the accounts to be read", |cx| {
            pane.read(cx).pending.is_none()
        });
        session.answer_add(XAccountMonitoringMutationOutcome::Confirmed(selection(
            "8", "eight",
        )));
        pane.update(&mut window.cx, |pane, cx| {
            pane.add_selection(SmolStr::new_static("eight"), cx);
        });
        settle_until(&mut window.cx, "the change to be reported", |cx| {
            pane.read(cx).notice.is_some()
        });
        settle_alert(window);
        let ok = window
            .debug_bounds("BASE_UI_ALERT_OK")
            .expect("the report should offer the button that says it was read")
            .center();
        window.simulate_click(ok, Modifiers::none());
        settle_alert(window);

        // The dialog goes, and another one opens over the same pane.
        close_dialog_from_its_footer(dialog, &pane, window);
        assert!(
            !window.cx.update(|cx| pane.read(cx).dialog_open()),
            "the dialog that went should leave the pane holding none"
        );
        let _second = open_dialog(&pane, window);
        settle_until(&mut window.cx, "the accounts to be read", |cx| {
            pane.read(cx).pending.is_none()
        });
        settle_alert(window);
        assert!(
            window.debug_bounds("BASE_UI_ALERT").is_none(),
            "a report a previous dialog made should not be raised by the next one"
        );
    }

    /// A change is asked for from the dialog and made by the pane, which then has to repaint
    /// the dialog — from inside a frame the dialog is already being updated for, which is the
    /// one time an entity may not be asked to update itself.
    ///
    /// This is the chain the control beside a search result starts: the dialog's frame, the
    /// pane's act, and the repaint the pane owes the dialog for it.
    #[gpui_kit::test]
    fn adding_an_account_should_not_re_enter_the_dialog_that_asked_for_it(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);
        let dialog = attach_dialog(&pane, cx);

        cx.update(|cx| {
            dialog.update(cx, |_, cx| {
                pane.update(cx, |pane, cx| {
                    assert!(pane.add_selection(SmolStr::new_static("eight"), cx));
                });
            });
        });
        cx.update(|cx| {
            assert_eq!(
                pane.read(cx)
                    .pending
                    .as_ref()
                    .and_then(|pending| pending.target.as_deref()),
                Some("eight"),
                "the account that was asked for is the one the change carries"
            );
        });
    }

    #[gpui_kit::test]
    fn a_confirmed_add_should_join_the_list_and_be_noticed(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);

        session.answer_add(XAccountMonitoringMutationOutcome::Confirmed(selection(
            "8", "eight",
        )));
        pane.update(cx, |pane, cx| {
            assert!(pane.add_selection(SmolStr::new_static("eight"), cx));
        });
        settle_until(cx, "the account to join the list", |cx| {
            pane.read(cx)
                .selections
                .as_ref()
                .is_some_and(|data| data.selections().len() == 2)
        });
        assert_eq!(usernames_of(&pane, cx), ["seven", "eight"]);
        assert!(matches!(
            cx.update(|cx| pane.read(cx).notice.clone()),
            Some(SelectionNotice::Added)
        ));
    }

    #[gpui_kit::test]
    fn a_refused_add_should_say_why_and_leave_the_list_alone(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);

        session.answer_add(XAccountMonitoringMutationOutcome::Rejected(
            XAccountMonitoringApiError::QuotaExceeded,
        ));
        pane.update(cx, |pane, cx| {
            pane.add_selection(SmolStr::new_static("eight"), cx);
        });
        settle_until(cx, "the refusal to be noticed", |cx| {
            pane.read(cx).notice.is_some()
        });

        assert!(matches!(
            cx.update(|cx| pane.read(cx).notice.clone()),
            Some(SelectionNotice::Problem(SelectionFailure::Rejected(
                XAccountMonitoringApiError::QuotaExceeded
            )))
        ));
        assert_eq!(usernames_of(&pane, cx), ["seven"]);
    }

    #[gpui_kit::test]
    fn an_unknown_add_should_say_so_and_leave_the_list_alone(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);

        session.answer_add(XAccountMonitoringMutationOutcome::Unknown(
            XAccountMonitoringMutationUnknown::Timeout,
        ));
        pane.update(cx, |pane, cx| {
            pane.add_selection(SmolStr::new_static("eight"), cx);
        });
        settle_until(cx, "the unknown outcome to be noticed", |cx| {
            pane.read(cx).notice.is_some()
        });

        assert!(matches!(
            cx.update(|cx| pane.read(cx).notice.clone()),
            Some(SelectionNotice::Problem(SelectionFailure::Unknown(
                XAccountMonitoringMutationUnknown::Timeout
            )))
        ));
        assert_eq!(usernames_of(&pane, cx), ["seven"]);
    }

    #[gpui_kit::test]
    fn a_confirmed_delete_should_leave_the_list(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) = live_pane_reading(
            cx,
            &source,
            &session,
            vec![selection("7", "seven"), selection("8", "eight")],
            3,
        );

        session.answer_delete(XAccountMonitoringMutationOutcome::Confirmed(()));
        pane.update(cx, |pane, cx| {
            pane.delete_selection(SmolStr::new_static("7"), cx);
        });
        settle_until(cx, "the account to leave the list", |cx| {
            pane.read(cx)
                .selections
                .as_ref()
                .is_some_and(|data| data.selections().len() == 1)
        });
        assert_eq!(usernames_of(&pane, cx), ["eight"]);
        assert!(cx.update(|cx| pane.read(cx).notice.is_none()));
    }

    #[gpui_kit::test]
    fn a_change_that_could_not_be_read_should_be_a_problem(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) = live_pane_reading(cx, &source, &session, Vec::new(), 3);

        session.refuse_selections(XAccountMonitoringApiError::Closed);
        pane.update(cx, |pane, cx| {
            pane.load_selections(cx);
        });
        settle_until(cx, "the failure to be noticed", |cx| {
            pane.read(cx).notice.is_some()
        });

        assert!(matches!(
            cx.update(|cx| pane.read(cx).notice.clone()),
            Some(SelectionNotice::Problem(SelectionFailure::Rpc(_)))
        ));
    }

    #[gpui_kit::test]
    fn a_change_should_be_refused_while_the_stream_is_not_live(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _session = source.session(ACCOUNT);
        let (pane, _root) = quiet_pane(cx, source);

        let started = pane.update(cx, |pane, cx| {
            pane.add_selection(SmolStr::new_static("eight"), cx)
        });

        assert!(!started, "nothing may be changed without a live stream");
    }

    #[gpui_kit::test]
    fn a_change_a_started_stream_displaced_should_be_forgotten(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);

        // A change is in flight, and the user restarts the stream before it answers: what
        // became of it is not something the pane knows, so it stops waiting for it.
        let block = session.block_mutation();
        pane.update(cx, |pane, cx| {
            pane.add_selection(SmolStr::new_static("eight"), cx);
        });
        assert!(cx.update(|cx| pane.read(cx).pending.is_some()));
        // The change is in flight once the session has been asked for it, which is what the
        // hold is inside.
        settle_until(cx, "the change to be in flight", |cx| {
            let _ = cx;
            session.state().add_calls == 1
        });

        let replacement = source.session(ACCOUNT);
        replacement.answer_history(vec![tweet("1", 1)], vec![profile("42", "kytrix")]);
        let _control = replacement.subscribe();
        pane.update(cx, |pane, cx| pane.start(true, cx));

        assert!(cx.update(|cx| pane.read(cx).pending.is_none()));
        settle_until(cx, "the displaced change to be let go", |cx| {
            let _ = cx;
            block.is_dropped()
        });
    }

    /// What a search found belongs to the query it was asked with, so editing the query takes
    /// the answer away rather than leaving it under a query it does not stand for — and what the
    /// next query is searched with is drawn in its place.
    ///
    /// The two searches find different accounts, which is what makes the page's own answer
    /// readable: the row the page draws is the one the second search found, named with a name
    /// the first one's row is not drawn at.
    #[gpui_kit::test]
    fn a_search_answer_should_belong_to_the_query_that_asked_for_it(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_search(vec![search_profile("9", "nine")]);
        session.answer_search(vec![search_profile("19", "nineteen")]);
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);
        let (dialog, window) = draw_dialog(cx, &pane);
        pane.update(&mut window.cx, |pane, cx| {
            pane.watch_selections(true, cx);
        });

        search_from_page(&pane, &dialog, "nine", window);
        assert!(
            window.debug_bounds("TEST_TWEET_SEARCH_ROW").is_some(),
            "the search page should draw the row the search found"
        );
        let first = drawn(window, "TEST_TWEET_PROFILE_NAME").size.width;

        // Editing the query is a question this answer was not to, so the page stops drawing it.
        let field = window
            .debug_bounds("TEST_TWEET_QUERY_FIELD")
            .expect("the search page should draw the field X is searched with")
            .center();
        window.simulate_click(field, Modifiers::none());
        window.simulate_input("teen");
        settle_pages(window);
        assert!(
            window.debug_bounds("TEST_TWEET_SEARCH_ROW").is_none(),
            "an answer should not be drawn under a query it does not stand for"
        );

        // And what the next search answers is drawn in its place: the page draws the account
        // that search found, in the name that account is drawn with.
        let search = window
            .debug_bounds("TEST_TWEET_SEARCH")
            .expect("the search page should draw the control that sends the search")
            .center();
        window.simulate_click(search, Modifiers::none());
        settle_until(&mut window.cx, "the second search to be answered", |cx| {
            pane.read(cx).searching.is_none()
        });
        window.update(|window, _| window.refresh());
        window.run_until_parked();
        assert!(
            window.debug_bounds("TEST_TWEET_SEARCH_ROW").is_some(),
            "the page should draw what the search it just ran found"
        );
        assert_eq!(
            session.state().searches_made,
            2,
            "both searches should have reached the service"
        );
        assert!(
            drawn(window, "TEST_TWEET_PROFILE_NAME").size.width > first,
            "the row the page draws should be the account the second query found rather than the one the first did"
        );
    }

    /// A search that found nobody is an answer, and the page that asked says so: an empty result
    /// is not the same as a query nothing has been searched with.
    #[gpui_kit::test]
    fn an_empty_search_answer_should_say_so_on_the_page_that_asked(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_search(Vec::new());
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);
        let (dialog, window) = draw_dialog(cx, &pane);
        pane.update(&mut window.cx, |pane, cx| {
            pane.watch_selections(true, cx);
        });

        search_from_page(&pane, &dialog, "nobody", window);

        assert!(
            window
                .debug_bounds("TEST_TWEET_NO_SEARCH_RESULTS")
                .is_some(),
            "an answer with nobody in it should say so on the page that asked"
        );
        assert!(
            window.debug_bounds("TEST_TWEET_SEARCH_LIST").is_none(),
            "there are no rows for a search that found none"
        );
    }

    /// A search that could not be answered is drawn where it was asked for, in the words the
    /// failure itself gives — and it is not an alert over the dialog, because a search is not a
    /// change to the accounts.
    #[gpui_kit::test]
    fn a_search_that_could_not_be_answered_should_be_drawn_where_it_was_asked(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        // Nothing answers a search, which is a service that refuses to answer it.
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);
        let (dialog, window) = draw_dialog(cx, &pane);
        pane.update(&mut window.cx, |pane, cx| {
            pane.watch_selections(true, cx);
        });

        search_from_page(&pane, &dialog, "nine", window);

        assert!(
            window.debug_bounds("TEST_TWEET_SEARCH_FAILED").is_some(),
            "a search that failed should be drawn on the page that asked for it"
        );
        assert!(
            window.debug_bounds("TEST_TWEET_QUERY_FIELD").is_some(),
            "the page should still offer the field the search was asked with"
        );
        settle_alert(window);
        assert!(
            window.debug_bounds("BASE_UI_ALERT").is_none(),
            "a search is not a change to the accounts, so it is not reported in an alert"
        );
    }

    #[gpui_kit::test]
    fn a_search_that_is_not_one_should_not_be_sent(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);
        let before = session.state().searches_made;

        for invalid in ["", "   ", "with\ncontrol", &"中".repeat(40)] {
            let started = pane.update(cx, |pane, cx| pane.search_users(SmolStr::new(invalid), cx));
            assert!(!started, "{invalid:?} is not a query");
        }

        assert_eq!(session.state().searches_made, before);
    }

    /// A binding carries one call at a time, so a search and a change cannot both be out: the
    /// pane refuses whichever is asked for while the other is, which is what keeps either from
    /// taking the place of the other and leaving it unanswered.
    #[gpui_kit::test]
    fn a_search_and_a_change_should_not_take_each_others_place(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_search(vec![search_profile("9", "nine")]);
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);

        // A change that is out is a search the pane does not send.
        let block = session.block_mutation();
        pane.update(cx, |pane, cx| {
            assert!(pane.add_selection(SmolStr::new_static("eight"), cx));
        });
        settle_until(cx, "the change to be in flight", |cx| {
            let _ = cx;
            session.state().add_calls == 1
        });
        let searches = session.state().searches_made;
        assert!(
            !pane.update(cx, |pane, cx| pane
                .search_users(SmolStr::new_static("nine"), cx)),
            "a search should not be sent while a change is out"
        );
        assert_eq!(
            session.state().searches_made,
            searches,
            "and it should not reach the service either"
        );

        // A search that is out is a change the pane does not send, and neither of them is
        // cancelled by the other because neither is ever sent while the other is out.
        pane.update(cx, |pane, cx| pane.watch_selections(true, cx));
        settle_until(cx, "the displaced change to be let go", |cx| {
            let _ = cx;
            block.is_dropped()
        });
        assert!(cx.update(|cx| pane.read(cx).pending.is_none()));
        pane.update(cx, |pane, cx| {
            assert!(pane.search_users(SmolStr::new_static("nine"), cx));
        });
        assert!(
            cx.update(|cx| pane.read(cx).searching.is_some()),
            "the search should be out"
        );
        assert!(
            !pane.update(cx, |pane, cx| {
                pane.add_selection(SmolStr::new_static("ten"), cx)
            }),
            "a change should not be sent while a search is out"
        );
        assert!(
            !pane.update(cx, |pane, cx| {
                pane.delete_selection(SmolStr::new_static("7"), cx)
            }),
            "nor should a removal"
        );
        assert!(
            cx.update(|cx| pane.read(cx).searching.is_some()),
            "and neither of them should have taken the search's place"
        );
    }

    /// A call the stream asks for takes the place of the call the binding was carrying, and what
    /// it displaced is closed out rather than left waiting for an answer that can no longer
    /// arrive: a search, whose page would otherwise spin forever, and a change, which is dropped
    /// rather than decided and settled by the read that follows.
    #[gpui_kit::test]
    fn a_call_the_stream_asked_for_should_close_out_what_it_displaced(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);
        // A dialog is open, because the calls the stream asks for as it comes up — the accounts
        // read again, their avatars watched — are the ones a dialog's presence asks for.
        let _dialog = attach_dialog(&pane, cx);

        // A search that is out when the stream comes back up: the read the stream asks for takes
        // its place, and the page that asked for it stops waiting.
        pane.update(cx, |pane, cx| {
            assert!(pane.search_users(SmolStr::new_static("nine"), cx));
        });
        assert!(cx.update(|cx| pane.read(cx).searching.is_some()));
        report_status(&pane, TweetStatus::Ready, cx);
        assert!(
            cx.update(|cx| pane.read(cx).searching.is_none()),
            "a search the stream took the place of should not be waited for"
        );
        settle_until(cx, "the read the stream asked for", |cx| {
            pane.read(cx).pending.is_none()
        });

        // A change that is out when the stream comes back up is dropped rather than left
        // pending, and the read the stream asks for is what settles the list.
        let block = session.block_mutation();
        pane.update(cx, |pane, cx| {
            assert!(pane.add_selection(SmolStr::new_static("eight"), cx));
        });
        settle_until(cx, "the change to be in flight", |cx| {
            let _ = cx;
            session.state().add_calls == 1
        });
        assert!(cx.update(|cx| pane.read(cx).pending.is_some()));
        report_status(&pane, TweetStatus::Ready, cx);
        assert!(
            cx.update(|cx| pane
                .read(cx)
                .pending
                .as_ref()
                .is_some_and(|pending| pending.operation.is_read())),
            "a change whose answer cannot arrive should be replaced by the read the stream asked \
             for, which is what settles the list"
        );
        settle_until(cx, "the read the stream asked for", |cx| {
            pane.read(cx).pending.is_none()
        });
        assert_eq!(
            usernames_of(&pane, cx),
            ["seven"],
            "the read is what settles a list the dropped change may have altered"
        );
        settle_until(cx, "the displaced change to be let go", |cx| {
            let _ = cx;
            block.is_dropped()
        });
    }

    /// What is remembered about a row waiting to be activated is a memo beside the rows: a read
    /// of the accounts keeps it for the rows the read still holds, and an account change is a
    /// pane starting over with a set of rows that are not the ones it named.
    #[gpui_kit::test]
    fn a_remembered_name_should_survive_a_read_and_not_an_account_change(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) =
            live_pane_reading(cx, &source, &session, vec![selection("7", "seven")], 3);

        session.answer_add(XAccountMonitoringMutationOutcome::Confirmed(pending("8")));
        pane.update(cx, |pane, cx| {
            assert!(pane.add_selection(SmolStr::new_static("eight"), cx));
        });
        settle_until(cx, "the account to be added", |cx| {
            pane.read(cx)
                .selections
                .as_ref()
                .is_some_and(|data| data.selections().len() == 2)
        });
        assert_eq!(
            remembered_names(&pane, cx),
            HashMap::from([(SmolStr::new_static("8"), SharedString::new_static("eight"))]),
            "an account that came back without a profile should be remembered by its username"
        );

        // The accounts are read again, and the row the read does not name a profile for is named
        // by what the pane was told when it was added.
        session.answer_selections(selections(vec![selection("7", "seven"), pending("8")], 3));
        pane.update(cx, |pane, cx| {
            assert!(pane.load_selections(cx));
        });
        settle_until(cx, "the accounts to be read again", |cx| {
            pane.read(cx).pending.is_none()
        });
        assert_eq!(
            remembered_names(&pane, cx),
            HashMap::from([(SmolStr::new_static("8"), SharedString::new_static("eight"))]),
            "a read should keep the name of a row that is still waiting to be activated"
        );
        assert!(
            cx.update(|cx| pane
                .read(cx)
                .selections
                .as_ref()
                .is_some_and(|data| data.contains_username("eight", &pane.read(cx).hints))),
            "and the account should still be recognised by that name"
        );

        // A read that no longer holds the row leaves nothing of it behind.
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        pane.update(cx, |pane, cx| {
            assert!(pane.load_selections(cx));
        });
        settle_until(cx, "the accounts to be read once more", |cx| {
            pane.read(cx).pending.is_none()
                && pane
                    .read(cx)
                    .selections
                    .as_ref()
                    .is_some_and(|data| data.selections().len() == 1)
        });
        assert!(
            cx.update(|cx| pane.read(cx).hints.is_empty()),
            "a name whose row the read no longer holds should not be remembered"
        );

        // Another account is another list of rows, and a name remembered for one of them is not
        // a name for the other.
        session.answer_add(XAccountMonitoringMutationOutcome::Confirmed(pending("8")));
        pane.update(cx, |pane, cx| {
            assert!(pane.add_selection(SmolStr::new_static("eight"), cx));
        });
        settle_until(cx, "the account to be added again", |cx| {
            pane.read(cx)
                .selections
                .as_ref()
                .is_some_and(|data| data.selections().len() == 2)
        });
        let _other = source.session(OTHER_ACCOUNT).subscribe();
        pane.update(cx, |pane, cx| {
            pane.reconcile_account(Some(SmolStr::new_static(OTHER_ACCOUNT)), cx);
        });
        assert!(
            cx.update(|cx| pane.read(cx).hints.is_empty()),
            "an account change should drop what was remembered about the account it left"
        );
    }

    #[gpui_kit::test]
    fn a_picture_the_service_reported_as_failed_should_be_drawn_as_one(cx: &mut TestAppContext) {
        let url = media_url("failed");
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) = live_pane(cx, &source, &session, Vec::new());
        settle_until(cx, "the timeline to arrive", |cx| {
            pane.read(cx).feed.is_some()
        });

        let mut feed = FakeFeed::new();
        let snapshot = feed.push("1", 1);
        let media = Arc::new(HashMap::from([(
            SmolStr::new(url.as_str()),
            MediaState::Failed,
        )]));
        report_update(&pane, snapshot, media, cx);

        assert_eq!(ids_of(&pane, cx), ["1"]);
        assert_eq!(
            cx.update(|cx| pane.read(cx).media.get(url.as_str()).cloned()),
            Some(MediaState::Failed)
        );
    }

    #[gpui_kit::test]
    fn a_timeline_that_fills_up_should_keep_the_newest_fifty(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) = live_pane_with(cx, &source, &session, MAX_TWEETS);

        let mut feed = FakeFeed::new();
        for index in 0..MAX_TWEETS {
            feed.push(&index.to_string(), index as i64);
        }
        let snapshot = feed.push("new", MAX_TWEETS as i64);
        report_update(&pane, snapshot, Arc::new(HashMap::new()), cx);

        let ids = ids_of(&pane, cx);
        assert_eq!(ids.len(), MAX_TWEETS);
        assert_eq!(ids[0], "new");
    }

    /// Draws one more frame with every cache missed, so what a debug selector reports is what this
    /// frame painted rather than what the frame before it left behind. A paint action is what fills
    /// a selector, and a frame that hits the body cache reuses the last paint range without drawing
    /// (AGENTS.md §10); `window.refresh()` is the one frame that cannot hit it. It matters
    /// for an absence most of all: a reused paint range would keep saying a row is gone.
    fn repaint(window: &mut VisualTestContext) {
        window.update(|window, _| window.refresh());
        window.run_until_parked();
    }

    /// One pane drawn into a window, which is what a frame's selectors are read from.
    ///
    /// The pane is drawn as the entity it is rather than by calling its body here, because
    /// what a row draws is read from the pane while the list draws it: a frame is the only
    /// thing that puts those two in an order.
    struct PaneHarness {
        pane: Entity<XAccountMonitoringWidget>,
    }

    impl Render for PaneHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.pane.clone())
        }
    }

    #[gpui_kit::test]
    fn a_frame_should_draw_the_rows_its_timeline_holds(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) = live_pane_with(cx, &source, &session, 2);
        {
            let (_, window) = cx.add_window_view(|_, _| PaneHarness { pane: pane.clone() });
            window.run_until_parked();
            // The rows, their text and the end row are all drawn inside the body, which is the one
            // cached layer (AGENTS.md §10).
            repaint(window);

            for selector in [
                "TEST_TWEET_LIST",
                "TEST_TWEET_ROW",
                "TEST_TWEET_TEXT",
                "TEST_TWEET_TIME",
                "TEST_TWEET_DISPLAY_NAME",
                "TEST_TWEET_VERIFIED",
                "TEST_TWEET_TIMELINE_END",
            ] {
                assert!(
                    window.debug_bounds(selector).is_some(),
                    "{selector} should be drawn"
                );
            }
        }

        // A pane that dropped its timeline draws the list and its end row only: the row that
        // says there are no more is the list's own footer rather than one of its tweets.
        pane.update(cx, |pane, cx| pane.clear(cx));
        let (_, window) = cx.add_window_view(|_, _| PaneHarness { pane: pane.clone() });
        window.run_until_parked();
        // A cleared timeline draws its end row and no tweet rows, and both readings are of the
        // cached body: the frame has to be a fresh one (AGENTS.md §10).
        repaint(window);
        assert!(window.debug_bounds("TEST_TWEET_ROW").is_none());
        assert!(
            window.debug_bounds("TEST_TWEET_TIMELINE_END").is_some(),
            "a cleared timeline still says there are no more tweets"
        );
    }

    #[gpui_kit::test]
    fn a_frame_should_offer_a_failed_picture_again(cx: &mut TestAppContext) {
        let url = media_url("retry");
        let (cache, root) = media_cache();
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        // A transport that refuses and a cache with nothing in it, so the tile the frame draws
        // is the one that failed.
        let (pane, _root) = live_pane_over(
            cx,
            &source,
            &session,
            vec![tweet_with_media("1", 1, url.as_str())],
            cache.clone(),
            root,
        );
        settle_until(cx, "the picture to fail", |cx| {
            pane.read(cx).media.get(url.as_str()) == Some(&MediaState::Failed)
        });

        {
            let (_, window) = cx.add_window_view(|_, _| PaneHarness { pane: pane.clone() });
            window.run_until_parked();
            // A row whose picture failed is still a row of the cached body.
            repaint(window);
            assert!(window.debug_bounds("TEST_TWEET_ROW").is_some());
        }

        // The retry control is part of the failed tile, and clicking it is what asks the
        // service for the picture again. The picture is put on disk first, which is what the
        // second attempt finds where the first found nothing.
        {
            let handle = cx.update(|cx| widget_tokio_runtime::global::handle(cx).handle().clone());
            handle.block_on(seed_picture(&cache, &url, b"image"));
        }
        pane.update(cx, |pane, cx| {
            pane.retry_media(SmolStr::new(url.as_str()), cx)
        });
        settle_until(cx, "the picture to be fetched again", |cx| {
            matches!(
                pane.read(cx).media.get(url.as_str()),
                Some(MediaState::Ready(_))
            )
        });
    }

    /// A feed nothing the list held survives in is one the list is built again for: a list holds
    /// the tweets it was given, so the feed a pane draws is the one it has now rather than the
    /// rows of the one before it.
    #[gpui_kit::test]
    fn a_feed_nothing_of_which_survives_should_be_held_by_a_list_built_again(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (pane, _root) = live_pane(cx, &source, &session, vec![tweet("1", 1), tweet("2", 2)]);

        let held = cx.update(|cx| pane.read(cx).list.entity_id());

        let mut feed = FakeFeed::new();
        let snapshot = feed.push("9", 9);
        report_update(&pane, snapshot, Arc::new(HashMap::new()), cx);

        assert_eq!(
            ids_of(&pane, cx),
            ["9"],
            "the pane holds the feed it was given"
        );
        let built = cx.update(|cx| pane.read(cx).list.entity_id());

        assert_ne!(
            held, built,
            "the list is built again for a feed none of what it held survives in"
        );
    }
}

//! The fake X account the X tests answer through.
//!
//! It is the seam both the service and the pane read X through, so it is shared rather than
//! copied: what a pane's tests need to control is the same session the service's tests take,
//! refuse and hold, and the same subscription they hand tweets to. The pictures a stream
//! fetches go through a fake transport for the same reason — a test that wants a picture on
//! disk puts one in the cache, and one that wants a picture to fail says so.

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use gpui_kit::{App, TestAppContext};
use kytrix_service_provider::service::{
    engine::{
        KytrixServiceError, XProfileEvent, XSubscriptionEvent, XTweetBatch, XTweetEvent,
        XTweetMedia, XTweetTextSegment,
    },
    x::{XApiError, XMutationOutcome, XProfile, XSelection, XSelectionStatus, XSelectionsResponse},
};
use network::transport::{error::TransportError, subscription::SubscriptionStatus};
use reqwest::Client;
use smol_str::{SmolStr, format_smolstr};
use tokio::sync::{Notify, mpsc, watch};
use url::Url;

use crate::{
    media::{MediaCache, MediaError, MediaTransport},
    model::{TweetSnapshot, TweetState},
    service::{TweetEventSubscription, TweetSession, TweetSource},
};

/// The account every fixture belongs to.
pub(super) const ACCOUNT: &str = "test-account";

/// Waits until the condition holds, which is what a test driving two executors at once needs.
///
/// Two executors are involved: a widget sends its commands from GPUI's own, and the service
/// answers on the application runtime. So both are given a turn — the pane first, because a
/// command that has not been sent is a service with nothing to answer.
pub(super) fn settle_until(cx: &mut TestAppContext, what: &str, ready: impl Fn(&App) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        let handle =
            cx.update(|cx: &mut App| widget_tokio_runtime::global::handle(cx).handle().clone());
        handle.block_on(async { tokio::task::yield_now().await });
        if cx.update(|cx| ready(cx)) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "{what} should happen");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// One tweet at one time, with nothing on it.
pub(super) fn tweet(tweet_id: &str, timestamp: i64) -> XTweetEvent {
    XTweetEvent {
        tweet_id: tweet_id.into(),
        timestamp,
        x_user_id: SmolStr::new_static("42"),
        text_segments: vec![XTweetTextSegment {
            text: SmolStr::new(format!("tweet {tweet_id}")),
            url: None,
        }],
        reference: None,
        media: Vec::new(),
    }
}

/// One tweet carrying one picture.
pub(super) fn tweet_with_media(tweet_id: &str, timestamp: i64, url: &str) -> XTweetEvent {
    let mut event = tweet(tweet_id, timestamp);
    event.media = vec![XTweetMedia::Image {
        image_url: SmolStr::new(url),
        width: 100,
        height: 100,
    }];
    event
}

/// The profile one author is read with.
pub(super) fn profile(x_user_id: &str, username: &str) -> XProfileEvent {
    XProfileEvent {
        x_user_id: x_user_id.into(),
        username: username.into(),
        display_name: SmolStr::new(format!("{username} display")),
        avatar_url: SmolStr::new(format!("https://pbs.twimg.com/{username}.jpg")),
        verified: true,
        revision: 1,
    }
}

/// One account being monitored.
pub(super) fn selection(x_user_id: &str, username: &str) -> XSelection {
    XSelection {
        x_user_id: x_user_id.into(),
        status: XSelectionStatus::Active,
        created_at: 1,
        updated_at: 2,
        profile: Some(search_profile(x_user_id, username)),
    }
}

/// One account X was searched for.
pub(super) fn search_profile(x_user_id: &str, username: &str) -> XProfile {
    XProfile {
        x_user_id: x_user_id.into(),
        username: username.into(),
        display_name: SmolStr::new(format!("{username} display")),
        avatar_url: SmolStr::new(format!("https://pbs.twimg.com/{username}.jpg")),
        description: SmolStr::new(format!("about {username}")),
        followers_count: 1_250,
        following_count: 12,
        protected: false,
        verified: true,
    }
}

/// One answer to a read of the monitored accounts.
pub(super) fn selections(selections: Vec<XSelection>, limit: usize) -> XSelectionsResponse {
    XSelectionsResponse {
        selections,
        x_account_limit: limit,
    }
}

/// The picture URL one tweet of the fixtures carries.
pub(super) fn media_url(name: &str) -> Url {
    Url::parse(&format!("https://pbs.twimg.com/media/{name}.jpg")).expect("a picture URL")
}

/// A feed a test grows the way a subscription does.
///
/// Each snapshot shares the tweets it inherited from the one before it, which is what makes a
/// later report an edit to a list rather than a new set of rows: a test that built every
/// snapshot from scratch would describe a feed whose tweets are all new.
pub(super) struct FakeFeed {
    state: TweetState,
}

impl FakeFeed {
    pub(super) fn new() -> Self {
        Self {
            state: TweetState::new(),
        }
    }

    /// Retains one tweet and hands back the feed it produced.
    pub(super) fn push(&mut self, tweet_id: &str, timestamp: i64) -> TweetSnapshot {
        self.state.push(tweet(tweet_id, timestamp));
        self.state.snapshot()
    }
}

/// A source of the sessions a test built, in the order it built them.
#[derive(Clone, Default)]
pub(super) struct FakeSource {
    state: Arc<Mutex<FakeSourceState>>,
}

#[derive(Default)]
struct FakeSourceState {
    sessions: VecDeque<FakeSetup>,
    acquired: Vec<SmolStr>,
}

/// What the next `acquire_session` call finds.
enum FakeSetup {
    /// A session for the account that was asked for.
    Ready(FakeSession),
    /// A refusal, the way an account whose credentials are gone answers.
    Failed(KytrixServiceError),
    /// An acquisition that never answers, so the caller's cancellation is the only way out.
    Blocked(Arc<OperationBlock>),
}

impl FakeSource {
    /// Queues a session for `account`, which is what the next acquisition finds.
    pub(super) fn session(&self, account: &str) -> FakeSession {
        let session = FakeSession::new(account);
        self.state()
            .sessions
            .push_back(FakeSetup::Ready(session.clone()));
        session
    }

    /// Queues a refusal in place of a session.
    pub(super) fn refuse(&self, error: KytrixServiceError) {
        self.state().sessions.push_back(FakeSetup::Failed(error));
    }

    /// Queues an acquisition that never answers.
    pub(super) fn block(&self) -> Arc<OperationBlock> {
        let block = Arc::new(OperationBlock::default());
        self.state()
            .sessions
            .push_back(FakeSetup::Blocked(block.clone()));
        block
    }

    /// The accounts the service has asked for a session of, in the order it asked.
    pub(super) fn requested(&self) -> Vec<SmolStr> {
        self.state().acquired.clone()
    }

    fn state(&self) -> MutexGuard<'_, FakeSourceState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl TweetSource for FakeSource {
    type Session = FakeSession;

    async fn acquire_session(&self, account_id: &str) -> Result<Self::Session, KytrixServiceError> {
        let setup = {
            let mut state = self.state();
            state.acquired.push(SmolStr::new(account_id));
            state
                .sessions
                .pop_front()
                .unwrap_or(FakeSetup::Failed(KytrixServiceError::Closed))
        };
        match setup {
            FakeSetup::Ready(session) => Ok(session),
            FakeSetup::Failed(error) => Err(error),
            FakeSetup::Blocked(block) => {
                let _guard = OperationGuard {
                    block: block.clone(),
                };
                block.started.notify_one();
                std::future::pending().await
            }
        }
    }
}

/// One account's session, as a test drives it.
#[derive(Clone)]
pub(super) struct FakeSession {
    account_id: SmolStr,
    state: Arc<Mutex<FakeSessionState>>,
}

pub(super) struct FakeSessionState {
    pub subscriptions: VecDeque<Result<FakeSubscription, KytrixServiceError>>,
    pub history: VecDeque<Result<XTweetBatch, XApiError>>,
    pub selections: VecDeque<Result<XSelectionsResponse, XApiError>>,
    pub searches: VecDeque<Result<Vec<XProfile>, XApiError>>,
    pub adds: VecDeque<XMutationOutcome<XSelection>>,
    pub deletes: VecDeque<XMutationOutcome<()>>,
    pub subscribes: usize,
    /// How many times the timeline already published was read.
    pub history_reads: usize,
    /// How many times the monitored accounts were read.
    pub fetches: usize,
    pub searches_made: usize,
    pub add_calls: usize,
    pub delete_calls: usize,
    /// A change that never answers, held for the next mutation that is asked for.
    pub blocked_mutation: Option<Arc<OperationBlock>>,
    /// A search that never answers, held for the next one that is asked for.
    pub blocked_search: Option<Arc<OperationBlock>>,
}

impl FakeSession {
    fn new(account_id: &str) -> Self {
        Self {
            account_id: SmolStr::new(account_id),
            state: Arc::new(Mutex::new(FakeSessionState {
                subscriptions: VecDeque::new(),
                history: VecDeque::new(),
                selections: VecDeque::new(),
                searches: VecDeque::new(),
                adds: VecDeque::new(),
                deletes: VecDeque::new(),
                subscribes: 0,
                history_reads: 0,
                fetches: 0,
                searches_made: 0,
                add_calls: 0,
                delete_calls: 0,
                blocked_mutation: None,
                blocked_search: None,
            })),
        }
    }

    /// Queues one live subscription and hands back the knobs that drive it.
    pub(super) fn subscribe(&self) -> FakeSubscriptionControl {
        let (events, receiver) = mpsc::channel(16);
        let (status, _) = watch::channel(SubscriptionStatus::Active);
        let drops = Arc::new(AtomicUsize::new(0));
        let fail_on_delivery = Arc::new(Mutex::new(None));
        self.state().subscriptions.push_back(Ok(FakeSubscription {
            events: receiver,
            status: status.clone(),
            drops: drops.clone(),
            fail_on_delivery: fail_on_delivery.clone(),
        }));
        FakeSubscriptionControl {
            events,
            status,
            drops,
            fail_on_delivery,
        }
    }

    /// Queues a subscription the account is refused.
    pub(super) fn refuse_subscription(&self, error: KytrixServiceError) {
        self.state().subscriptions.push_back(Err(error));
    }

    /// Queues what the timeline already published holds.
    pub(super) fn answer_history(&self, tweets: Vec<XTweetEvent>, profiles: Vec<XProfileEvent>) {
        self.state()
            .history
            .push_back(Ok(XTweetBatch { tweets, profiles }));
    }

    /// Queues a history the account is refused.
    pub(super) fn refuse_history(&self, error: XApiError) {
        self.state().history.push_back(Err(error));
    }

    /// Queues what a read of the monitored accounts answers with.
    pub(super) fn answer_selections(&self, response: XSelectionsResponse) {
        self.state().selections.push_back(Ok(response));
    }

    /// Queues a read the account is refused.
    pub(super) fn refuse_selections(&self, error: XApiError) {
        self.state().selections.push_back(Err(error));
    }

    /// Queues what a search answers with.
    pub(super) fn answer_search(&self, profiles: Vec<XProfile>) {
        self.state().searches.push_back(Ok(profiles));
    }

    /// Queues a change X was asked to make.
    pub(super) fn answer_add(&self, outcome: XMutationOutcome<XSelection>) {
        self.state().adds.push_back(outcome);
    }

    /// Queues a change X was asked to make.
    pub(super) fn answer_delete(&self, outcome: XMutationOutcome<()>) {
        self.state().deletes.push_back(outcome);
    }

    /// Queues a change that never answers.
    pub(super) fn block_mutation(&self) -> Arc<OperationBlock> {
        let block = Arc::new(OperationBlock::default());
        self.state().blocked_mutation = Some(block.clone());
        block
    }

    /// Queues a search that never answers.
    ///
    /// What a held search is for is the state a pane is in while one is out: nothing answers it,
    /// so what the pane does with it is the only thing that moves.
    pub(super) fn block_search(&self) -> Arc<OperationBlock> {
        let block = Arc::new(OperationBlock::default());
        self.state().blocked_search = Some(block.clone());
        block
    }

    pub(super) fn state(&self) -> MutexGuard<'_, FakeSessionState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl TweetSession for FakeSession {
    type Subscription = FakeSubscription;

    fn account_id(&self) -> &str {
        &self.account_id
    }

    async fn subscribe_x(&self) -> Result<Self::Subscription, KytrixServiceError> {
        let mut state = self.state();
        state.subscribes += 1;
        state
            .subscriptions
            .pop_front()
            .unwrap_or(Err(KytrixServiceError::Closed))
    }

    async fn fetch_tweets(&self) -> Result<XTweetBatch, XApiError> {
        let mut state = self.state();
        state.history_reads += 1;
        state
            .history
            .pop_front()
            .unwrap_or(Err(XApiError::NotReady))
    }

    async fn fetch_selections(&self) -> Result<XSelectionsResponse, XApiError> {
        let mut state = self.state();
        state.fetches += 1;
        state
            .selections
            .pop_front()
            .unwrap_or(Err(XApiError::NotReady))
    }

    async fn search_users(&self, _: &str) -> Result<Vec<XProfile>, XApiError> {
        let (answer, blocked) = {
            let mut state = self.state();
            state.searches_made += 1;
            (
                state
                    .searches
                    .pop_front()
                    .unwrap_or(Err(XApiError::NotReady)),
                state.blocked_search.take(),
            )
        };
        hold(blocked).await;
        answer
    }

    async fn add_selection(&self, _: &str) -> XMutationOutcome<XSelection> {
        let (outcome, blocked) = {
            let mut state = self.state();
            state.add_calls += 1;
            (
                state
                    .adds
                    .pop_front()
                    .unwrap_or(XMutationOutcome::Rejected(XApiError::NotReady)),
                state.blocked_mutation.take(),
            )
        };
        hold(blocked).await;
        outcome
    }

    async fn delete_selection(&self, _: &str) -> XMutationOutcome<()> {
        let (outcome, blocked) = {
            let mut state = self.state();
            state.delete_calls += 1;
            (
                state
                    .deletes
                    .pop_front()
                    .unwrap_or(XMutationOutcome::Rejected(XApiError::NotReady)),
                state.blocked_mutation.take(),
            )
        };
        hold(blocked).await;
        outcome
    }
}

/// Holds one answer inside the session for as long as the test wants it held.
///
/// What a held answer leaves behind is the request itself: nothing is ever answered, so what a
/// test reads is what the caller does with a call that is still out — and the block says whether
/// the caller let it go.
async fn hold(blocked: Option<Arc<OperationBlock>>) {
    let Some(block) = blocked else {
        return;
    };
    let _guard = OperationGuard {
        block: block.clone(),
    };
    block.started.notify_one();
    std::future::pending().await
}

/// One live subscription inside the fake session.
pub(super) struct FakeSubscription {
    events: mpsc::Receiver<Result<XSubscriptionEvent, TransportError>>,
    status: watch::Sender<SubscriptionStatus>,
    drops: Arc<AtomicUsize>,
    /// The status this subscription takes on as it hands the next event over, which is how a
    /// test puts a credential change and a tweet on the wire at the same moment.
    fail_on_delivery: Arc<Mutex<Option<TransportError>>>,
}

impl Drop for FakeSubscription {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// One live subscription, as a test drives it.
pub(super) struct FakeSubscriptionControl {
    pub events: mpsc::Sender<Result<XSubscriptionEvent, TransportError>>,
    pub status: watch::Sender<SubscriptionStatus>,
    /// How many times this subscription has been dropped, which is how a test sees the service
    /// let go of an account it no longer follows.
    pub drops: Arc<AtomicUsize>,
    fail_on_delivery: Arc<Mutex<Option<TransportError>>>,
}

impl FakeSubscriptionControl {
    /// Fails the subscription as it hands its next event over.
    pub(super) fn fail_on_delivery(&self, error: TransportError) {
        *self
            .fail_on_delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
    }
}

impl TweetEventSubscription for FakeSubscription {
    fn observe_status(&self) -> watch::Receiver<SubscriptionStatus> {
        self.status.subscribe()
    }

    async fn recv(&mut self) -> Result<XSubscriptionEvent, TransportError> {
        let event = self
            .events
            .recv()
            .await
            .unwrap_or(Err(TransportError::Closed));
        let failed = self
            .fail_on_delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(error) = failed {
            self.status.send_replace(SubscriptionStatus::Failed(error));
        }
        event
    }
}

/// The transport a stream fetches its pictures over.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum FakeTransportMode {
    /// A client that reaches a server the test started, with no proxy in the path.
    #[default]
    Serve,
    /// A transport that refuses every request, which is a picture that fails at once.
    Fail,
    /// A transport that never answers, which is a picture that is still being fetched.
    Block,
}

/// A transport a test drives the media path with.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct FakeTransport {
    mode: FakeTransportMode,
}

impl FakeTransport {
    pub(super) const fn failing() -> Self {
        Self {
            mode: FakeTransportMode::Fail,
        }
    }

    pub(super) const fn blocking() -> Self {
        Self {
            mode: FakeTransportMode::Block,
        }
    }
}

impl MediaTransport for FakeTransport {
    async fn client(
        &self,
        _destination: &Url,
        _cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<Client, MediaError> {
        match self.mode {
            FakeTransportMode::Serve => Client::builder()
                .no_proxy()
                .build()
                .map_err(|error| MediaError::Network(format_smolstr!("{error}"))),
            FakeTransportMode::Fail => Err(MediaError::Network(SmolStr::new_static(
                "the test transport refuses every request",
            ))),
            FakeTransportMode::Block => std::future::pending().await,
        }
    }
}

/// A cache in a directory of the test's own.
pub(super) fn media_cache() -> (MediaCache, tempfile::TempDir) {
    let root = test_temp("media-cache");
    (
        MediaCache::with_options(Some(root.path().to_path_buf()), 64, 4 * 1_024 * 1_024),
        root,
    )
}

/// A temporary directory of the test's own, under the workspace's build output.
///
/// The system temporary directory is not usable here: a test that runs behind a restricted
/// integrity token cannot create directories in `%TEMP%`, which leaves every test about a file
/// on disk failing for a reason that has nothing to do with the code under test. The build
/// output is the workspace's own writable scratch space, so the directory goes there.
pub(super) fn test_temp(name: &str) -> tempfile::TempDir {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .and_then(std::path::Path::parent)
        .expect("this widget lives three directories below the workspace root");
    let root = workspace.join("target").join("test-temp").join("tweets");
    std::fs::create_dir_all(&root).expect("the test temporary directory should be created");
    tempfile::Builder::new()
        .prefix(name)
        .tempdir_in(&root)
        .expect("a temporary directory should be created")
}

/// Puts one finished picture in the cache, which is what a row draws a picture from.
pub(super) async fn seed_picture(cache: &MediaCache, url: &Url, bytes: &[u8]) -> PathBuf {
    cache.seed(url, bytes).await
}

/// One operation a fake can be held inside.
#[derive(Default)]
pub(super) struct OperationBlock {
    started: Notify,
    dropped: AtomicBool,
    dropped_notify: Notify,
}

struct OperationGuard {
    block: Arc<OperationBlock>,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        self.block.dropped.store(true, Ordering::SeqCst);
        self.block.dropped_notify.notify_waiters();
    }
}

impl OperationBlock {
    pub(super) async fn wait_started(&self) {
        tokio::time::timeout(Duration::from_secs(2), self.started.notified())
            .await
            .expect("the blocked operation should start");
    }

    pub(super) async fn wait_dropped(&self) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !self.dropped.load(Ordering::SeqCst) {
                self.dropped_notify.notified().await;
            }
        })
        .await
        .expect("the blocked operation should be dropped");
    }

    /// Whether the operation has been let go, for a test that turns its own executor rather
    /// than parking on this one.
    pub(super) fn is_dropped(&self) -> bool {
        self.dropped.load(Ordering::SeqCst)
    }
}

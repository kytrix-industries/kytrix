use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

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

pub(super) const ACCOUNT: &str = "test-account";

pub(super) fn tweet(tweet_id: &str, timestamp: i64) -> XTweetEvent {
    XTweetEvent {
        tweet_id: tweet_id.into(),
        timestamp,
        x_user_id: SmolStr::new_static("42"),
        text_segments: vec![XTweetTextSegment {
            text: format_smolstr!("tweet {tweet_id}"),
            url: None,
        }],
        reference: None,
        media: Vec::new(),
    }
}

pub(super) fn tweet_with_media(tweet_id: &str, timestamp: i64, url: &str) -> XTweetEvent {
    let mut event = tweet(tweet_id, timestamp);
    event.media = vec![XTweetMedia::Image {
        image_url: SmolStr::new(url),
        width: 100,
        height: 100,
    }];
    event
}

pub(super) fn profile(x_user_id: &str, username: &str) -> XProfileEvent {
    XProfileEvent {
        x_user_id: x_user_id.into(),
        username: username.into(),
        display_name: format_smolstr!("{username} display"),
        avatar_url: format_smolstr!("https://pbs.twimg.com/{username}.jpg"),
        verified: true,
        revision: 1,
    }
}

pub(super) fn selection(x_user_id: &str, username: &str) -> XSelection {
    XSelection {
        x_user_id: x_user_id.into(),
        status: XSelectionStatus::Active,
        created_at: 1,
        updated_at: 2,
        profile: Some(search_profile(x_user_id, username)),
    }
}

pub(super) fn search_profile(x_user_id: &str, username: &str) -> XProfile {
    XProfile {
        x_user_id: x_user_id.into(),
        username: username.into(),
        display_name: format_smolstr!("{username} display"),
        avatar_url: format_smolstr!("https://pbs.twimg.com/{username}.jpg"),
        description: format_smolstr!("about {username}"),
        followers_count: 1_250,
        following_count: 12,
        protected: false,
        verified: true,
    }
}

pub(super) fn selections(selections: Vec<XSelection>, limit: usize) -> XSelectionsResponse {
    XSelectionsResponse {
        selections,
        x_account_limit: limit,
    }
}

pub(super) fn media_url(name: &str) -> Url {
    Url::parse(&format!("https://pbs.twimg.com/media/{name}.jpg")).expect("a picture URL")
}

pub(super) struct FakeFeed {
    state: TweetState,
}

impl FakeFeed {
    pub(super) fn new() -> Self {
        Self {
            state: TweetState::new(),
        }
    }

    pub(super) fn push(&mut self, tweet_id: &str, timestamp: i64) -> TweetSnapshot {
        self.state.push(tweet(tweet_id, timestamp));
        self.state.snapshot()
    }
}

#[derive(Clone, Default)]
pub(super) struct FakeSource {
    state: Arc<Mutex<FakeSourceState>>,
}

#[derive(Default)]
struct FakeSourceState {
    sessions: VecDeque<FakeSetup>,
    acquired: Vec<SmolStr>,
}

enum FakeSetup {
    Ready(FakeSession),
    Failed(KytrixServiceError),
    Blocked(Arc<OperationBlock>),
}

impl FakeSource {
    pub(super) fn session(&self, account: &str) -> FakeSession {
        let session = FakeSession::new(account);
        self.state()
            .sessions
            .push_back(FakeSetup::Ready(session.clone()));
        session
    }

    pub(super) fn refuse(&self, error: KytrixServiceError) {
        self.state().sessions.push_back(FakeSetup::Failed(error));
    }

    pub(super) fn block(&self) -> Arc<OperationBlock> {
        let block = Arc::new(OperationBlock::default());
        self.state()
            .sessions
            .push_back(FakeSetup::Blocked(block.clone()));
        block
    }

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
    pub history_reads: usize,
    pub fetches: usize,
    pub searches_made: usize,
    pub add_calls: usize,
    pub delete_calls: usize,
    pub blocked_mutation: Option<Arc<OperationBlock>>,
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

    pub(super) fn refuse_subscription(&self, error: KytrixServiceError) {
        self.state().subscriptions.push_back(Err(error));
    }

    pub(super) fn answer_history(&self, tweets: Vec<XTweetEvent>, profiles: Vec<XProfileEvent>) {
        self.state()
            .history
            .push_back(Ok(XTweetBatch { tweets, profiles }));
    }

    pub(super) fn refuse_history(&self, error: XApiError) {
        self.state().history.push_back(Err(error));
    }

    pub(super) fn answer_selections(&self, response: XSelectionsResponse) {
        self.state().selections.push_back(Ok(response));
    }

    pub(super) fn refuse_selections(&self, error: XApiError) {
        self.state().selections.push_back(Err(error));
    }

    pub(super) fn answer_search(&self, profiles: Vec<XProfile>) {
        self.state().searches.push_back(Ok(profiles));
    }

    pub(super) fn answer_add(&self, outcome: XMutationOutcome<XSelection>) {
        self.state().adds.push_back(outcome);
    }

    pub(super) fn answer_delete(&self, outcome: XMutationOutcome<()>) {
        self.state().deletes.push_back(outcome);
    }

    pub(super) fn block_mutation(&self) -> Arc<OperationBlock> {
        let block = Arc::new(OperationBlock::default());
        self.state().blocked_mutation = Some(block.clone());
        block
    }

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

pub(super) struct FakeSubscription {
    events: mpsc::Receiver<Result<XSubscriptionEvent, TransportError>>,
    status: watch::Sender<SubscriptionStatus>,
    drops: Arc<AtomicUsize>,
    fail_on_delivery: Arc<Mutex<Option<TransportError>>>,
}

impl Drop for FakeSubscription {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

pub(super) struct FakeSubscriptionControl {
    pub events: mpsc::Sender<Result<XSubscriptionEvent, TransportError>>,
    pub status: watch::Sender<SubscriptionStatus>,
    pub drops: Arc<AtomicUsize>,
    fail_on_delivery: Arc<Mutex<Option<TransportError>>>,
}

impl FakeSubscriptionControl {
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum FakeTransportMode {
    #[default]
    Serve,
    Fail,
    Block,
}

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

pub(super) fn media_cache() -> (MediaCache, tempfile::TempDir) {
    let root = test_temp("media-cache");
    (
        MediaCache::with_options(Some(root.path().to_path_buf()), 64, 4 * 1_024 * 1_024),
        root,
    )
}

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

pub(super) async fn seed_picture(cache: &MediaCache, url: &Url, bytes: &[u8]) -> PathBuf {
    cache.seed(url, bytes).await
}

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

    pub(super) fn is_dropped(&self) -> bool {
        self.dropped.load(Ordering::SeqCst)
    }
}

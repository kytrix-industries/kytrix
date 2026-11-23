//! The fakes the news tests answer through.
//!
//! They are the seam both the service and the pane read through, so they are shared rather
//! than copied: what a pane's tests need to control is the same subscription the service's
//! tests refuse, hold and close, and what a pane's tests need to hear is the same play a
//! machine with an output device would make.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use kytrix_service_provider::service::engine::{
    ErrorCode, KytrixServiceError, NewsEvent, NewsKind,
};
use network::transport::{error::TransportError, subscription::SubscriptionStatus};
use smol_str::{SmolStr, format_smolstr};
use tokio::sync::{Notify, mpsc, watch};

use crate::{
    model::{MAX_NEWS_ITEMS, NewsSnapshot, NewsState},
    service::{NewsEventSubscription, NewsSource},
    sound::NotificationSound,
};

/// The account every fixture scope belongs to.
pub(crate) const ACCOUNT: &str = "test-account";

/// One reported item.
pub(crate) fn event(timestamp: i64) -> NewsEvent {
    NewsEvent {
        timestamp,
        title: Some(SmolStr::new_static("Title")),
        content: format_smolstr!("News {timestamp}"),
        category: NewsKind::General,
    }
}

/// A feed a test grows the way a subscription does.
///
/// Each snapshot shares the items it inherited from the one before it, which is what
/// makes a later report an edit to a list rather than a new set of rows: a test that
/// built every snapshot from scratch would describe a feed whose items are all new.
pub(crate) struct FakeFeed {
    state: NewsState,
}

impl FakeFeed {
    pub(crate) fn new() -> Self {
        Self {
            state: NewsState::new(MAX_NEWS_ITEMS),
        }
    }

    /// Retains one reported item and hands back the feed it produced.
    pub(crate) fn push(&mut self, timestamp: i64) -> NewsSnapshot {
        self.state.push(event(timestamp));
        self.snapshot()
    }

    pub(crate) fn snapshot(&self) -> NewsSnapshot {
        self.state.snapshot()
    }
}

#[derive(Clone, Default)]
pub(crate) struct FakeSource {
    state: Arc<Mutex<FakeState>>,
}

#[derive(Default)]
struct FakeState {
    setups: VecDeque<FakeSetup>,
    subscriptions: usize,
}

/// What the next `subscribe_news` call finds.
enum FakeSetup {
    /// A live subscription.
    Ready(FakeSubscription),
    /// A refusal, the way a session without the entitlement answers.
    Failed(KytrixServiceError),
    /// A setup that never answers, so the caller's cancellation is the only way out.
    Blocked(Arc<OperationBlock>),
}

/// One live subscription inside the fake source.
///
/// It is named by the crate because it is the associated type the source's own seam
/// publishes it as.
pub(crate) struct FakeSubscription {
    events: mpsc::Receiver<Result<NewsEvent, TransportError>>,
    status: watch::Sender<SubscriptionStatus>,
    drops: Arc<AtomicUsize>,
}

impl Drop for FakeSubscription {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// One live subscription, as a test drives it.
pub(crate) struct FakeSubscriptionControl {
    pub events: mpsc::Sender<Result<NewsEvent, TransportError>>,
    pub status: watch::Sender<SubscriptionStatus>,
    /// How many times this subscription has been dropped, which is how a test sees the
    /// service let go of an account it no longer follows.
    pub drops: Arc<AtomicUsize>,
}

/// One operation a fake source can be held inside.
#[derive(Default)]
pub(crate) struct OperationBlock {
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
    pub(crate) async fn wait_started(&self) {
        tokio::time::timeout(Duration::from_secs(2), self.started.notified())
            .await
            .expect("the blocked operation should start");
    }

    pub(crate) async fn wait_dropped(&self) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !self.dropped.load(Ordering::SeqCst) {
                self.dropped_notify.notified().await;
            }
        })
        .await
        .expect("the blocked operation should be dropped");
    }
}

impl FakeSource {
    /// Queues one live subscription.
    pub(crate) fn subscribe(&self) -> FakeSubscriptionControl {
        let (events, receiver) = mpsc::channel(16);
        let (status, _) = watch::channel(SubscriptionStatus::Active);
        let drops = Arc::new(AtomicUsize::new(0));
        self.state()
            .setups
            .push_back(FakeSetup::Ready(FakeSubscription {
                events: receiver,
                status: status.clone(),
                drops: drops.clone(),
            }));
        FakeSubscriptionControl {
            events,
            status,
            drops,
        }
    }

    /// Queues a refusal, the way a session without news access answers.
    pub(crate) fn refuse_entitlement(&self) {
        self.refuse(KytrixServiceError::Server(ErrorCode::EntitlementRequired));
    }

    pub(crate) fn refuse(&self, error: KytrixServiceError) {
        self.state().setups.push_back(FakeSetup::Failed(error));
    }

    /// Queues a subscription setup that never answers.
    pub(crate) fn block(&self) -> Arc<OperationBlock> {
        let block = Arc::new(OperationBlock::default());
        self.state()
            .setups
            .push_back(FakeSetup::Blocked(block.clone()));
        block
    }

    /// How many subscriptions the service has asked for.
    pub(crate) fn subscriptions(&self) -> usize {
        self.state().subscriptions
    }

    fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl NewsSource for FakeSource {
    type Subscription = FakeSubscription;

    async fn subscribe_news(&self) -> Result<Self::Subscription, KytrixServiceError> {
        let setup = {
            let mut state = self.state();
            state.subscriptions += 1;
            state
                .setups
                .pop_front()
                .unwrap_or(FakeSetup::Failed(KytrixServiceError::Closed))
        };
        match setup {
            FakeSetup::Ready(subscription) => Ok(subscription),
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

impl NewsEventSubscription for FakeSubscription {
    fn observe_status(&self) -> watch::Receiver<SubscriptionStatus> {
        self.status.subscribe()
    }

    async fn recv(&mut self) -> Result<NewsEvent, TransportError> {
        self.events
            .recv()
            .await
            .unwrap_or(Err(TransportError::Closed))
    }
}

/// What a pane asked of its sound, in the order it asked.
///
/// Whether a clip would be audible is the machine's business, so this answers for a device
/// that is always there and records the plays instead: what a pane decides is whether to ask.
#[derive(Default)]
pub(crate) struct RecordedSound {
    plays: AtomicUsize,
    /// The device a pane reads, which is there from the start: a sound that is not one the
    /// pane may be heard on is what the tests that say so send through it.
    available: AtomicBool,
}

impl RecordedSound {
    pub(crate) fn with_device() -> Arc<Self> {
        let sound = Arc::new(Self::default());
        sound.set_available(true);

        sound
    }

    /// How many plays the pane asked for.
    pub(crate) fn plays(&self) -> usize {
        self.plays.load(Ordering::SeqCst)
    }

    /// Says whether this sound is one a device could be heard on.
    pub(crate) fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::SeqCst);
    }
}

impl NotificationSound for Arc<RecordedSound> {
    fn play(&self) {
        self.plays.fetch_add(1, Ordering::SeqCst);
    }

    fn is_available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }
}

//! The fake session the indices tests answer through.
//!
//! It is the seam the service, the selector and the pane all read index data
//! through, so it is shared rather than copied: what the pane's tests need to
//! control is the same session the service's tests refuse, delay and close.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use kytrix_proto::types::v1 as proto_types;
use kytrix_service_provider::service::engine::{
    ErrorCode, FinancialIndexEvent, FinancialIndexTopic, KytrixServiceError, KytrixSessionState,
};
use network::transport::{error::TransportError, subscription::SubscriptionStatus};
use smol_str::{SmolStr, format_smolstr};
use tokio::sync::{Notify, mpsc, watch};

use crate::{
    model::{LocalizedFinancialIndexName, SelectedFinancialIndex},
    service::{FinancialIndexEventSubscription, FinancialIndexSource},
};

/// The account every fixture scope belongs to.
pub(crate) const ACCOUNT: &str = "test-account";

/// Lets a widget and its service run until what a test is waiting for has happened.
///
/// Two executors are involved: a widget sends its commands from GPUI's own, and the
/// service answers on the application runtime. So both are given a turn — the widget
/// first, because a command that has not been sent is a service with nothing to answer.
pub(crate) fn settle_until(
    cx: &mut gpui_kit::TestAppContext,
    what: &str,
    ready: impl Fn(&gpui_kit::App) -> bool,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        let handle = cx.update(runtime_handle);
        handle.block_on(async { tokio::task::yield_now().await });
        if cx.update(|cx| ready(cx)) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "{what} should happen");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The installed application runtime's handle.
fn runtime_handle(cx: &mut gpui_kit::App) -> tokio::runtime::Handle {
    widget_tokio_runtime::global::handle(cx).handle().clone()
}

pub(crate) fn topic(code: &str) -> FinancialIndexTopic {
    FinancialIndexTopic::from_code(code).expect("a test index code should be valid")
}

pub(crate) fn selected(code: &str) -> SelectedFinancialIndex {
    SelectedFinancialIndex {
        topic: topic(code),
        name: LocalizedFinancialIndexName {
            en: SmolStr::new(format_smolstr!("{code} English")),
            zh: SmolStr::new(format_smolstr!("{code} Chinese")),
        },
    }
}

pub(crate) fn event(
    topic: FinancialIndexTopic,
    latest: u32,
    timestamp: i64,
) -> FinancialIndexEvent {
    FinancialIndexEvent {
        topic,
        last_price: latest,
        open_price: 100,
        high_price: latest.max(100),
        low_price: latest.min(100),
        close_price: 100,
        is_trading: true,
        timestamp,
    }
}

pub(crate) fn session(limit: u32) -> KytrixSessionState {
    KytrixSessionState {
        entitlement_revision: 1,
        plan_code: proto_types::EntitlementPlanCode::Free,
        entitlements: BTreeMap::from([(
            "financial_indices".to_owned(),
            proto_types::EntitlementFeatureValue {
                value: Some(proto_types::entitlement_feature_value::Value::Limit(limit)),
            },
        )]),
        catalog: Vec::new(),
        next_change_at: None,
    }
}

/// A manifest of two categories, each with one index.
pub(crate) fn manifest() -> proto_types::FinancialIndices {
    proto_types::FinancialIndices {
        categories: vec![
            category("asia", "Asia", "亚洲"),
            category("america", "America", "美洲"),
        ],
        indices: vec![
            index("HSI", "asia", "Hang Seng Index", "恒生指数"),
            index("SPX", "america", "S&P 500", "标普 500"),
        ],
    }
}

fn category(code: &str, en: &str, zh: &str) -> proto_types::FinancialIndexCategory {
    proto_types::FinancialIndexCategory {
        code: code.to_owned(),
        name: Some(localized(en, zh)),
    }
}

fn index(code: &str, category: &str, en: &str, zh: &str) -> proto_types::FinancialIndexDefinition {
    proto_types::FinancialIndexDefinition {
        code: code.to_owned(),
        category: category.to_owned(),
        name: Some(localized(en, zh)),
        // A protobuf field is a `String`, so the fixture builds one.
        request_code: format!(".{code}"),
    }
}

fn localized(en: &str, zh: &str) -> proto_types::FinancialIndexLocalizedName {
    proto_types::FinancialIndexLocalizedName {
        en: en.to_owned(),
        zh: zh.to_owned(),
    }
}

#[derive(Clone)]
pub(crate) struct FakeSource {
    state: Arc<Mutex<FakeState>>,
    session: watch::Sender<Option<KytrixSessionState>>,
}

#[derive(Default)]
struct FakeState {
    setups: VecDeque<FakeSetup>,
    subscribed: Vec<Vec<FinancialIndexTopic>>,
    manifest: proto_types::FinancialIndices,
    manifest_calls: usize,
}

/// What the next `subscribe_financial_index` call finds.
enum FakeSetup {
    /// A live subscription, and the drop counter of the subscription it replaces —
    /// which the service is expected to have released already.
    Ready(FakeSubscription, Option<Arc<AtomicUsize>>),
    Failed(KytrixServiceError),
    /// A setup that never answers, so the caller's cancellation is the only way out.
    Blocked(Arc<OperationBlock>),
}

/// One live subscription inside the fake source.
///
/// It is named by the crate because it is the associated type the source's own
/// seam publishes it as.
pub(crate) struct FakeSubscription {
    events: mpsc::Receiver<Result<FinancialIndexEvent, TransportError>>,
    status: watch::Sender<SubscriptionStatus>,
    drops: Arc<AtomicUsize>,
    /// Held when the venue is to take its time letting go of this subscription.
    release: Option<std::sync::mpsc::Receiver<()>>,
}

impl Drop for FakeSubscription {
    fn drop(&mut self) {
        // A venue does not necessarily let go of a subscription the instant the client
        // does, which is exactly why a replacement waits for the release instead of
        // assuming it: this is the moment a test can hold a replacement at.
        if let Some(release) = self.release.take() {
            let _ = release.recv_timeout(Duration::from_secs(2));
        }
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// One live subscription, as a test drives it.
pub(crate) struct FakeSubscriptionControl {
    pub events: mpsc::Sender<Result<FinancialIndexEvent, TransportError>>,
    pub status: watch::Sender<SubscriptionStatus>,
    /// How many times this subscription has been dropped, which is what a
    /// replacement has to have happened before.
    pub drops: Arc<AtomicUsize>,
    release: Option<std::sync::mpsc::Sender<()>>,
}

impl FakeSubscriptionControl {
    /// Lets the venue let go of this subscription.
    pub(crate) fn release(&self) {
        if let Some(release) = self.release.as_ref() {
            let _ = release.send(());
        }
    }
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

impl Default for FakeSource {
    fn default() -> Self {
        let (session, _) = watch::channel(None);
        Self {
            state: Arc::new(Mutex::new(FakeState::default())),
            session,
        }
    }
}

impl FakeSource {
    /// A source holding a session that may follow `limit` indices.
    pub(crate) fn with_session(limit: u32) -> Self {
        let source = Self::default();
        source.session.send_replace(Some(session(limit)));
        source
    }

    /// Queues one live subscription.
    pub(crate) fn subscribe(&self) -> FakeSubscriptionControl {
        self.subscribe_with(None, None)
    }

    /// Queues a subscription the venue holds on to until the test releases it, which
    /// is what makes a replacement's ordering observable rather than a matter of luck.
    pub(crate) fn subscribe_holding(&self) -> FakeSubscriptionControl {
        let (release, held) = std::sync::mpsc::channel();
        self.subscribe_with(None, Some((release, held)))
    }

    /// Queues a subscription the service may only reach once `previous` — the
    /// subscription it replaces — has been dropped.
    pub(crate) fn subscribe_after_drop(
        &self,
        previous: Option<Arc<AtomicUsize>>,
    ) -> FakeSubscriptionControl {
        self.subscribe_with(previous, None)
    }

    fn subscribe_with(
        &self,
        previous: Option<Arc<AtomicUsize>>,
        release: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    ) -> FakeSubscriptionControl {
        let (events, receiver) = mpsc::channel(16);
        let (status, _) = watch::channel(SubscriptionStatus::Active);
        let drops = Arc::new(AtomicUsize::new(0));
        let (release, held) = match release {
            Some((release, held)) => (Some(release), Some(held)),
            None => (None, None),
        };
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .setups
            .push_back(FakeSetup::Ready(
                FakeSubscription {
                    events: receiver,
                    status: status.clone(),
                    drops: drops.clone(),
                    release: held,
                },
                previous,
            ));
        FakeSubscriptionControl {
            events,
            status,
            drops,
            release,
        }
    }

    /// Queues a subscription the session refuses, the way a seat over the account's
    /// index limit is refused.
    pub(crate) fn refuse_selection(&self) {
        self.refuse(KytrixServiceError::Server(
            ErrorCode::FinancialIndexTopicLimitExceeded,
        ));
    }

    pub(crate) fn refuse(&self, error: KytrixServiceError) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .setups
            .push_back(FakeSetup::Failed(error));
    }

    /// Queues a subscription setup that never answers.
    pub(crate) fn block(&self) -> Arc<OperationBlock> {
        let block = Arc::new(OperationBlock::default());
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .setups
            .push_back(FakeSetup::Blocked(block.clone()));
        block
    }

    pub(crate) fn set_manifest(&self, manifest: proto_types::FinancialIndices) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .manifest = manifest;
    }

    /// The topics every subscription has been asked for, in order.
    pub(crate) fn subscribed(&self) -> Vec<Vec<FinancialIndexTopic>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .subscribed
            .clone()
    }

    pub(crate) fn manifest_calls(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .manifest_calls
    }
}

impl FinancialIndexSource for FakeSource {
    type Subscription = FakeSubscription;

    fn session_state(&self) -> watch::Receiver<Option<KytrixSessionState>> {
        self.session.subscribe()
    }

    async fn fetch_manifest(&self) -> Result<proto_types::FinancialIndices, KytrixServiceError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.manifest_calls += 1;
        Ok(state.manifest.clone())
    }

    async fn subscribe_financial_index(
        &self,
        topics: Arc<[FinancialIndexTopic]>,
    ) -> Result<Self::Subscription, KytrixServiceError> {
        let setup = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.subscribed.push(topics.to_vec());
            state
                .setups
                .pop_front()
                .unwrap_or(FakeSetup::Failed(KytrixServiceError::Closed))
        };
        match setup {
            FakeSetup::Ready(subscription, previous) => {
                // The service counts every topic a session subscribes to against one
                // index limit, so a subscription that opens while the one it replaces
                // is still live is the refusal a real session answers.
                if previous.is_some_and(|drops| drops.load(Ordering::SeqCst) == 0) {
                    Err(KytrixServiceError::Server(
                        ErrorCode::FinancialIndexTopicLimitExceeded,
                    ))
                } else {
                    Ok(subscription)
                }
            }
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

impl FinancialIndexEventSubscription for FakeSubscription {
    fn observe_status(&self) -> watch::Receiver<SubscriptionStatus> {
        self.status.subscribe()
    }

    async fn recv(&mut self) -> Result<FinancialIndexEvent, TransportError> {
        self.events
            .recv()
            .await
            .unwrap_or(Err(TransportError::Closed))
    }
}

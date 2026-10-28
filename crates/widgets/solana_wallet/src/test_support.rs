//! The fake Solana wallet the Solana wallet monitor tests answer through.
//!
//! It is the seam both the service and the pane read Solana wallets through, so it is shared
//! rather than copied: what a pane's tests need to control is the same session the
//! service's tests take, refuse and hold, and the same subscription they hand transfers
//! to.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use gpui_kit::{App, TestAppContext};
use kytrix_service_provider::service::{
    engine::{KytrixServiceError, SolanaWalletTransfer},
    solana_wallet::{
        SolanaWalletApiError, SolanaWalletMutationOutcome, SolanaWalletSelection,
        SolanaWalletSelectionStatus, SolanaWalletSelectionsResponse,
    },
};
use network::transport::{error::TransportError, subscription::SubscriptionStatus};
use smol_str::SmolStr;
use tokio::sync::{Notify, mpsc, watch};

use crate::{
    model::{TransferSnapshot, TransferState},
    service::{SolanaWalletEventSubscription, SolanaWalletSession, SolanaWalletSource},
};

/// The account every fixture scope belongs to.
pub(crate) const ACCOUNT: &str = "test-account";

/// Lets a widget and its service run until what a test is waiting for has happened.
///
/// Two executors are involved: a widget sends its commands from GPUI's own, and the
/// service answers on the application runtime. So both are given a turn — the widget
/// first, because a command that has not been sent is a service with nothing to answer.
pub(crate) fn settle_until(cx: &mut TestAppContext, what: &str, ready: impl Fn(&App) -> bool) {
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
fn runtime_handle(cx: &mut App) -> tokio::runtime::Handle {
    widget_tokio_runtime::global::handle(cx).handle().clone()
}

/// One reported transfer.
pub(crate) fn transfer(event_id: &str, timestamp: i64) -> SolanaWalletTransfer {
    SolanaWalletTransfer {
        event_id: SmolStr::new(event_id),
        from_address: SmolStr::new_static("So11111111111111111111111111111111111111112"),
        to_address: SmolStr::new_static("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
        amount: SmolStr::new_static("1.25"),
        token_symbol: SmolStr::new_static("SOL"),
        timestamp,
    }
}

/// One monitored address.
pub(crate) fn selection(address: &str) -> SolanaWalletSelection {
    selection_at(address, 1)
}

/// One monitored address, last changed at `updated_at`.
///
/// The list is read in the account's recency order, so a test that cares which of two
/// addresses reads first says when each was last changed.
pub(crate) fn selection_at(address: &str, updated_at: i64) -> SolanaWalletSelection {
    SolanaWalletSelection {
        address: SmolStr::new(address),
        status: SolanaWalletSelectionStatus::Active,
        created_at: 1,
        updated_at,
    }
}

/// What a read of the monitored addresses answers with.
pub(crate) fn selections(
    selections: Vec<SolanaWalletSelection>,
    limit: usize,
) -> SolanaWalletSelectionsResponse {
    SolanaWalletSelectionsResponse {
        selections,
        solana_wallet_monitor_address_limit: limit,
    }
}

/// A feed a test grows the way a subscription does.
///
/// Each snapshot shares the transfers it inherited from the one before it, which is what
/// makes a later report an edit to a list rather than a new set of rows: a test that built
/// every snapshot from scratch would describe a feed whose transfers are all new.
pub(crate) struct FakeFeed {
    state: TransferState,
}

impl FakeFeed {
    pub(crate) fn new() -> Self {
        Self {
            state: TransferState::new(),
        }
    }

    /// Retains one reported transfer and hands back the feed it produced.
    pub(crate) fn push(&mut self, event_id: &str, timestamp: i64) -> TransferSnapshot {
        self.state.push(transfer(event_id, timestamp));
        self.snapshot()
    }

    pub(crate) fn snapshot(&self) -> TransferSnapshot {
        self.state.snapshot()
    }
}

#[derive(Clone, Default)]
pub(crate) struct FakeSource {
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
    /// An acquisition that never answers, so the caller's cancellation is the only way
    /// out.
    Blocked(Arc<OperationBlock>),
}

impl FakeSource {
    /// Queues a session for `account`, which is what the next acquisition finds.
    pub(crate) fn session(&self, account: &str) -> FakeSession {
        let session = FakeSession::new(account);
        self.state()
            .sessions
            .push_back(FakeSetup::Ready(session.clone()));
        session
    }

    /// Queues a refusal in place of a session.
    pub(crate) fn refuse(&self, error: KytrixServiceError) {
        self.state().sessions.push_back(FakeSetup::Failed(error));
    }

    /// Queues an acquisition that never answers.
    pub(crate) fn block(&self) -> Arc<OperationBlock> {
        let block = Arc::new(OperationBlock::default());
        self.state()
            .sessions
            .push_back(FakeSetup::Blocked(block.clone()));
        block
    }

    /// How many sessions the service has asked for.
    pub(crate) fn sessions(&self) -> usize {
        self.state().acquired.len()
    }

    /// The accounts the service has asked for a session of, in the order it asked.
    pub(crate) fn requested(&self) -> Vec<SmolStr> {
        self.state().acquired.clone()
    }

    fn state(&self) -> MutexGuard<'_, FakeSourceState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SolanaWalletSource for FakeSource {
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
///
/// The account is held outside the shared state because it never changes and a session's
/// own `account_id` is a borrow of it.
#[derive(Clone)]
pub(crate) struct FakeSession {
    account_id: SmolStr,
    state: Arc<Mutex<FakeSessionState>>,
}

pub(crate) struct FakeSessionState {
    pub subscriptions: VecDeque<Result<FakeSubscription, KytrixServiceError>>,
    pub selections: VecDeque<Result<SolanaWalletSelectionsResponse, SolanaWalletApiError>>,
    pub adds: VecDeque<SolanaWalletMutationOutcome<SolanaWalletSelection>>,
    pub deletes: VecDeque<SolanaWalletMutationOutcome<()>>,
    pub subscribes: usize,
    pub fetches: usize,
    pub add_calls: usize,
    pub delete_calls: usize,
    /// A change that never answers, held for the next mutation that is asked for.
    pub blocked_mutation: Option<Arc<OperationBlock>>,
    pub blocked_selections: Option<Arc<OperationBlock>>,
}

impl FakeSession {
    fn new(account_id: &str) -> Self {
        Self {
            account_id: SmolStr::new(account_id),
            state: Arc::new(Mutex::new(FakeSessionState {
                subscriptions: VecDeque::new(),
                selections: VecDeque::new(),
                adds: VecDeque::new(),
                deletes: VecDeque::new(),
                subscribes: 0,
                fetches: 0,
                add_calls: 0,
                delete_calls: 0,
                blocked_mutation: None,
                blocked_selections: None,
            })),
        }
    }

    /// Queues one live subscription and hands back the knobs that drive it.
    pub(crate) fn subscribe(&self) -> FakeSubscriptionControl {
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
    pub(crate) fn refuse_subscription(&self, error: KytrixServiceError) {
        self.state().subscriptions.push_back(Err(error));
    }

    /// Queues what a read of the monitored addresses answers with.
    pub(crate) fn answer_selections(&self, response: SolanaWalletSelectionsResponse) {
        self.state().selections.push_back(Ok(response));
    }

    /// Queues a read the account is refused.
    pub(crate) fn refuse_selections(&self, error: SolanaWalletApiError) {
        self.state().selections.push_back(Err(error));
    }

    /// Queues a change that never answers.
    pub(crate) fn block_mutation(&self) -> Arc<OperationBlock> {
        let block = Arc::new(OperationBlock::default());
        self.state().blocked_mutation = Some(block.clone());
        block
    }

    /// Holds the next read of the monitored addresses open.
    ///
    /// A read that never answers is what a pane waiting for one is: it is the window in which
    /// a pane has a live stream and still does not know what it is following.
    pub(crate) fn block_selections(&self) -> Arc<OperationBlock> {
        let block = Arc::new(OperationBlock::default());
        self.state().blocked_selections = Some(block.clone());
        block
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, FakeSessionState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SolanaWalletSession for FakeSession {
    type Subscription = FakeSubscription;

    fn account_id(&self) -> &str {
        &self.account_id
    }

    async fn subscribe_solana_wallet(&self) -> Result<Self::Subscription, KytrixServiceError> {
        let mut state = self.state();
        state.subscribes += 1;
        state
            .subscriptions
            .pop_front()
            .unwrap_or(Err(KytrixServiceError::Closed))
    }

    async fn fetch_selections(
        &self,
    ) -> Result<SolanaWalletSelectionsResponse, SolanaWalletApiError> {
        let (response, blocked) = {
            let mut state = self.state();
            state.fetches += 1;
            (
                state
                    .selections
                    .pop_front()
                    .unwrap_or(Err(SolanaWalletApiError::NotReady)),
                state.blocked_selections.take(),
            )
        };
        block_operation(blocked).await;
        response
    }

    async fn add_selection(&self, _: &str) -> SolanaWalletMutationOutcome<SolanaWalletSelection> {
        let (outcome, blocked) = {
            let mut state = self.state();
            state.add_calls += 1;
            (
                state
                    .adds
                    .pop_front()
                    .unwrap_or(SolanaWalletMutationOutcome::Rejected(
                        SolanaWalletApiError::NotReady,
                    )),
                state.blocked_mutation.take(),
            )
        };
        block_operation(blocked).await;
        outcome
    }

    async fn delete_selection(&self, _: &str) -> SolanaWalletMutationOutcome<()> {
        let (outcome, blocked) = {
            let mut state = self.state();
            state.delete_calls += 1;
            (
                state
                    .deletes
                    .pop_front()
                    .unwrap_or(SolanaWalletMutationOutcome::Rejected(
                        SolanaWalletApiError::NotReady,
                    )),
                state.blocked_mutation.take(),
            )
        };
        block_operation(blocked).await;
        outcome
    }
}

/// Holds an operation inside the session for as long as the test wants it held.
async fn block_operation(blocked: Option<Arc<OperationBlock>>) {
    let Some(block) = blocked else {
        return;
    };
    let _guard = OperationGuard {
        block: block.clone(),
    };
    block.started.notify_one();
    block.released.notified().await;
}

/// One live subscription inside the fake session.
pub(crate) struct FakeSubscription {
    events: mpsc::Receiver<Result<SolanaWalletTransfer, TransportError>>,
    status: watch::Sender<SubscriptionStatus>,
    drops: Arc<AtomicUsize>,
    /// The status this subscription takes on as it hands the next event over, which is how
    /// a test puts a credential change and a transfer on the wire at the same moment.
    fail_on_delivery: Arc<Mutex<Option<TransportError>>>,
}

impl Drop for FakeSubscription {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// One live subscription, as a test drives it.
pub(crate) struct FakeSubscriptionControl {
    pub events: mpsc::Sender<Result<SolanaWalletTransfer, TransportError>>,
    pub status: watch::Sender<SubscriptionStatus>,
    /// How many times this subscription has been dropped, which is how a test sees the
    /// service let go of an account it no longer follows.
    pub drops: Arc<AtomicUsize>,
    fail_on_delivery: Arc<Mutex<Option<TransportError>>>,
}

impl FakeSubscriptionControl {
    /// Fails the subscription as it hands its next event over.
    ///
    /// A credential change reaches a subscription as a status of its own, and a transfer
    /// that was already on the wire is delivered beside it rather than after it. This is
    /// what a test uses to put those two on the wire together.
    pub(crate) fn fail_on_delivery(&self, error: TransportError) {
        *self
            .fail_on_delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
    }
}

impl SolanaWalletEventSubscription for FakeSubscription {
    fn observe_status(&self) -> watch::Receiver<SubscriptionStatus> {
        self.status.subscribe()
    }

    async fn recv(&mut self) -> Result<SolanaWalletTransfer, TransportError> {
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

/// One operation a fake can be held inside.
///
/// Holding one is how a test pins the window an operation is open in: a read that is held
/// is a read that has not answered yet, so what a pane reports while it is in flight is what
/// the test is looking at rather than a race against the answer.
#[derive(Default)]
pub(crate) struct OperationBlock {
    started: Notify,
    released: Notify,
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

    /// Lets the operation it holds answer.
    ///
    /// A held read answers with what the session was told to answer, so a test that waits
    /// for one to leave the window it pinned does not have to hand it the answer again.
    pub(crate) fn release(&self) {
        self.released.notify_one();
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

    /// Whether the operation has been let go, for a test that turns its own executor rather
    /// than parking on this one.
    pub(crate) fn is_dropped(&self) -> bool {
        self.dropped.load(Ordering::SeqCst)
    }
}

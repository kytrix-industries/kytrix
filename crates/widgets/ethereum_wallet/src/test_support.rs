use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use kytrix_service_provider::service::{
    engine::{EthereumWalletTransfer, KytrixServiceError},
    ethereum_wallet::{
        EthereumWalletApiError, EthereumWalletMutationOutcome, EthereumWalletSelection,
        EthereumWalletSelectionsResponse,
    },
};
use network::transport::{error::TransportError, subscription::SubscriptionStatus};
use smol_str::SmolStr;
use tokio::sync::{Notify, mpsc, watch};

use crate::{
    model::{TransferSnapshot, TransferState},
    service::{EthereumWalletEventSubscription, EthereumWalletSession, EthereumWalletSource},
};

pub(crate) const ACCOUNT: &str = "test-account";

pub(crate) fn transfer(event_id: &str, timestamp: i64) -> EthereumWalletTransfer {
    EthereumWalletTransfer {
        event_id: SmolStr::new(event_id),
        from_address: SmolStr::new_static("0x0123456789abcdef0123456789abcdef01234567"),
        to_address: SmolStr::new_static("0x89abcdef0123456789abcdef0123456789abcdef"),
        amount: SmolStr::new_static("1.25"),
        token_symbol: SmolStr::new_static("ETH"),
        timestamp,
    }
}

pub(crate) fn selection(address: &str) -> EthereumWalletSelection {
    selection_at(address, 1)
}

pub(crate) fn selection_at(address: &str, created_at: i64) -> EthereumWalletSelection {
    EthereumWalletSelection {
        address: SmolStr::new(address),
        created_at,
    }
}

pub(crate) fn selections(
    selections: Vec<EthereumWalletSelection>,
    limit: usize,
) -> EthereumWalletSelectionsResponse {
    EthereumWalletSelectionsResponse {
        selections,
        ethereum_wallet_monitor_address_limit: limit,
    }
}

pub(crate) struct FakeFeed {
    state: TransferState,
}

impl FakeFeed {
    pub(crate) fn new() -> Self {
        Self {
            state: TransferState::new(),
        }
    }

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

enum FakeSetup {
    Ready(FakeSession),
    Failed(KytrixServiceError),
    Blocked(Arc<OperationBlock>),
}

impl FakeSource {
    pub(crate) fn session(&self, account: &str) -> FakeSession {
        let session = FakeSession::new(account);
        self.state()
            .sessions
            .push_back(FakeSetup::Ready(session.clone()));
        session
    }

    pub(crate) fn refuse(&self, error: KytrixServiceError) {
        self.state().sessions.push_back(FakeSetup::Failed(error));
    }

    pub(crate) fn block(&self) -> Arc<OperationBlock> {
        let block = Arc::new(OperationBlock::default());
        self.state()
            .sessions
            .push_back(FakeSetup::Blocked(block.clone()));
        block
    }

    pub(crate) fn sessions(&self) -> usize {
        self.state().acquired.len()
    }

    pub(crate) fn requested(&self) -> Vec<SmolStr> {
        self.state().acquired.clone()
    }

    fn state(&self) -> MutexGuard<'_, FakeSourceState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl EthereumWalletSource for FakeSource {
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
pub(crate) struct FakeSession {
    account_id: SmolStr,
    state: Arc<Mutex<FakeSessionState>>,
}

pub(crate) struct FakeSessionState {
    pub subscriptions: VecDeque<Result<FakeSubscription, KytrixServiceError>>,
    pub selections: VecDeque<Result<EthereumWalletSelectionsResponse, EthereumWalletApiError>>,
    pub adds: VecDeque<EthereumWalletMutationOutcome<EthereumWalletSelection>>,
    pub deletes: VecDeque<EthereumWalletMutationOutcome<()>>,
    pub subscribes: usize,
    pub fetches: usize,
    pub add_calls: usize,
    pub delete_calls: usize,
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

    pub(crate) fn refuse_subscription(&self, error: KytrixServiceError) {
        self.state().subscriptions.push_back(Err(error));
    }

    pub(crate) fn answer_selections(&self, response: EthereumWalletSelectionsResponse) {
        self.state().selections.push_back(Ok(response));
    }

    pub(crate) fn refuse_selections(&self, error: EthereumWalletApiError) {
        self.state().selections.push_back(Err(error));
    }

    pub(crate) fn block_mutation(&self) -> Arc<OperationBlock> {
        let block = Arc::new(OperationBlock::default());
        self.state().blocked_mutation = Some(block.clone());
        block
    }

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

impl EthereumWalletSession for FakeSession {
    type Subscription = FakeSubscription;

    fn account_id(&self) -> &str {
        &self.account_id
    }

    async fn subscribe_ethereum_wallet(&self) -> Result<Self::Subscription, KytrixServiceError> {
        let mut state = self.state();
        state.subscribes += 1;
        state
            .subscriptions
            .pop_front()
            .unwrap_or(Err(KytrixServiceError::Closed))
    }

    async fn fetch_selections(
        &self,
    ) -> Result<EthereumWalletSelectionsResponse, EthereumWalletApiError> {
        let (response, blocked) = {
            let mut state = self.state();
            state.fetches += 1;
            (
                state
                    .selections
                    .pop_front()
                    .unwrap_or(Err(EthereumWalletApiError::NotReady)),
                state.blocked_selections.take(),
            )
        };
        block_operation(blocked).await;
        response
    }

    async fn add_selection(
        &self,
        _: &str,
    ) -> EthereumWalletMutationOutcome<EthereumWalletSelection> {
        let (outcome, blocked) = {
            let mut state = self.state();
            state.add_calls += 1;
            (
                state
                    .adds
                    .pop_front()
                    .unwrap_or(EthereumWalletMutationOutcome::Rejected(
                        EthereumWalletApiError::NotReady,
                    )),
                state.blocked_mutation.take(),
            )
        };
        block_operation(blocked).await;
        outcome
    }

    async fn delete_selection(&self, _: &str) -> EthereumWalletMutationOutcome<()> {
        let (outcome, blocked) = {
            let mut state = self.state();
            state.delete_calls += 1;
            (
                state
                    .deletes
                    .pop_front()
                    .unwrap_or(EthereumWalletMutationOutcome::Rejected(
                        EthereumWalletApiError::NotReady,
                    )),
                state.blocked_mutation.take(),
            )
        };
        block_operation(blocked).await;
        outcome
    }
}

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

pub(crate) struct FakeSubscription {
    events: mpsc::Receiver<Result<EthereumWalletTransfer, TransportError>>,
    status: watch::Sender<SubscriptionStatus>,
    drops: Arc<AtomicUsize>,
    fail_on_delivery: Arc<Mutex<Option<TransportError>>>,
}

impl Drop for FakeSubscription {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

pub(crate) struct FakeSubscriptionControl {
    pub events: mpsc::Sender<Result<EthereumWalletTransfer, TransportError>>,
    pub status: watch::Sender<SubscriptionStatus>,
    pub drops: Arc<AtomicUsize>,
    fail_on_delivery: Arc<Mutex<Option<TransportError>>>,
}

impl FakeSubscriptionControl {
    pub(crate) fn fail_on_delivery(&self, error: TransportError) {
        *self
            .fail_on_delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
    }
}

impl EthereumWalletEventSubscription for FakeSubscription {
    fn observe_status(&self) -> watch::Receiver<SubscriptionStatus> {
        self.status.subscribe()
    }

    async fn recv(&mut self) -> Result<EthereumWalletTransfer, TransportError> {
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

    pub(crate) fn is_dropped(&self) -> bool {
        self.dropped.load(Ordering::SeqCst)
    }
}

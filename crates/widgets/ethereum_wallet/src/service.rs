//! The Ethereum wallet protocol, its background service, and the seams a test drives it through.
//!
//! # One account, two things a pane does with it
//!
//! A pane showing this widget follows the transfers its monitored addresses make, and it
//! manages the list of addresses being watched. Both belong to the same account and both
//! are answered with the same session — the transfer stream is subscribed with it and
//! every change to the list is sent with it — so one service owns both and one run loop
//! serializes them. A separate service for the dialog would take a second session for the
//! same account and hold it after the dialog closed.
//!
//! # The session is the service's, the list is the pane's
//!
//! Nothing about the addresses is kept here. The list a pane shows is the last answer it
//! was given, and the projection of a confirmed change is [`SelectionData`]'s, so the only
//! thing this service knows about an account is which session it holds for it. What it
//! does own is the *session*: a change is only made through the session the stream was
//! opened with, which is what keeps a dialog acting on an account the pane has left from
//! changing anything — such a request is refused with the session it disagrees with.
//!
//! # Readiness
//!
//! `Ready` means the subscription is live, and the transfers are published before it:
//! being live is what makes the transfers already on the wire the pane's content,
//! including none at all. A subscription that never came up publishes why instead, and
//! the pane reads the addresses through the session the service still holds.
//!
//! # Transfers are account data
//!
//! A transfer names two Ethereum wallets and an amount, and which of them an Ethereum wallet is watching is
//! the account's business. A transfer that was already on the wire when the credentials
//! behind the subscription changed therefore belongs to an account the pane is no longer
//! reading as, and is dropped rather than shown: the subscription's own status is what
//! says so, and it is checked before a transfer is converted.

use std::future::Future;

use kytrix_service_provider::service::{
    connector::KytrixServiceConnector,
    engine::{EthereumWalletTransfer, KytrixAccountSession, KytrixServiceError},
    ethereum_wallet::{
        EthereumWalletApiError, EthereumWalletMutationOutcome, EthereumWalletSelection,
        EthereumWalletSelectionsResponse,
    },
};
use network::transport::{
    error::TransportError,
    subscription::{Subscription, SubscriptionStatus},
};
use smol_str::SmolStr;
use tokio::{sync::watch, time::Instant};
use widget::{
    error::LocalizedError,
    protocol::WidgetProtocol,
    runtime::{ServiceContext, WidgetService},
    stream::{StreamPublishError, StreamPublisher},
};

use super::model::{SelectionData, TransferSnapshot, TransferState};

/// What a pane asks its service to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum EthereumWalletRequest {
    /// Follows an account's transfers, keeping the stream that already follows it.
    Load(EthereumWalletScope),
    /// Reopens an account's stream, for one whose stream has failed.
    Retry(EthereumWalletScope),
    /// Follows nothing, because nobody is signed in.
    Stop,
    /// Reads the addresses the account monitors.
    FetchSelections(EthereumWalletScope),
    /// Monitors one more address.
    AddSelection {
        scope: EthereumWalletScope,
        address: SmolStr,
    },
    /// Stops monitoring one address.
    DeleteSelection {
        scope: EthereumWalletScope,
        address: SmolStr,
    },
}

/// What the service answered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum EthereumWalletResponse {
    /// The account's stream is running.
    StreamStarted,
    /// Nothing is followed.
    Stopped,
    /// The addresses the account monitors.
    Selections(SelectionData),
    /// What became of an address the pane asked to monitor.
    Added(EthereumWalletMutationOutcome<EthereumWalletSelection>),
    /// What became of an address the pane asked to stop monitoring.
    Deleted(EthereumWalletMutationOutcome<()>),
}

/// The account whose transfers a stream carries.
///
/// Every request names one, including the ones that change the address list: the pane's
/// account can change while a dialog is still open, and an answer about the account the
/// pane has left is not one the pane may act on.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct EthereumWalletScope {
    account_id: SmolStr,
}

impl EthereumWalletScope {
    /// The scope an account follows, or `None` when nobody is signed in.
    pub(super) fn new(account_id: Option<&SmolStr>) -> Option<Self> {
        let account_id = account_id.filter(|account_id| !account_id.is_empty())?;
        Some(Self {
            account_id: account_id.clone(),
        })
    }

    pub(super) fn account_id(&self) -> &str {
        &self.account_id
    }
}

/// What a subscription the pane holds is doing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum EthereumWalletStatus {
    /// The subscription is being established.
    Connecting,
    /// The subscription is live.
    Ready,
    /// The subscription ended; the pane is finished until the user recovers it.
    Failed(EthereumWalletStreamError),
}

/// Why a subscription stopped, or why it never started.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum EthereumWalletStreamError {
    /// The account's session could not be taken, or the subscription was refused.
    Service(KytrixServiceError),
    Transport(TransportError),
}

impl LocalizedError for EthereumWalletStreamError {
    fn localized_message(&self, cx: &gpui_kit::App) -> gpui_kit::SharedString {
        match self {
            Self::Service(error) => error.localized_message(cx),
            Self::Transport(error) => error.localized_message(cx),
        }
    }
}

/// One message of an account's stream.
#[derive(Clone, Debug)]
pub(super) enum EthereumWalletStreamMessage {
    Status(EthereumWalletStatus),
    /// Every transfer the subscription retains, newest first.
    Snapshot(TransferSnapshot),
}

pub(super) struct EthereumWalletProtocol;

impl WidgetProtocol for EthereumWalletProtocol {
    type Request = EthereumWalletRequest;
    type Response = EthereumWalletResponse;
    type StreamKey = EthereumWalletScope;
    type StreamMessage = EthereumWalletStreamMessage;
    type Error = EthereumWalletApiError;
}

/// The Ethereum wallet an account reads through.
///
/// The service is written against this seam rather than against the connector, so a test
/// can drive every state a session and a subscription can be in without one.
pub(super) trait EthereumWalletSource: Clone + Send + Sync + 'static {
    type Session: EthereumWalletSession;

    /// Takes the account's session, which answers both the stream and every request the
    /// pane makes about the addresses it monitors.
    fn acquire_session(
        &self,
        account_id: &str,
    ) -> impl Future<Output = Result<Self::Session, KytrixServiceError>> + Send;
}

/// One account's session.
pub(super) trait EthereumWalletSession: Clone + Send + Sync + 'static {
    type Subscription: EthereumWalletEventSubscription;

    /// The account this session is for, which is what a stream keyed by another one is
    /// checked against.
    fn account_id(&self) -> &str;

    fn subscribe_ethereum_wallet(
        &self,
    ) -> impl Future<Output = Result<Self::Subscription, KytrixServiceError>> + Send;

    fn fetch_selections(
        &self,
    ) -> impl Future<Output = Result<EthereumWalletSelectionsResponse, EthereumWalletApiError>> + Send;

    fn add_selection(
        &self,
        address: &str,
    ) -> impl Future<Output = EthereumWalletMutationOutcome<EthereumWalletSelection>> + Send;

    fn delete_selection(
        &self,
        address: &str,
    ) -> impl Future<Output = EthereumWalletMutationOutcome<()>> + Send;
}

/// One live transfer subscription.
pub(super) trait EthereumWalletEventSubscription: Send + 'static {
    fn observe_status(&self) -> watch::Receiver<SubscriptionStatus>;

    fn recv(
        &mut self,
    ) -> impl Future<Output = Result<EthereumWalletTransfer, TransportError>> + Send;
}

impl EthereumWalletSource for KytrixServiceConnector {
    type Session = KytrixAccountSession;

    async fn acquire_session(&self, account_id: &str) -> Result<Self::Session, KytrixServiceError> {
        self.acquire_account_session(account_id).await
    }
}

impl EthereumWalletSession for KytrixAccountSession {
    type Subscription = Subscription<EthereumWalletTransfer>;

    fn account_id(&self) -> &str {
        self.profile().account_id()
    }

    async fn subscribe_ethereum_wallet(&self) -> Result<Self::Subscription, KytrixServiceError> {
        KytrixAccountSession::subscribe_ethereum_wallet(self).await
    }

    async fn fetch_selections(
        &self,
    ) -> Result<EthereumWalletSelectionsResponse, EthereumWalletApiError> {
        KytrixAccountSession::fetch_ethereum_wallet_selections(self).await
    }

    async fn add_selection(
        &self,
        address: &str,
    ) -> EthereumWalletMutationOutcome<EthereumWalletSelection> {
        KytrixAccountSession::add_ethereum_wallet_selection(self, address).await
    }

    async fn delete_selection(&self, address: &str) -> EthereumWalletMutationOutcome<()> {
        KytrixAccountSession::delete_ethereum_wallet_selection(self, address).await
    }
}

impl EthereumWalletEventSubscription for Subscription<EthereumWalletTransfer> {
    fn observe_status(&self) -> watch::Receiver<SubscriptionStatus> {
        Subscription::observe_status(self)
    }

    async fn recv(&mut self) -> Result<EthereumWalletTransfer, TransportError> {
        Subscription::recv(self).await
    }
}

/// The service that follows one pane's Ethereum wallet.
pub(super) struct EthereumWalletService<S = KytrixServiceConnector> {
    source: S,
}

impl<S> EthereumWalletService<S> {
    pub(super) const fn new(source: S) -> Self {
        Self { source }
    }
}

/// The session the account a stream belongs to is answered with.
struct ActiveSession<S> {
    scope: EthereumWalletScope,
    session: S,
}

impl<S> WidgetService<EthereumWalletProtocol> for EthereumWalletService<S>
where
    S: EthereumWalletSource,
{
    async fn run(self, mut context: ServiceContext<EthereumWalletProtocol>) {
        let mut active: Option<ActiveSession<S::Session>> = None;

        while let Some(request) = context.next_rpc().await {
            let (mut reply, request) = request.into_parts();
            match request {
                EthereumWalletRequest::Stop => {
                    if let Some(active) = active.take() {
                        context.cancel_stream(&active.scope);
                    }
                    let _ = reply.respond(Ok(EthereumWalletResponse::Stopped));
                }
                request @ (EthereumWalletRequest::Load(_) | EthereumWalletRequest::Retry(_)) => {
                    let (scope, retry) = match request {
                        EthereumWalletRequest::Load(scope) => (scope, false),
                        EthereumWalletRequest::Retry(scope) => (scope, true),
                        EthereumWalletRequest::Stop
                        | EthereumWalletRequest::FetchSelections(_)
                        | EthereumWalletRequest::AddSelection { .. }
                        | EthereumWalletRequest::DeleteSelection { .. } => continue,
                    };
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }

                    // A stream the service is already holding for this account is the one
                    // that was asked for: what the pane is waiting for is the account, and
                    // nothing about the stream changes. A retry is the caller saying the
                    // stream it has is not one it wants, so it is replaced even when the
                    // account behind it is the same.
                    if !retry
                        && active.as_ref().is_some_and(|active| active.scope == scope)
                        && context.stream_generation(&scope).is_some()
                    {
                        let _ = reply.respond(Ok(EthereumWalletResponse::StreamStarted));
                        continue;
                    }

                    let acquire = self.source.acquire_session(scope.account_id());
                    tokio::pin!(acquire);
                    let session = tokio::select! {
                        biased;
                        () = reply.cancelled() => continue,
                        result = &mut acquire => match result {
                            Ok(session) => session,
                            Err(error) => {
                                let _ = reply.respond(Err(session_failure(error)));
                                continue;
                            }
                        },
                    };
                    // The stream is keyed by the account the pane asked for, so a session
                    // that turned out to be for another one is answered rather than
                    // streamed: a transfer is not data a pane may show under a name it
                    // does not belong to.
                    if session.account_id() != scope.account_id() {
                        let _ = reply.respond(Err(EthereumWalletApiError::AccountChanged));
                        continue;
                    }

                    if let Some(previous) = active.take() {
                        context.cancel_stream(&previous.scope);
                    }
                    let streaming = session.clone();
                    context.start_stream(scope.clone(), move |publisher| async move {
                        run_stream(streaming, publisher).await;
                    });
                    active = Some(ActiveSession {
                        scope: scope.clone(),
                        session,
                    });
                    if reply
                        .respond(Ok(EthereumWalletResponse::StreamStarted))
                        .is_err()
                    {
                        // Nobody is waiting for this stream any more, so it is nobody's
                        // stream: a pane that went away must not leave a subscription
                        // holding an account it is not reading.
                        context.cancel_stream(&scope);
                        active = None;
                    }
                }
                EthereumWalletRequest::FetchSelections(scope) => {
                    let session = match matching_session(&active, &scope) {
                        Ok(session) => session,
                        Err(error) => {
                            let _ = reply.respond(Err(error));
                            continue;
                        }
                    };
                    let fetch = session.fetch_selections();
                    tokio::pin!(fetch);
                    let response = tokio::select! {
                        biased;
                        () = reply.cancelled() => continue,
                        response = &mut fetch => match response {
                            Ok(response) => response,
                            Err(error) => {
                                let _ = reply.respond(Err(error));
                                continue;
                            }
                        },
                    };
                    let _ = reply.respond(Ok(EthereumWalletResponse::Selections(
                        SelectionData::new(response),
                    )));
                }
                EthereumWalletRequest::AddSelection { scope, address } => {
                    let session = match matching_session(&active, &scope) {
                        Ok(session) => session,
                        Err(error) => {
                            let _ = reply.respond(Err(error));
                            continue;
                        }
                    };
                    let add = session.add_selection(&address);
                    tokio::pin!(add);
                    let outcome = tokio::select! {
                        biased;
                        // A caller that went away takes its question with it. The change
                        // may or may not have been made, and nothing here decides which:
                        // the pane reads the list again instead of guessing, and this
                        // service holds no list that could be guessed wrong.
                        () = reply.cancelled() => continue,
                        outcome = &mut add => outcome,
                    };
                    let _ = reply.respond(Ok(EthereumWalletResponse::Added(outcome)));
                }
                EthereumWalletRequest::DeleteSelection { scope, address } => {
                    let session = match matching_session(&active, &scope) {
                        Ok(session) => session,
                        Err(error) => {
                            let _ = reply.respond(Err(error));
                            continue;
                        }
                    };
                    let delete = session.delete_selection(&address);
                    tokio::pin!(delete);
                    let outcome = tokio::select! {
                        biased;
                        () = reply.cancelled() => continue,
                        outcome = &mut delete => outcome,
                    };
                    let _ = reply.respond(Ok(EthereumWalletResponse::Deleted(outcome)));
                }
            }
        }
    }
}

/// The session a request about the addresses is answered with.
///
/// A request that names an account the service is not holding is refused with what is
/// wrong rather than answered from another account's session: not holding one at all is a
/// pane that asked too early, and holding a different one is a pane whose account changed
/// under it.
fn matching_session<'a, S>(
    active: &'a Option<ActiveSession<S>>,
    scope: &EthereumWalletScope,
) -> Result<&'a S, EthereumWalletApiError> {
    let Some(active) = active.as_ref() else {
        return Err(EthereumWalletApiError::NotReady);
    };
    if &active.scope != scope {
        return Err(EthereumWalletApiError::AccountChanged);
    }
    Ok(&active.session)
}

/// What a session that could not be taken means to the request that asked for it.
///
/// The four that name a condition of their own are named as themselves: an expired login,
/// a changed account, a timeout and a closed service all read differently to a user than
/// a service error does.
fn session_failure(error: KytrixServiceError) -> EthereumWalletApiError {
    match error {
        KytrixServiceError::Authentication(_) => EthereumWalletApiError::Authentication,
        KytrixServiceError::AccountChanged => EthereumWalletApiError::AccountChanged,
        KytrixServiceError::Timeout => EthereumWalletApiError::Timeout,
        KytrixServiceError::Closed => EthereumWalletApiError::Closed,
        error => EthereumWalletApiError::Service(error),
    }
}

/// Runs the account's subscription.
async fn run_stream<S>(session: S, publisher: StreamPublisher<EthereumWalletProtocol>)
where
    S: EthereumWalletSession,
{
    let mut published = EthereumWalletStatus::Connecting;
    if publish_status(&publisher, EthereumWalletStatus::Connecting)
        .await
        .is_err()
    {
        return;
    }

    let subscription = tokio::select! {
        biased;
        () = publisher.cancelled() => return,
        result = session.subscribe_ethereum_wallet() => result,
    };
    let mut subscription = match subscription {
        Ok(subscription) => subscription,
        Err(error) => {
            publish_failure(&publisher, EthereumWalletStreamError::Service(error)).await;
            return;
        }
    };

    let mut status = subscription.observe_status();
    let mut state = TransferState::new();
    let mut feed_published = false;

    // A subscription can already be live before its first change, so where it is now is
    // where the stream starts.
    let current = status.borrow_and_update().clone();
    if !apply_subscription_status(
        &publisher,
        &current,
        &state,
        &mut feed_published,
        &mut published,
    )
    .await
    {
        return;
    }

    loop {
        tokio::select! {
            biased;
            () = publisher.cancelled() => return,
            changed = status.changed() => {
                let next = if changed.is_err() {
                    SubscriptionStatus::Closed
                } else {
                    status.borrow_and_update().clone()
                };
                if !apply_subscription_status(
                    &publisher,
                    &next,
                    &state,
                    &mut feed_published,
                    &mut published,
                )
                .await
                {
                    return;
                }
            }
            event = subscription.recv() => {
                // A replaced stream is a stream whose account nobody is reading any
                // more, so what it delivers is not converted either.
                if publisher.is_cancelled() {
                    return;
                }
                // A transfer already on the wire belongs to the credentials the
                // subscription was opened with. If those are gone, so is the pane's
                // standing to read it, and the subscription's own status is what says so.
                let current = status.borrow_and_update().clone();
                if let Some(error) = ended_subscription(&current) {
                    publish_failure(&publisher, error).await;
                    return;
                }
                let transfer = match event {
                    Ok(transfer) => transfer,
                    Err(error) => {
                        publish_failure(&publisher, EthereumWalletStreamError::Transport(error)).await;
                        return;
                    }
                };
                if !state.push(transfer) {
                    continue;
                }
                if publisher
                    .publish_latest(EthereumWalletStreamMessage::Snapshot(state.snapshot()))
                    .is_err()
                {
                    return;
                }
                feed_published = true;
            }
        }
    }
}

/// Applies one subscription status, publishing what it means to the pane.
///
/// Returns whether the stream may keep running: a status that ended it has already
/// published why, and the task settles on the pane that will replace it.
async fn apply_subscription_status(
    publisher: &StreamPublisher<EthereumWalletProtocol>,
    status: &SubscriptionStatus,
    state: &TransferState,
    feed_published: &mut bool,
    published: &mut EthereumWalletStatus,
) -> bool {
    if let Some(error) = ended_subscription(status) {
        publish_failure(publisher, error).await;
        return false;
    }

    match status {
        // A closing subscription is on its way to a terminal status of its own, and that
        // is where the failure is reported from.
        SubscriptionStatus::Closing => true,
        SubscriptionStatus::Connecting | SubscriptionStatus::Reconnecting => {
            publish_change(publisher, EthereumWalletStatus::Connecting, published)
                .await
                .is_ok()
        }
        SubscriptionStatus::Active => {
            // The transfers are published with the readiness they come with, once: a pane
            // that becomes ready holds what the subscription holds, and a reconnect does
            // not make the transfers the pane already has new again.
            if !*feed_published {
                if publisher
                    .publish_latest(EthereumWalletStreamMessage::Snapshot(state.snapshot()))
                    .is_err()
                {
                    return false;
                }
                *feed_published = true;
            }
            publish_change(publisher, EthereumWalletStatus::Ready, published)
                .await
                .is_ok()
        }
        // Handled above, where the failure they carry is published.
        SubscriptionStatus::Closed | SubscriptionStatus::Failed(_) => false,
    }
}

/// Why a subscription is no longer one a transfer may be read from.
fn ended_subscription(status: &SubscriptionStatus) -> Option<EthereumWalletStreamError> {
    match status {
        SubscriptionStatus::Closed => {
            Some(EthereumWalletStreamError::Transport(TransportError::Closed))
        }
        SubscriptionStatus::Failed(error) => {
            Some(EthereumWalletStreamError::Transport(error.clone()))
        }
        SubscriptionStatus::Connecting
        | SubscriptionStatus::Active
        | SubscriptionStatus::Reconnecting
        | SubscriptionStatus::Closing => None,
    }
}

/// Publishes a status the pane has not been told yet.
async fn publish_status(
    publisher: &StreamPublisher<EthereumWalletProtocol>,
    status: EthereumWalletStatus,
) -> Result<(), StreamPublishError> {
    publisher
        .publish_reliable(EthereumWalletStreamMessage::Status(status))
        .await
}

/// Publishes a status change, if it is one.
async fn publish_change(
    publisher: &StreamPublisher<EthereumWalletProtocol>,
    status: EthereumWalletStatus,
    published: &mut EthereumWalletStatus,
) -> Result<(), StreamPublishError> {
    if *published == status {
        return Ok(());
    }
    publish_status(publisher, status.clone()).await?;
    *published = status;
    Ok(())
}

/// Publishes the failure a stream ended with, and holds the account it took.
///
/// A terminal failure is not retried: the pane asks for a new stream when the user takes
/// the recovery, and until then this task stays where it is so the framework still sees
/// the account as taken — and so the addresses the account monitors can still be read
/// and changed, which is the other half of what the session was taken for.
async fn publish_failure(
    publisher: &StreamPublisher<EthereumWalletProtocol>,
    error: EthereumWalletStreamError,
) {
    let _ = publish_status(publisher, EthereumWalletStatus::Failed(error)).await;
    publisher.cancelled().await;
}

#[cfg(test)]
mod tests {

    /// The handle of the runtime this test runs on.
    ///
    /// A test drives its subject on its own runtime rather than on a shared one. The
    /// clock these tests read is the runtime's: `#[tokio::test(start_paused = true)]`
    /// pauses it and `tokio::time::advance` moves it, and both only affect the runtime
    /// whose clock was paused. Work started on another runtime keeps waiting on real
    /// time while the test advances a clock it never reads.
    #[allow(
        clippy::disallowed_methods,
        reason = "a test drives its subject on its own runtime so the paused clock it advances is the one the subject reads"
    )]
    fn current_test_handle() -> &'static tokio::runtime::Handle {
        Box::leak(Box::new(tokio::runtime::Handle::current()))
    }
    use std::{sync::atomic::Ordering, time::Duration};

    use kytrix_service_provider::service::ethereum_wallet::EthereumWalletMutationUnknown;
    use widget::{
        rpc::RpcCallError,
        runtime::WidgetRuntime,
        stream::{LatestStream, ReliableStream},
    };

    use super::*;
    use crate::test_support::{ACCOUNT, FakeSource, selection, selections, transfer};

    fn account(name: &'static str) -> EthereumWalletScope {
        EthereumWalletScope::new(Some(&SmolStr::new_static(name)))
            .expect("an account should be a scope")
    }

    fn scope() -> EthereumWalletScope {
        account(ACCOUNT)
    }

    fn runtime(source: &FakeSource) -> WidgetRuntime<EthereumWalletProtocol> {
        WidgetRuntime::spawn(
            current_test_handle(),
            EthereumWalletService::new(source.clone()),
        )
    }

    async fn load(runtime: &WidgetRuntime<EthereumWalletProtocol>, scope: EthereumWalletScope) {
        let response = runtime
            .client()
            .call(EthereumWalletRequest::Load(scope))
            .await
            .expect("the stream should start")
            .into_value();
        assert_eq!(response, EthereumWalletResponse::StreamStarted);
    }

    async fn retry(runtime: &WidgetRuntime<EthereumWalletProtocol>, scope: EthereumWalletScope) {
        let response = runtime
            .client()
            .call(EthereumWalletRequest::Retry(scope))
            .await
            .expect("the stream should start again")
            .into_value();
        assert_eq!(response, EthereumWalletResponse::StreamStarted);
    }

    async fn fetch(
        runtime: &WidgetRuntime<EthereumWalletProtocol>,
        scope: EthereumWalletScope,
    ) -> Result<EthereumWalletResponse, RpcCallError<EthereumWalletApiError>> {
        runtime
            .client()
            .call(EthereumWalletRequest::FetchSelections(scope))
            .await
            .map(widget::rpc::RpcResponse::into_value)
    }

    fn addresses(response: &EthereumWalletResponse) -> Vec<&str> {
        let EthereumWalletResponse::Selections(data) = response else {
            panic!("the answer should be the addresses");
        };
        data.selections()
            .iter()
            .map(|selection| selection.address.as_str())
            .collect()
    }

    async fn next_reliable(
        reliable: &mut ReliableStream<EthereumWalletProtocol>,
        predicate: impl Fn(&EthereumWalletStreamMessage) -> bool,
    ) -> EthereumWalletStreamMessage {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let envelope = reliable
                    .recv()
                    .await
                    .expect("the reliable stream should stay open");
                if predicate(envelope.message().as_ref()) {
                    return envelope.message().as_ref().clone();
                }
            }
        })
        .await
        .expect("the expected reliable state should arrive")
    }

    async fn receive_status(
        reliable: &mut ReliableStream<EthereumWalletProtocol>,
        expected: EthereumWalletStatus,
    ) {
        let status = next_reliable(
            reliable,
            |message| matches!(message, EthereumWalletStreamMessage::Status(status) if *status == expected),
        )
        .await;
        assert!(matches!(status, EthereumWalletStreamMessage::Status(_)));
    }

    async fn receive_failure(
        reliable: &mut ReliableStream<EthereumWalletProtocol>,
    ) -> EthereumWalletStreamError {
        let failure = next_reliable(reliable, |message| {
            matches!(
                message,
                EthereumWalletStreamMessage::Status(EthereumWalletStatus::Failed(_))
            )
        })
        .await;
        let EthereumWalletStreamMessage::Status(EthereumWalletStatus::Failed(error)) = failure
        else {
            panic!("the stream should have reported a failure");
        };
        error
    }

    async fn next_feed(
        latest: &mut LatestStream<EthereumWalletProtocol>,
        predicate: impl Fn(&TransferSnapshot) -> bool,
    ) -> TransferSnapshot {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                latest
                    .changed()
                    .await
                    .expect("the latest stream should stay open");
                while let Some(envelope) = latest.next_unseen() {
                    if let EthereumWalletStreamMessage::Snapshot(snapshot) =
                        envelope.message().as_ref()
                        && predicate(snapshot)
                    {
                        return snapshot.clone();
                    }
                }
            }
        })
        .await
        .expect("the expected feed should arrive")
    }

    fn contents(snapshot: &TransferSnapshot) -> Vec<String> {
        snapshot
            .items()
            .iter()
            .map(|item| item.event_id().to_owned())
            .collect()
    }

    /// A running service with one account taken and its subscription live.
    async fn live(
        source: &FakeSource,
    ) -> (
        WidgetRuntime<EthereumWalletProtocol>,
        ReliableStream<EthereumWalletProtocol>,
        LatestStream<EthereumWalletProtocol>,
    ) {
        let mut runtime = runtime(source);
        let reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let latest = runtime.latest_stream();
        load(&runtime, scope()).await;
        (runtime, reliable, latest)
    }

    #[tokio::test]
    async fn the_stream_should_publish_transfers_and_call_itself_live() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let (runtime, mut reliable, mut latest) = live(&source).await;

        receive_status(&mut reliable, EthereumWalletStatus::Ready).await;
        // Readiness describes the transfers that came with it, not a feed still on its
        // way: by the time the pane is told the subscription is live, the feed is on the
        // stream.
        let live = latest
            .next_unseen()
            .expect("a live feed is published with the readiness it comes with");
        let EthereumWalletStreamMessage::Snapshot(live) = live.message().as_ref() else {
            panic!("the latest stream should hold the feed");
        };
        assert!(
            live.items().is_empty(),
            "a live feed is published even before it holds anything"
        );

        control
            .events
            .send(Ok(transfer("one", 1)))
            .await
            .expect("the live subscription should accept a transfer");
        let feed = next_feed(&mut latest, |snapshot| !snapshot.items().is_empty()).await;
        assert_eq!(contents(&feed), ["one"]);

        runtime.shutdown().await;
        assert_eq!(
            control.drops.load(Ordering::SeqCst),
            1,
            "shutdown should let the subscription go"
        );
    }

    #[tokio::test]
    async fn a_transfer_the_feed_already_holds_should_not_move_it() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let (runtime, mut reliable, mut latest) = live(&source).await;
        receive_status(&mut reliable, EthereumWalletStatus::Ready).await;

        control
            .events
            .send(Ok(transfer("one", 1)))
            .await
            .expect("the live subscription should accept a transfer");
        _ = next_feed(&mut latest, |snapshot| !snapshot.items().is_empty()).await;

        control
            .events
            .send(Ok(transfer("one", 1)))
            .await
            .expect("the duplicate should reach the service");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), latest.changed())
                .await
                .is_err(),
            "a transfer the feed already holds is the same event reported twice"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_status_the_pane_already_has_should_not_be_published_twice() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let (runtime, mut reliable, _) = live(&source).await;
        receive_status(&mut reliable, EthereumWalletStatus::Ready).await;

        control.status.send_replace(SubscriptionStatus::Active);

        assert!(
            tokio::time::timeout(Duration::from_millis(50), reliable.recv())
                .await
                .is_err(),
            "a status the pane already reads must not be repeated"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_session_that_could_not_be_taken_should_be_answered_as_the_service_said() {
        let source = FakeSource::default();
        source.refuse(KytrixServiceError::Authentication(SmolStr::new_static(
            "expired",
        )));
        let runtime = runtime(&source);

        let error = runtime
            .client()
            .call(EthereumWalletRequest::Load(scope()))
            .await
            .expect_err("an account whose credentials are gone cannot be streamed");

        assert!(matches!(
            error,
            RpcCallError::Service {
                source: EthereumWalletApiError::Authentication,
                ..
            }
        ));
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_subscription_that_could_not_be_opened_should_report_why_without_a_feed() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        session.refuse_subscription(KytrixServiceError::Closed);
        let (runtime, mut reliable, mut latest) = live(&source).await;

        assert_eq!(
            receive_failure(&mut reliable).await,
            EthereumWalletStreamError::Service(KytrixServiceError::Closed)
        );
        assert!(
            latest.next_unseen().is_none(),
            "a subscription that never came up publishes no transfers"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_subscription_that_closed_should_report_a_transport_failure() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        control.status.send_replace(SubscriptionStatus::Closed);
        let (runtime, mut reliable, mut latest) = live(&source).await;

        assert_eq!(
            receive_failure(&mut reliable).await,
            EthereumWalletStreamError::Transport(TransportError::Closed)
        );
        assert!(latest.next_unseen().is_none());
        runtime.shutdown().await;
    }

    /// An Ethereum wallet transfer names the Ethereum wallets an account is watching, so one that was already
    /// on the wire when the credentials behind the subscription changed is not data the
    /// pane may be shown: the subscription's own status is checked before it is converted.
    #[tokio::test]
    async fn a_transfer_that_arrived_after_the_credentials_changed_should_not_be_published() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let (runtime, mut reliable, mut latest) = live(&source).await;
        receive_status(&mut reliable, EthereumWalletStatus::Ready).await;
        // The empty feed a live subscription publishes is read before the stale transfer
        // is sent, so what is left on the latest stream is only what came after it.
        let _ = next_feed(&mut latest, |snapshot| snapshot.items().is_empty()).await;

        control
            .events
            .send(Ok(transfer("stale", 9)))
            .await
            .expect("the transfer should reach the subscription");
        // The credentials change as the transfer is handed over, which is what puts the two
        // on the wire together rather than one after the other.
        control.fail_on_delivery(TransportError::CredentialChanged);

        assert_eq!(
            receive_failure(&mut reliable).await,
            EthereumWalletStreamError::Transport(TransportError::CredentialChanged)
        );
        assert!(
            latest.next_unseen().is_none(),
            "a transfer of credentials the pane no longer reads as is not shown"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn an_account_that_signed_out_should_release_its_subscription() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let (runtime, mut reliable, _) = live(&source).await;
        receive_status(&mut reliable, EthereumWalletStatus::Ready).await;

        for _ in 0..2 {
            let response = runtime
                .client()
                .call(EthereumWalletRequest::Stop)
                .await
                .expect("stopping should be answered")
                .into_value();
            assert_eq!(response, EthereumWalletResponse::Stopped);
        }

        assert_eq!(
            control.drops.load(Ordering::SeqCst),
            1,
            "signing out gives up the subscription rather than keeping a seat"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn another_account_should_replace_the_stream_and_let_the_old_subscription_go() {
        let source = FakeSource::default();
        let first = source.session("first");
        let first_subscription = first.subscribe();
        let second = source.session("second");
        let second_subscription = second.subscribe();
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");

        load(&runtime, account("first")).await;
        receive_status(&mut reliable, EthereumWalletStatus::Ready).await;
        load(&runtime, account("second")).await;
        receive_status(&mut reliable, EthereumWalletStatus::Ready).await;

        assert_eq!(source.sessions(), 2);
        assert_eq!(
            source.requested(),
            [SmolStr::new_static("first"), SmolStr::new_static("second")],
            "a session is taken for the account the pane asked about"
        );
        assert_eq!(
            first_subscription.drops.load(Ordering::SeqCst),
            1,
            "the subscription of an account the pane left is not left running"
        );
        assert_eq!(second_subscription.drops.load(Ordering::SeqCst), 0);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn loading_an_account_twice_should_keep_the_stream_it_already_has() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let runtime = runtime(&source);

        load(&runtime, scope()).await;
        load(&runtime, scope()).await;

        assert_eq!(source.sessions(), 1);
        assert_eq!(session.state().subscribes, 1);
        assert_eq!(control.drops.load(Ordering::SeqCst), 0);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_retry_should_replace_the_stream_of_a_failed_account() {
        let source = FakeSource::default();
        let failed = source.session(ACCOUNT);
        failed.refuse_subscription(KytrixServiceError::Closed);
        let recovered = source.session(ACCOUNT);
        let control = recovered.subscribe();
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");

        load(&runtime, scope()).await;
        assert_eq!(
            receive_failure(&mut reliable).await,
            EthereumWalletStreamError::Service(KytrixServiceError::Closed)
        );

        retry(&runtime, scope()).await;
        receive_status(&mut reliable, EthereumWalletStatus::Ready).await;

        assert_eq!(source.sessions(), 2, "a retry takes the account again");
        assert_eq!(control.drops.load(Ordering::SeqCst), 0);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_session_of_another_account_should_be_answered_rather_than_streamed() {
        let source = FakeSource::default();
        let other = source.session("other");
        let runtime = runtime(&source);
        let mut latest = runtime.latest_stream();

        let error = runtime
            .client()
            .call(EthereumWalletRequest::Load(scope()))
            .await
            .expect_err("a stream is keyed by the account it was asked for");

        assert!(matches!(
            error,
            RpcCallError::Service {
                source: EthereumWalletApiError::AccountChanged,
                ..
            }
        ));
        assert_eq!(
            other.state().subscribes,
            0,
            "a session for another account is never subscribed with"
        );
        assert!(latest.next_unseen().is_none());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn the_addresses_should_be_read_through_the_account_the_stream_is_holding() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (runtime, _, _) = live(&source).await;
        session.answer_selections(selections(
            vec![selection("0x1111111111111111111111111111111111111111")],
            3,
        ));

        let response = fetch(&runtime, scope())
            .await
            .expect("the account is taken, so a read is answered");

        assert_eq!(
            addresses(&response),
            ["0x1111111111111111111111111111111111111111"]
        );
        assert_eq!(session.state().fetches, 1);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_read_before_the_account_is_taken_should_be_answered_as_not_ready() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let runtime = runtime(&source);

        let error = fetch(&runtime, scope())
            .await
            .expect_err("there is no session to answer a read yet");

        assert!(matches!(
            error,
            RpcCallError::Service {
                source: EthereumWalletApiError::NotReady,
                ..
            }
        ));
        assert_eq!(session.state().fetches, 0);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_change_for_an_account_the_service_is_not_holding_should_be_refused() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (runtime, _, _) = live(&source).await;

        let error = runtime
            .client()
            .call(EthereumWalletRequest::DeleteSelection {
                scope: account("other"),
                address: SmolStr::new_static("0x1111111111111111111111111111111111111111"),
            })
            .await
            .expect_err("another account's addresses are not this pane's to change");

        assert!(matches!(
            error,
            RpcCallError::Service {
                source: EthereumWalletApiError::AccountChanged,
                ..
            }
        ));
        assert_eq!(
            session.state().delete_calls,
            0,
            "a refused request never reaches the account"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_change_should_be_answered_with_what_the_service_confirmed() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (runtime, _, _) = live(&source).await;
        session
            .state()
            .adds
            .push_back(EthereumWalletMutationOutcome::Confirmed(selection(
                "0x1111111111111111111111111111111111111111",
            )));

        let response = runtime
            .client()
            .call(EthereumWalletRequest::AddSelection {
                scope: scope(),
                address: SmolStr::new_static("0x1111111111111111111111111111111111111111"),
            })
            .await
            .expect("the account is taken, so the change is sent")
            .into_value();

        assert_eq!(
            response,
            EthereumWalletResponse::Added(EthereumWalletMutationOutcome::Confirmed(selection(
                "0x1111111111111111111111111111111111111111"
            )))
        );
        assert_eq!(session.state().add_calls, 1);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_change_the_service_refused_should_be_answered_as_refused() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (runtime, _, _) = live(&source).await;
        session
            .state()
            .adds
            .push_back(EthereumWalletMutationOutcome::Rejected(
                EthereumWalletApiError::Conflict,
            ));

        let response = runtime
            .client()
            .call(EthereumWalletRequest::AddSelection {
                scope: scope(),
                address: SmolStr::new_static("0x1111111111111111111111111111111111111111"),
            })
            .await
            .expect("a refusal is an answer rather than a failed call")
            .into_value();

        assert_eq!(
            response,
            EthereumWalletResponse::Added(EthereumWalletMutationOutcome::Rejected(
                EthereumWalletApiError::Conflict
            ))
        );
        runtime.shutdown().await;
    }

    /// A change whose caller went away is dropped rather than decided: whether it was
    /// made is something the service cannot know, and it holds no list that could be
    /// guessed wrong. What it must not do is stay stuck on the question.
    #[tokio::test]
    async fn a_change_the_caller_gave_up_on_should_leave_the_service_answering() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (runtime, _, _) = live(&source).await;
        let block = session.block_mutation();

        let client = runtime.client();
        let pending = tokio::spawn(async move {
            client
                .call(EthereumWalletRequest::AddSelection {
                    scope: scope(),
                    address: SmolStr::new_static("0x1111111111111111111111111111111111111111"),
                })
                .await
        });
        block.wait_started().await;
        pending.abort();
        let _ = pending.await;
        block.wait_dropped().await;

        assert_eq!(session.state().add_calls, 1);
        session.answer_selections(selections(Vec::new(), 3));
        let response = fetch(&runtime, scope())
            .await
            .expect("the service should be free to answer the next request");
        assert!(addresses(&response).is_empty());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_caller_that_went_away_should_not_leave_a_stream_behind() {
        let source = FakeSource::default();
        let block = source.block();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let runtime = runtime(&source);
        let mut latest = runtime.latest_stream();

        let client = runtime.client();
        let pending =
            tokio::spawn(async move { client.call(EthereumWalletRequest::Load(scope())).await });
        block.wait_started().await;
        pending.abort();
        let _ = pending.await;
        block.wait_dropped().await;

        assert!(
            latest.next_unseen().is_none(),
            "a stream nobody waited for publishes nothing"
        );
        assert_eq!(
            session.state().subscribes,
            0,
            "a subscription nobody waited for is never asked for"
        );

        // The service is free again, and the account can be taken by the next request.
        load(&runtime, scope()).await;
        assert_eq!(source.sessions(), 2);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_reconnect_should_return_to_live_without_republishing_the_feed() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let (runtime, mut reliable, mut latest) = live(&source).await;
        receive_status(&mut reliable, EthereumWalletStatus::Ready).await;

        control
            .events
            .send(Ok(transfer("one", 1)))
            .await
            .expect("the live subscription should accept a transfer");
        _ = next_feed(&mut latest, |snapshot| !snapshot.items().is_empty()).await;

        control
            .status
            .send_replace(SubscriptionStatus::Reconnecting);
        receive_status(&mut reliable, EthereumWalletStatus::Connecting).await;
        control.status.send_replace(SubscriptionStatus::Active);
        receive_status(&mut reliable, EthereumWalletStatus::Ready).await;

        assert!(
            tokio::time::timeout(Duration::from_millis(100), latest.changed())
                .await
                .is_err(),
            "the transfers a pane kept across a reconnect are not republished as new"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_mutation_of_an_unknown_outcome_should_be_answered_as_unknown() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let _control = session.subscribe();
        let (runtime, _, _) = live(&source).await;
        session
            .state()
            .deletes
            .push_back(EthereumWalletMutationOutcome::Unknown(
                EthereumWalletMutationUnknown::Timeout,
            ));

        let response = runtime
            .client()
            .call(EthereumWalletRequest::DeleteSelection {
                scope: scope(),
                address: SmolStr::new_static("0x1111111111111111111111111111111111111111"),
            })
            .await
            .expect("an unknown outcome is an answer rather than a failed call")
            .into_value();

        assert_eq!(
            response,
            EthereumWalletResponse::Deleted(EthereumWalletMutationOutcome::Unknown(
                EthereumWalletMutationUnknown::Timeout
            ))
        );
        runtime.shutdown().await;
    }

    #[test]
    fn a_subscription_status_should_say_which_ones_end_a_stream() {
        for status in [
            SubscriptionStatus::Connecting,
            SubscriptionStatus::Active,
            SubscriptionStatus::Reconnecting,
            SubscriptionStatus::Closing,
        ] {
            assert!(
                ended_subscription(&status).is_none(),
                "{status:?} is a subscription a transfer may still be read from"
            );
        }
        assert_eq!(
            ended_subscription(&SubscriptionStatus::Closed),
            Some(EthereumWalletStreamError::Transport(TransportError::Closed))
        );
        assert_eq!(
            ended_subscription(&SubscriptionStatus::Failed(
                TransportError::CredentialChanged
            )),
            Some(EthereumWalletStreamError::Transport(
                TransportError::CredentialChanged
            ))
        );
    }
}

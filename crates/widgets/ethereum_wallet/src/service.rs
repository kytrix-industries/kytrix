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
    error::TranslatedError,
    protocol::WidgetProtocol,
    runtime::{ServiceContext, WidgetService},
    stream::{StreamPublishError, StreamPublisher},
};

use super::model::{SelectionData, TransferSnapshot, TransferState};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum EthereumWalletRequest {
    Load(EthereumWalletScope),
    Retry(EthereumWalletScope),
    Stop,
    FetchSelections(EthereumWalletScope),
    AddSelection {
        scope: EthereumWalletScope,
        address: SmolStr,
    },
    DeleteSelection {
        scope: EthereumWalletScope,
        address: SmolStr,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum EthereumWalletResponse {
    StreamStarted,
    Stopped,
    Selections(SelectionData),
    Added(EthereumWalletMutationOutcome<EthereumWalletSelection>),
    Deleted(EthereumWalletMutationOutcome<()>),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct EthereumWalletScope {
    account_id: SmolStr,
}

impl EthereumWalletScope {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum EthereumWalletStatus {
    Connecting,
    Ready,
    Failed(EthereumWalletStreamError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum EthereumWalletStreamError {
    Service(KytrixServiceError),
    Transport(TransportError),
}

impl TranslatedError for EthereumWalletStreamError {
    fn translated_message(&self, translate: &i18n::translate::Translate) -> SmolStr {
        match self {
            Self::Service(error) => error.translated_message(translate),
            Self::Transport(error) => error.translated_message(translate),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum EthereumWalletStreamMessage {
    Status(EthereumWalletStatus),
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

pub(super) trait EthereumWalletSource: Clone + Send + Sync + 'static {
    type Session: EthereumWalletSession;

    fn acquire_session(
        &self,
        account_id: &str,
    ) -> impl Future<Output = Result<Self::Session, KytrixServiceError>> + Send;
}

pub(super) trait EthereumWalletSession: Clone + Send + Sync + 'static {
    type Subscription: EthereumWalletEventSubscription;

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

pub(super) struct EthereumWalletService<S = KytrixServiceConnector> {
    source: S,
}

impl<S> EthereumWalletService<S> {
    pub(super) const fn new(source: S) -> Self {
        Self { source }
    }
}

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

fn session_failure(error: KytrixServiceError) -> EthereumWalletApiError {
    match error {
        KytrixServiceError::Authentication(_) => EthereumWalletApiError::Authentication,
        KytrixServiceError::AccountChanged => EthereumWalletApiError::AccountChanged,
        KytrixServiceError::Timeout => EthereumWalletApiError::Timeout,
        KytrixServiceError::Closed => EthereumWalletApiError::Closed,
        error => EthereumWalletApiError::Service(error),
    }
}

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
                if publisher.is_cancelled() {
                    return;
                }
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
        SubscriptionStatus::Closing => true,
        SubscriptionStatus::Connecting | SubscriptionStatus::Reconnecting => {
            publish_change(publisher, EthereumWalletStatus::Connecting, published)
                .await
                .is_ok()
        }
        SubscriptionStatus::Active => {
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
        SubscriptionStatus::Closed | SubscriptionStatus::Failed(_) => false,
    }
}

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

async fn publish_status(
    publisher: &StreamPublisher<EthereumWalletProtocol>,
    status: EthereumWalletStatus,
) -> Result<(), StreamPublishError> {
    publisher
        .publish_reliable(EthereumWalletStreamMessage::Status(status))
        .await
}

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

async fn publish_failure(
    publisher: &StreamPublisher<EthereumWalletProtocol>,
    error: EthereumWalletStreamError,
) {
    let _ = publish_status(publisher, EthereumWalletStatus::Failed(error)).await;
    publisher.cancelled().await;
}

#[cfg(test)]
mod tests {

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

    #[tokio::test]
    async fn a_transfer_that_arrived_after_the_credentials_changed_should_not_be_published() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let (runtime, mut reliable, mut latest) = live(&source).await;
        receive_status(&mut reliable, EthereumWalletStatus::Ready).await;
        let _ = next_feed(&mut latest, |snapshot| snapshot.items().is_empty()).await;

        control
            .events
            .send(Ok(transfer("stale", 9)))
            .await
            .expect("the transfer should reach the subscription");
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

use std::{future::Future, sync::Arc};

use i18n::{t_smol, translate::Translate};
use kytrix_service_provider::service::{
    connector::KytrixServiceConnector,
    engine::{ErrorCode, KytrixAccountSession, KytrixServiceError},
    global_shipping_chokepoint::{GlobalShippingChokepointArea, GlobalShippingChokepointUpdate},
};
use network::transport::{error::TransportError, subscription::Subscription};
use smol_str::SmolStr;
use thiserror::Error;
use tokio::time::Instant;
use widget::{
    error::TranslatedError,
    protocol::WidgetProtocol,
    runtime::{ServiceContext, WidgetService},
    stream::StreamPublisher,
};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct GlobalShippingChokepointScope {
    pub account: SmolStr,
    pub area: GlobalShippingChokepointArea,
}

impl GlobalShippingChokepointScope {
    pub(super) fn new(account: SmolStr) -> Self {
        Self {
            account,
            area: GlobalShippingChokepointArea::Hormuz,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum GlobalShippingChokepointRequest {
    Load {
        scope: GlobalShippingChokepointScope,
    },
    Stop,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum GlobalShippingChokepointResponse {
    StreamStarted,
    Stopped,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(super) enum AisStreamError {
    #[error(transparent)]
    Service(#[from] KytrixServiceError),
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl TranslatedError for AisStreamError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Service(KytrixServiceError::Server(ErrorCode::EntitlementRequired)) => {
                t_smol!(
                    translate,
                    "widget.global_shipping_chokepoint.entitlement_required"
                )
            }
            Self::Service(error) => error.translated_message(translate),
            Self::Transport(error) => error.translated_message(translate),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum AisStreamMessage {
    Update(GlobalShippingChokepointUpdate),
    Failed(Arc<AisStreamError>),
}

pub(super) struct GlobalShippingChokepointProtocol;

impl WidgetProtocol for GlobalShippingChokepointProtocol {
    type Request = GlobalShippingChokepointRequest;
    type Response = GlobalShippingChokepointResponse;
    type StreamKey = GlobalShippingChokepointArea;
    type StreamMessage = AisStreamMessage;
    type Error = AisStreamError;
}

pub(super) trait GlobalShippingChokepointSource: Clone + Send + Sync + 'static {
    type Session: GlobalShippingChokepointSession;

    fn acquire_session(
        &self,
        account_id: &str,
    ) -> impl Future<Output = Result<Self::Session, KytrixServiceError>> + Send;
}

pub(super) trait GlobalShippingChokepointSession: Clone + Send + Sync + 'static {
    type Subscription: GlobalShippingChokepointEventSubscription;

    fn subscribe_global_shipping_chokepoint(
        &self,
        area: GlobalShippingChokepointArea,
    ) -> impl Future<Output = Result<Self::Subscription, KytrixServiceError>> + Send;
}

pub(super) trait GlobalShippingChokepointEventSubscription: Send + 'static {
    fn recv(
        &mut self,
    ) -> impl Future<Output = Result<GlobalShippingChokepointUpdate, TransportError>> + Send;
}

impl GlobalShippingChokepointSource for KytrixServiceConnector {
    type Session = KytrixAccountSession;

    async fn acquire_session(&self, account_id: &str) -> Result<Self::Session, KytrixServiceError> {
        self.acquire_account_session(account_id).await
    }
}

impl GlobalShippingChokepointSession for KytrixAccountSession {
    type Subscription = Subscription<GlobalShippingChokepointUpdate>;

    async fn subscribe_global_shipping_chokepoint(
        &self,
        area: GlobalShippingChokepointArea,
    ) -> Result<Self::Subscription, KytrixServiceError> {
        KytrixAccountSession::subscribe_global_shipping_chokepoint(self, area).await
    }
}

impl GlobalShippingChokepointEventSubscription for Subscription<GlobalShippingChokepointUpdate> {
    async fn recv(&mut self) -> Result<GlobalShippingChokepointUpdate, TransportError> {
        Subscription::recv(self).await
    }
}

pub(super) struct GlobalShippingChokepointService<S = KytrixServiceConnector> {
    source: S,
}

impl<S> GlobalShippingChokepointService<S> {
    pub(super) const fn new(source: S) -> Self {
        Self { source }
    }
}

impl<S> WidgetService<GlobalShippingChokepointProtocol> for GlobalShippingChokepointService<S>
where
    S: GlobalShippingChokepointSource,
{
    async fn run(self, mut context: ServiceContext<GlobalShippingChokepointProtocol>) {
        let mut active: Option<GlobalShippingChokepointArea> = None;
        let source = self.source.clone();

        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            match request {
                GlobalShippingChokepointRequest::Stop => {
                    if let Some(area) = active.take() {
                        context.cancel_stream(&area);
                    }
                    let _ = reply.respond(Ok(GlobalShippingChokepointResponse::Stopped));
                }
                GlobalShippingChokepointRequest::Load { scope } => {
                    let key = scope.area;
                    if let Some(previous) = active.replace(key) {
                        context.cancel_stream(&previous);
                    }
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        let _ = reply.respond(Ok(GlobalShippingChokepointResponse::StreamStarted));
                        continue;
                    }
                    let source = source.clone();
                    context.start_stream(key, move |publisher| async move {
                        run_stream(source, scope, publisher).await;
                    });
                    let _ = reply.respond(Ok(GlobalShippingChokepointResponse::StreamStarted));
                }
            }
        }
    }
}

async fn run_stream<S>(
    source: S,
    scope: GlobalShippingChokepointScope,
    publisher: StreamPublisher<GlobalShippingChokepointProtocol>,
) where
    S: GlobalShippingChokepointSource,
{
    let session = tokio::select! {
        biased;
        () = publisher.cancelled() => return,
        result = source.acquire_session(&scope.account) => result,
    };
    let session = match session {
        Ok(session) => session,
        Err(error) => return publish_failure(&publisher, error.into()).await,
    };

    let subscription = tokio::select! {
        biased;
        () = publisher.cancelled() => return,
        result = session.subscribe_global_shipping_chokepoint(scope.area) => result,
    };
    let mut subscription = match subscription {
        Ok(subscription) => subscription,
        Err(error) => return publish_failure(&publisher, error.into()).await,
    };

    loop {
        tokio::select! {
            biased;
            () = publisher.cancelled() => break,
            received = subscription.recv() => match received {
                Ok(update) => {
                    if publisher
                        .publish_latest(AisStreamMessage::Update(update))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    publish_failure(&publisher, error.into()).await;
                    break;
                }
            }
        }
    }
}

async fn publish_failure(
    publisher: &StreamPublisher<GlobalShippingChokepointProtocol>,
    error: AisStreamError,
) {
    let _ = publisher
        .publish_reliable(AisStreamMessage::Failed(Arc::new(error)))
        .await;
    publisher.cancelled().await;
}

#[cfg(test)]
mod tests {

    #[allow(
        clippy::disallowed_methods,
        reason = "a test drives its subject on its own runtime so the tasks it awaits are the ones it spawned"
    )]
    fn current_test_handle() -> &'static tokio::runtime::Handle {
        Box::leak(Box::new(tokio::runtime::Handle::current()))
    }
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex, MutexGuard,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use kytrix_service_provider::service::global_shipping_chokepoint::GlobalShippingChokepointPosition;
    use tokio::sync::{Notify, mpsc};
    use widget::{
        runtime::WidgetRuntime,
        stream::{LatestStream, ReliableStream},
    };

    use super::*;

    const ACCOUNT: &str = "test-account";

    fn scope() -> GlobalShippingChokepointScope {
        GlobalShippingChokepointScope::new(SmolStr::new_static(ACCOUNT))
    }

    fn runtime(source: &FakeSource) -> WidgetRuntime<GlobalShippingChokepointProtocol> {
        WidgetRuntime::spawn(
            current_test_handle(),
            GlobalShippingChokepointService::new(source.clone()),
        )
    }

    async fn load(
        runtime: &WidgetRuntime<GlobalShippingChokepointProtocol>,
    ) -> GlobalShippingChokepointResponse {
        runtime
            .client()
            .call(GlobalShippingChokepointRequest::Load { scope: scope() })
            .await
            .expect("the stream should start")
            .into_value()
    }

    async fn stop(
        runtime: &WidgetRuntime<GlobalShippingChokepointProtocol>,
    ) -> GlobalShippingChokepointResponse {
        runtime
            .client()
            .call(GlobalShippingChokepointRequest::Stop)
            .await
            .expect("stopping should be answered")
            .into_value()
    }

    fn position(mmsi: u32) -> GlobalShippingChokepointPosition {
        GlobalShippingChokepointPosition {
            mmsi,
            latitude: 0,
            longitude: 0,
            sog: 0,
            cog: 0,
            heading: 0,
            nav_status: 0,
            last_update: 0,
        }
    }

    fn update(
        sequence: u64,
        positions: Vec<GlobalShippingChokepointPosition>,
    ) -> GlobalShippingChokepointUpdate {
        GlobalShippingChokepointUpdate {
            epoch: 7,
            keyframe: false,
            sequence,
            base: sequence.saturating_sub(1),
            ships: Vec::new(),
            positions,
            removed: Vec::new(),
            timestamp: 1,
        }
    }

    async fn wait_for(what: &str, ready: impl Fn() -> bool) {
        let settled = tokio::time::timeout(Duration::from_secs(2), async {
            while !ready() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(settled.is_ok(), "{what} should happen");
    }

    async fn next_reliable(
        reliable: &mut ReliableStream<GlobalShippingChokepointProtocol>,
        predicate: impl Fn(&AisStreamMessage) -> bool,
    ) -> AisStreamMessage {
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
        .expect("the expected reliable message should arrive")
    }

    async fn receive_failure(
        reliable: &mut ReliableStream<GlobalShippingChokepointProtocol>,
    ) -> AisStreamError {
        let message = next_reliable(reliable, |message| {
            matches!(message, AisStreamMessage::Failed(_))
        })
        .await;
        let AisStreamMessage::Failed(error) = message else {
            panic!("the message should be a failure");
        };
        (*error).clone()
    }

    async fn next_update(
        latest: &mut LatestStream<GlobalShippingChokepointProtocol>,
    ) -> GlobalShippingChokepointUpdate {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                latest
                    .changed()
                    .await
                    .expect("the latest stream should stay open");
                while let Some(envelope) = latest.next_unseen() {
                    if let AisStreamMessage::Update(update) = envelope.message().as_ref() {
                        return update.clone();
                    }
                }
            }
        })
        .await
        .expect("an update should be published")
    }

    #[tokio::test]
    async fn a_load_should_start_the_area_stream_and_answer_stream_started() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let runtime = runtime(&source);
        let mut latest = runtime.latest_stream();

        assert_eq!(
            load(&runtime).await,
            GlobalShippingChokepointResponse::StreamStarted
        );

        control
            .events
            .send(Ok(update(1, vec![position(1)])))
            .await
            .expect("the live subscription should accept a push");
        assert_eq!(next_update(&mut latest).await.positions, [position(1)]);

        assert_eq!(source.requested(), [SmolStr::new_static(ACCOUNT)]);
        assert_eq!(
            session.state().areas.as_slice(),
            [GlobalShippingChokepointArea::Hormuz]
        );
        assert_eq!(session.state().subscribes, 1);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_repeated_load_should_replace_the_stream_and_let_the_old_subscription_go() {
        let source = FakeSource::default();
        let first = source.session(ACCOUNT);
        let first_control = first.subscribe();
        let second = source.session(ACCOUNT);
        let second_control = second.subscribe();
        let runtime = runtime(&source);
        let mut latest = runtime.latest_stream();

        load(&runtime).await;
        first_control
            .events
            .send(Ok(update(1, vec![position(1)])))
            .await
            .expect("the first subscription should accept a push");
        assert_eq!(next_update(&mut latest).await.sequence, 1);

        load(&runtime).await;
        wait_for("the replaced stream should let its subscription go", || {
            first_control.drops.load(Ordering::SeqCst) == 1
        })
        .await;
        second_control
            .events
            .send(Ok(update(2, vec![position(2)])))
            .await
            .expect("the second subscription should accept a push");
        assert_eq!(next_update(&mut latest).await.sequence, 2);

        assert_eq!(source.sessions(), 2, "a load takes the account again");
        assert_eq!(
            second_control.drops.load(Ordering::SeqCst),
            0,
            "the stream the pane is reading keeps its subscription"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_stop_should_cancel_the_stream_and_answer_stopped() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let runtime = runtime(&source);
        let mut latest = runtime.latest_stream();

        load(&runtime).await;
        control
            .events
            .send(Ok(update(1, vec![position(1)])))
            .await
            .expect("the live subscription should accept a push");
        let _ = next_update(&mut latest).await;

        assert_eq!(
            stop(&runtime).await,
            GlobalShippingChokepointResponse::Stopped
        );
        wait_for("the stopped stream should let its subscription go", || {
            control.drops.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(
            stop(&runtime).await,
            GlobalShippingChokepointResponse::Stopped,
            "stopping nothing is still an answer"
        );
        assert!(
            latest.next_unseen().is_none(),
            "a cancelled stream publishes nothing more"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_load_should_answer_before_the_session_is_taken() {
        let source = FakeSource::default();
        let block = source.block();
        let runtime = runtime(&source);

        assert_eq!(
            load(&runtime).await,
            GlobalShippingChokepointResponse::StreamStarted
        );
        block.wait_started().await;

        assert_eq!(
            stop(&runtime).await,
            GlobalShippingChokepointResponse::Stopped
        );
        block.wait_dropped().await;
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_subscription_that_never_opens_should_be_cancelled_with_its_stream() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let block = session.block_subscription();
        let runtime = runtime(&source);

        assert_eq!(
            load(&runtime).await,
            GlobalShippingChokepointResponse::StreamStarted
        );
        block.wait_started().await;

        assert_eq!(
            stop(&runtime).await,
            GlobalShippingChokepointResponse::Stopped
        );
        block.wait_dropped().await;
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_session_that_could_not_be_taken_should_fail_the_stream_rather_than_the_load() {
        let source = FakeSource::default();
        source.refuse(KytrixServiceError::Server(ErrorCode::EntitlementRequired));
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let mut latest = runtime.latest_stream();

        assert_eq!(
            load(&runtime).await,
            GlobalShippingChokepointResponse::StreamStarted
        );
        assert_eq!(
            receive_failure(&mut reliable).await,
            AisStreamError::Service(KytrixServiceError::Server(ErrorCode::EntitlementRequired))
        );
        assert!(
            latest.next_unseen().is_none(),
            "a stream that never started publishes no update"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_subscription_that_could_not_be_opened_should_fail_the_stream() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        session.refuse_subscription(KytrixServiceError::Closed);
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let mut latest = runtime.latest_stream();

        assert_eq!(
            load(&runtime).await,
            GlobalShippingChokepointResponse::StreamStarted
        );
        assert_eq!(
            receive_failure(&mut reliable).await,
            AisStreamError::Service(KytrixServiceError::Closed)
        );
        assert!(latest.next_unseen().is_none());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_load_after_a_failure_should_replace_the_failed_stream() {
        let source = FakeSource::default();
        let failed = source.session(ACCOUNT);
        failed.refuse_subscription(KytrixServiceError::Closed);
        let recovered = source.session(ACCOUNT);
        let control = recovered.subscribe();
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let mut latest = runtime.latest_stream();

        load(&runtime).await;
        assert_eq!(
            receive_failure(&mut reliable).await,
            AisStreamError::Service(KytrixServiceError::Closed)
        );

        load(&runtime).await;
        control
            .events
            .send(Ok(update(1, vec![position(1)])))
            .await
            .expect("the recovered subscription should accept a push");
        assert_eq!(next_update(&mut latest).await.positions, [position(1)]);
        assert_eq!(source.sessions(), 2, "a recovery takes the account again");
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_broken_subscription_should_fail_the_stream_with_its_transport_error() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let mut latest = runtime.latest_stream();

        assert_eq!(
            load(&runtime).await,
            GlobalShippingChokepointResponse::StreamStarted
        );
        control
            .events
            .send(Err(TransportError::Closed))
            .await
            .expect("the subscription should break");
        assert_eq!(
            receive_failure(&mut reliable).await,
            AisStreamError::Transport(TransportError::Closed)
        );
        assert!(
            latest.next_unseen().is_none(),
            "a broken stream publishes no update of its own"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), async {
                while control.drops.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_err(),
            "a failed stream holds its subscription until the pane replaces it"
        );

        assert_eq!(
            stop(&runtime).await,
            GlobalShippingChokepointResponse::Stopped
        );
        wait_for("the replaced stream should let its subscription go", || {
            control.drops.load(Ordering::SeqCst) == 1
        })
        .await;
        runtime.shutdown().await;
    }

    #[test]
    fn an_entitlement_refusal_should_stay_classifiable_as_the_service_error_it_is() {
        let error =
            AisStreamError::from(KytrixServiceError::Server(ErrorCode::EntitlementRequired));
        assert!(matches!(
            error,
            AisStreamError::Service(KytrixServiceError::Server(ErrorCode::EntitlementRequired))
        ));

        let error = AisStreamError::from(KytrixServiceError::Closed);
        assert!(matches!(
            error,
            AisStreamError::Service(KytrixServiceError::Closed)
        ));
    }

    #[tokio::test]
    async fn an_empty_increment_should_still_be_published_as_an_update() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let runtime = runtime(&source);
        let mut latest = runtime.latest_stream();

        load(&runtime).await;
        control
            .events
            .send(Ok(update(1, Vec::new())))
            .await
            .expect("a heartbeat is a push like any other");
        let published = next_update(&mut latest).await;

        assert_eq!(published.sequence, 1);
        assert!(
            published.positions.is_empty(),
            "a tick that changed nothing is still the tick the pane draws"
        );
        assert!(!published.keyframe);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn shutting_down_should_let_the_subscription_go() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let runtime = runtime(&source);
        let mut latest = runtime.latest_stream();

        load(&runtime).await;
        control
            .events
            .send(Ok(update(1, vec![position(1)])))
            .await
            .expect("the live subscription should accept a push");
        let _ = next_update(&mut latest).await;

        runtime.shutdown().await;
        assert_eq!(
            control.drops.load(Ordering::SeqCst),
            1,
            "shutdown should let the subscription go rather than leak it"
        );
    }

    #[derive(Clone, Default)]
    struct FakeSource {
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
        fn session(&self, account: &str) -> FakeSession {
            let session = FakeSession::new(account);
            self.state()
                .sessions
                .push_back(FakeSetup::Ready(session.clone()));
            session
        }

        fn refuse(&self, error: KytrixServiceError) {
            self.state().sessions.push_back(FakeSetup::Failed(error));
        }

        fn block(&self) -> Arc<OperationBlock> {
            let block = Arc::new(OperationBlock::default());
            self.state()
                .sessions
                .push_back(FakeSetup::Blocked(block.clone()));
            block
        }

        fn requested(&self) -> Vec<SmolStr> {
            self.state().acquired.clone()
        }

        fn sessions(&self) -> usize {
            self.state().acquired.len()
        }

        fn state(&self) -> MutexGuard<'_, FakeSourceState> {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    impl GlobalShippingChokepointSource for FakeSource {
        type Session = FakeSession;

        async fn acquire_session(
            &self,
            account_id: &str,
        ) -> Result<Self::Session, KytrixServiceError> {
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
    struct FakeSession {
        state: Arc<Mutex<FakeSessionState>>,
    }

    #[derive(Default)]
    struct FakeSessionState {
        subscriptions: VecDeque<FakeSubscribe>,
        subscribes: usize,
        areas: Vec<GlobalShippingChokepointArea>,
    }

    enum FakeSubscribe {
        Ready(FakeSubscription),
        Failed(KytrixServiceError),
        Blocked(Arc<OperationBlock>),
    }

    impl FakeSession {
        fn new(_account: &str) -> Self {
            Self {
                state: Arc::new(Mutex::new(FakeSessionState::default())),
            }
        }

        fn subscribe(&self) -> FakeSubscriptionControl {
            let (events, receiver) = mpsc::channel(8);
            let drops = Arc::new(AtomicUsize::new(0));
            self.state()
                .subscriptions
                .push_back(FakeSubscribe::Ready(FakeSubscription {
                    events: receiver,
                    drops: drops.clone(),
                }));
            FakeSubscriptionControl { events, drops }
        }

        fn refuse_subscription(&self, error: KytrixServiceError) {
            self.state()
                .subscriptions
                .push_back(FakeSubscribe::Failed(error));
        }

        fn block_subscription(&self) -> Arc<OperationBlock> {
            let block = Arc::new(OperationBlock::default());
            self.state()
                .subscriptions
                .push_back(FakeSubscribe::Blocked(block.clone()));
            block
        }

        fn state(&self) -> MutexGuard<'_, FakeSessionState> {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    impl GlobalShippingChokepointSession for FakeSession {
        type Subscription = FakeSubscription;

        async fn subscribe_global_shipping_chokepoint(
            &self,
            area: GlobalShippingChokepointArea,
        ) -> Result<Self::Subscription, KytrixServiceError> {
            let next = {
                let mut state = self.state();
                state.subscribes += 1;
                state.areas.push(area);
                state
                    .subscriptions
                    .pop_front()
                    .unwrap_or(FakeSubscribe::Failed(KytrixServiceError::Closed))
            };
            match next {
                FakeSubscribe::Ready(subscription) => Ok(subscription),
                FakeSubscribe::Failed(error) => Err(error),
                FakeSubscribe::Blocked(block) => {
                    let _guard = OperationGuard {
                        block: block.clone(),
                    };
                    block.started.notify_one();
                    std::future::pending().await
                }
            }
        }
    }

    struct FakeSubscription {
        events: mpsc::Receiver<Result<GlobalShippingChokepointUpdate, TransportError>>,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for FakeSubscription {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl GlobalShippingChokepointEventSubscription for FakeSubscription {
        async fn recv(&mut self) -> Result<GlobalShippingChokepointUpdate, TransportError> {
            self.events
                .recv()
                .await
                .unwrap_or(Err(TransportError::Closed))
        }
    }

    struct FakeSubscriptionControl {
        events: mpsc::Sender<Result<GlobalShippingChokepointUpdate, TransportError>>,
        drops: Arc<AtomicUsize>,
    }

    #[derive(Default)]
    struct OperationBlock {
        started: Notify,
        dropped: AtomicBool,
    }

    struct OperationGuard {
        block: Arc<OperationBlock>,
    }

    impl Drop for OperationGuard {
        fn drop(&mut self) {
            self.block.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl OperationBlock {
        async fn wait_started(&self) {
            tokio::time::timeout(Duration::from_secs(2), self.started.notified())
                .await
                .expect("the blocked operation should start");
        }

        async fn wait_dropped(&self) {
            wait_for("the blocked operation to be dropped", || {
                self.dropped.load(Ordering::SeqCst)
            })
            .await;
        }
    }
}

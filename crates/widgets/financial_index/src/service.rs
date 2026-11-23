use std::{future::Future, sync::Arc};

use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use i18n::{t_smol, translate::Translate};
use kytrix_proto::types::v1 as proto_types;
use kytrix_service_provider::service::{
    connector::KytrixServiceConnector,
    engine::{
        ErrorCode, FinancialIndexEvent, FinancialIndexTopic, KytrixServiceError, KytrixSessionState,
    },
};
use network::transport::{
    error::TransportError,
    subscription::{Subscription, SubscriptionStatus},
};
use smol_str::SmolStr;
use thiserror::Error;
use tokio::{
    sync::{oneshot, watch},
    time::Instant,
};
use widget::{
    error::TranslatedError,
    protocol::WidgetProtocol,
    rpc::RpcRequestContext,
    runtime::{ServiceContext, WidgetService},
    stream::{StreamPublishError, StreamPublisher},
};

use super::model::{
    FinancialIndexCatalog, FinancialIndexQuote, MAX_SELECTED_INDICES, SelectedFinancialIndex,
    prepare_catalog, selection_topics,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum FinancialIndexRequest {
    Load(FinancialIndexScope),
    Retry(FinancialIndexScope),
    Stop,
    FetchManifest,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum FinancialIndexResponse {
    StreamStarted,
    Stopped,
    Manifest {
        catalog: FinancialIndexCatalog,
        limit: Option<u32>,
    },
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct FinancialIndexScope {
    account_id: SmolStr,
    topics: Arc<[FinancialIndexTopic]>,
}

impl FinancialIndexScope {
    pub(super) fn new(
        account_id: Option<&SmolStr>,
        selection: &[SelectedFinancialIndex],
    ) -> Option<Self> {
        if selection.is_empty() {
            return None;
        }
        let account_id = account_id.filter(|account_id| !account_id.is_empty())?;
        Some(Self {
            account_id: account_id.clone(),
            topics: selection_topics(selection),
        })
    }

    fn validate(&self) -> Result<(), FinancialIndexServiceError> {
        if self.topics.len() > MAX_SELECTED_INDICES {
            return Err(FinancialIndexServiceError::Data(
                super::model::FinancialIndexDataError::SelectionCapacity {
                    limit: MAX_SELECTED_INDICES,
                },
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum FinancialIndexStatus {
    Connecting,
    Ready,
    Failed(FinancialIndexStreamError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum FinancialIndexStreamError {
    Service(KytrixServiceError),
    Transport(TransportError),
    FinancialIndexLimit { limit: Option<u32> },
}

impl TranslatedError for FinancialIndexStreamError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Service(error) => error.translated_message(translate),
            Self::Transport(error) => error.translated_message(translate),
            Self::FinancialIndexLimit { limit: Some(limit) } => {
                if *limit == 1 {
                    t_smol!(
                        translate,
                        "error.financial_index.index_limit_one",
                        limit = limit
                    )
                } else {
                    t_smol!(
                        translate,
                        "error.financial_index.index_limit",
                        limit = limit
                    )
                }
            }
            Self::FinancialIndexLimit { limit: None } => {
                t_smol!(translate, "error.financial_index.index_limit_unknown")
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum FinancialIndexStreamMessage {
    Status(FinancialIndexStatus),
    Quotes(Arc<HashMap<FinancialIndexTopic, FinancialIndexQuote>>),
}

pub(super) struct FinancialIndexProtocol;

impl WidgetProtocol for FinancialIndexProtocol {
    type Request = FinancialIndexRequest;
    type Response = FinancialIndexResponse;
    type StreamKey = FinancialIndexScope;
    type StreamMessage = FinancialIndexStreamMessage;
    type Error = FinancialIndexServiceError;
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(super) enum FinancialIndexServiceError {
    #[error(transparent)]
    Service(#[from] KytrixServiceError),
    #[error(transparent)]
    Data(#[from] super::model::FinancialIndexDataError),
}

impl TranslatedError for FinancialIndexServiceError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Service(error) => error.translated_message(translate),
            Self::Data(error) => error.translated_message(translate),
        }
    }
}

pub(super) trait FinancialIndexSource: Clone + Send + Sync + 'static {
    type Subscription: FinancialIndexEventSubscription;

    fn session_state(&self) -> watch::Receiver<Option<KytrixSessionState>>;

    fn fetch_manifest(
        &self,
    ) -> impl Future<Output = Result<proto_types::FinancialIndices, KytrixServiceError>> + Send;

    fn subscribe_financial_index(
        &self,
        topics: Arc<[FinancialIndexTopic]>,
    ) -> impl Future<Output = Result<Self::Subscription, KytrixServiceError>> + Send;
}

pub(super) trait FinancialIndexEventSubscription: Send + 'static {
    fn observe_status(&self) -> watch::Receiver<SubscriptionStatus>;

    fn recv(&mut self) -> impl Future<Output = Result<FinancialIndexEvent, TransportError>> + Send;
}

impl FinancialIndexSource for KytrixServiceConnector {
    type Subscription = Subscription<FinancialIndexEvent>;

    fn session_state(&self) -> watch::Receiver<Option<KytrixSessionState>> {
        KytrixServiceConnector::subscribe_session_state(self)
    }

    fn fetch_manifest(
        &self,
    ) -> impl Future<Output = Result<proto_types::FinancialIndices, KytrixServiceError>> + Send
    {
        KytrixServiceConnector::fetch_financial_index(self)
    }

    async fn subscribe_financial_index(
        &self,
        topics: Arc<[FinancialIndexTopic]>,
    ) -> Result<Self::Subscription, KytrixServiceError> {
        KytrixServiceConnector::subscribe_financial_index(self, &topics).await
    }
}

impl FinancialIndexEventSubscription for Subscription<FinancialIndexEvent> {
    fn observe_status(&self) -> watch::Receiver<SubscriptionStatus> {
        Subscription::observe_status(self)
    }

    fn recv(&mut self) -> impl Future<Output = Result<FinancialIndexEvent, TransportError>> + Send {
        Subscription::recv(self)
    }
}

pub(super) struct FinancialIndexService<S = KytrixServiceConnector> {
    source: S,
}

impl<S> FinancialIndexService<S> {
    pub(super) const fn new(source: S) -> Self {
        Self { source }
    }
}

impl<S> WidgetService<FinancialIndexProtocol> for FinancialIndexService<S>
where
    S: FinancialIndexSource,
{
    async fn run(self, mut context: ServiceContext<FinancialIndexProtocol>) {
        let mut active: Option<ActiveStream> = None;

        while let Some(request) = context.next_rpc().await {
            let (mut reply, request) = request.into_parts();
            match request {
                FinancialIndexRequest::FetchManifest => {
                    if let Some(result) = fetch_manifest(&self.source, &mut reply).await {
                        let _ = reply.respond(result);
                    }
                }
                FinancialIndexRequest::Stop => {
                    if retire(&mut context, &mut active, &mut reply).await {
                        let _ = reply.respond(Ok(FinancialIndexResponse::Stopped));
                    }
                }
                request @ (FinancialIndexRequest::Load(_) | FinancialIndexRequest::Retry(_)) => {
                    let (scope, retry) = match request {
                        FinancialIndexRequest::Load(scope) => (scope, false),
                        FinancialIndexRequest::Retry(scope) => (scope, true),
                        FinancialIndexRequest::Stop | FinancialIndexRequest::FetchManifest => {
                            continue;
                        }
                    };
                    if let Err(error) = scope.validate() {
                        let _ = reply.respond(Err(error));
                        continue;
                    }
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }

                    let replacing =
                        retry || active.as_ref().is_some_and(|stream| stream.scope != scope);
                    if replacing && !retire(&mut context, &mut active, &mut reply).await {
                        continue;
                    }
                    if context.stream_generation(&scope).is_some() {
                        let _ = reply.respond(Ok(FinancialIndexResponse::StreamStarted));
                        continue;
                    }

                    let source = self.source.clone();
                    let stream_scope = scope.clone();
                    let (retirement, retired) = oneshot::channel();
                    context.start_stream(scope.clone(), move |publisher| async move {
                        run_stream(source, stream_scope, publisher, reply, retirement).await;
                    });
                    active = Some(ActiveStream { scope, retired });
                }
            }
        }
    }
}

struct ActiveStream {
    scope: FinancialIndexScope,
    retired: oneshot::Receiver<()>,
}

async fn retire(
    context: &mut ServiceContext<FinancialIndexProtocol>,
    active: &mut Option<ActiveStream>,
    reply: &mut RpcRequestContext<FinancialIndexProtocol>,
) -> bool {
    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
        return false;
    }
    let Some(mut stream) = active.take() else {
        return true;
    };
    context.cancel_stream(&stream.scope);

    let deadline = reply.deadline();
    let retired = tokio::select! {
        biased;
        () = reply.cancelled() => false,
        () = tokio::time::sleep_until(deadline) => false,
        _ = &mut stream.retired => true,
    };
    if !retired {
        *active = Some(stream);
    }
    retired
}

async fn fetch_manifest<S>(
    source: &S,
    reply: &mut RpcRequestContext<FinancialIndexProtocol>,
) -> Option<Result<FinancialIndexResponse, FinancialIndexServiceError>>
where
    S: FinancialIndexSource,
{
    let deadline = reply.deadline();
    let manifest = tokio::select! {
        biased;
        () = reply.cancelled() => return None,
        () = tokio::time::sleep_until(deadline) => Err(KytrixServiceError::Timeout),
        result = source.fetch_manifest() => result,
    };
    let limit = session_limit(&source.session_state());

    Some(
        manifest
            .map_err(FinancialIndexServiceError::from)
            .and_then(|manifest| {
                prepare_catalog(manifest).map_err(FinancialIndexServiceError::from)
            })
            .map(|catalog| FinancialIndexResponse::Manifest { catalog, limit }),
    )
}

async fn run_stream<S>(
    source: S,
    scope: FinancialIndexScope,
    publisher: StreamPublisher<FinancialIndexProtocol>,
    mut reply: RpcRequestContext<FinancialIndexProtocol>,
    retirement: oneshot::Sender<()>,
) where
    S: FinancialIndexSource,
{
    let _retirement = StreamRetirement(Some(retirement));
    let mut published = FinancialIndexStatus::Connecting;
    if publish_status(&publisher, FinancialIndexStatus::Connecting)
        .await
        .is_err()
    {
        return;
    }

    let session = source.session_state();
    let topics = scope.topics.clone();
    let deadline = reply.deadline();
    let setup = tokio::select! {
        biased;
        () = publisher.cancelled() => return,
        () = reply.cancelled() => return,
        () = tokio::time::sleep_until(deadline) => return,
        result = source.subscribe_financial_index(topics.clone()) => result,
    };
    let mut subscription = match setup {
        Ok(subscription) => subscription,
        Err(error) => {
            let error = stream_error(&error, session_limit(&session));
            respond(reply, Ok(FinancialIndexResponse::StreamStarted));
            publish_failure(&publisher, error).await;
            return;
        }
    };
    if !respond(reply, Ok(FinancialIndexResponse::StreamStarted)) {
        return;
    }

    let mut status = subscription.observe_status();
    let mut membership = HashSet::with_capacity(topics.len());
    membership.extend(topics.iter().cloned());
    let mut quotes: HashMap<FinancialIndexTopic, FinancialIndexQuote> =
        HashMap::with_capacity(topics.len());
    let mut quoted = HashSet::with_capacity(topics.len());
    let mut connected = false;

    let current = status.borrow_and_update().clone();
    if !apply_subscription_status(
        &publisher,
        current,
        &topics,
        &mut quoted,
        &mut connected,
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
                    next,
                    &topics,
                    &mut quoted,
                    &mut connected,
                    &mut published,
                )
                .await
                {
                    return;
                }
            }
            event = subscription.recv() => {
                let event = match event {
                    Ok(event) => event,
                    Err(error) => {
                        publish_failure(&publisher, FinancialIndexStreamError::Transport(error)).await;
                        return;
                    }
                };
                if !membership.contains(&event.topic) {
                    continue;
                }
                let quote = FinancialIndexQuote::from_event(&event);
                match quotes.get(&event.topic) {
                    Some(previous) if previous.timestamp > quote.timestamp => continue,
                    Some(previous) if *previous == quote => continue,
                    _ => {}
                }
                quotes.insert(event.topic.clone(), quote);
                quoted.insert(event.topic);
                if publisher
                    .publish_latest(FinancialIndexStreamMessage::Quotes(Arc::new(quotes.clone())))
                    .is_err()
                {
                    return;
                }
                let next = connected_status(connected, quoted.len(), topics.len());
                if publish_change(&publisher, next, &mut published).await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn apply_subscription_status(
    publisher: &StreamPublisher<FinancialIndexProtocol>,
    status: SubscriptionStatus,
    topics: &Arc<[FinancialIndexTopic]>,
    quoted: &mut HashSet<FinancialIndexTopic>,
    connected: &mut bool,
    published: &mut FinancialIndexStatus,
) -> bool {
    match status {
        SubscriptionStatus::Connecting | SubscriptionStatus::Reconnecting => {
            quoted.clear();
            *connected = false;
            publish_change(publisher, FinancialIndexStatus::Connecting, published)
                .await
                .is_ok()
        }
        SubscriptionStatus::Active => {
            *connected = true;
            let next = connected_status(*connected, quoted.len(), topics.len());
            publish_change(publisher, next, published).await.is_ok()
        }
        SubscriptionStatus::Closing => true,
        SubscriptionStatus::Closed => {
            publish_failure(
                publisher,
                FinancialIndexStreamError::Transport(TransportError::Closed),
            )
            .await;
            false
        }
        SubscriptionStatus::Failed(error) => {
            publish_failure(publisher, FinancialIndexStreamError::Transport(error)).await;
            false
        }
    }
}

fn connected_status(connected: bool, quoted: usize, selected: usize) -> FinancialIndexStatus {
    if connected && selected > 0 && quoted == selected {
        FinancialIndexStatus::Ready
    } else {
        FinancialIndexStatus::Connecting
    }
}

fn stream_error(error: &KytrixServiceError, limit: Option<u32>) -> FinancialIndexStreamError {
    match error {
        KytrixServiceError::Server(ErrorCode::FinancialIndexTopicLimitExceeded) => {
            FinancialIndexStreamError::FinancialIndexLimit { limit }
        }
        _ => FinancialIndexStreamError::Service(error.clone()),
    }
}

fn session_limit(session: &watch::Receiver<Option<KytrixSessionState>>) -> Option<u32> {
    let session = session.borrow();
    let state = session.as_ref()?;
    let limit = state
        .entitlements
        .get("financial_indices")
        .and_then(|entitlement| entitlement.value.as_ref())
        .map_or(0, |value| match value {
            proto_types::entitlement_feature_value::Value::Limit(limit) => *limit,
            proto_types::entitlement_feature_value::Value::Enabled(_) => 0,
        });
    Some(limit)
}

fn respond(
    reply: RpcRequestContext<FinancialIndexProtocol>,
    response: Result<FinancialIndexResponse, FinancialIndexServiceError>,
) -> bool {
    reply.respond(response).is_ok()
}

struct StreamRetirement(Option<oneshot::Sender<()>>);

impl Drop for StreamRetirement {
    fn drop(&mut self) {
        if let Some(retirement) = self.0.take() {
            let _ = retirement.send(());
        }
    }
}

async fn publish_status(
    publisher: &StreamPublisher<FinancialIndexProtocol>,
    status: FinancialIndexStatus,
) -> Result<(), StreamPublishError> {
    publisher
        .publish_reliable(FinancialIndexStreamMessage::Status(status))
        .await
}

async fn publish_change(
    publisher: &StreamPublisher<FinancialIndexProtocol>,
    status: FinancialIndexStatus,
    published: &mut FinancialIndexStatus,
) -> Result<(), StreamPublishError> {
    if *published == status {
        return Ok(());
    }
    publish_status(publisher, status.clone()).await?;
    *published = status;
    Ok(())
}

async fn publish_failure(
    publisher: &StreamPublisher<FinancialIndexProtocol>,
    error: FinancialIndexStreamError,
) {
    let _ = publish_status(publisher, FinancialIndexStatus::Failed(error)).await;
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

    use widget::{
        rpc::RpcCallError,
        runtime::WidgetRuntime,
        stream::{LatestStream, ReliableStream},
    };

    use super::*;
    use crate::test_support::{ACCOUNT, FakeSource, event, manifest, selected, session, topic};

    fn scope(codes: &[&str]) -> FinancialIndexScope {
        let selection = codes.iter().map(|code| selected(code)).collect::<Vec<_>>();
        FinancialIndexScope::new(Some(&SmolStr::new_static(ACCOUNT)), &selection)
            .expect("a selection for an account should be a scope")
    }

    fn runtime(source: &FakeSource) -> WidgetRuntime<FinancialIndexProtocol> {
        WidgetRuntime::spawn(
            current_test_handle(),
            FinancialIndexService::new(source.clone()),
        )
    }

    async fn load(runtime: &WidgetRuntime<FinancialIndexProtocol>, scope: FinancialIndexScope) {
        let response = runtime
            .client()
            .call(FinancialIndexRequest::Load(scope))
            .await
            .expect("the stream should start")
            .into_value();
        assert_eq!(response, FinancialIndexResponse::StreamStarted);
    }

    async fn next_reliable(
        reliable: &mut ReliableStream<FinancialIndexProtocol>,
        predicate: impl Fn(&FinancialIndexStreamMessage) -> bool,
    ) -> FinancialIndexStreamMessage {
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

    async fn next_quotes(
        latest: &mut LatestStream<FinancialIndexProtocol>,
        predicate: impl Fn(&HashMap<FinancialIndexTopic, FinancialIndexQuote>) -> bool,
    ) -> HashMap<FinancialIndexTopic, FinancialIndexQuote> {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                latest
                    .changed()
                    .await
                    .expect("the latest stream should stay open");
                while let Some(envelope) = latest.next_unseen() {
                    if let FinancialIndexStreamMessage::Quotes(quotes) = envelope.message().as_ref()
                        && predicate(quotes)
                    {
                        return quotes.as_ref().clone();
                    }
                }
            }
        })
        .await
        .expect("the expected quotes should arrive")
    }

    async fn receive_status(
        reliable: &mut ReliableStream<FinancialIndexProtocol>,
        expected: FinancialIndexStatus,
    ) {
        let status = next_reliable(reliable, |message| {
            matches!(message, FinancialIndexStreamMessage::Status(status) if *status == expected)
        })
        .await;
        assert!(matches!(status, FinancialIndexStreamMessage::Status(_)));
    }

    #[test]
    fn a_scope_needs_both_an_account_and_a_selection() {
        let selection = [selected("HSI")];
        let account = SmolStr::new_static(ACCOUNT);

        assert!(FinancialIndexScope::new(Some(&account), &selection).is_some());
        assert!(FinancialIndexScope::new(None, &selection).is_none());
        assert!(FinancialIndexScope::new(Some(&account), &[]).is_none());
        assert!(FinancialIndexScope::new(Some(&SmolStr::new_static("")), &selection).is_none());
    }

    #[tokio::test]
    async fn manifest_should_cross_the_typed_service_boundary() {
        let source = FakeSource::with_session(3);
        source.set_manifest(manifest());
        let runtime = runtime(&source);

        let response = runtime
            .client()
            .call(FinancialIndexRequest::FetchManifest)
            .await
            .expect("the manifest should load")
            .into_value();
        let FinancialIndexResponse::Manifest { catalog, limit } = response else {
            panic!("the manifest request should answer with a catalogue");
        };

        assert_eq!(catalog.categories.len(), 2);
        assert_eq!(catalog.categories[0].code, "asia");
        assert_eq!(catalog.indices.len(), 2);
        assert_eq!(catalog.indices[0].index.topic.code(), "HSI");
        assert_eq!(limit, Some(3), "the read carries the limit it was made for");
        assert_eq!(source.manifest_calls(), 1);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_manifest_read_should_carry_no_limit_when_the_session_has_none() {
        let source = FakeSource::default();
        source.set_manifest(manifest());
        let runtime = runtime(&source);

        let response = runtime
            .client()
            .call(FinancialIndexRequest::FetchManifest)
            .await
            .expect("the manifest should load")
            .into_value();
        let FinancialIndexResponse::Manifest { limit, .. } = response else {
            panic!("the manifest request should answer with a catalogue");
        };

        assert_eq!(limit, None);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_malformed_manifest_should_fail_the_request_rather_than_the_pane() {
        let source = FakeSource::default();
        let mut broken = manifest();
        broken.indices[0].name = None;
        source.set_manifest(broken);
        let runtime = runtime(&source);

        let error = runtime
            .client()
            .call(FinancialIndexRequest::FetchManifest)
            .await
            .expect_err("a manifest without a name should be refused");

        assert!(matches!(
            error,
            RpcCallError::Service {
                source: FinancialIndexServiceError::Data(_),
                ..
            }
        ));
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn quotes_should_accumulate_and_ignore_a_repeat_or_an_older_event() {
        let source = FakeSource::default();
        let control = source.subscribe();
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let mut latest = runtime.latest_stream();
        let hsi = topic("HSI");
        let spx = topic("SPX");

        load(&runtime, scope(&["HSI", "SPX"])).await;
        receive_status(&mut reliable, FinancialIndexStatus::Connecting).await;
        control
            .events
            .send(Ok(event(hsi.clone(), 101, 2)))
            .await
            .expect("the first quote should be accepted");
        control
            .events
            .send(Ok(event(hsi.clone(), 99, 1)))
            .await
            .expect("the older quote should be seen");
        control
            .events
            .send(Ok(event(hsi.clone(), 101, 2)))
            .await
            .expect("the repeated quote should be seen");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), reliable.recv())
                .await
                .is_err(),
            "one index of two is not a ready scope"
        );

        control
            .events
            .send(Ok(event(spx.clone(), 201, 3)))
            .await
            .expect("the second index should quote");
        receive_status(&mut reliable, FinancialIndexStatus::Ready).await;
        let quotes = next_quotes(&mut latest, |quotes| quotes.len() == 2).await;

        assert_eq!(
            (
                quotes[&hsi].latest,
                quotes[&hsi].timestamp,
                quotes[&spx].latest
            ),
            (101, 2, 201),
            "an older or repeated event must not roll a quote back"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_scope_should_only_be_ready_once_every_index_has_quoted() {
        let source = FakeSource::default();
        let control = source.subscribe();
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let mut latest = runtime.latest_stream();

        load(&runtime, scope(&["HSI", "SPX"])).await;
        receive_status(&mut reliable, FinancialIndexStatus::Connecting).await;

        control
            .events
            .send(Ok(event(topic("HSI"), 101, 1)))
            .await
            .expect("the first index should quote");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), reliable.recv())
                .await
                .is_err(),
            "one index of two is not a ready scope"
        );

        control
            .events
            .send(Ok(event(topic("SPX"), 201, 2)))
            .await
            .expect("the second index should quote");
        receive_status(&mut reliable, FinancialIndexStatus::Ready).await;
        let quotes = next_quotes(&mut latest, |quotes| quotes.len() == 2).await;

        assert_eq!(quotes.len(), 2);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_reconnect_should_take_readiness_back_until_the_scope_quotes_again() {
        let source = FakeSource::default();
        let control = source.subscribe();
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let mut latest = runtime.latest_stream();

        load(&runtime, scope(&["HSI"])).await;
        control
            .events
            .send(Ok(event(topic("HSI"), 101, 1)))
            .await
            .expect("the index should quote");
        let quotes = next_quotes(&mut latest, |quotes| !quotes.is_empty()).await;
        receive_status(&mut reliable, FinancialIndexStatus::Ready).await;

        control
            .status
            .send_replace(SubscriptionStatus::Reconnecting);
        receive_status(&mut reliable, FinancialIndexStatus::Connecting).await;

        control.status.send_replace(SubscriptionStatus::Active);
        control
            .events
            .send(Ok(event(topic("HSI"), 102, 2)))
            .await
            .expect("the re-established subscription should quote");
        receive_status(&mut reliable, FinancialIndexStatus::Ready).await;
        let recovered = next_quotes(&mut latest, |quotes| {
            quotes
                .get(&topic("HSI"))
                .is_some_and(|quote| quote.latest == 102)
        })
        .await;

        assert_eq!(quotes[&topic("HSI")].latest, 101);
        assert_eq!(recovered[&topic("HSI")].latest, 102);
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_selection_change_should_release_the_old_subscription_before_the_new_one() {
        let source = FakeSource::default();
        let first = source.subscribe_holding();
        let second = source.subscribe_after_drop(Some(first.drops.clone()));
        let runtime = runtime(&source);

        load(&runtime, scope(&["HSI"])).await;
        let replacement = tokio::spawn({
            let client = runtime.client();
            async move {
                client
                    .call(FinancialIndexRequest::Load(scope(&["HSI", "SPX"])))
                    .await
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            source.subscribed(),
            vec![vec![topic("HSI")]],
            "a replacement waits for the old subscription to be let go"
        );

        first.release();
        replacement
            .await
            .expect("the replacement call should finish")
            .expect("the replacement should start");
        assert_eq!(
            source.subscribed(),
            vec![vec![topic("HSI")], vec![topic("HSI"), topic("SPX")]]
        );
        assert_eq!(first.drops.load(Ordering::SeqCst), 1);
        assert_eq!(second.drops.load(Ordering::SeqCst), 0);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn loading_a_scope_twice_should_keep_the_stream_it_already_has() {
        let source = FakeSource::default();
        let control = source.subscribe();
        let runtime = runtime(&source);

        load(&runtime, scope(&["HSI"])).await;
        load(&runtime, scope(&["HSI"])).await;

        assert_eq!(source.subscribed(), vec![vec![topic("HSI")]]);
        assert_eq!(control.drops.load(Ordering::SeqCst), 0);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn retry_should_replace_the_stream_of_a_failed_scope() {
        let source = FakeSource::default();
        source.refuse(KytrixServiceError::Closed);
        let recovered = source.subscribe();
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");

        load(&runtime, scope(&["HSI"])).await;
        let failed = next_reliable(&mut reliable, |message| {
            matches!(
                message,
                FinancialIndexStreamMessage::Status(FinancialIndexStatus::Failed(_))
            )
        })
        .await;
        let FinancialIndexStreamMessage::Status(FinancialIndexStatus::Failed(error)) = failed
        else {
            panic!("the stream should have reported a failure");
        };
        assert_eq!(
            error,
            FinancialIndexStreamError::Service(KytrixServiceError::Closed)
        );

        let response = runtime
            .client()
            .call(FinancialIndexRequest::Retry(scope(&["HSI"])))
            .await
            .expect("the retry should open a new stream")
            .into_value();
        assert_eq!(response, FinancialIndexResponse::StreamStarted);
        assert_eq!(source.subscribed(), vec![vec![topic("HSI")]; 2]);
        assert_eq!(recovered.drops.load(Ordering::SeqCst), 0);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn stop_should_release_the_subscription_and_be_idempotent() {
        let source = FakeSource::default();
        let control = source.subscribe();
        let runtime = runtime(&source);

        load(&runtime, scope(&["HSI"])).await;
        for _ in 0..2 {
            let response = runtime
                .client()
                .call(FinancialIndexRequest::Stop)
                .await
                .expect("stopping should be answered")
                .into_value();
            assert_eq!(response, FinancialIndexResponse::Stopped);
        }

        assert_eq!(control.drops.load(Ordering::SeqCst), 1);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_refused_selection_should_report_the_limit_the_session_published() {
        let source = FakeSource::with_session(3);
        source.refuse_selection();
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");

        load(&runtime, scope(&["HSI", "SPX"])).await;
        let failed = next_reliable(&mut reliable, |message| {
            matches!(
                message,
                FinancialIndexStreamMessage::Status(FinancialIndexStatus::Failed(_))
            )
        })
        .await;
        let FinancialIndexStreamMessage::Status(FinancialIndexStatus::Failed(error)) = failed
        else {
            panic!("the stream should have reported a failure");
        };

        assert_eq!(
            error,
            FinancialIndexStreamError::FinancialIndexLimit { limit: Some(3) }
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_refused_selection_should_say_so_when_the_session_has_no_limit_to_give() {
        let source = FakeSource::default();
        source.refuse_selection();
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");

        load(&runtime, scope(&["HSI", "SPX"])).await;
        let failed = next_reliable(&mut reliable, |message| {
            matches!(
                message,
                FinancialIndexStreamMessage::Status(FinancialIndexStatus::Failed(_))
            )
        })
        .await;
        let FinancialIndexStreamMessage::Status(FinancialIndexStatus::Failed(error)) = failed
        else {
            panic!("the stream should have reported a failure");
        };

        assert_eq!(
            error,
            FinancialIndexStreamError::FinancialIndexLimit { limit: None }
        );
        runtime.shutdown().await;
    }

    #[test]
    fn a_session_without_an_indices_entitlement_should_follow_no_indices() {
        let mut state = session(3);
        state.entitlements.remove("financial_indices");
        let (_sender, receiver) = watch::channel(Some(state));

        assert_eq!(session_limit(&receiver), Some(0));
    }

    #[test]
    fn an_indices_entitlement_that_is_a_flag_should_follow_no_indices() {
        let mut state = session(3);
        state.entitlements.insert(
            "financial_indices".to_owned(),
            proto_types::EntitlementFeatureValue {
                value: Some(proto_types::entitlement_feature_value::Value::Enabled(true)),
            },
        );
        let (_sender, receiver) = watch::channel(Some(state));

        assert_eq!(session_limit(&receiver), Some(0));
    }

    #[test]
    fn an_indices_entitlement_without_a_value_should_follow_no_indices() {
        let mut state = session(3);
        state.entitlements.insert(
            "financial_indices".to_owned(),
            proto_types::EntitlementFeatureValue::default(),
        );
        let (_sender, receiver) = watch::channel(Some(state));

        assert_eq!(session_limit(&receiver), Some(0));
    }

    #[test]
    fn no_session_should_leave_the_index_limit_unknown() {
        let (_sender, receiver) = watch::channel(None);

        assert_eq!(session_limit(&receiver), None);
    }

    #[test]
    fn a_session_should_name_the_index_limit_it_carries() {
        let (_sender, receiver) = watch::channel(Some(session(5)));

        assert_eq!(session_limit(&receiver), Some(5));
    }

    #[tokio::test]
    async fn an_oversized_scope_should_be_refused_without_subscribing() {
        let source = FakeSource::default();
        let runtime = runtime(&source);
        let selection = (0..=MAX_SELECTED_INDICES)
            .map(|index| selected(&format!("INDEX_{index}")))
            .collect::<Vec<_>>();
        let oversized = FinancialIndexScope::new(Some(&SmolStr::new_static(ACCOUNT)), &selection)
            .expect("an oversized selection is still a scope");

        let error = runtime
            .client()
            .call(FinancialIndexRequest::Load(oversized))
            .await
            .expect_err("an oversized scope should be refused");

        assert!(matches!(
            error,
            RpcCallError::Service {
                source: FinancialIndexServiceError::Data(_),
                ..
            }
        ));
        assert!(source.subscribed().is_empty());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_caller_that_went_away_should_not_leave_a_stream_behind() {
        let source = FakeSource::default();
        let active = source.subscribe();
        let block = source.block();
        let replacement = source.subscribe_after_drop(Some(active.drops.clone()));
        let runtime = runtime(&source);

        load(&runtime, scope(&["HSI"])).await;

        let client = runtime.client();
        let pending = tokio::spawn(async move {
            client
                .call(FinancialIndexRequest::Load(scope(&["HSI", "SPX"])))
                .await
        });
        block.wait_started().await;
        pending.abort();
        let _ = pending.await;
        block.wait_dropped().await;

        load(&runtime, scope(&["HSI", "SPX"])).await;
        assert_eq!(active.drops.load(Ordering::SeqCst), 1);
        assert_eq!(replacement.drops.load(Ordering::SeqCst), 0);
        runtime.shutdown().await;
    }
}

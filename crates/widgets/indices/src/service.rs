//! The indices protocol, its background service, and the seams a test drives it
//! through.
//!
//! One service instance follows one scope: the account the pane is for, and the
//! topics it selected. The scope is the stream key as well, which is what makes the
//! framework's generations do the filtering — a message can only ever describe the
//! scope it was published for, so the pane never sees one from a selection it has
//! left.
//!
//! # Replacing a selection
//!
//! A replacement releases the subscription it replaces *before* it opens the new
//! one, and the service waits for the old stream to say it has let its subscription
//! go. That wait is not bookkeeping: the service counts every topic a session
//! subscribes to against a single index limit, so a replacement that subscribed
//! while the old selection was still live would briefly ask for the union of both —
//! and a selection that already fills the limit would be refused for the very topics
//! it is replacing. The retirement signal is what makes the two orderings one.
//!
//! # What a status means
//!
//! `Ready` is the protocol's own boundary rather than "the socket is up": the
//! subscription is active *and* every index of the scope has quoted, so a pane that
//! shows `Ready` is a pane whose whole selection is on screen. A reconnect takes
//! that back — the venue is re-establishing the subscription, and what it quoted
//! before says nothing about what it will quote now — while the quotes themselves
//! stay, so a pane keeps showing the last numbers it had instead of blanking.

use std::{future::Future, sync::Arc};

use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use gpui_kit::{App, SharedString};
use kytrix_proto::types as proto_types;
use kytrix_service_provider::service::{
    connector::KytrixServiceConnector,
    engine::{ErrorCode, IndexEvent, IndexTopic, KytrixServiceError, KytrixSessionState},
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
    error::LocalizedError,
    protocol::WidgetProtocol,
    rpc::RpcRequestContext,
    runtime::{ServiceContext, WidgetService},
    stream::{StreamPublishError, StreamPublisher},
};

use super::model::{
    IndexCatalog, IndexQuote, MAX_SELECTED_INDICES, SelectedIndex, prepare_catalog,
    selection_topics,
};

/// What a pane asks its service to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum IndicesRequest {
    /// Follows this scope, keeping the stream that already follows it.
    Load(IndicesScope),
    /// Reopens the scope's stream, for a scope whose stream has failed.
    Retry(IndicesScope),
    /// Follows nothing: nobody is signed in, or nothing is selected.
    Stop,
    /// Reads the catalogue a selector lists.
    FetchManifest,
}

/// What the service did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum IndicesResponse {
    /// The scope's stream is running.
    StreamStarted,
    /// Nothing is followed.
    Stopped,
    /// The catalogue a selector lists, and the number of indices the account may follow.
    Manifest {
        catalog: IndexCatalog,
        limit: Option<u32>,
    },
}

/// The indices one subscription follows, for one account.
///
/// It is the request's subject and the stream's key at once: a scope *is* what a
/// stream is about.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct IndicesScope {
    account_id: SmolStr,
    topics: Arc<[IndexTopic]>,
}

impl IndicesScope {
    /// The scope a selection follows, or `None` when there is nothing to follow.
    ///
    /// Both halves are needed. A selection with nobody signed in is a pane the
    /// service would refuse, and an account with nothing selected is a pane that has
    /// asked for nothing; neither is a scope, so neither gets a stream.
    pub(super) fn new(account_id: Option<&SmolStr>, selection: &[SelectedIndex]) -> Option<Self> {
        if selection.is_empty() {
            return None;
        }
        let account_id = account_id.filter(|account_id| !account_id.is_empty())?;
        Some(Self {
            account_id: account_id.clone(),
            topics: selection_topics(selection),
        })
    }

    /// Whether the service may open this scope.
    ///
    /// The selection is normalized where it is accepted — persisted config and the
    /// selector both cap it — so this is the second of the two places the limit is
    /// enforced rather than a check that a caller is trusted to have made.
    fn validate(&self) -> Result<(), IndicesServiceError> {
        if self.topics.len() > MAX_SELECTED_INDICES {
            return Err(IndicesServiceError::Data(
                super::model::IndicesDataError::SelectionCapacity {
                    limit: MAX_SELECTED_INDICES,
                },
            ));
        }
        Ok(())
    }
}

/// What one subscription is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum IndicesStatus {
    /// The subscription is being established, or has not quoted every index yet.
    Connecting,
    /// Every index of the scope has quoted.
    Ready,
    /// The subscription ended; the pane is finished until the user recovers it.
    Failed(IndicesStreamError),
}

/// Why a subscription stopped, or why it never started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum IndicesStreamError {
    Service(KytrixServiceError),
    Transport(TransportError),
    /// The service refused the selection because the account may follow fewer
    /// indices, with the account's own limit when the session has published one.
    IndexLimit {
        limit: Option<u32>,
    },
}

impl LocalizedError for IndicesStreamError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Service(error) => error.localized_message(cx),
            Self::Transport(error) => error.localized_message(cx),
            Self::IndexLimit { limit: Some(limit) } => {
                // A limit of one is a single index, and the sentence says so.
                if *limit == 1 {
                    i18n::t!(cx, "error.indices.index_limit_one", limit = limit)
                } else {
                    i18n::t!(cx, "error.indices.index_limit", limit = limit)
                }
            }
            Self::IndexLimit { limit: None } => i18n::t!(cx, "error.indices.index_limit_unknown"),
        }
    }
}

/// One message of the scope's stream.
#[derive(Clone, Debug)]
pub(super) enum IndicesStreamMessage {
    Status(IndicesStatus),
    /// Every quote the subscription holds, which only ever grows within a scope.
    Quotes(Arc<HashMap<IndexTopic, IndexQuote>>),
}

pub(super) struct IndicesProtocol;

impl WidgetProtocol for IndicesProtocol {
    type Request = IndicesRequest;
    type Response = IndicesResponse;
    type StreamKey = IndicesScope;
    type StreamMessage = IndicesStreamMessage;
    type Error = IndicesServiceError;
}

/// Why a request could not be answered.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(super) enum IndicesServiceError {
    #[error(transparent)]
    Service(#[from] KytrixServiceError),
    #[error(transparent)]
    Data(#[from] super::model::IndicesDataError),
}

impl LocalizedError for IndicesServiceError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Service(error) => error.localized_message(cx),
            Self::Data(error) => error.localized_message(cx),
        }
    }
}

/// The index data a pane reads through.
///
/// The service is written against this seam rather than against the connector, so a
/// test can drive every ordering a session can produce without a session.
pub(super) trait IndicesSource: Clone + Send + Sync + 'static {
    type Subscription: IndexEventSubscription;

    /// The account's session state, which names the index limit it may follow.
    fn session_state(&self) -> watch::Receiver<Option<KytrixSessionState>>;

    fn fetch_manifest(
        &self,
    ) -> impl Future<Output = Result<proto_types::IndexManifest, KytrixServiceError>> + Send;

    fn subscribe_indices(
        &self,
        topics: Arc<[IndexTopic]>,
    ) -> impl Future<Output = Result<Self::Subscription, KytrixServiceError>> + Send;
}

/// One live index subscription.
pub(super) trait IndexEventSubscription: Send + 'static {
    fn observe_status(&self) -> watch::Receiver<SubscriptionStatus>;

    fn recv(&mut self) -> impl Future<Output = Result<IndexEvent, TransportError>> + Send;
}

impl IndicesSource for KytrixServiceConnector {
    type Subscription = Subscription<IndexEvent>;

    fn session_state(&self) -> watch::Receiver<Option<KytrixSessionState>> {
        KytrixServiceConnector::subscribe_session_state(self)
    }

    fn fetch_manifest(
        &self,
    ) -> impl Future<Output = Result<proto_types::IndexManifest, KytrixServiceError>> + Send {
        KytrixServiceConnector::fetch_index_manifest(self)
    }

    async fn subscribe_indices(
        &self,
        topics: Arc<[IndexTopic]>,
    ) -> Result<Self::Subscription, KytrixServiceError> {
        KytrixServiceConnector::subscribe_indices(self, &topics).await
    }
}

impl IndexEventSubscription for Subscription<IndexEvent> {
    fn observe_status(&self) -> watch::Receiver<SubscriptionStatus> {
        Subscription::observe_status(self)
    }

    fn recv(&mut self) -> impl Future<Output = Result<IndexEvent, TransportError>> + Send {
        Subscription::recv(self)
    }
}

/// The service that follows one pane's indices.
pub(super) struct IndicesService<S = KytrixServiceConnector> {
    source: S,
}

impl<S> IndicesService<S> {
    pub(super) const fn new(source: S) -> Self {
        Self { source }
    }
}

impl<S> WidgetService<IndicesProtocol> for IndicesService<S>
where
    S: IndicesSource,
{
    async fn run(self, mut context: ServiceContext<IndicesProtocol>) {
        let mut active: Option<ActiveStream> = None;

        while let Some(request) = context.next_rpc().await {
            let (mut reply, request) = request.into_parts();
            match request {
                IndicesRequest::FetchManifest => {
                    if let Some(result) = fetch_manifest(&self.source, &mut reply).await {
                        let _ = reply.respond(result);
                    }
                }
                IndicesRequest::Stop => {
                    if retire(&mut context, &mut active, &mut reply).await {
                        let _ = reply.respond(Ok(IndicesResponse::Stopped));
                    }
                }
                request @ (IndicesRequest::Load(_) | IndicesRequest::Retry(_)) => {
                    let (scope, retry) = match request {
                        IndicesRequest::Load(scope) => (scope, false),
                        IndicesRequest::Retry(scope) => (scope, true),
                        IndicesRequest::Stop | IndicesRequest::FetchManifest => continue,
                    };
                    if let Err(error) = scope.validate() {
                        let _ = reply.respond(Err(error));
                        continue;
                    }
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }

                    // A scope is what a stream is about, so a different one replaces
                    // the stream the service is running, and the same one is only
                    // reopened because the pane is retrying it.
                    let replacing =
                        retry || active.as_ref().is_some_and(|stream| stream.scope != scope);
                    if replacing && !retire(&mut context, &mut active, &mut reply).await {
                        continue;
                    }
                    if context.stream_generation(&scope).is_some() {
                        let _ = reply.respond(Ok(IndicesResponse::StreamStarted));
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

/// The stream the service is running, and the way it says its subscription is gone.
struct ActiveStream {
    scope: IndicesScope,
    retired: oneshot::Receiver<()>,
}

/// Releases the running stream and waits for its subscription to be let go.
///
/// Returns whether the caller may go on: a retirement that did not arrive before the
/// request's deadline leaves the old subscription in a state nobody can vouch for,
/// and opening a second one on top of it is exactly what the wait exists to prevent.
async fn retire(
    context: &mut ServiceContext<IndicesProtocol>,
    active: &mut Option<ActiveStream>,
    reply: &mut RpcRequestContext<IndicesProtocol>,
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
    reply: &mut RpcRequestContext<IndicesProtocol>,
) -> Option<Result<IndicesResponse, IndicesServiceError>>
where
    S: IndicesSource,
{
    let deadline = reply.deadline();
    let manifest = tokio::select! {
        biased;
        () = reply.cancelled() => return None,
        () = tokio::time::sleep_until(deadline) => Err(KytrixServiceError::Timeout),
        result = source.fetch_manifest() => result,
    };
    // The limit travels with the catalogue rather than as a read of its own: a selector
    // draws the two together, and two reads could disagree about the account they
    // describe. Only a pane the service has no session for leaves the selector nothing to
    // draw: a feature the session does not carry is off, so a session always names a limit.
    let limit = session_limit(&source.session_state());

    Some(
        manifest
            .map_err(IndicesServiceError::from)
            .and_then(|manifest| prepare_catalog(manifest).map_err(IndicesServiceError::from))
            .map(|catalog| IndicesResponse::Manifest { catalog, limit }),
    )
}

/// Runs the scope's subscription, and answers the request that opened it.
///
/// The answer is sent from here rather than by the loop because the subscription is
/// what it answers for: a caller that went away while the subscription was being
/// established takes the setup down with it instead of leaving a subscription nobody
/// asked for. What the answer says is that the service took the scope — whether the
/// subscription then works is the stream's own status to report, because that is
/// where the failures a pane shows already live.
async fn run_stream<S>(
    source: S,
    scope: IndicesScope,
    publisher: StreamPublisher<IndicesProtocol>,
    mut reply: RpcRequestContext<IndicesProtocol>,
    retirement: oneshot::Sender<()>,
) where
    S: IndicesSource,
{
    // Declared before everything else, so that the subscription and the setup that
    // produced it are dropped before the service is told they are gone: the signal
    // is what a replacement waits on, and it has to mean what it says.
    let _retirement = StreamRetirement(Some(retirement));
    let mut published = IndicesStatus::Connecting;
    if publish_status(&publisher, IndicesStatus::Connecting)
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
        result = source.subscribe_indices(topics.clone()) => result,
    };
    let mut subscription = match setup {
        Ok(subscription) => subscription,
        Err(error) => {
            let error = stream_error(&error, session_limit(&session));
            respond(reply, Ok(IndicesResponse::StreamStarted));
            publish_failure(&publisher, error).await;
            return;
        }
    };
    if !respond(reply, Ok(IndicesResponse::StreamStarted)) {
        return;
    }

    let mut status = subscription.observe_status();
    let mut membership = HashSet::with_capacity(topics.len());
    membership.extend(topics.iter().cloned());
    let mut quotes: HashMap<IndexTopic, IndexQuote> = HashMap::with_capacity(topics.len());
    let mut quoted = HashSet::with_capacity(topics.len());
    let mut connected = false;

    // A subscription can already be up before its first change, so where it is now is
    // where the stream starts.
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
                        publish_failure(&publisher, IndicesStreamError::Transport(error)).await;
                        return;
                    }
                };
                if !membership.contains(&event.topic) {
                    continue;
                }
                let quote = IndexQuote::from_event(&event);
                match quotes.get(&event.topic) {
                    // The venue's own clock orders two quotes of one index, so an
                    // older one is not a correction of a newer one.
                    Some(previous) if previous.timestamp > quote.timestamp => continue,
                    Some(previous) if *previous == quote => continue,
                    _ => {}
                }
                quotes.insert(event.topic.clone(), quote);
                quoted.insert(event.topic);
                if publisher
                    .publish_latest(IndicesStreamMessage::Quotes(Arc::new(quotes.clone())))
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

/// Applies one subscription status, which the stream's own readiness follows.
///
/// A reconnect gives up the readiness the scope had: the venue is re-establishing
/// the subscription, so the quotes that arrive next are the ones that decide it
/// again. The quotes already held are kept, because a pane that is reconnecting
/// should show the last numbers it had rather than nothing.
///
/// Returns whether the stream may keep running: a status that ended it has already
/// published why, and the task settles on the pane that will replace it.
async fn apply_subscription_status(
    publisher: &StreamPublisher<IndicesProtocol>,
    status: SubscriptionStatus,
    topics: &Arc<[IndexTopic]>,
    quoted: &mut HashSet<IndexTopic>,
    connected: &mut bool,
    published: &mut IndicesStatus,
) -> bool {
    match status {
        SubscriptionStatus::Connecting | SubscriptionStatus::Reconnecting => {
            quoted.clear();
            *connected = false;
            publish_change(publisher, IndicesStatus::Connecting, published)
                .await
                .is_ok()
        }
        SubscriptionStatus::Active => {
            *connected = true;
            let next = connected_status(*connected, quoted.len(), topics.len());
            publish_change(publisher, next, published).await.is_ok()
        }
        // A closing subscription is on its way to a terminal status of its own, and
        // that is where the failure is reported from.
        SubscriptionStatus::Closing => true,
        SubscriptionStatus::Closed => {
            publish_failure(
                publisher,
                IndicesStreamError::Transport(TransportError::Closed),
            )
            .await;
            false
        }
        SubscriptionStatus::Failed(error) => {
            publish_failure(publisher, IndicesStreamError::Transport(error)).await;
            false
        }
    }
}

/// The status a subscription that is up has: ready once every index has quoted.
fn connected_status(connected: bool, quoted: usize, selected: usize) -> IndicesStatus {
    if connected && selected > 0 && quoted == selected {
        IndicesStatus::Ready
    } else {
        IndicesStatus::Connecting
    }
}

/// What a refused subscription means to the pane.
///
/// A selection the account may not follow is the one refusal that is about the
/// selection rather than about the connection, so it carries the number of indices
/// the account may follow — the number the session state published, when it has
/// published one.
fn stream_error(error: &KytrixServiceError, limit: Option<u32>) -> IndicesStreamError {
    match error {
        KytrixServiceError::Server(ErrorCode::IndexTopicLimitExceeded) => {
            IndicesStreamError::IndexLimit { limit }
        }
        _ => IndicesStreamError::Service(error.clone()),
    }
}

/// The number of indices the signed-in account may follow, as the session holds it.
///
/// A session that exists always names a limit, because the entitlement contract makes a
/// feature the session does not carry off: a missing `indices` entry, or one that is not
/// a limit, is an account that may follow no indices rather than one whose limit nobody
/// knows. `None` is only for a pane there is no session for at all.
fn session_limit(session: &watch::Receiver<Option<KytrixSessionState>>) -> Option<u32> {
    let session = session.borrow();
    let state = session.as_ref()?;
    let limit = state
        .entitlements
        .get("indices")
        .and_then(|entitlement| entitlement.value.as_ref())
        .map_or(0, |value| match value {
            proto_types::entitlement_value::Value::Limit(limit) => *limit,
            proto_types::entitlement_value::Value::Enabled(_) => 0,
        });
    Some(limit)
}

/// Answers the request that opened one stream.
///
/// Returns whether the caller was still there to hear it. A caller that went away
/// gets no answer, and everything the answer was for — the subscription included —
/// goes with it rather than being left for a pane that has moved on.
fn respond(
    reply: RpcRequestContext<IndicesProtocol>,
    response: Result<IndicesResponse, IndicesServiceError>,
) -> bool {
    reply.respond(response).is_ok()
}

/// Tells the service that this stream has let its subscription go.
///
/// The signal is sent when the task is dropped, which is what lets a replacement
/// wait for the old subscription instead of racing it.
struct StreamRetirement(Option<oneshot::Sender<()>>);

impl Drop for StreamRetirement {
    fn drop(&mut self) {
        if let Some(retirement) = self.0.take() {
            let _ = retirement.send(());
        }
    }
}

/// Publishes a status the pane has not been told yet.
async fn publish_status(
    publisher: &StreamPublisher<IndicesProtocol>,
    status: IndicesStatus,
) -> Result<(), StreamPublishError> {
    publisher
        .publish_reliable(IndicesStreamMessage::Status(status))
        .await
}

/// Publishes a status change, if it is one.
async fn publish_change(
    publisher: &StreamPublisher<IndicesProtocol>,
    status: IndicesStatus,
    published: &mut IndicesStatus,
) -> Result<(), StreamPublishError> {
    if *published == status {
        return Ok(());
    }
    publish_status(publisher, status.clone()).await?;
    *published = status;
    Ok(())
}

/// Publishes the failure a stream ended with, and holds the scope it took.
///
/// A terminal failure is not retried: the pane asks for a new stream when the user
/// takes the recovery, and until then this task stays where it is so the framework
/// still sees the scope as taken.
async fn publish_failure(publisher: &StreamPublisher<IndicesProtocol>, error: IndicesStreamError) {
    let _ = publish_status(publisher, IndicesStatus::Failed(error)).await;
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

    use widget::{
        rpc::RpcCallError,
        runtime::WidgetRuntime,
        stream::{LatestStream, ReliableStream},
    };

    use super::*;
    use crate::test_support::{ACCOUNT, FakeSource, event, manifest, selected, session, topic};

    fn scope(codes: &[&str]) -> IndicesScope {
        let selection = codes.iter().map(|code| selected(code)).collect::<Vec<_>>();
        IndicesScope::new(Some(&SmolStr::new_static(ACCOUNT)), &selection)
            .expect("a selection for an account should be a scope")
    }

    fn runtime(source: &FakeSource) -> WidgetRuntime<IndicesProtocol> {
        WidgetRuntime::spawn(current_test_handle(), IndicesService::new(source.clone()))
    }

    async fn load(runtime: &WidgetRuntime<IndicesProtocol>, scope: IndicesScope) {
        let response = runtime
            .client()
            .call(IndicesRequest::Load(scope))
            .await
            .expect("the stream should start")
            .into_value();
        assert_eq!(response, IndicesResponse::StreamStarted);
    }

    async fn next_reliable(
        reliable: &mut ReliableStream<IndicesProtocol>,
        predicate: impl Fn(&IndicesStreamMessage) -> bool,
    ) -> IndicesStreamMessage {
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
        latest: &mut LatestStream<IndicesProtocol>,
        predicate: impl Fn(&HashMap<IndexTopic, IndexQuote>) -> bool,
    ) -> HashMap<IndexTopic, IndexQuote> {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                latest
                    .changed()
                    .await
                    .expect("the latest stream should stay open");
                while let Some(envelope) = latest.next_unseen() {
                    if let IndicesStreamMessage::Quotes(quotes) = envelope.message().as_ref()
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
        reliable: &mut ReliableStream<IndicesProtocol>,
        expected: IndicesStatus,
    ) {
        let status = next_reliable(reliable, |message| {
            matches!(message, IndicesStreamMessage::Status(status) if *status == expected)
        })
        .await;
        assert!(matches!(status, IndicesStreamMessage::Status(_)));
    }

    #[test]
    fn a_scope_needs_both_an_account_and_a_selection() {
        let selection = [selected("HSI")];
        let account = SmolStr::new_static(ACCOUNT);

        assert!(IndicesScope::new(Some(&account), &selection).is_some());
        assert!(IndicesScope::new(None, &selection).is_none());
        assert!(IndicesScope::new(Some(&account), &[]).is_none());
        assert!(IndicesScope::new(Some(&SmolStr::new_static("")), &selection).is_none());
    }

    #[tokio::test]
    async fn manifest_should_cross_the_typed_service_boundary() {
        let source = FakeSource::with_session(3);
        source.set_manifest(manifest());
        let runtime = runtime(&source);

        let response = runtime
            .client()
            .call(IndicesRequest::FetchManifest)
            .await
            .expect("the manifest should load")
            .into_value();
        let IndicesResponse::Manifest { catalog, limit } = response else {
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

    /// A session that has published no state is an account the service does not know,
    /// and a limit nobody published is not one a selector may draw.
    #[tokio::test]
    async fn a_manifest_read_should_carry_no_limit_when_the_session_has_none() {
        let source = FakeSource::default();
        source.set_manifest(manifest());
        let runtime = runtime(&source);

        let response = runtime
            .client()
            .call(IndicesRequest::FetchManifest)
            .await
            .expect("the manifest should load")
            .into_value();
        let IndicesResponse::Manifest { limit, .. } = response else {
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
            .call(IndicesRequest::FetchManifest)
            .await
            .expect_err("a manifest without a name should be refused");

        assert!(matches!(
            error,
            RpcCallError::Service {
                source: IndicesServiceError::Data(_),
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
        receive_status(&mut reliable, IndicesStatus::Connecting).await;
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
        receive_status(&mut reliable, IndicesStatus::Ready).await;
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
        receive_status(&mut reliable, IndicesStatus::Connecting).await;

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
        receive_status(&mut reliable, IndicesStatus::Ready).await;
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
        receive_status(&mut reliable, IndicesStatus::Ready).await;

        control
            .status
            .send_replace(SubscriptionStatus::Reconnecting);
        receive_status(&mut reliable, IndicesStatus::Connecting).await;

        control.status.send_replace(SubscriptionStatus::Active);
        control
            .events
            .send(Ok(event(topic("HSI"), 102, 2)))
            .await
            .expect("the re-established subscription should quote");
        receive_status(&mut reliable, IndicesStatus::Ready).await;
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

    /// The replacement of a live scope is the one ordering a session is unforgiving
    /// about: it counts the topics of every subscription against one index limit, so a
    /// replacement that subscribed before the previous subscription was gone would
    /// briefly ask for the union of both. The fake refuses that the way a session at
    /// its limit would, and holds the old subscription until the test lets it go — so
    /// this fails if the service does not wait for the release.
    ///
    /// It runs on its own threads because the hold happens inside the venue's own drop:
    /// on a single-threaded runtime that hold would block the test as well, and the
    /// assertion below could not tell a service that waited from one that did not.
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
                    .call(IndicesRequest::Load(scope(&["HSI", "SPX"])))
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
                IndicesStreamMessage::Status(IndicesStatus::Failed(_))
            )
        })
        .await;
        let IndicesStreamMessage::Status(IndicesStatus::Failed(error)) = failed else {
            panic!("the stream should have reported a failure");
        };
        assert_eq!(
            error,
            IndicesStreamError::Service(KytrixServiceError::Closed)
        );

        let response = runtime
            .client()
            .call(IndicesRequest::Retry(scope(&["HSI"])))
            .await
            .expect("the retry should open a new stream")
            .into_value();
        assert_eq!(response, IndicesResponse::StreamStarted);
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
                .call(IndicesRequest::Stop)
                .await
                .expect("stopping should be answered")
                .into_value();
            assert_eq!(response, IndicesResponse::Stopped);
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
                IndicesStreamMessage::Status(IndicesStatus::Failed(_))
            )
        })
        .await;
        let IndicesStreamMessage::Status(IndicesStatus::Failed(error)) = failed else {
            panic!("the stream should have reported a failure");
        };

        assert_eq!(error, IndicesStreamError::IndexLimit { limit: Some(3) });
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
                IndicesStreamMessage::Status(IndicesStatus::Failed(_))
            )
        })
        .await;
        let IndicesStreamMessage::Status(IndicesStatus::Failed(error)) = failed else {
            panic!("the stream should have reported a failure");
        };

        assert_eq!(error, IndicesStreamError::IndexLimit { limit: None });
        runtime.shutdown().await;
    }

    /// A session that exists always names a limit: the contract makes a feature the
    /// session does not carry off, so a missing entry is an account that may follow no
    /// indices rather than one whose limit nobody knows.
    #[test]
    fn a_session_without_an_indices_entitlement_should_follow_no_indices() {
        let mut state = session(3);
        state.entitlements.remove("indices");
        let (_sender, receiver) = watch::channel(Some(state));

        assert_eq!(session_limit(&receiver), Some(0));
    }

    /// A feature whose value is a flag is not a number, so it cannot be a limit either;
    /// an enabled flag still says nothing about how many indices may be followed.
    #[test]
    fn an_indices_entitlement_that_is_a_flag_should_follow_no_indices() {
        let mut state = session(3);
        state.entitlements.insert(
            "indices".to_owned(),
            proto_types::EntitlementValue {
                value: Some(proto_types::entitlement_value::Value::Enabled(true)),
            },
        );
        let (_sender, receiver) = watch::channel(Some(state));

        assert_eq!(session_limit(&receiver), Some(0));
    }

    /// An entitlement with neither branch set is not a message a session should publish,
    /// and the contract reads what it does not know as off.
    #[test]
    fn an_indices_entitlement_without_a_value_should_follow_no_indices() {
        let mut state = session(3);
        state.entitlements.insert(
            "indices".to_owned(),
            proto_types::EntitlementValue::default(),
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
        let oversized = IndicesScope::new(Some(&SmolStr::new_static(ACCOUNT)), &selection)
            .expect("an oversized selection is still a scope");

        let error = runtime
            .client()
            .call(IndicesRequest::Load(oversized))
            .await
            .expect_err("an oversized scope should be refused");

        assert!(matches!(
            error,
            RpcCallError::Service {
                source: IndicesServiceError::Data(_),
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
                .call(IndicesRequest::Load(scope(&["HSI", "SPX"])))
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

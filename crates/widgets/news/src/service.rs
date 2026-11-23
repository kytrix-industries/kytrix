use std::{convert::Infallible, future::Future};

use i18n::{t_smol, translate::Translate};
use kytrix_service_provider::service::{
    connector::KytrixServiceConnector,
    engine::{ErrorCode, KytrixServiceError, NewsEvent},
};
use network::transport::{
    error::TransportError,
    subscription::{Subscription, SubscriptionStatus},
};
use smol_str::SmolStr;
use tokio::time::Instant;
use widget::{
    error::TranslatedError,
    protocol::WidgetProtocol,
    rpc::RpcRequestContext,
    runtime::{ServiceContext, WidgetService},
    stream::{StreamPublishError, StreamPublisher},
};

use super::model::{MAX_NEWS_ITEMS, NewsSnapshot, NewsState};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum NewsRequest {
    Load(NewsScope),
    Retry(NewsScope),
    Stop,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum NewsResponse {
    StreamStarted,
    Stopped,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct NewsScope {
    account_id: SmolStr,
}

impl NewsScope {
    pub(super) fn new(account_id: Option<&SmolStr>) -> Option<Self> {
        let account_id = account_id.filter(|account_id| !account_id.is_empty())?;
        Some(Self {
            account_id: account_id.clone(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum NewsStatus {
    Connecting,
    Ready,
    Failed(NewsStreamError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum NewsStreamError {
    Service(KytrixServiceError),
    EntitlementRequired,
    Transport(TransportError),
}

impl TranslatedError for NewsStreamError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Service(error) => error.translated_message(translate),
            Self::EntitlementRequired => t_smol!(translate, "error.news.entitlement_required"),
            Self::Transport(error) => error.translated_message(translate),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum NewsStreamMessage {
    Status(NewsStatus),
    Snapshot(NewsSnapshot),
}

pub(super) struct NewsProtocol;

impl WidgetProtocol for NewsProtocol {
    type Request = NewsRequest;
    type Response = NewsResponse;
    type StreamKey = NewsScope;
    type StreamMessage = NewsStreamMessage;
    type Error = Infallible;
}

pub(super) trait NewsSource: Clone + Send + Sync + 'static {
    type Subscription: NewsEventSubscription;

    fn subscribe_news(
        &self,
    ) -> impl Future<Output = Result<Self::Subscription, KytrixServiceError>> + Send;
}

pub(super) trait NewsEventSubscription: Send + 'static {
    fn observe_status(&self) -> tokio::sync::watch::Receiver<SubscriptionStatus>;

    fn recv(&mut self) -> impl Future<Output = Result<NewsEvent, TransportError>> + Send;
}

impl NewsSource for KytrixServiceConnector {
    type Subscription = Subscription<NewsEvent>;

    async fn subscribe_news(&self) -> Result<Self::Subscription, KytrixServiceError> {
        KytrixServiceConnector::subscribe_news(self).await
    }
}

impl NewsEventSubscription for Subscription<NewsEvent> {
    fn observe_status(&self) -> tokio::sync::watch::Receiver<SubscriptionStatus> {
        Subscription::observe_status(self)
    }

    fn recv(&mut self) -> impl Future<Output = Result<NewsEvent, TransportError>> + Send {
        Subscription::recv(self)
    }
}

pub(super) struct NewsService<S = KytrixServiceConnector> {
    source: S,
}

impl<S> NewsService<S> {
    pub(super) const fn new(source: S) -> Self {
        Self { source }
    }
}

impl<S> WidgetService<NewsProtocol> for NewsService<S>
where
    S: NewsSource,
{
    async fn run(self, mut context: ServiceContext<NewsProtocol>) {
        let mut active: Option<NewsScope> = None;

        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            match request {
                NewsRequest::Stop => {
                    if let Some(scope) = active.take() {
                        context.cancel_stream(&scope);
                    }
                    let _ = reply.respond(Ok(NewsResponse::Stopped));
                }
                request @ (NewsRequest::Load(_) | NewsRequest::Retry(_)) => {
                    let (scope, retry) = match request {
                        NewsRequest::Load(scope) => (scope, false),
                        NewsRequest::Retry(scope) => (scope, true),
                        NewsRequest::Stop => continue,
                    };
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }

                    if (retry || active.as_ref().is_some_and(|active| active != &scope))
                        && let Some(active) = active.take()
                    {
                        context.cancel_stream(&active);
                    }
                    if context.stream_generation(&scope).is_some() {
                        let _ = reply.respond(Ok(NewsResponse::StreamStarted));
                        continue;
                    }

                    let source = self.source.clone();
                    context.start_stream(scope.clone(), move |publisher| async move {
                        run_stream(source, publisher, reply).await;
                    });
                    active = Some(scope);
                }
            }
        }
    }
}

async fn run_stream<S>(
    source: S,
    publisher: StreamPublisher<NewsProtocol>,
    mut reply: RpcRequestContext<NewsProtocol>,
) where
    S: NewsSource,
{
    let mut published = NewsStatus::Connecting;
    if publish_status(&publisher, NewsStatus::Connecting)
        .await
        .is_err()
    {
        return;
    }

    let deadline = reply.deadline();
    let setup = tokio::select! {
        biased;
        () = publisher.cancelled() => return,
        () = reply.cancelled() => return,
        () = tokio::time::sleep_until(deadline) => return,
        result = source.subscribe_news() => result,
    };
    let mut subscription = match setup {
        Ok(subscription) => subscription,
        Err(error) => {
            let _ = reply.respond(Ok(NewsResponse::StreamStarted));
            publish_failure(&publisher, stream_error(&error)).await;
            return;
        }
    };
    if reply.respond(Ok(NewsResponse::StreamStarted)).is_err() {
        return;
    }

    let mut status = subscription.observe_status();
    let mut state = NewsState::new(MAX_NEWS_ITEMS);
    let mut feed_published = false;

    let current = status.borrow_and_update().clone();
    if !apply_subscription_status(
        &publisher,
        current,
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
                    next,
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
                let event = match event {
                    Ok(event) => event,
                    Err(error) => {
                        publish_failure(&publisher, NewsStreamError::Transport(error)).await;
                        return;
                    }
                };
                if !state.push(event) {
                    continue;
                }
                if publisher
                    .publish_latest(NewsStreamMessage::Snapshot(state.snapshot()))
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
    publisher: &StreamPublisher<NewsProtocol>,
    status: SubscriptionStatus,
    state: &NewsState,
    feed_published: &mut bool,
    published: &mut NewsStatus,
) -> bool {
    match status {
        SubscriptionStatus::Connecting | SubscriptionStatus::Reconnecting => {
            publish_change(publisher, NewsStatus::Connecting, published)
                .await
                .is_ok()
        }
        SubscriptionStatus::Active => {
            if !*feed_published {
                if publisher
                    .publish_latest(NewsStreamMessage::Snapshot(state.snapshot()))
                    .is_err()
                {
                    return false;
                }
                *feed_published = true;
            }
            publish_change(publisher, NewsStatus::Ready, published)
                .await
                .is_ok()
        }
        SubscriptionStatus::Closing => true,
        SubscriptionStatus::Closed => {
            publish_failure(
                publisher,
                NewsStreamError::Transport(TransportError::Closed),
            )
            .await;
            false
        }
        SubscriptionStatus::Failed(error) => {
            publish_failure(publisher, NewsStreamError::Transport(error)).await;
            false
        }
    }
}

fn stream_error(error: &KytrixServiceError) -> NewsStreamError {
    match error {
        KytrixServiceError::Server(ErrorCode::EntitlementRequired) => {
            NewsStreamError::EntitlementRequired
        }
        _ => NewsStreamError::Service(error.clone()),
    }
}

async fn publish_status(
    publisher: &StreamPublisher<NewsProtocol>,
    status: NewsStatus,
) -> Result<(), StreamPublishError> {
    publisher
        .publish_reliable(NewsStreamMessage::Status(status))
        .await
}

async fn publish_change(
    publisher: &StreamPublisher<NewsProtocol>,
    status: NewsStatus,
    published: &mut NewsStatus,
) -> Result<(), StreamPublishError> {
    if *published == status {
        return Ok(());
    }
    publish_status(publisher, status.clone()).await?;
    *published = status;
    Ok(())
}

async fn publish_failure(publisher: &StreamPublisher<NewsProtocol>, error: NewsStreamError) {
    let _ = publish_status(publisher, NewsStatus::Failed(error)).await;
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
        runtime::WidgetRuntime,
        stream::{LatestStream, ReliableStream},
    };

    use super::*;
    use crate::test_support::{ACCOUNT, FakeSource, event};

    fn account(name: &'static str) -> NewsScope {
        NewsScope::new(Some(&SmolStr::new_static(name))).expect("an account should be a scope")
    }

    fn scope() -> NewsScope {
        account(ACCOUNT)
    }

    fn runtime(source: &FakeSource) -> WidgetRuntime<NewsProtocol> {
        WidgetRuntime::spawn(current_test_handle(), NewsService::new(source.clone()))
    }

    async fn load(runtime: &WidgetRuntime<NewsProtocol>, scope: NewsScope) {
        let response = runtime
            .client()
            .call(NewsRequest::Load(scope))
            .await
            .expect("the stream should start")
            .into_value();
        assert_eq!(response, NewsResponse::StreamStarted);
    }

    async fn next_reliable(
        reliable: &mut ReliableStream<NewsProtocol>,
        predicate: impl Fn(&NewsStreamMessage) -> bool,
    ) -> NewsStreamMessage {
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

    async fn receive_status(reliable: &mut ReliableStream<NewsProtocol>, expected: NewsStatus) {
        let status = next_reliable(
            reliable,
            |message| matches!(message, NewsStreamMessage::Status(status) if *status == expected),
        )
        .await;
        assert!(matches!(status, NewsStreamMessage::Status(_)));
    }

    async fn receive_failure(reliable: &mut ReliableStream<NewsProtocol>) -> NewsStreamError {
        let failure = next_reliable(reliable, |message| {
            matches!(message, NewsStreamMessage::Status(NewsStatus::Failed(_)))
        })
        .await;
        let NewsStreamMessage::Status(NewsStatus::Failed(error)) = failure else {
            panic!("the stream should have reported a failure");
        };
        error
    }

    async fn next_feed(
        latest: &mut LatestStream<NewsProtocol>,
        predicate: impl Fn(&NewsSnapshot) -> bool,
    ) -> NewsSnapshot {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                latest
                    .changed()
                    .await
                    .expect("the latest stream should stay open");
                while let Some(envelope) = latest.next_unseen() {
                    if let NewsStreamMessage::Snapshot(snapshot) = envelope.message().as_ref()
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

    fn contents(snapshot: &NewsSnapshot) -> Vec<String> {
        snapshot
            .items()
            .iter()
            .map(|item| item.content().to_string())
            .collect()
    }

    #[tokio::test]
    async fn the_stream_should_publish_the_feed_and_let_its_subscription_go_on_shutdown() {
        let source = FakeSource::default();
        let control = source.subscribe();
        let mut runtime = runtime(&source);
        let mut latest = runtime.latest_stream();
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");

        load(&runtime, scope()).await;
        receive_status(&mut reliable, NewsStatus::Ready).await;
        let live = latest
            .next_unseen()
            .expect("a live feed is published with the readiness it comes with");
        let NewsStreamMessage::Snapshot(live) = live.message().as_ref() else {
            panic!("the latest stream should hold the feed");
        };
        assert!(
            live.items().is_empty(),
            "a live feed is published even before it holds anything"
        );

        control
            .events
            .send(Ok(event(1)))
            .await
            .expect("the live subscription should accept news");
        let feed = next_feed(&mut latest, |snapshot| !snapshot.items().is_empty()).await;
        assert_eq!(contents(&feed), ["News 1"]);

        for timestamp in 2..=(MAX_NEWS_ITEMS as i64 + 2) {
            control
                .events
                .send(Ok(event(timestamp)))
                .await
                .expect("newer news should be accepted");
        }
        let feed = next_feed(&mut latest, |snapshot| {
            snapshot.items().len() == MAX_NEWS_ITEMS
        })
        .await;
        assert_eq!(
            contents(&feed)[0],
            format!("News {}", MAX_NEWS_ITEMS + 2),
            "the feed reads newest first and holds only what it may"
        );

        control
            .events
            .send(Ok(event(0)))
            .await
            .expect("older news should reach the service");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), latest.changed())
                .await
                .is_err(),
            "news older than the whole feed is refused rather than published"
        );

        runtime.shutdown().await;
        assert_eq!(
            control.drops.load(Ordering::SeqCst),
            1,
            "shutdown should let the subscription go"
        );
    }

    #[tokio::test]
    async fn another_account_should_replace_the_stream_and_let_the_old_subscription_go() {
        let source = FakeSource::default();
        let first = source.subscribe();
        let second = source.subscribe();
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");

        load(&runtime, account("first")).await;
        receive_status(&mut reliable, NewsStatus::Ready).await;
        load(&runtime, account("second")).await;
        receive_status(&mut reliable, NewsStatus::Ready).await;

        assert_eq!(source.subscriptions(), 2);
        assert_eq!(
            first.drops.load(Ordering::SeqCst),
            1,
            "the subscription of an account the pane left is not left running"
        );
        assert!(
            first.events.send(Ok(event(i64::MAX))).await.is_err(),
            "a retired subscription accepts nothing more"
        );
        assert_eq!(second.drops.load(Ordering::SeqCst), 0);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_reconnect_should_return_to_live() {
        let source = FakeSource::default();
        let control = source.subscribe();
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let mut latest = runtime.latest_stream();

        load(&runtime, scope()).await;
        receive_status(&mut reliable, NewsStatus::Ready).await;

        control
            .status
            .send_replace(SubscriptionStatus::Reconnecting);
        receive_status(&mut reliable, NewsStatus::Connecting).await;

        control.status.send_replace(SubscriptionStatus::Active);
        receive_status(&mut reliable, NewsStatus::Ready).await;

        control
            .events
            .send(Ok(event(2)))
            .await
            .expect("the re-established subscription should accept news");
        let feed = next_feed(&mut latest, |snapshot| !snapshot.items().is_empty()).await;
        assert_eq!(
            contents(&feed),
            ["News 2"],
            "the news a pane kept across a reconnect is not republished as new"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_status_the_pane_already_has_should_not_be_published_twice() {
        let source = FakeSource::default();
        let control = source.subscribe();
        control.status.send_replace(SubscriptionStatus::Connecting);
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");

        load(&runtime, scope()).await;
        control.status.send_replace(SubscriptionStatus::Active);
        for expected in [NewsStatus::Connecting, NewsStatus::Ready] {
            receive_status(&mut reliable, expected).await;
        }

        assert!(
            tokio::time::timeout(Duration::from_millis(50), reliable.recv())
                .await
                .is_err(),
            "a status the pane already reads must not be repeated"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_missing_entitlement_should_be_named_as_a_refusal() {
        let source = FakeSource::default();
        source.refuse_entitlement();
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let mut latest = runtime.latest_stream();

        load(&runtime, scope()).await;

        assert_eq!(
            receive_failure(&mut reliable).await,
            NewsStreamError::EntitlementRequired
        );
        assert!(
            latest.next_unseen().is_none(),
            "a subscription that was refused publishes no feed"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_failed_setup_should_report_the_service_error() {
        let source = FakeSource::default();
        source.refuse(KytrixServiceError::Closed);
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");

        load(&runtime, scope()).await;

        assert_eq!(
            receive_failure(&mut reliable).await,
            NewsStreamError::Service(KytrixServiceError::Closed)
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn an_initial_closed_status_should_publish_a_failure_without_a_feed() {
        let source = FakeSource::default();
        let control = source.subscribe();
        control.status.send_replace(SubscriptionStatus::Closed);
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let mut latest = runtime.latest_stream();

        load(&runtime, scope()).await;

        assert_eq!(
            receive_failure(&mut reliable).await,
            NewsStreamError::Transport(TransportError::Closed)
        );
        assert!(
            latest.next_unseen().is_none(),
            "a subscription that never came up publishes no feed"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn retry_should_replace_the_stream_of_a_failed_account() {
        let source = FakeSource::default();
        source.refuse(KytrixServiceError::Closed);
        let recovered = source.subscribe();
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");

        load(&runtime, scope()).await;
        assert_eq!(
            receive_failure(&mut reliable).await,
            NewsStreamError::Service(KytrixServiceError::Closed)
        );

        let response = runtime
            .client()
            .call(NewsRequest::Retry(scope()))
            .await
            .expect("the retry should open a new stream")
            .into_value();
        assert_eq!(response, NewsResponse::StreamStarted);
        assert_eq!(source.subscriptions(), 2);
        assert_eq!(recovered.drops.load(Ordering::SeqCst), 0);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn loading_an_account_twice_should_keep_the_stream_it_already_has() {
        let source = FakeSource::default();
        let control = source.subscribe();
        let runtime = runtime(&source);

        load(&runtime, scope()).await;
        load(&runtime, scope()).await;

        assert_eq!(source.subscriptions(), 1);
        assert_eq!(control.drops.load(Ordering::SeqCst), 0);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn stop_should_release_the_subscription_and_be_idempotent() {
        let source = FakeSource::default();
        let control = source.subscribe();
        let runtime = runtime(&source);

        load(&runtime, scope()).await;
        for _ in 0..2 {
            let response = runtime
                .client()
                .call(NewsRequest::Stop)
                .await
                .expect("stopping should be answered")
                .into_value();
            assert_eq!(response, NewsResponse::Stopped);
        }

        assert_eq!(control.drops.load(Ordering::SeqCst), 1);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_caller_that_went_away_should_not_leave_a_stream_behind() {
        let source = FakeSource::default();
        let block = source.block();
        let replacement = source.subscribe();
        let runtime = runtime(&source);
        let mut latest = runtime.latest_stream();

        let client = runtime.client();
        let pending =
            tokio::spawn(async move { client.call(NewsRequest::Load(account("first"))).await });
        block.wait_started().await;
        pending.abort();
        let _ = pending.await;
        block.wait_dropped().await;

        assert!(
            latest.next_unseen().is_none(),
            "a setup nobody waited for publishes no feed"
        );

        let response = runtime
            .client()
            .call(NewsRequest::Retry(account("first")))
            .await
            .expect("a retry should open the stream the abandoned setup never did")
            .into_value();
        assert_eq!(response, NewsResponse::StreamStarted);
        assert_eq!(
            source.subscriptions(),
            2,
            "the retry asks for a subscription of its own"
        );
        assert_eq!(
            replacement.drops.load(Ordering::SeqCst),
            0,
            "the stream the retry opened keeps its subscription"
        );
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_setup_should_not_reach_a_live_subscription() {
        let source = FakeSource::default();
        let block = source.block();
        let control = source.subscribe();
        let runtime = runtime(&source);
        let mut latest = runtime.latest_stream();
        let client = runtime.client();
        let load = tokio::spawn(async move {
            client
                .call_with_timeout(NewsRequest::Load(account("first")), Duration::from_secs(1))
                .await
        });

        block.wait_started().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        let result = load.await.expect("the expired setup should finish");
        assert!(matches!(
            result,
            Err(widget::rpc::RpcCallError::DeadlineExceeded { .. })
        ));
        block.wait_dropped().await;
        assert!(latest.next_unseen().is_none());

        let response = runtime
            .client()
            .call(NewsRequest::Retry(account("first")))
            .await
            .expect("a retry should open the stream the expired setup never did")
            .into_value();
        assert_eq!(response, NewsResponse::StreamStarted);
        assert_eq!(source.subscriptions(), 2);
        assert_eq!(control.drops.load(Ordering::SeqCst), 0);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_coalesced_feed_should_still_hold_every_item() {
        let source = FakeSource::default();
        let control = source.subscribe();
        let runtime = runtime(&source);
        let mut latest = runtime.latest_stream();

        load(&runtime, scope()).await;
        let _ = next_feed(&mut latest, |snapshot| snapshot.items().is_empty()).await;

        for timestamp in 1..=8 {
            control
                .events
                .send(Ok(event(timestamp)))
                .await
                .expect("the subscription should accept news");
        }

        let feed = next_feed(&mut latest, |snapshot| snapshot.items().len() == 8).await;
        assert_eq!(
            contents(&feed).first().map(String::as_str),
            Some("News 8"),
            "the feed the pane is handed holds every item that arrived"
        );
        runtime.shutdown().await;
    }
}

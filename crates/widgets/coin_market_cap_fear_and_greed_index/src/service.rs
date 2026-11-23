use std::{future::Future, sync::Arc, time::Duration};

use kytrix_service_provider::service::{
    connector::KytrixServiceConnector, engine::KytrixServiceError,
};
use smol_str::SmolStr;
use thiserror::Error;
use tokio::{
    sync::mpsc,
    time::{Instant, sleep_until},
};
use widget::{
    protocol::{StreamGeneration, WidgetProtocol},
    runtime::{ServiceContext, WidgetService},
    stream::StreamPublisher,
};

use crate::model::{
    CoinMarketCapFearAndGreedIndexParseError, CoinMarketCapFearAndGreedIndexSnapshot,
};

pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

pub(crate) const STREAM_COMMAND_CAPACITY: usize = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CoinMarketCapFearAndGreedIndexRequest {
    Load(CoinMarketCapFearAndGreedIndexStreamKey),
    Retry(CoinMarketCapFearAndGreedIndexStreamKey),
    Refresh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CoinMarketCapFearAndGreedIndexResponse {
    StreamStarted,
    RefreshQueued,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CoinMarketCapFearAndGreedIndexStreamKey {
    pub account_id: SmolStr,
}

#[derive(Clone, Debug)]
pub(crate) enum CoinMarketCapFearAndGreedIndexStreamMessage {
    Failed {
        error: CoinMarketCapFearAndGreedIndexFetchError,
    },
    Snapshot(Arc<CoinMarketCapFearAndGreedIndexSnapshot>),
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum CoinMarketCapFearAndGreedIndexFetchError {
    #[error("the fear and greed request failed: {0}")]
    Service(#[from] KytrixServiceError),
    #[error("the fear and greed snapshot could not be read: {0}")]
    Parse(#[from] CoinMarketCapFearAndGreedIndexParseError),
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum CoinMarketCapFearAndGreedIndexServiceError {
    #[error("the fear and greed stream is closed")]
    StreamClosed,
    #[error("a fear and greed refresh is already waiting")]
    StreamBusy,
}

pub(crate) struct CoinMarketCapFearAndGreedIndexProtocol;

impl WidgetProtocol for CoinMarketCapFearAndGreedIndexProtocol {
    type Request = CoinMarketCapFearAndGreedIndexRequest;
    type Response = CoinMarketCapFearAndGreedIndexResponse;
    type StreamKey = CoinMarketCapFearAndGreedIndexStreamKey;
    type StreamMessage = CoinMarketCapFearAndGreedIndexStreamMessage;
    type Error = CoinMarketCapFearAndGreedIndexServiceError;
}

pub(crate) trait CoinMarketCapFearAndGreedIndexSource:
    Clone + Send + Sync + 'static
{
    fn fetch_snapshot(
        &self,
    ) -> impl Future<
        Output = Result<
            Arc<CoinMarketCapFearAndGreedIndexSnapshot>,
            CoinMarketCapFearAndGreedIndexFetchError,
        >,
    > + Send;
}

impl CoinMarketCapFearAndGreedIndexSource for KytrixServiceConnector {
    async fn fetch_snapshot(
        &self,
    ) -> Result<Arc<CoinMarketCapFearAndGreedIndexSnapshot>, CoinMarketCapFearAndGreedIndexFetchError>
    {
        let index = self.fetch_coin_market_cap_fear_and_greed_index().await?;
        Ok(Arc::new(CoinMarketCapFearAndGreedIndexSnapshot::parse(
            index,
        )?))
    }
}

enum StreamCommand {
    Refresh,
}

struct ActiveStream {
    key: CoinMarketCapFearAndGreedIndexStreamKey,
    generation: StreamGeneration,
    commands: mpsc::Sender<StreamCommand>,
}

pub(crate) struct CoinMarketCapFearAndGreedIndexService<S = KytrixServiceConnector> {
    source: S,
}

impl<S> CoinMarketCapFearAndGreedIndexService<S> {
    pub(crate) const fn new(source: S) -> Self {
        Self { source }
    }
}

impl<S> WidgetService<CoinMarketCapFearAndGreedIndexProtocol>
    for CoinMarketCapFearAndGreedIndexService<S>
where
    S: CoinMarketCapFearAndGreedIndexSource,
{
    async fn run(self, mut context: ServiceContext<CoinMarketCapFearAndGreedIndexProtocol>) {
        let mut active: Option<ActiveStream> = None;

        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            match request {
                CoinMarketCapFearAndGreedIndexRequest::Load(key)
                | CoinMarketCapFearAndGreedIndexRequest::Retry(key) => {
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    if let Some(stream) = active.take() {
                        context.cancel_stream(&stream.key);
                    }
                    let (commands, stream_commands) = mpsc::channel(STREAM_COMMAND_CAPACITY);
                    let source = self.source.clone();
                    let generation = context.replace_stream(key.clone(), move |publisher| {
                        run_stream(source, stream_commands, publisher)
                    });
                    if reply
                        .respond(Ok(CoinMarketCapFearAndGreedIndexResponse::StreamStarted))
                        .is_ok()
                    {
                        active = Some(ActiveStream {
                            key,
                            generation,
                            commands,
                        });
                    } else {
                        context.cancel_stream(&key);
                        active = None;
                    }
                }
                CoinMarketCapFearAndGreedIndexRequest::Refresh => {
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    let running = active.as_ref().filter(|stream| {
                        context
                            .stream_generation(&stream.key)
                            .is_some_and(|generation| generation == stream.generation)
                    });
                    let Some(stream) = running else {
                        let _ = reply.respond(Err(
                            CoinMarketCapFearAndGreedIndexServiceError::StreamClosed,
                        ));
                        continue;
                    };
                    match stream.commands.try_send(StreamCommand::Refresh) {
                        Ok(()) => {
                            let _ = reply
                                .respond(Ok(CoinMarketCapFearAndGreedIndexResponse::RefreshQueued));
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            let _ = reply.respond(Err(
                                CoinMarketCapFearAndGreedIndexServiceError::StreamBusy,
                            ));
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            let _ = reply.respond(Err(
                                CoinMarketCapFearAndGreedIndexServiceError::StreamClosed,
                            ));
                        }
                    }
                }
            }
        }
    }
}

async fn run_stream<S>(
    source: S,
    mut commands: mpsc::Receiver<StreamCommand>,
    publisher: StreamPublisher<CoinMarketCapFearAndGreedIndexProtocol>,
) where
    S: CoinMarketCapFearAndGreedIndexSource,
{
    let mut next_refresh = Instant::now();

    loop {
        tokio::select! {
            biased;
            () = publisher.clone().cancelled_owned() => return,
            command = commands.recv() => {
                if !matches!(command, Some(StreamCommand::Refresh)) {
                    return;
                }
            }
            () = sleep_until(next_refresh) => {}
        }

        let fetch = source.fetch_snapshot();
        let result = tokio::select! {
            biased;
            () = publisher.clone().cancelled_owned() => return,
            result = fetch => result,
        };
        match result {
            Ok(snapshot) => {
                if publisher
                    .publish_latest(CoinMarketCapFearAndGreedIndexStreamMessage::Snapshot(
                        snapshot,
                    ))
                    .is_err()
                {
                    return;
                }
            }
            Err(error) => {
                let _ = publisher
                    .publish_reliable(CoinMarketCapFearAndGreedIndexStreamMessage::Failed { error })
                    .await;
                return;
            }
        }

        next_refresh = Instant::now() + REFRESH_INTERVAL;
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use tokio::time::{advance, timeout};
    use widget::{
        rpc::RpcCallError,
        runtime::WidgetRuntime,
        stream::{LatestStream, ReliableStream},
    };

    use super::*;
    use crate::test_support::{FakeSource, FetchPlan};

    fn current_test_handle() -> tokio::runtime::Handle {
        tokio::runtime::Handle::try_current()
            .expect("a test runs inside the runtime whose clock it advances")
    }

    fn account() -> CoinMarketCapFearAndGreedIndexStreamKey {
        CoinMarketCapFearAndGreedIndexStreamKey {
            account_id: SmolStr::new_static("test-account"),
        }
    }

    fn another_account() -> CoinMarketCapFearAndGreedIndexStreamKey {
        CoinMarketCapFearAndGreedIndexStreamKey {
            account_id: SmolStr::new_static("second-account"),
        }
    }

    fn service(source: FakeSource) -> CoinMarketCapFearAndGreedIndexService<FakeSource> {
        CoinMarketCapFearAndGreedIndexService::new(source)
    }

    async fn receive_failure(
        reliable: &mut ReliableStream<CoinMarketCapFearAndGreedIndexProtocol>,
        key: &CoinMarketCapFearAndGreedIndexStreamKey,
    ) -> CoinMarketCapFearAndGreedIndexFetchError {
        let envelope = reliable
            .recv()
            .await
            .expect("the failure channel should remain open");
        assert_eq!(
            envelope.key(),
            key,
            "a failure names the account it is about"
        );
        let CoinMarketCapFearAndGreedIndexStreamMessage::Failed { error } =
            envelope.message().as_ref()
        else {
            panic!("the reliable channel should only carry failures");
        };
        error.clone()
    }

    async fn receive_snapshot(
        latest: &mut LatestStream<CoinMarketCapFearAndGreedIndexProtocol>,
        key: &CoinMarketCapFearAndGreedIndexStreamKey,
    ) -> Arc<CoinMarketCapFearAndGreedIndexSnapshot> {
        latest
            .changed()
            .await
            .expect("a snapshot should be published");
        let envelope = latest
            .next_unseen()
            .expect("the published snapshot should be unseen");
        assert_eq!(
            envelope.key(),
            key,
            "a snapshot names the account it is about"
        );
        let CoinMarketCapFearAndGreedIndexStreamMessage::Snapshot(snapshot) =
            envelope.message().as_ref()
        else {
            panic!("the latest channel should only carry snapshots");
        };
        snapshot.clone()
    }

    async fn load(
        service: &WidgetRuntime<CoinMarketCapFearAndGreedIndexProtocol>,
        key: CoinMarketCapFearAndGreedIndexStreamKey,
    ) {
        let response = service
            .client()
            .call(CoinMarketCapFearAndGreedIndexRequest::Load(key))
            .await
            .expect("the load should be accepted");
        assert_eq!(
            response.into_value(),
            CoinMarketCapFearAndGreedIndexResponse::StreamStarted
        );
    }

    async fn settle_fetches(source: &FakeSource, count: usize) {
        for _ in 0..1_000 {
            if source.fetch_count() == count {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!(
            "the service should have fetched {count} time(s), not {}",
            source.fetch_count()
        );
    }

    async fn settle_cancelled(source: &FakeSource, count: usize) {
        for _ in 0..1_000 {
            if source.cancelled_fetches() == count {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!(
            "the service should have dropped {count} fetch(es), not {}",
            source.cancelled_fetches()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_load_should_fetch_at_once_and_then_once_an_interval() {
        let source = FakeSource::new([
            FetchPlan::Snapshot(46),
            FetchPlan::Snapshot(40),
            FetchPlan::Snapshot(30),
        ]);
        let runtime = WidgetRuntime::spawn(&current_test_handle(), service(source.clone()));
        let mut latest = runtime.latest_stream();

        load(&runtime, account()).await;
        let first = timeout(
            REFRESH_INTERVAL / 2,
            receive_snapshot(&mut latest, &account()),
        )
        .await
        .expect("the first fetch must not wait out an interval");
        assert_eq!(first.current, 46);
        assert_eq!(first.yesterday.as_ref().map(|row| row.score), Some(45));
        assert_eq!(source.fetch_count(), 1);

        advance(REFRESH_INTERVAL - Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            source.fetch_count(),
            1,
            "nothing is due before the interval has elapsed"
        );

        advance(Duration::from_secs(1)).await;
        settle_fetches(&source, 2).await;
        let second = receive_snapshot(&mut latest, &account()).await;
        assert_eq!(second.current, 40, "the next cycle reads the new answer");

        advance(REFRESH_INTERVAL - Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            source.fetch_count(),
            2,
            "the second interval is a full interval"
        );

        advance(Duration::from_secs(1)).await;
        settle_fetches(&source, 3).await;
        let third = receive_snapshot(&mut latest, &account()).await;
        assert_eq!(third.current, 30);

        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_unchanged_answer_should_still_be_published() {
        let source = FakeSource::new([FetchPlan::Snapshot(46), FetchPlan::Snapshot(46)]);
        let runtime = WidgetRuntime::spawn(&current_test_handle(), service(source.clone()));
        let mut latest = runtime.latest_stream();

        load(&runtime, account()).await;
        let first = receive_snapshot(&mut latest, &account()).await;

        advance(REFRESH_INTERVAL).await;
        let second = receive_snapshot(&mut latest, &account()).await;

        assert_eq!(
            first.as_ref(),
            second.as_ref(),
            "the same reading is the same snapshot"
        );
        assert_eq!(source.fetch_count(), 2);

        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_fetch_should_be_reported_and_end_the_loop() {
        let source = FakeSource::new([FetchPlan::Failure, FetchPlan::Snapshot(9)]);
        let mut runtime = WidgetRuntime::spawn(&current_test_handle(), service(source.clone()));
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let mut latest = runtime.latest_stream();

        load(&runtime, account()).await;
        let error = receive_failure(&mut reliable, &account()).await;
        assert!(
            matches!(&error, CoinMarketCapFearAndGreedIndexFetchError::Service(_)),
            "the failure names what the source refused with: {error:?}"
        );
        assert!(
            latest.next_unseen().is_none(),
            "a failed fetch publishes no snapshot"
        );

        for _ in 0..2 {
            advance(REFRESH_INTERVAL).await;
            tokio::task::yield_now().await;
            assert_eq!(
                source.fetch_count(),
                1,
                "a failed loop does not retry by itself"
            );
        }

        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_manual_refresh_should_fetch_again_and_reset_the_schedule() {
        let source = FakeSource::new([
            FetchPlan::Snapshot(46),
            FetchPlan::Snapshot(40),
            FetchPlan::Snapshot(30),
        ]);
        let runtime = WidgetRuntime::spawn(&current_test_handle(), service(source.clone()));
        let mut latest = runtime.latest_stream();
        load(&runtime, account()).await;
        let _ = receive_snapshot(&mut latest, &account()).await;

        advance(Duration::from_secs(30)).await;
        let response = runtime
            .client()
            .call(CoinMarketCapFearAndGreedIndexRequest::Refresh)
            .await
            .expect("a refresh should be queued");
        assert_eq!(
            response.into_value(),
            CoinMarketCapFearAndGreedIndexResponse::RefreshQueued
        );

        let refreshed = timeout(
            Duration::from_secs(1),
            receive_snapshot(&mut latest, &account()),
        )
        .await
        .expect("the refresh should fetch without waiting out the interval");
        assert_eq!(refreshed.current, 40);
        assert_eq!(source.fetch_count(), 2);

        advance(REFRESH_INTERVAL - Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            source.fetch_count(),
            2,
            "the refresh moved the next fetch to 90 seconds"
        );

        advance(Duration::from_secs(1)).await;
        settle_fetches(&source, 3).await;

        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_without_a_stream_should_be_refused() {
        let source = FakeSource::new([FetchPlan::Snapshot(46)]);
        let runtime = WidgetRuntime::spawn(&current_test_handle(), service(source));

        let error = runtime
            .client()
            .call(CoinMarketCapFearAndGreedIndexRequest::Refresh)
            .await
            .expect_err("there is nothing to refresh yet");
        assert!(
            matches!(
                &error,
                RpcCallError::Service {
                    source: CoinMarketCapFearAndGreedIndexServiceError::StreamClosed,
                    ..
                }
            ),
            "a closed stream is what the pane reports: {error:?}"
        );

        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_while_one_is_waiting_should_be_refused() {
        let source = FakeSource::new([FetchPlan::Pending]);
        let runtime = WidgetRuntime::spawn(&current_test_handle(), service(source.clone()));
        load(&runtime, account()).await;
        settle_fetches(&source, 1).await;

        let response = runtime
            .client()
            .call(CoinMarketCapFearAndGreedIndexRequest::Refresh)
            .await
            .expect("the first refresh should be queued");
        assert_eq!(
            response.into_value(),
            CoinMarketCapFearAndGreedIndexResponse::RefreshQueued
        );

        let error = runtime
            .client()
            .call(CoinMarketCapFearAndGreedIndexRequest::Refresh)
            .await
            .expect_err("a second refresh should be refused");
        assert!(
            matches!(
                &error,
                RpcCallError::Service {
                    source: CoinMarketCapFearAndGreedIndexServiceError::StreamBusy,
                    ..
                }
            ),
            "a queue of one is full: {error:?}"
        );

        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn another_account_should_replace_the_stream() {
        let source = FakeSource::new([FetchPlan::Pending, FetchPlan::Snapshot(46)]);
        let runtime = WidgetRuntime::spawn(&current_test_handle(), service(source.clone()));
        let mut latest = runtime.latest_stream();
        load(&runtime, account()).await;
        settle_fetches(&source, 1).await;

        load(&runtime, another_account()).await;
        let snapshot = receive_snapshot(&mut latest, &another_account()).await;

        assert_eq!(snapshot.current, 46);
        settle_cancelled(&source, 1).await;
        assert_eq!(
            source.fetch_count(),
            2,
            "the stream of the account that was replaced fetches nothing more"
        );

        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_shut_down_service_should_fetch_nothing_and_drop_what_is_in_flight() {
        let source = FakeSource::new([FetchPlan::Snapshot(46), FetchPlan::Pending]);
        let runtime = WidgetRuntime::spawn(&current_test_handle(), service(source.clone()));
        let mut latest = runtime.latest_stream();
        load(&runtime, account()).await;
        let _ = receive_snapshot(&mut latest, &account()).await;

        advance(REFRESH_INTERVAL).await;
        settle_fetches(&source, 2).await;

        runtime.shutdown().await;

        assert_eq!(source.in_flight(), 0, "the fetch in flight was dropped");
        assert_eq!(source.cancelled_fetches(), 1);
        let fetches = source.fetch_count();
        advance(REFRESH_INTERVAL * 2).await;
        tokio::task::yield_now().await;
        assert_eq!(
            source.fetch_count(),
            fetches,
            "a shut-down service fetches nothing"
        );
    }
}

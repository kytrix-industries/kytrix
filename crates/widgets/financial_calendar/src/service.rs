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
    util::deadline_millis,
};

use crate::model::{CalendarParseError, CalendarSnapshot};

pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);

pub(crate) const STREAM_COMMAND_CAPACITY: usize = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CalendarRequest {
    Load(CalendarStreamKey),
    Retry(CalendarStreamKey),
    Refresh,
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CalendarResponse {
    StreamStarted,
    RefreshQueued,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CalendarStreamKey {
    pub account_id: SmolStr,
}

#[derive(Clone, Debug)]
pub(crate) enum CalendarStreamMessage {
    Failed {
        error: CalendarFetchError,
    },
    Snapshot {
        calendar: Arc<CalendarSnapshot>,
        next_refresh_at: i64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum CalendarFetchError {
    #[error("the financial calendar request failed: {0}")]
    Service(#[from] KytrixServiceError),
    #[error("the financial calendar could not be read: {0}")]
    Parse(#[from] CalendarParseError),
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum CalendarServiceError {
    #[error("the financial calendar stream is closed")]
    StreamClosed,
    #[error("a financial calendar refresh is already waiting")]
    StreamBusy,
}

pub(crate) struct CalendarProtocol;

impl WidgetProtocol for CalendarProtocol {
    type Request = CalendarRequest;
    type Response = CalendarResponse;
    type StreamKey = CalendarStreamKey;
    type StreamMessage = CalendarStreamMessage;
    type Error = CalendarServiceError;
}

pub(crate) trait CalendarSource: Clone + Send + Sync + 'static {
    fn fetch_calendar(
        &self,
    ) -> impl Future<Output = Result<Arc<CalendarSnapshot>, CalendarFetchError>> + Send;
}

impl CalendarSource for KytrixServiceConnector {
    async fn fetch_calendar(&self) -> Result<Arc<CalendarSnapshot>, CalendarFetchError> {
        let calendar = self.fetch_financial_calendar().await?;
        Ok(Arc::new(CalendarSnapshot::parse(calendar)?))
    }
}

enum StreamCommand {
    Refresh,
}

struct ActiveStream {
    key: CalendarStreamKey,
    generation: StreamGeneration,
    commands: mpsc::Sender<StreamCommand>,
}

pub(crate) struct CalendarService<S = KytrixServiceConnector> {
    source: S,
}

impl<S> CalendarService<S> {
    pub(crate) const fn new(source: S) -> Self {
        Self { source }
    }
}

impl<S> WidgetService<CalendarProtocol> for CalendarService<S>
where
    S: CalendarSource,
{
    async fn run(self, mut context: ServiceContext<CalendarProtocol>) {
        let mut active: Option<ActiveStream> = None;

        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            match request {
                CalendarRequest::Load(key) | CalendarRequest::Retry(key) => {
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
                    if reply.respond(Ok(CalendarResponse::StreamStarted)).is_ok() {
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
                CalendarRequest::Refresh => {
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    let running = active.as_ref().filter(|stream| {
                        context
                            .stream_generation(&stream.key)
                            .is_some_and(|generation| generation == stream.generation)
                    });
                    let Some(stream) = running else {
                        let _ = reply.respond(Err(CalendarServiceError::StreamClosed));
                        continue;
                    };
                    match stream.commands.try_send(StreamCommand::Refresh) {
                        Ok(()) => {
                            let _ = reply.respond(Ok(CalendarResponse::RefreshQueued));
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            let _ = reply.respond(Err(CalendarServiceError::StreamBusy));
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            let _ = reply.respond(Err(CalendarServiceError::StreamClosed));
                        }
                    }
                }
                CalendarRequest::Stop => {
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    if let Some(stream) = active.take() {
                        context.cancel_stream(&stream.key);
                    }
                    let _ = reply.respond(Ok(CalendarResponse::Stopped));
                }
            }
        }
    }
}

async fn run_stream<S>(
    source: S,
    mut commands: mpsc::Receiver<StreamCommand>,
    publisher: StreamPublisher<CalendarProtocol>,
) where
    S: CalendarSource,
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

        let fetch = source.fetch_calendar();
        let result = tokio::select! {
            biased;
            () = publisher.clone().cancelled_owned() => return,
            result = fetch => result,
        };
        match result {
            Ok(calendar) => {
                next_refresh = Instant::now() + REFRESH_INTERVAL;
                let message = CalendarStreamMessage::Snapshot {
                    calendar,
                    next_refresh_at: deadline_millis(next_refresh),
                };
                if publisher.publish_latest(message).is_err() {
                    return;
                }
            }
            Err(error) => {
                let _ = publisher
                    .publish_reliable(CalendarStreamMessage::Failed { error })
                    .await;
                return;
            }
        }
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
        util::unix_millis,
    };

    use super::*;
    use crate::test_support::{FakeSource, FetchPlan};

    fn current_test_handle() -> tokio::runtime::Handle {
        tokio::runtime::Handle::try_current()
            .expect("a test runs inside the runtime whose clock it advances")
    }

    fn account() -> CalendarStreamKey {
        CalendarStreamKey {
            account_id: SmolStr::new_static("test-account"),
        }
    }

    fn another_account() -> CalendarStreamKey {
        CalendarStreamKey {
            account_id: SmolStr::new_static("second-account"),
        }
    }

    fn service(source: FakeSource) -> CalendarService<FakeSource> {
        CalendarService::new(source)
    }

    async fn receive_failure(
        reliable: &mut ReliableStream<CalendarProtocol>,
        key: &CalendarStreamKey,
    ) -> CalendarFetchError {
        let envelope = reliable
            .recv()
            .await
            .expect("the failure channel should remain open");
        assert_eq!(
            envelope.key(),
            key,
            "a failure names the account it is about"
        );
        let CalendarStreamMessage::Failed { error } = envelope.message().as_ref() else {
            panic!("the reliable channel should only carry failures");
        };
        error.clone()
    }

    async fn receive_snapshot(
        latest: &mut LatestStream<CalendarProtocol>,
        key: &CalendarStreamKey,
    ) -> (Arc<CalendarSnapshot>, i64) {
        latest
            .changed()
            .await
            .expect("a calendar should be published");
        let envelope = latest
            .next_unseen()
            .expect("the published calendar should be unseen");
        assert_eq!(
            envelope.key(),
            key,
            "a calendar names the account it is about"
        );
        match envelope.message().as_ref() {
            CalendarStreamMessage::Snapshot {
                calendar,
                next_refresh_at,
            } => (calendar.clone(), *next_refresh_at),
            CalendarStreamMessage::Failed { .. } => {
                panic!("the latest channel should only carry answers")
            }
        }
    }

    async fn load(service: &WidgetRuntime<CalendarProtocol>, key: CalendarStreamKey) {
        let response = service
            .client()
            .call(CalendarRequest::Load(key))
            .await
            .expect("the load should be accepted");
        assert_eq!(response.into_value(), CalendarResponse::StreamStarted);
    }

    async fn settle_fetches(source: &FakeSource, count: usize) {
        for _ in 0..1_000 {
            if source.fetch_count() == count {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!(
            "the service should have read {count} time(s), not {}",
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
            "the service should have dropped {count} read(s), not {}",
            source.cancelled_fetches()
        );
    }

    #[test]
    fn the_calendar_should_be_read_every_ten_minutes() {
        assert_eq!(REFRESH_INTERVAL, Duration::from_secs(10 * 60));
    }

    #[tokio::test(start_paused = true)]
    async fn a_load_should_read_at_once_and_then_once_an_interval() {
        let source = FakeSource::new([
            FetchPlan::Snapshot(2),
            FetchPlan::Snapshot(3),
            FetchPlan::Snapshot(4),
        ]);
        let runtime = WidgetRuntime::spawn(&current_test_handle(), service(source.clone()));
        let mut latest = runtime.latest_stream();

        load(&runtime, account()).await;
        let (first, first_due) = timeout(
            REFRESH_INTERVAL / 2,
            receive_snapshot(&mut latest, &account()),
        )
        .await
        .expect("the first read must not wait out an interval");
        assert_eq!(first.events().len(), 2);
        assert_eq!(source.fetch_count(), 1);
        assert!(
            first_due > deadline_millis(Instant::now()),
            "the answer arms a deadline that is still ahead of the pane"
        );

        advance(REFRESH_INTERVAL - Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            source.fetch_count(),
            1,
            "nothing is due before the interval has elapsed"
        );

        advance(Duration::from_secs(1)).await;
        settle_fetches(&source, 2).await;
        let (second, second_due) = receive_snapshot(&mut latest, &account()).await;
        assert_eq!(second.events().len(), 3, "the next cycle reads again");
        assert!(
            second_due > unix_millis(),
            "and arms a deadline of its own, still ahead of the pane that counts it down"
        );

        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_unchanged_answer_should_still_be_published() {
        let source = FakeSource::new([FetchPlan::Snapshot(2), FetchPlan::Snapshot(2)]);
        let runtime = WidgetRuntime::spawn(&current_test_handle(), service(source.clone()));
        let mut latest = runtime.latest_stream();

        load(&runtime, account()).await;
        let (first, _) = receive_snapshot(&mut latest, &account()).await;

        advance(REFRESH_INTERVAL).await;
        let (second, _) = receive_snapshot(&mut latest, &account()).await;

        assert_eq!(
            first.as_ref(),
            second.as_ref(),
            "the same calendar is the same answer"
        );
        assert_eq!(source.fetch_count(), 2);

        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_read_should_be_reported_and_end_the_loop() {
        let source = FakeSource::new([FetchPlan::Failure, FetchPlan::Snapshot(2)]);
        let mut runtime = WidgetRuntime::spawn(&current_test_handle(), service(source.clone()));
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let mut latest = runtime.latest_stream();

        load(&runtime, account()).await;
        let error = receive_failure(&mut reliable, &account()).await;
        assert!(
            matches!(&error, CalendarFetchError::Service(_)),
            "the failure names what the source refused with: {error:?}"
        );
        assert!(
            latest.next_unseen().is_none(),
            "a failed read publishes no calendar"
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
    async fn a_manual_refresh_should_read_again_and_reset_the_schedule() {
        let source = FakeSource::new([
            FetchPlan::Snapshot(2),
            FetchPlan::Snapshot(3),
            FetchPlan::Snapshot(4),
        ]);
        let runtime = WidgetRuntime::spawn(&current_test_handle(), service(source.clone()));
        let mut latest = runtime.latest_stream();
        load(&runtime, account()).await;
        let _ = receive_snapshot(&mut latest, &account()).await;

        advance(Duration::from_secs(30)).await;
        let response = runtime
            .client()
            .call(CalendarRequest::Refresh)
            .await
            .expect("a refresh should be queued");
        assert_eq!(response.into_value(), CalendarResponse::RefreshQueued);

        let (refreshed, due) = timeout(
            Duration::from_secs(1),
            receive_snapshot(&mut latest, &account()),
        )
        .await
        .expect("the refresh should read without waiting out the interval");
        assert_eq!(refreshed.events().len(), 3);
        assert_eq!(source.fetch_count(), 2);
        assert!(
            due > deadline_millis(Instant::now() + REFRESH_INTERVAL - Duration::from_secs(2)),
            "the deadline the refresh arms is a whole interval from the refresh"
        );

        advance(REFRESH_INTERVAL - Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            source.fetch_count(),
            2,
            "the refresh moved the next read to the end of a new interval"
        );

        advance(Duration::from_secs(1)).await;
        settle_fetches(&source, 3).await;

        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_without_a_stream_should_be_refused() {
        let source = FakeSource::new([FetchPlan::Snapshot(2)]);
        let runtime = WidgetRuntime::spawn(&current_test_handle(), service(source));

        let error = runtime
            .client()
            .call(CalendarRequest::Refresh)
            .await
            .expect_err("there is nothing to refresh yet");
        assert!(
            matches!(
                &error,
                RpcCallError::Service {
                    source: CalendarServiceError::StreamClosed,
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
            .call(CalendarRequest::Refresh)
            .await
            .expect("the first refresh should be queued");
        assert_eq!(response.into_value(), CalendarResponse::RefreshQueued);

        let error = runtime
            .client()
            .call(CalendarRequest::Refresh)
            .await
            .expect_err("a second refresh should be refused");
        assert!(
            matches!(
                &error,
                RpcCallError::Service {
                    source: CalendarServiceError::StreamBusy,
                    ..
                }
            ),
            "a queue of one is full: {error:?}"
        );

        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_should_cancel_the_stream_and_end_its_schedule() {
        let source = FakeSource::new([FetchPlan::Pending]);
        let runtime = WidgetRuntime::spawn(&current_test_handle(), service(source.clone()));
        load(&runtime, account()).await;
        settle_fetches(&source, 1).await;

        let response = runtime
            .client()
            .call(CalendarRequest::Stop)
            .await
            .expect("a stop should be accepted");
        assert_eq!(response.into_value(), CalendarResponse::Stopped);
        settle_cancelled(&source, 1).await;

        let reads = source.fetch_count();
        advance(REFRESH_INTERVAL * 2).await;
        tokio::task::yield_now().await;
        assert_eq!(
            source.fetch_count(),
            reads,
            "a stopped loop reads nothing more"
        );

        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_without_a_stream_should_be_accepted() {
        let source = FakeSource::new([FetchPlan::Snapshot(2)]);
        let runtime = WidgetRuntime::spawn(&current_test_handle(), service(source.clone()));

        let response = runtime
            .client()
            .call(CalendarRequest::Stop)
            .await
            .expect("a stop should be accepted");
        assert_eq!(response.into_value(), CalendarResponse::Stopped);
        assert_eq!(source.fetch_count(), 0, "and starts no read by doing so");

        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn another_account_should_replace_the_stream() {
        let source = FakeSource::new([FetchPlan::Pending, FetchPlan::Snapshot(2)]);
        let runtime = WidgetRuntime::spawn(&current_test_handle(), service(source.clone()));
        let mut latest = runtime.latest_stream();
        load(&runtime, account()).await;
        settle_fetches(&source, 1).await;

        load(&runtime, another_account()).await;
        let (calendar, _) = receive_snapshot(&mut latest, &another_account()).await;

        assert_eq!(calendar.events().len(), 2);
        settle_cancelled(&source, 1).await;
        assert_eq!(
            source.fetch_count(),
            2,
            "the stream of the account that was replaced reads nothing more"
        );

        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_shut_down_service_should_read_nothing_and_drop_what_is_in_flight() {
        let source = FakeSource::new([FetchPlan::Snapshot(2), FetchPlan::Pending]);
        let runtime = WidgetRuntime::spawn(&current_test_handle(), service(source.clone()));
        let mut latest = runtime.latest_stream();
        load(&runtime, account()).await;
        let _ = receive_snapshot(&mut latest, &account()).await;

        advance(REFRESH_INTERVAL).await;
        settle_fetches(&source, 2).await;

        runtime.shutdown().await;

        assert_eq!(source.in_flight(), 0, "the read in flight was dropped");
        assert_eq!(source.cancelled_fetches(), 1);
        let fetches = source.fetch_count();
        advance(REFRESH_INTERVAL * 2).await;
        tokio::task::yield_now().await;
        assert_eq!(
            source.fetch_count(),
            fetches,
            "a shut-down service reads nothing"
        );
    }
}

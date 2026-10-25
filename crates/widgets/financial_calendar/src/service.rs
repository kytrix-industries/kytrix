//! The background service the financial calendar pane talks to.
//!
//! A calendar is a whole answer: the service reads all of it at once, and every read
//! replaces the last one entirely. So the service is a poll loop rather than a
//! subscription — one stream task per account does two things, in this order, every
//! time it runs:
//!
//! 1. fetches the calendar and publishes it on the latest-value channel, together
//!    with the instant the *next* read is due. The two travel in one message
//!    because they are one fact: a pane that took its deadline from another channel
//!    could count down to a read the answer already came from;
//! 2. sleeps until that deadline, or wakes early for a manual refresh, which also
//!    restarts the interval from the moment it ran.
//!
//! A newer answer replaces an older one nobody has read, so a slow frame cannot make
//! the pane work through a backlog of calendars it will never draw.
//!
//! A failed read is terminal: it publishes itself on the reliable channel and the
//! loop ends, because a service that just refused is not worth asking again every ten
//! minutes until someone notices. Only the user's recovery restarts it, and the
//! recovery is a new load of the same account.
//!
//! The account is the whole of the scope. The calendar is the same for every reader,
//! but the answer is served over an authenticated connection, so it is not one a
//! pane may keep showing after the account behind it changed. Signing out is not an
//! answer the pane rejects: it is a reason to stop asking, so it cancels the loop
//! instead of leaving it polling for a reader nobody is.
//!
//! There is no widget-level timeout. One fetch is one RPC, and the connector that
//! sends it owns the operation budget and turns it into a typed timeout; a second
//! budget here would only race the first.

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

/// How often the calendar is read again on its own.
///
/// A calendar is scheduled days ahead: ten minutes is frequent enough that a reading
/// the collector revised shows up while the reader is still looking at it, and rare
/// enough that a pane left open overnight is not a stream of whole answers.
pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// The refresh command queue never needs to hold more than the one command a manual
/// refresh sends: a second one while the first is waiting is answered as busy rather
/// than queued.
pub(crate) const STREAM_COMMAND_CAPACITY: usize = 1;

/// What the pane asks the service to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CalendarRequest {
    /// Starts following an account, or replaces the stream that was following
    /// another one.
    Load(CalendarStreamKey),
    /// The same, for an account the user recovered from a failure.
    Retry(CalendarStreamKey),
    /// Reads the current account's calendar once, without waiting for the next
    /// interval.
    Refresh,
    /// Stops the loop this pane was reading, because the account behind it signed
    /// out.
    Stop,
}

/// What the service answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CalendarResponse {
    StreamStarted,
    RefreshQueued,
    Stopped,
}

/// The account one answer belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CalendarStreamKey {
    pub account_id: SmolStr,
}

/// Everything the stream publishes: a failure on the reliable channel, one whole
/// answer on the latest-value one.
#[derive(Clone, Debug)]
pub(crate) enum CalendarStreamMessage {
    Failed {
        error: CalendarFetchError,
    },
    Snapshot {
        calendar: Arc<CalendarSnapshot>,
        /// The Unix millisecond the next automatic read is due.
        next_refresh_at: i64,
    },
}

/// Why a read produced no calendar.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum CalendarFetchError {
    #[error("the financial calendar request failed: {0}")]
    Service(#[from] KytrixServiceError),
    #[error("the financial calendar could not be read: {0}")]
    Parse(#[from] CalendarParseError),
}

/// Why a request could not be accepted.
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

/// The seam between this widget and the service that answers it, so a test can
/// answer instead.
///
/// It stays crate-private, together with the answer it hands over: nothing outside
/// this crate constructs a widget from a source, so there is no reason for the model
/// to become part of the crate's public surface.
pub(crate) trait CalendarSource: Clone + Send + Sync + 'static {
    /// One read: one RPC, answered with a calendar that has been read and ordered.
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

/// The one command a manual refresh sends to the poll loop.
enum StreamCommand {
    Refresh,
}

/// The stream this service is currently following.
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
                    // The answer is worth less than the request it belongs to: a
                    // caller that has already given up is not waiting for it.
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    // One pane follows one account, so the stream it was reading is
                    // replaced — including one keyed by an account it no longer
                    // reads. Replacing by the *new* key alone would leave the old
                    // account's loop polling the service for a pane that has moved
                    // on.
                    if let Some(stream) = active.take() {
                        context.cancel_stream(&stream.key);
                    }
                    let (commands, stream_commands) = mpsc::channel(STREAM_COMMAND_CAPACITY);
                    let source = self.source.clone();
                    // Replacing is what makes a recovery start over: after a failure
                    // the loop has already ended, and a stream that ran out its task
                    // would otherwise answer `StreamStarted` for a loop that no
                    // longer exists.
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
                        // Nobody is listening to this stream any more, and the
                        // command channel is gone with the answer.
                        context.cancel_stream(&key);
                        active = None;
                    }
                }
                CalendarRequest::Refresh => {
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    // A refresh is only worth sending to a loop that is still
                    // running: after a failure the loop has ended, and a command
                    // handed to a channel nobody reads would be answered as queued
                    // and never run. The next explicit load is what starts a loop
                    // again.
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
                    // Signing out is a fact about the reader, not a failure of the
                    // read: cancel the loop rather than leave it asking the service
                    // for an account nobody is signed in to any more. Stopping a pane
                    // that never started one is the same state arrived at twice.
                    if let Some(stream) = active.take() {
                        context.cancel_stream(&stream.key);
                    }
                    let _ = reply.respond(Ok(CalendarResponse::Stopped));
                }
            }
        }
    }
}

/// Runs the poll loop of one account until the publisher is cancelled or a read
/// fails.
///
/// The first read happens as soon as the stream starts rather than after an
/// interval: a pane that has just been opened has nothing to show, and a countdown
/// to its first data would be a countdown to nothing. A failure ends the loop after
/// it has been published — the recovery is a fresh load of the account, not a retry
/// from here.
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
                // The schedule is armed before the answer is published, so the
                // deadline that travels with it is the one this read just set
                // rather than the one it replaced.
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
                // The reliable channel waits for capacity and its own send is
                // cancellation-aware, so a failure is either delivered or the
                // publisher it belonged to is gone.
                let _ = publisher
                    .publish_reliable(CalendarStreamMessage::Failed { error })
                    .await;
                return;
            }
        }

        // A manual refresh restarts the interval from the moment it ran, so a pane
        // the user just refreshed does not read again seconds later.
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

    /// The handle of the runtime this test runs on.
    ///
    /// The clock these tests read is the runtime's: `#[tokio::test(start_paused =
    /// true)]` pauses it and `tokio::time::advance` moves it, and both only affect
    /// the runtime whose clock was paused. The service is therefore spawned on the
    /// same runtime the test advances rather than on a shared one, which would keep
    /// waiting on real time while the test moves a clock it never reads.
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

    /// One whole answer, and the deadline the service armed with it.
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

    /// Loads `key` and answers `StreamStarted`.
    async fn load(service: &WidgetRuntime<CalendarProtocol>, key: CalendarStreamKey) {
        let response = service
            .client()
            .call(CalendarRequest::Load(key))
            .await
            .expect("the load should be accepted");
        assert_eq!(response.into_value(), CalendarResponse::StreamStarted);
    }

    /// Yields until the fake has been asked `count` times, so a test that asserts an
    /// absence knows the read really happened.
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

    /// Yields until the fake has dropped `count` read(s), so a test that asserts a
    /// cancellation knows the task that owned the read was really dropped.
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

    /// The cadence is the product's: ten minutes. A test advances this constant, so
    /// without this assertion it would happily pass at any interval.
    #[test]
    fn the_calendar_should_be_read_every_ten_minutes() {
        assert_eq!(REFRESH_INTERVAL, Duration::from_secs(10 * 60));
    }

    /// The first cycle reads immediately, and the next one arrives exactly one
    /// interval later — proven by nothing happening one second before the deadline.
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

        // One second short of the interval, nothing has been asked for.
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

    /// An answer that reads the same is still an answer the service publishes: the
    /// pane is what decides whether it is worth a frame.
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

    /// A refused read is reported on the reliable channel and the loop ends: the next
    /// interval passes without a second request.
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

    /// A refresh reads immediately and restarts the interval from there.
    ///
    /// The refresh happens at 30 seconds, so the read the original schedule would
    /// have run at 600 is the one now due at 630: one second before the new deadline
    /// the count tells the two apart.
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

        // The bounded wait would give up at 31 seconds: the read has to happen now
        // rather than wait out the rest of the interval.
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

        // The interval restarted at 30, so the next read is due at 630: at 629
        // seconds nothing has been asked for, and the count would already be three
        // if the refresh had been ignored.
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

    /// Without a stream there is nothing to refresh, and the pane is told so.
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

    /// The command queue holds one refresh; a second one while the first is still
    /// waiting is answered as busy rather than piling up behind it.
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

    /// Signing out is a reason to stop asking, not a failure to report: the read in
    /// flight is dropped and no later interval runs.
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

    /// A pane that signs out before its first answer is asking to stop something that
    /// was never started; that is the state it wants, not an error.
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

    /// Another account replaces the stream: the read of the account that left is
    /// dropped rather than left running, and the replacement starts from its own
    /// first answer.
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

    /// Shutting the service down ends the loop and drops the read in flight.
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

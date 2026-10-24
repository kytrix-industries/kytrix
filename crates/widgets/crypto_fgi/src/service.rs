//! The background service the fear and greed pane talks to.
//!
//! The collector publishes the index every 60 seconds and the answer is one whole
//! snapshot, so the service is a poll loop rather than a subscription: one stream
//! task per account does two things, in this order, every time it runs:
//!
//! 1. fetches the snapshot and publishes it on the latest-value channel — a newer
//!    snapshot replaces an older one nobody has read, so a slow frame cannot make
//!    the pane work through a backlog of readings it will never draw;
//! 2. sleeps until the next automatic fetch, or wakes early for a manual refresh,
//!    which also restarts the interval from the moment it ran.
//!
//! A failed fetch is terminal: it publishes itself on the reliable channel and the
//! loop ends, because a service that just refused is not worth asking again every
//! minute until someone notices. Only the user's recovery restarts it, and the
//! recovery is a new load of the same account.
//!
//! The account is the whole of the scope. The index is the same for every reader,
//! but the answer is served over an authenticated connection, so it is not one a
//! pane may keep showing after the account behind it changed.
//!
//! There is no widget-level timeout. One fetch is one RPC, and the connector that
//! sends it owns the 15-second operation budget and turns it into a typed timeout;
//! a second budget here would only race the first.

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

use crate::model::{FgiParseError, FgiSnapshot};

/// How often the pane re-reads the index on its own.
///
/// The collector publishes once a minute, so the interval matches the data rather
/// than polling a value that cannot have moved in between.
pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// The refresh command queue never needs to hold more than the one command a
/// manual refresh sends: a second one while the first is waiting is answered as
/// busy rather than queued.
pub(crate) const STREAM_COMMAND_CAPACITY: usize = 1;

/// What the pane asks the service to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FgiRequest {
    /// Starts following an account, or replaces the stream that was following
    /// another one.
    Load(FgiStreamKey),
    /// The same, for an account the user recovered from a failure.
    Retry(FgiStreamKey),
    /// Fetches the current account's snapshot once, without waiting for the next
    /// interval.
    Refresh,
}

/// What the service answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FgiResponse {
    StreamStarted,
    RefreshQueued,
}

/// The account one snapshot belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FgiStreamKey {
    pub account_id: SmolStr,
}

/// Everything the stream publishes: a failure on the reliable channel, a snapshot
/// on the latest-value one.
#[derive(Clone, Debug)]
pub(crate) enum FgiStreamMessage {
    Failed { error: FgiFetchError },
    Snapshot(Arc<FgiSnapshot>),
}

/// Why a fetch produced no snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum FgiFetchError {
    #[error("the fear and greed request failed: {0}")]
    Service(#[from] KytrixServiceError),
    #[error("the fear and greed snapshot could not be read: {0}")]
    Parse(#[from] FgiParseError),
}

/// Why a request could not be accepted.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum FgiServiceError {
    #[error("the fear and greed stream is closed")]
    StreamClosed,
    #[error("a fear and greed refresh is already waiting")]
    StreamBusy,
}

pub(crate) struct FgiProtocol;

impl WidgetProtocol for FgiProtocol {
    type Request = FgiRequest;
    type Response = FgiResponse;
    type StreamKey = FgiStreamKey;
    type StreamMessage = FgiStreamMessage;
    type Error = FgiServiceError;
}

/// The seam between this widget and the service that answers it, so a test can
/// answer instead.
///
/// It stays crate-private, together with the snapshot it hands over: nothing
/// outside this crate constructs a widget from a source, so there is no reason for
/// the model to become part of the crate's public surface.
pub(crate) trait FgiSource: Clone + Send + Sync + 'static {
    /// One fetch: one RPC, answered with a snapshot that has been parsed and
    /// validated.
    fn fetch_snapshot(
        &self,
    ) -> impl Future<Output = Result<Arc<FgiSnapshot>, FgiFetchError>> + Send;
}

impl FgiSource for KytrixServiceConnector {
    async fn fetch_snapshot(&self) -> Result<Arc<FgiSnapshot>, FgiFetchError> {
        let index = self.fetch_cmc_fear_greed_index().await?;
        Ok(Arc::new(FgiSnapshot::parse(index)?))
    }
}

/// The one command a manual refresh sends to the poll loop.
enum StreamCommand {
    Refresh,
}

/// The stream this service is currently following.
struct ActiveStream {
    key: FgiStreamKey,
    generation: StreamGeneration,
    commands: mpsc::Sender<StreamCommand>,
}

pub(crate) struct FgiService<S = KytrixServiceConnector> {
    source: S,
}

impl<S> FgiService<S> {
    pub(crate) const fn new(source: S) -> Self {
        Self { source }
    }
}

impl<S> WidgetService<FgiProtocol> for FgiService<S>
where
    S: FgiSource,
{
    async fn run(self, mut context: ServiceContext<FgiProtocol>) {
        let mut active: Option<ActiveStream> = None;

        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            match request {
                FgiRequest::Load(key) | FgiRequest::Retry(key) => {
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
                    // Replacing is what makes a recovery start over: after a
                    // failure the loop has already ended, and a stream that ran out
                    // its task would otherwise answer `StreamStarted` for a loop
                    // that no longer exists.
                    let generation = context.replace_stream(key.clone(), move |publisher| {
                        run_stream(source, stream_commands, publisher)
                    });
                    if reply.respond(Ok(FgiResponse::StreamStarted)).is_ok() {
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
                FgiRequest::Refresh => {
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
                        let _ = reply.respond(Err(FgiServiceError::StreamClosed));
                        continue;
                    };
                    match stream.commands.try_send(StreamCommand::Refresh) {
                        Ok(()) => {
                            let _ = reply.respond(Ok(FgiResponse::RefreshQueued));
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            let _ = reply.respond(Err(FgiServiceError::StreamBusy));
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            let _ = reply.respond(Err(FgiServiceError::StreamClosed));
                        }
                    }
                }
            }
        }
    }
}

/// Runs the poll loop of one account until the publisher is cancelled or a fetch
/// fails.
///
/// The first fetch happens as soon as the stream starts rather than after a minute:
/// a pane that has just been opened has nothing to show, and a countdown to its
/// first data would be a countdown to nothing. A failure ends the loop after it has
/// been published — the recovery is a fresh load of the account, not a retry from
/// here.
async fn run_stream<S>(
    source: S,
    mut commands: mpsc::Receiver<StreamCommand>,
    publisher: StreamPublisher<FgiProtocol>,
) where
    S: FgiSource,
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
                    .publish_latest(FgiStreamMessage::Snapshot(snapshot))
                    .is_err()
                {
                    return;
                }
            }
            Err(error) => {
                // The reliable channel waits for capacity and its own send is
                // cancellation-aware, so a failure is either delivered or the
                // publisher it belonged to is gone.
                let _ = publisher
                    .publish_reliable(FgiStreamMessage::Failed { error })
                    .await;
                return;
            }
        }

        // A manual refresh restarts the interval from the moment it ran, so a pane
        // the user just refreshed does not fetch again seconds later.
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

    fn account() -> FgiStreamKey {
        FgiStreamKey {
            account_id: SmolStr::new_static("test-account"),
        }
    }

    fn another_account() -> FgiStreamKey {
        FgiStreamKey {
            account_id: SmolStr::new_static("second-account"),
        }
    }

    fn service(source: FakeSource) -> FgiService<FakeSource> {
        FgiService::new(source)
    }

    async fn receive_failure(
        reliable: &mut ReliableStream<FgiProtocol>,
        key: &FgiStreamKey,
    ) -> FgiFetchError {
        let envelope = reliable
            .recv()
            .await
            .expect("the failure channel should remain open");
        assert_eq!(
            envelope.key(),
            key,
            "a failure names the account it is about"
        );
        let FgiStreamMessage::Failed { error } = envelope.message().as_ref() else {
            panic!("the reliable channel should only carry failures");
        };
        error.clone()
    }

    async fn receive_snapshot(
        latest: &mut LatestStream<FgiProtocol>,
        key: &FgiStreamKey,
    ) -> Arc<FgiSnapshot> {
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
        let FgiStreamMessage::Snapshot(snapshot) = envelope.message().as_ref() else {
            panic!("the latest channel should only carry snapshots");
        };
        snapshot.clone()
    }

    /// Loads `key` and answers `StreamStarted`.
    async fn load(service: &WidgetRuntime<FgiProtocol>, key: FgiStreamKey) {
        let response = service
            .client()
            .call(FgiRequest::Load(key))
            .await
            .expect("the load should be accepted");
        assert_eq!(response.into_value(), FgiResponse::StreamStarted);
    }

    /// Yields until the fake has been asked `count` times, so a test that asserts
    /// an absence knows the fetch really happened.
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

    /// Yields until the fake has dropped `count` fetch(es), so a test that asserts a
    /// cancellation knows the task that owned the fetch was really dropped.
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

    /// The first cycle fetches immediately, and the next two arrive exactly one
    /// interval after the one before — proven by nothing happening one second before
    /// each deadline.
    ///
    /// Every wait for a fetch is bounded or counted: an un-timed receive under a
    /// paused clock would move time to the next timer and pass for any interval,
    /// including a wrong one.
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
        let second = receive_snapshot(&mut latest, &account()).await;
        assert_eq!(second.current, 40, "the next cycle reads the new answer");

        // The cadence repeats: the second interval is a full interval too, rather
        // than an accident of the clock the load happened on.
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

    /// An answer that folded into the same reading is still a fetch that answered:
    /// the service publishes it and leaves the decision not to redraw to the pane.
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

    /// A refused fetch is reported on the reliable channel, and the loop ends: the
    /// next interval passes without a second request.
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
            matches!(&error, FgiFetchError::Service(_)),
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

    /// A refresh runs a fetch immediately and restarts the interval from there.
    ///
    /// The refresh happens at 30 seconds, so the fetch the original schedule would
    /// have run at 60 is the one now due at 90: a refresh that was ignored would
    /// have fetched at 60 and a refresh that kept the old deadline would fetch at 60
    /// as well — one second before the new deadline the count tells the two apart.
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
            .call(FgiRequest::Refresh)
            .await
            .expect("a refresh should be queued");
        assert_eq!(response.into_value(), FgiResponse::RefreshQueued);

        // The bounded wait would give up at 31 seconds: the fetch has to happen now
        // rather than wait out the rest of the interval.
        let refreshed = timeout(
            Duration::from_secs(1),
            receive_snapshot(&mut latest, &account()),
        )
        .await
        .expect("the refresh should fetch without waiting out the interval");
        assert_eq!(refreshed.current, 40);
        assert_eq!(source.fetch_count(), 2);

        // The interval restarted at 30, so the next fetch is due at 90: at 89
        // seconds nothing has been asked for, and the count would already be three
        // if the refresh had been ignored.
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

    /// Without a stream there is nothing to refresh, and the pane is told so.
    #[tokio::test(start_paused = true)]
    async fn a_refresh_without_a_stream_should_be_refused() {
        let source = FakeSource::new([FetchPlan::Snapshot(46)]);
        let runtime = WidgetRuntime::spawn(&current_test_handle(), service(source));

        let error = runtime
            .client()
            .call(FgiRequest::Refresh)
            .await
            .expect_err("there is nothing to refresh yet");
        assert!(
            matches!(
                &error,
                RpcCallError::Service {
                    source: FgiServiceError::StreamClosed,
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
            .call(FgiRequest::Refresh)
            .await
            .expect("the first refresh should be queued");
        assert_eq!(response.into_value(), FgiResponse::RefreshQueued);

        let error = runtime
            .client()
            .call(FgiRequest::Refresh)
            .await
            .expect_err("a second refresh should be refused");
        assert!(
            matches!(
                &error,
                RpcCallError::Service {
                    source: FgiServiceError::StreamBusy,
                    ..
                }
            ),
            "a queue of one is full: {error:?}"
        );

        runtime.shutdown().await;
    }

    /// Another account replaces the stream: the old fetch is dropped rather than
    /// left running, and the replacement starts from its own first reading.
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

    /// Shutting the service down ends the loop and drops the fetch in flight.
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

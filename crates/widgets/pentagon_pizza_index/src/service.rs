//! The Pentagon Pizza Index protocol, its polling service, and the seams a test
//! drives it through.
//!
//! # A poll, not a subscription
//!
//! The index publishes one point an hour and the service answers with the newest
//! one, so there is nothing to subscribe to: the pane re-reads it on an interval and
//! counts down to the next read. One stream task owns that loop, and every round
//! does three things in this order:
//!
//! 1. report that it is fetching, and when the next fetch is due, so the pane can
//!    count down to something it can see;
//! 2. read the index once and turn the point into the reading the pane draws — a
//!    reading identical to the one already held is **not** republished, so a quiet
//!    hour does not schedule a frame;
//! 3. report readiness *after* the reading it describes, never before, because the
//!    two channels have no order between them and a status that ran ahead of its own
//!    reading would claim a reading the pane does not have.
//!
//! A failed round stops the clock without ending the scope: the task stays alive
//! and waits for a manual refresh, so a service that is briefly unreachable does not
//! turn into a request every interval until someone notices. Only an explicit load
//! or refresh starts the clock again.
//!
//! # The account is the scope
//!
//! The index is the same for every reader, but the answer is served over an
//! authenticated connection, so it is not one a pane may keep showing after the
//! account behind it changed. The stream key is therefore the account rather than a
//! unit-shaped scope, and a pane that loses its account asks the service to stop the
//! stream instead of leaving it polling for an answer nobody can draw.
//!
//! # There is no widget-level timeout
//!
//! One fetch is one RPC, and the connector that sends it owns the operation budget
//! and turns it into a typed timeout; a second budget here would only race the
//! first.

use std::{future::Future, sync::Arc, time::Duration};

use gpui_kit::{App, SharedString};
use kytrix_proto::types as proto_types;
use kytrix_service_provider::service::{
    connector::KytrixServiceConnector, engine::KytrixServiceError,
};
use smol_str::SmolStr;
use thiserror::Error;
use tokio::{sync::mpsc, time::Instant};
use widget::{
    error::LocalizedError,
    protocol::{StreamGeneration, WidgetProtocol},
    runtime::{ServiceContext, WidgetService},
    stream::{StreamPublishError, StreamPublisher},
    util::deadline_millis,
};

use super::model::{PizzaDataError, PizzaReading};

/// How often the index is re-read on its own.
///
/// The index gains a point about once an hour, and this reads faster than that: the
/// extra reads are what keep a new point from arriving up to an hour late, and one
/// RPC every ten minutes is a cost the service carries without noticing. Reading
/// meaningfully faster would only ask the same point again.
pub(super) const REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// The pending-refresh queue never needs to hold more than the one command a
/// manual refresh sends: a second one while the first is pending is answered as
/// busy rather than queued behind it.
const STREAM_COMMAND_CAPACITY: usize = 1;

/// What the pane asks the service to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PizzaRequest {
    /// Starts following an account, or replaces the stream that was following
    /// another one.
    ///
    /// One request serves the framework's first load and its recovery alike: an
    /// explicit load is what starts a generation, and a generation reads the index.
    Load(PizzaStreamKey),
    /// Reads the index once, without waiting for the next interval.
    Refresh,
    /// Stops following the account whose stream is active.
    ///
    /// A pane that loses its account has nothing left to read, and the stream it was
    /// reading belongs to the account that left. Stopping it here is what keeps a
    /// signed-out pane from asking the service for an answer it cannot draw.
    Unload,
}

/// What the service answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PizzaResponse {
    StreamStarted,
    RefreshQueued,
    StreamStopped,
}

/// The account one reading belongs to.
///
/// The reading itself is the same for every account, but it is served *per*
/// authenticated account, so it is not one a pane may keep showing after the
/// account behind it changed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct PizzaStreamKey {
    pub account_id: SmolStr,
}

/// Where the poll loop is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PizzaStatus {
    /// The stream is starting.
    Connecting,
    /// A read is in flight; `next_refresh_at` is the Unix millisecond the following
    /// automatic read is due.
    Refreshing { next_refresh_at: i64 },
    /// The reading of `revision` has been published, and the loop is idle until
    /// `next_refresh_at`.
    Ready { revision: u64, next_refresh_at: i64 },
    /// The last round produced no reading.
    Failed(PizzaFetchError),
}

impl PizzaStatus {
    /// When the next automatic read is due, if one is scheduled.
    pub(super) const fn next_refresh_at(&self) -> Option<i64> {
        match self {
            Self::Connecting | Self::Failed(_) => None,
            Self::Refreshing { next_refresh_at }
            | Self::Ready {
                next_refresh_at, ..
            } => Some(*next_refresh_at),
        }
    }
}

/// Why a round produced no reading.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(super) enum PizzaFetchError {
    #[error("the Pentagon Pizza Index request failed: {0}")]
    Service(#[from] KytrixServiceError),
    /// The answer was not one this pane could read.
    #[error("the Pentagon Pizza Index answer was unreadable: {0}")]
    Data(#[from] PizzaDataError),
}

impl LocalizedError for PizzaFetchError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Service(error) => error.localized_message(cx),
            Self::Data(error) => error.localized_message(cx),
        }
    }
}

/// Why a request could not be accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub(super) enum PizzaServiceError {
    #[error("the Pentagon Pizza Index stream is closed")]
    StreamClosed,
    #[error("a Pentagon Pizza Index refresh is already in flight")]
    StreamBusy,
}

impl LocalizedError for PizzaServiceError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::StreamClosed => i18n::t!(cx, "error.pentagon_pizza_index.unavailable"),
            Self::StreamBusy => i18n::t!(cx, "error.pentagon_pizza_index.busy"),
        }
    }
}

/// One message of the index's stream.
#[derive(Clone, Debug)]
pub(super) enum PizzaStreamMessage {
    Status(PizzaStatus),
    /// One reading, with the revision it belongs to.
    ///
    /// The revision travels with the reading rather than being inferred from the
    /// status that follows it: the two channels have no order between them, so a
    /// pane that had to guess would be guessing which reading a status describes.
    Snapshot(Arc<PizzaSnapshot>),
}

/// One reading as a round publishes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PizzaSnapshot {
    revision: u64,
    reading: PizzaReading,
}

impl PizzaSnapshot {
    pub(super) const fn new(revision: u64, reading: PizzaReading) -> Self {
        Self { revision, reading }
    }

    /// Which reading this is within the stream's generation.
    pub(super) const fn revision(&self) -> u64 {
        self.revision
    }

    /// The reading itself.
    pub(super) const fn reading(&self) -> PizzaReading {
        self.reading
    }
}

pub(super) struct PizzaProtocol;

impl WidgetProtocol for PizzaProtocol {
    type Request = PizzaRequest;
    type Response = PizzaResponse;
    type StreamKey = PizzaStreamKey;
    type StreamMessage = PizzaStreamMessage;
    type Error = PizzaServiceError;
}

/// The seam between this widget and the service, so a test can answer instead.
///
/// One call is one answer: what comes back is the index itself rather than a
/// reading, so that a malformed answer is refused by the model in one place rather
/// than by every source in its own way.
pub(super) trait PizzaSource: Clone + Send + Sync + 'static {
    fn fetch(
        &self,
    ) -> impl Future<Output = Result<proto_types::PentagonPizzaIndex, PizzaFetchError>> + Send;
}

impl PizzaSource for KytrixServiceConnector {
    async fn fetch(&self) -> Result<proto_types::PentagonPizzaIndex, PizzaFetchError> {
        Ok(self.fetch_pentagon_pizza_index().await?)
    }
}

/// The refresh command a manual refresh sends to the poll loop.
enum StreamCommand {
    Refresh,
}

/// The stream this service is currently following.
struct ActiveStream {
    key: PizzaStreamKey,
    generation: StreamGeneration,
    commands: mpsc::Sender<StreamCommand>,
}

/// The service that polls one account's index.
pub(super) struct PizzaService<S = KytrixServiceConnector> {
    source: S,
}

impl<S> PizzaService<S> {
    pub(super) const fn new(source: S) -> Self {
        Self { source }
    }
}

impl<S> WidgetService<PizzaProtocol> for PizzaService<S>
where
    S: PizzaSource,
{
    async fn run(self, mut context: ServiceContext<PizzaProtocol>) {
        let mut active: Option<ActiveStream> = None;

        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            match request {
                PizzaRequest::Load(key) => {
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
                    // A load always replaces. A stream that answered `StreamStarted`
                    // for a generation nobody replaced would leave the pane waiting
                    // on a schedule that no longer exists: after a failure the loop
                    // is alive but off the clock, and "already reading" is the one
                    // state this cannot tell from "reported a failure and stopped".
                    let generation = context.replace_stream(key.clone(), move |publisher| {
                        run_stream(source, stream_commands, publisher)
                    });
                    if reply.respond(Ok(PizzaResponse::StreamStarted)).is_ok() {
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
                PizzaRequest::Unload => {
                    // A pane that lost its account has nothing left to draw, so the
                    // stream is stopped even when the caller that asked is gone: the
                    // answer is not what makes the stream worth closing.
                    if let Some(stream) = active.take() {
                        context.cancel_stream(&stream.key);
                    }
                    let _ = reply.respond(Ok(PizzaResponse::StreamStopped));
                }
                PizzaRequest::Refresh => {
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    // A refresh is only worth sending to a loop that is still
                    // running. A loop that failed is still running — it waits for
                    // exactly this command, which is what puts it back on the clock —
                    // but one whose task ended has no reader left for the command,
                    // and answering "queued" to it would promise a round nobody runs.
                    let running = active.as_ref().filter(|stream| {
                        context
                            .stream_generation(&stream.key)
                            .is_some_and(|generation| generation == stream.generation)
                    });
                    let Some(stream) = running else {
                        let _ = reply.respond(Err(PizzaServiceError::StreamClosed));
                        continue;
                    };
                    // The queue holds one command and the loop takes a round at a
                    // time: a second click while the first read is in flight is
                    // refused here rather than queued behind it.
                    match stream.commands.try_send(StreamCommand::Refresh) {
                        Ok(()) => {
                            let _ = reply.respond(Ok(PizzaResponse::RefreshQueued));
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            let _ = reply.respond(Err(PizzaServiceError::StreamBusy));
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            let _ = reply.respond(Err(PizzaServiceError::StreamClosed));
                        }
                    }
                }
            }
        }
    }
}

/// What the poll loop carries between rounds.
#[derive(Default)]
struct StreamState {
    latest: Option<Arc<PizzaReading>>,
    revision: u64,
}

/// Runs the poll loop until the publisher is cancelled.
///
/// The loop has two states, and a failure is what moves it between them: it is
/// either **scheduled** — sleeping until the next automatic read — or **waiting**,
/// which is where a failed round leaves it. The task stays alive so the pane does
/// not have to be replaced to be recovered, but nothing is on the clock; only a
/// manual refresh moves it back to scheduled.
async fn run_stream<S>(
    source: S,
    mut commands: mpsc::Receiver<StreamCommand>,
    publisher: StreamPublisher<PizzaProtocol>,
) where
    S: PizzaSource,
{
    let mut state = StreamState::default();
    let _ = publish_status(&publisher, PizzaStatus::Connecting).await;

    // The first round runs as soon as the stream starts rather than after a whole
    // interval: a pane that has just been opened has nothing to show, and a
    // countdown to its first reading would be a countdown to nothing.
    let mut next_refresh = Instant::now() + REFRESH_INTERVAL;
    let mut scheduled = match run_refresh_round(&source, next_refresh, &mut state, &publisher).await
    {
        RefreshRoundOutcome::Scheduled => true,
        RefreshRoundOutcome::Waiting => false,
        RefreshRoundOutcome::Stop => return,
    };

    loop {
        let attempted_at = if scheduled {
            // A manual refresh also resets the automatic schedule, so a pane the
            // user just refreshed does not read again seconds later.
            let sleep = tokio::time::sleep_until(next_refresh);
            tokio::pin!(sleep);
            tokio::select! {
                biased;
                () = publisher.clone().cancelled_owned() => return,
                command = commands.recv() => {
                    if !matches!(command, Some(StreamCommand::Refresh)) {
                        return;
                    }

                    Instant::now()
                }
                () = &mut sleep => Instant::now(),
            }
        } else {
            // Nothing is on the clock, so the only thing left that can happen is a
            // refresh or the end of the scope.
            tokio::select! {
                biased;
                () = publisher.clone().cancelled_owned() => return,
                command = commands.recv() => {
                    if !matches!(command, Some(StreamCommand::Refresh)) {
                        return;
                    }

                    Instant::now()
                }
            }
        };
        next_refresh = attempted_at + REFRESH_INTERVAL;
        scheduled = match run_refresh_round(&source, next_refresh, &mut state, &publisher).await {
            RefreshRoundOutcome::Scheduled => true,
            RefreshRoundOutcome::Waiting => false,
            RefreshRoundOutcome::Stop => return,
        };
    }
}

/// What the loop does after one round.
///
/// The three answers are the three things a round can mean, and a failure is the
/// middle one: the loop keeps running so the reading does not have to be replaced
/// to be recovered, but nothing is on the clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RefreshRoundOutcome {
    /// The round published its result; the next automatic read is due.
    Scheduled,
    /// The round reported a failure: nothing is on the clock until a refresh.
    Waiting,
    /// The publisher is gone, so there is nobody left to report to.
    Stop,
}

/// Reads the index once and reports it.
async fn run_refresh_round<S>(
    source: &S,
    next_refresh: Instant,
    state: &mut StreamState,
    publisher: &StreamPublisher<PizzaProtocol>,
) -> RefreshRoundOutcome
where
    S: PizzaSource,
{
    let next_refresh_at = deadline_millis(next_refresh);
    if publish_status(publisher, PizzaStatus::Refreshing { next_refresh_at })
        .await
        .is_err()
    {
        return RefreshRoundOutcome::Stop;
    }
    let answer = tokio::select! {
        biased;
        () = publisher.clone().cancelled_owned() => return RefreshRoundOutcome::Stop,
        answer = source.fetch() => answer,
    };
    let reading = match answer {
        Ok(index) => match PizzaReading::from_wire(index) {
            Ok(reading) => reading,
            Err(error) => {
                publish_failure(publisher, PizzaFetchError::Data(error)).await;
                return RefreshRoundOutcome::Waiting;
            }
        },
        Err(error) => {
            publish_failure(publisher, error).await;
            return RefreshRoundOutcome::Waiting;
        }
    };

    // A round that read the same reading is not a change: publishing it again would
    // bump the revision and schedule a frame to draw the same pixels.
    if state.latest.as_deref() != Some(&reading) {
        state.revision = state.revision.saturating_add(1);
        state.latest = Some(Arc::new(reading));
        let snapshot = PizzaSnapshot::new(state.revision, reading);
        if publisher
            .publish_latest(PizzaStreamMessage::Snapshot(Arc::new(snapshot)))
            .is_err()
        {
            return RefreshRoundOutcome::Stop;
        }
    }

    if publish_status(
        publisher,
        PizzaStatus::Ready {
            revision: state.revision,
            next_refresh_at,
        },
    )
    .await
    .is_err()
    {
        return RefreshRoundOutcome::Stop;
    }

    RefreshRoundOutcome::Scheduled
}

/// Publishes a failed round.
///
/// The loop is still alive after one — off the clock, not gone — so a closed
/// publisher is noticed by the next write rather than reported here.
async fn publish_failure(publisher: &StreamPublisher<PizzaProtocol>, error: PizzaFetchError) {
    let _ = publish_status(publisher, PizzaStatus::Failed(error)).await;
}

async fn publish_status(
    publisher: &StreamPublisher<PizzaProtocol>,
    status: PizzaStatus,
) -> Result<(), StreamPublishError> {
    publisher
        .publish_reliable(PizzaStreamMessage::Status(status))
        .await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use widget::{rpc::RpcCallError, runtime::WidgetRuntime, stream::ReliableStream};

    use super::*;
    use crate::{
        model::PizzaLevel,
        test_support::{ACCOUNT, FakeSource, FetchPlan, OBSERVED_AT, reading},
    };

    /// The handle of the runtime this test runs on.
    ///
    /// `Handle::current` is a call the workspace forbids outside this one place, and
    /// the service is spawned on the runtime the test itself runs on: a shared
    /// runtime would keep waiting on a clock no test drives.
    #[allow(
        clippy::disallowed_methods,
        reason = "the test spawns the service on the runtime it runs on, which is the only handle it can name"
    )]
    fn current_test_handle() -> &'static tokio::runtime::Handle {
        Box::leak(Box::new(tokio::runtime::Handle::current()))
    }

    fn account() -> PizzaStreamKey {
        PizzaStreamKey {
            account_id: SmolStr::new_static(ACCOUNT),
        }
    }

    fn another_account() -> PizzaStreamKey {
        PizzaStreamKey {
            account_id: SmolStr::new_static("second-account"),
        }
    }

    fn runtime(source: &FakeSource) -> WidgetRuntime<PizzaProtocol> {
        WidgetRuntime::spawn(current_test_handle(), PizzaService::new(source.clone()))
    }

    async fn load(runtime: &WidgetRuntime<PizzaProtocol>) {
        let response = runtime
            .client()
            .call(PizzaRequest::Load(account()))
            .await
            .expect("the stream should start")
            .into_value();
        assert_eq!(response, PizzaResponse::StreamStarted);
    }

    async fn next_reliable(
        reliable: &mut ReliableStream<PizzaProtocol>,
        predicate: impl Fn(&PizzaStreamMessage) -> bool,
    ) -> PizzaStreamMessage {
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

    /// Reads the reliable stream until a status equal to `expected` arrives.
    async fn receive_status(reliable: &mut ReliableStream<PizzaProtocol>, expected: PizzaStatus) {
        next_reliable(
            reliable,
            |message| matches!(message, PizzaStreamMessage::Status(status) if *status == expected),
        )
        .await;
    }

    /// Yields until the fake has been asked `count` times, so a test that asserts
    /// an absence knows the fetch really happened.
    async fn settle_fetches(source: &FakeSource, count: usize) {
        for _ in 0..1_000 {
            if source.fetches() == count {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!(
            "the service should have fetched {count} time(s), not {}",
            source.fetches()
        );
    }

    /// Yields until `count` fetch(es) have been dropped, so a test that asserts a
    /// cancellation waits for the drop rather than for a scheduler turn.
    ///
    /// Cancelling a stream token does not drop the fetch it was serving: the task
    /// is polled again, sees the cancellation, and only then is the future dropped.
    /// A test that read the count straight after the load that replaced the stream
    /// would be asserting on the scheduler's order, not on the service.
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

    /// A round publishes the reading and then reports the revision that reading
    /// belongs to, which is what lets the two channels meet.
    #[tokio::test]
    async fn a_round_should_publish_the_reading_and_report_its_revision() {
        let source = FakeSource::new([FetchPlan::Snapshot(3)]);
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let mut latest = runtime.latest_stream();

        load(&runtime).await;
        receive_status(&mut reliable, PizzaStatus::Connecting).await;
        latest
            .changed()
            .await
            .expect("the latest stream should stay open");
        let envelope = latest
            .next_unseen()
            .expect("the reading should have been published");
        let PizzaStreamMessage::Snapshot(published) = envelope.message().as_ref() else {
            panic!("the latest channel carries the reading");
        };
        assert_eq!(published.reading(), reading(3));
        assert_eq!(published.revision(), 1, "the first reading is revision one");

        let ready = next_reliable(&mut reliable, |message| {
            matches!(
                message,
                PizzaStreamMessage::Status(PizzaStatus::Ready { .. })
            )
        })
        .await;
        let PizzaStreamMessage::Status(PizzaStatus::Ready { revision, .. }) = ready else {
            panic!("the stream should have reported readiness");
        };
        assert_eq!(revision, 1, "the first reading is revision one");
        runtime.shutdown().await;
    }

    /// A round that read the same reading is not a change: republishing it would
    /// bump the revision and schedule a frame to draw the same pixels.
    #[tokio::test]
    async fn a_refresh_that_reads_the_same_reading_should_not_republish_it() {
        let source = FakeSource::new([FetchPlan::Snapshot(3), FetchPlan::Snapshot(3)]);
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");

        load(&runtime).await;
        receive_status(&mut reliable, PizzaStatus::Connecting).await;
        let _ = next_reliable(&mut reliable, |message| {
            matches!(
                message,
                PizzaStreamMessage::Status(PizzaStatus::Ready { revision: 1, .. })
            )
        })
        .await;

        runtime
            .client()
            .call(PizzaRequest::Refresh)
            .await
            .expect("the refresh should be queued")
            .into_value();
        let ready = next_reliable(&mut reliable, |message| {
            matches!(
                message,
                PizzaStreamMessage::Status(PizzaStatus::Ready { revision: 1, .. })
            )
        })
        .await;
        let PizzaStreamMessage::Status(PizzaStatus::Ready { .. }) = ready else {
            panic!("the second round should have reported readiness");
        };
        assert_eq!(
            source.fetches(),
            2,
            "the same reading was read twice and published once"
        );
        runtime.shutdown().await;
    }

    /// A failed round takes the loop off the clock without ending the scope: the
    /// refresh that follows is still answered, and the round it starts is the one
    /// that puts the loop back on the clock.
    #[tokio::test]
    async fn a_failed_round_should_stop_the_clock_without_ending_the_scope() {
        let source = FakeSource::new([FetchPlan::Failure, FetchPlan::Snapshot(2)]);
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");

        load(&runtime).await;
        let failed = next_reliable(&mut reliable, |message| {
            matches!(message, PizzaStreamMessage::Status(PizzaStatus::Failed(_)))
        })
        .await;
        let PizzaStreamMessage::Status(PizzaStatus::Failed(error)) = failed else {
            panic!("the stream should have reported a failure");
        };
        assert!(matches!(error, PizzaFetchError::Service(_)));
        assert_eq!(
            source.fetches(),
            1,
            "a failed round is off the clock rather than scheduled again"
        );

        let queued = runtime
            .client()
            .call(PizzaRequest::Refresh)
            .await
            .expect("the loop a failure left waiting should still take a refresh")
            .into_value();
        assert_eq!(queued, PizzaResponse::RefreshQueued);
        let ready = next_reliable(&mut reliable, |message| {
            matches!(
                message,
                PizzaStreamMessage::Status(PizzaStatus::Ready { revision: 1, .. })
            )
        })
        .await;
        let PizzaStreamMessage::Status(PizzaStatus::Ready {
            next_refresh_at, ..
        }) = ready
        else {
            panic!("the refreshed round should have reported readiness");
        };
        assert!(
            next_refresh_at > 0,
            "a round that answered puts the next read back on the clock"
        );
        assert_eq!(source.fetches(), 2, "the refresh ran the round it promised");
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_refused_answer_should_fail_the_round_rather_than_report_calm() {
        let source = FakeSource::new([FetchPlan::UnknownLevel]);
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");

        load(&runtime).await;
        let failed = next_reliable(&mut reliable, |message| {
            matches!(message, PizzaStreamMessage::Status(PizzaStatus::Failed(_)))
        })
        .await;
        let PizzaStreamMessage::Status(PizzaStatus::Failed(error)) = failed else {
            panic!("the stream should have reported a failure");
        };
        assert_eq!(
            error,
            PizzaFetchError::Data(PizzaDataError::UnknownLevel { level: 9 })
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_load_should_replace_the_generation_it_replaces() {
        let source = FakeSource::new([FetchPlan::Snapshot(2), FetchPlan::Snapshot(2)]);
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");

        load(&runtime).await;
        receive_status(&mut reliable, PizzaStatus::Connecting).await;
        let replaced = runtime
            .client()
            .call(PizzaRequest::Load(account()))
            .await
            .expect("the second load should start")
            .into_value();
        assert_eq!(replaced, PizzaResponse::StreamStarted);
        settle_fetches(&source, 2).await;
        assert_eq!(
            source.fetches(),
            2,
            "a replacement reads the index again rather than reusing the old generation"
        );
        runtime.shutdown().await;
    }

    /// Another account is another scope: its fetch is what the pane then reads, and
    /// the stream of the account that was replaced fetches nothing more.
    #[tokio::test]
    async fn another_account_should_replace_the_stream() {
        let source = FakeSource::new([FetchPlan::Pending, FetchPlan::Snapshot(4)]);
        let runtime = runtime(&source);
        let mut latest = runtime.latest_stream();

        load(&runtime).await;
        settle_fetches(&source, 1).await;
        runtime
            .client()
            .call(PizzaRequest::Load(another_account()))
            .await
            .expect("the second account should start")
            .into_value();

        latest
            .changed()
            .await
            .expect("the replacement should publish its own reading");
        let envelope = latest
            .next_unseen()
            .expect("the replacement's reading should be unseen");
        assert_eq!(envelope.key(), &another_account());
        let PizzaStreamMessage::Snapshot(snapshot) = envelope.message().as_ref() else {
            panic!("the latest channel carries the reading");
        };
        assert_eq!(snapshot.reading().level(), PizzaLevel::DoubleTake);
        assert_eq!(snapshot.reading().observed_at(), OBSERVED_AT);
        settle_cancelled(&source, 1).await;
        assert_eq!(
            source.cancelled_fetches(),
            1,
            "the first account's fetch was dropped rather than left running"
        );
        assert_eq!(
            source.fetches(),
            2,
            "the stream of the account that was replaced fetches nothing more"
        );

        runtime.shutdown().await;
    }

    /// A stream whose account left is stopped rather than left polling: the unload
    /// drops the read in flight, and there is no stream left to refresh afterwards.
    #[tokio::test]
    async fn an_unload_should_stop_the_stream_it_was_serving() {
        let source = FakeSource::new([FetchPlan::Pending]);
        let runtime = runtime(&source);

        load(&runtime).await;
        settle_fetches(&source, 1).await;

        let stopped = runtime
            .client()
            .call(PizzaRequest::Unload)
            .await
            .expect("the service should stop the stream")
            .into_value();
        assert_eq!(stopped, PizzaResponse::StreamStopped);
        settle_cancelled(&source, 1).await;
        assert_eq!(
            source.fetches(),
            1,
            "the stream that was stopped does not fetch again"
        );

        let error = runtime
            .client()
            .call(PizzaRequest::Refresh)
            .await
            .expect_err("there is no stream to refresh after the unload");
        assert!(matches!(
            error,
            RpcCallError::Service {
                source: PizzaServiceError::StreamClosed,
                ..
            }
        ));
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_refresh_before_any_load_should_be_refused() {
        let source = FakeSource::default();
        let runtime = runtime(&source);

        let error = runtime
            .client()
            .call(PizzaRequest::Refresh)
            .await
            .expect_err("there is no stream to refresh");

        assert!(matches!(
            error,
            RpcCallError::Service {
                source: PizzaServiceError::StreamClosed,
                ..
            }
        ));
        runtime.shutdown().await;
    }
}

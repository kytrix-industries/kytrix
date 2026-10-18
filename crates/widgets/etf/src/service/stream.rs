//! The background service the ETF pane talks to.
//!
//! ETF flow is published twice a day at most, so "live" here means *poll*: one
//! stream task per account owns the schedule and does three things, in this order,
//! every time it runs:
//!
//! 1. report that it is fetching, and when the next fetch is due, so the pane can
//!    count down to something it can see;
//! 2. read the answer into a snapshot of all four assets and publish it as the
//!    latest value — an answer that changed nothing keeps the allocation it had,
//!    and the pane above compares by pointer to decide whether there is anything
//!    to redraw;
//! 3. report readiness *after* the snapshot it describes, never before, because the
//!    two channels have no order between them and a status that ran ahead of its
//!    own snapshot would put a chart on screen that is not there yet.
//!
//! A failed fetch is terminal for the refresh loop but not for the account: the
//! stream stays alive and waits for a manual refresh, so a service that is briefly
//! unreachable does not turn into a request every eight hours until someone
//! notices. Only an explicit load or refresh starts the loop again.
//!
//! The account is the whole of the scope. Which asset, which window and which fund
//! a pane is showing are *views* of one snapshot — the answer already contains
//! every asset and the whole history — so they are the pane's business and never
//! replace the stream.

use std::{future::Future, sync::Arc, time::Duration};

use kytrix_service_provider::service::{
    connector::KytrixServiceConnector, engine::KytrixServiceError,
};
use smol_str::SmolStr;
use thiserror::Error;
use tokio::{
    sync::mpsc,
    time::{Instant, timeout},
};
use widget::{
    protocol::{StreamGeneration, WidgetProtocol},
    runtime::{ServiceContext, WidgetService},
    stream::{StreamPublishError, StreamPublisher},
    util::deadline_millis,
};

use super::super::model::{EtfParseError, EtfSnapshot, parse_etf};

/// How often the pane re-reads the snapshot on its own.
///
/// The funds publish their daily flow once a trading day, so eight hours is
/// already faster than the data can move: it exists to pick up a new day shortly
/// after it is published, not to poll for changes within one.
pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(8 * 60 * 60);

/// How long one fetch may take before it is treated as a failure.
pub(crate) const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);

/// The refresh command queue never needs to hold more than the one command a
/// manual refresh sends: a second one while the first is pending is answered as
/// busy rather than queued.
pub(crate) const STREAM_COMMAND_CAPACITY: usize = 1;

/// What the pane asks the service to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EtfRequest {
    /// Starts following an account, or replaces the stream that was following
    /// another one.
    Load(EtfStreamKey),
    /// The same, for an account the user recovered from a failure.
    Retry(EtfStreamKey),
    /// Fetches the current account's snapshot once, without waiting for the next
    /// interval.
    Refresh,
}

/// What the service answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EtfResponse {
    StreamStarted,
    RefreshQueued,
}

/// The account one snapshot belongs to.
///
/// The answer itself is the same for every account, but it is served *per*
/// authenticated account, so it is not one a pane may keep showing after the
/// account behind it changed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct EtfStreamKey {
    pub account_id: SmolStr,
}

/// Where the poll loop is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EtfStatus {
    /// The stream is starting.
    Connecting,
    /// A fetch is in flight; `next_refresh_at` is the Unix millisecond the
    /// following automatic one is due.
    Refreshing { next_refresh_at: i64 },
    /// The snapshot of `revision` has been published and the loop is idle until
    /// `next_refresh_at`.
    Ready { revision: u64, next_refresh_at: i64 },
    /// The last fetch produced no snapshot.
    Failed { error: EtfFetchError },
}

impl EtfStatus {
    /// When the next automatic fetch is due, if one is scheduled.
    ///
    /// A failure takes the loop off the clock, so it is also what a pane reads to
    /// decide whether it has anything to count down to.
    pub(crate) const fn next_refresh_at(&self) -> Option<i64> {
        match self {
            Self::Connecting | Self::Failed { .. } => None,
            Self::Refreshing { next_refresh_at }
            | Self::Ready {
                next_refresh_at, ..
            } => Some(*next_refresh_at),
        }
    }
}

/// Why a fetch produced no snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum EtfFetchError {
    #[error("the ETF request failed: {0}")]
    Service(#[from] KytrixServiceError),
    #[error("the ETF snapshot could not be read: {0}")]
    Parse(#[from] EtfParseError),
    #[error("the ETF request did not answer within {0:?}")]
    Timeout(Duration),
}

/// Everything the service publishes: state on the reliable channel, snapshots on
/// the latest-value one.
#[derive(Clone, Debug)]
pub(crate) enum EtfStreamMessage {
    Status(EtfStatus),
    Snapshot {
        revision: u64,
        snapshot: Arc<EtfSnapshot>,
    },
}

/// Why a request could not be accepted.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum EtfServiceError {
    #[error("the ETF refresh queue is closed")]
    StreamClosed,
    #[error("an ETF refresh is already in flight")]
    StreamBusy,
}

pub(crate) struct EtfProtocol;

impl WidgetProtocol for EtfProtocol {
    type Request = EtfRequest;
    type Response = EtfResponse;
    type StreamKey = EtfStreamKey;
    type StreamMessage = EtfStreamMessage;
    type Error = EtfServiceError;
}

/// The seam between this widget and the service that answers it, so a test can
/// answer instead.
///
/// It stays crate-private, together with the snapshot it hands over: nothing
/// outside this crate constructs a widget from a source, so there is no reason for
/// the model to become part of the crate's public surface.
pub(crate) trait EtfSource: Clone + Send + Sync + 'static {
    fn fetch_snapshot(
        &self,
    ) -> impl Future<Output = Result<Arc<EtfSnapshot>, EtfFetchError>> + Send;
}

impl EtfSource for KytrixServiceConnector {
    async fn fetch_snapshot(&self) -> Result<Arc<EtfSnapshot>, EtfFetchError> {
        parse_etf(self.fetch_etf().await?)
            .map(Arc::new)
            .map_err(Into::into)
    }
}

/// The refresh command a manual refresh sends to the poll loop.
pub(crate) enum StreamCommand {
    Refresh,
}

/// One manual refresh, guarded so the command queue cannot fill with duplicates.
#[derive(Clone, Default)]
pub(crate) struct RefreshGate {
    active: Arc<std::sync::atomic::AtomicBool>,
}

impl RefreshGate {
    fn try_acquire(&self) -> Option<RefreshPermit> {
        self.active
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .ok()
            .map(|_| RefreshPermit {
                active: self.active.clone(),
            })
    }
}

pub(crate) struct RefreshPermit {
    active: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for RefreshPermit {
    fn drop(&mut self) {
        self.active
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

pub(crate) struct ActiveStream {
    key: EtfStreamKey,
    generation: StreamGeneration,
    commands: mpsc::Sender<StreamCommand>,
    gate: RefreshGate,
}

/// What the poll loop carries between cycles.
#[derive(Default)]
pub(crate) struct StreamState {
    latest: Option<Arc<EtfSnapshot>>,
    revision: u64,
}

pub(crate) struct EtfService<S = KytrixServiceConnector> {
    source: S,
    refresh_interval: Duration,
    attempt_timeout: Duration,
}

impl<S> EtfService<S> {
    pub(crate) const fn new(source: S) -> Self {
        Self {
            source,
            refresh_interval: REFRESH_INTERVAL,
            attempt_timeout: ATTEMPT_TIMEOUT,
        }
    }

    /// The same service with timings a test can wait out.
    #[cfg(test)]
    pub(crate) fn with_timing(
        source: S,
        refresh_interval: Duration,
        attempt_timeout: Duration,
    ) -> Self {
        Self {
            source,
            refresh_interval: refresh_interval.max(Duration::from_millis(1)),
            attempt_timeout: attempt_timeout.max(Duration::from_millis(1)),
        }
    }
}

impl<S> WidgetService<EtfProtocol> for EtfService<S>
where
    S: EtfSource,
{
    async fn run(self, mut context: ServiceContext<EtfProtocol>) {
        let mut active: Option<ActiveStream> = None;

        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            match request {
                EtfRequest::Load(key) | EtfRequest::Retry(key) => {
                    // The answer is worth less than the request it belongs to: a
                    // caller that has already given up is not waiting for it.
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    let (commands, stream_commands) = mpsc::channel(STREAM_COMMAND_CAPACITY);
                    let source = self.source.clone();
                    let refresh_interval = self.refresh_interval;
                    let attempt_timeout = self.attempt_timeout;
                    // Replacing is what makes a recovery start over: after a
                    // failure the loop has already ended, and a stream that ran
                    // out its task would otherwise answer `StreamStarted` for a
                    // loop that no longer exists.
                    let generation = context.replace_stream(key.clone(), move |publisher| {
                        run_stream(
                            source,
                            refresh_interval,
                            attempt_timeout,
                            stream_commands,
                            publisher,
                        )
                    });
                    if reply.respond(Ok(EtfResponse::StreamStarted)).is_ok() {
                        active = Some(ActiveStream {
                            key,
                            generation,
                            commands,
                            gate: RefreshGate::default(),
                        });
                    } else {
                        // Nobody is listening to this stream any more, and the
                        // command channel is gone with the answer.
                        context.cancel_stream(&key);
                        active = None;
                    }
                }
                EtfRequest::Refresh => {
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    // A refresh is only worth sending to a loop that is still
                    // running: after a failure the loop has ended, and a command
                    // handed to a channel nobody reads would be answered as
                    // queued and never run. The next explicit load is what starts
                    // a loop again.
                    let running = active.as_ref().filter(|stream| {
                        context
                            .stream_generation(&stream.key)
                            .is_some_and(|generation| generation == stream.generation)
                    });
                    let Some(stream) = running else {
                        let _ = reply.respond(Err(EtfServiceError::StreamClosed));
                        continue;
                    };
                    // The gate spans the whole round trip: a second click while
                    // the first fetch is in flight is refused here rather than
                    // queued behind it.
                    let Some(permit) = stream.gate.try_acquire() else {
                        let _ = reply.respond(Err(EtfServiceError::StreamBusy));
                        continue;
                    };
                    match stream.commands.try_send(StreamCommand::Refresh) {
                        Ok(()) => {
                            let _ = reply.respond(Ok(EtfResponse::RefreshQueued));
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            drop(permit);
                            let _ = reply.respond(Err(EtfServiceError::StreamBusy));
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            drop(permit);
                            let _ = reply.respond(Err(EtfServiceError::StreamClosed));
                        }
                    }
                }
            }
        }
    }
}

/// Runs the poll loop of one account until the publisher is cancelled.
///
/// The loop has two states, and a failure is what moves it between them. It is
/// either **scheduled** — sleeping until the next automatic fetch — or **waiting**,
/// which is where a failed fetch leaves it: the task stays alive so the account
/// does not have to be replaced to be recovered, but nothing is on the clock. Only
/// a manual refresh moves it back to scheduled, and a fetch that fails while
/// waiting leaves it waiting.
async fn run_stream<S>(
    source: S,
    refresh_interval: Duration,
    attempt_timeout: Duration,
    mut commands: mpsc::Receiver<StreamCommand>,
    publisher: StreamPublisher<EtfProtocol>,
) where
    S: EtfSource,
{
    let mut state = StreamState::default();
    let mut next_refresh = Instant::now() + refresh_interval;
    let _ = publish_status(&publisher, EtfStatus::Connecting).await;

    // The first fetch happens as soon as the stream starts rather than after eight
    // hours: a pane that has just been opened has nothing to show, and a countdown
    // to its first data would be a countdown to nothing.
    let mut scheduled = match run_refresh_cycle(
        &source,
        attempt_timeout,
        next_refresh,
        &mut state,
        &publisher,
    )
    .await
    {
        RefreshCycleOutcome::Scheduled => true,
        RefreshCycleOutcome::Waiting => false,
        RefreshCycleOutcome::Stop => return,
    };

    loop {
        let attempted_at = if scheduled {
            // A manual refresh also resets the automatic schedule, so a pane the
            // user just refreshed does not fetch again seconds later.
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
            // refresh or the end of the account.
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
        next_refresh = attempted_at + refresh_interval;
        scheduled = match run_refresh_cycle(
            &source,
            attempt_timeout,
            next_refresh,
            &mut state,
            &publisher,
        )
        .await
        {
            RefreshCycleOutcome::Scheduled => true,
            RefreshCycleOutcome::Waiting => false,
            RefreshCycleOutcome::Stop => return,
        };
    }
}

/// What the loop does after one cycle.
///
/// The three answers are the three things a cycle can mean, and a failure is the
/// middle one: the loop keeps running so the account does not have to be replaced
/// to be recovered, but nothing is on the clock. Collapsing a failure into "the
/// cycle reported its result" would put a refused request back on the schedule and
/// turn a service that is briefly unreachable into a request every eight hours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefreshCycleOutcome {
    /// The cycle published its result; the next automatic fetch is due.
    Scheduled,
    /// The cycle reported a failure: nothing is on the clock until a refresh.
    Waiting,
    /// The publisher is gone, so there is nobody left to report to.
    Stop,
}

/// Fetches one snapshot and reports it.
///
/// A failure is published and then the cycle answers [`RefreshCycleOutcome::Waiting`]:
/// the loop is what decides whether to schedule another attempt, and after a
/// failure it does not — a service that just refused is not worth asking again
/// every eight hours.
async fn run_refresh_cycle<S>(
    source: &S,
    attempt_timeout: Duration,
    next_refresh: Instant,
    state: &mut StreamState,
    publisher: &StreamPublisher<EtfProtocol>,
) -> RefreshCycleOutcome
where
    S: EtfSource,
{
    let next_refresh_at = deadline_millis(next_refresh);

    let status = EtfStatus::Refreshing { next_refresh_at };
    if publish_status(publisher, status).await.is_err() {
        return RefreshCycleOutcome::Stop;
    }

    let fetch = fetch_changed_snapshot(source, state.latest.as_ref());
    let outcome = tokio::select! {
        biased;
        () = publisher.clone().cancelled_owned() => return RefreshCycleOutcome::Stop,
        result = timeout(attempt_timeout, fetch) => result,
    };
    match outcome {
        Ok(Ok((snapshot, changed))) => {
            if changed {
                state.revision = state.revision.saturating_add(1);
                let published = EtfStreamMessage::Snapshot {
                    revision: state.revision,
                    snapshot: snapshot.clone(),
                };
                if publisher.publish_latest(published).is_err() {
                    return RefreshCycleOutcome::Stop;
                }
                state.latest = Some(snapshot);
            }
            if publish_status(
                publisher,
                EtfStatus::Ready {
                    revision: state.revision,
                    next_refresh_at,
                },
            )
            .await
            .is_err()
            {
                return RefreshCycleOutcome::Stop;
            }
            RefreshCycleOutcome::Scheduled
        }
        Ok(Err(error)) => {
            publish_failure(publisher, error).await;
            RefreshCycleOutcome::Waiting
        }
        Err(_) => {
            publish_failure(publisher, EtfFetchError::Timeout(attempt_timeout)).await;
            RefreshCycleOutcome::Waiting
        }
    }
}

/// Publishes a failed cycle, ignoring the outcome: the caller only needs to know
/// the loop is still alive, and a closed publisher is noticed by the next write.
async fn publish_failure(publisher: &StreamPublisher<EtfProtocol>, error: EtfFetchError) {
    let _ = publisher
        .publish_reliable(EtfStreamMessage::Status(EtfStatus::Failed { error }))
        .await;
}

async fn publish_status(
    publisher: &StreamPublisher<EtfProtocol>,
    status: EtfStatus,
) -> Result<(), StreamPublishError> {
    publisher
        .publish_reliable(EtfStreamMessage::Status(status))
        .await
}

/// Fetches a snapshot, reusing the previous one when the answer did not change.
///
/// Reusing the allocation is what lets every layer above compare by pointer: a
/// pane that is handed the same `Arc` has nothing to redraw, and a revision that
/// did not move is what says so.
pub(crate) async fn fetch_changed_snapshot<S>(
    source: &S,
    previous: Option<&Arc<EtfSnapshot>>,
) -> Result<(Arc<EtfSnapshot>, bool), EtfFetchError>
where
    S: EtfSource,
{
    let snapshot = source.fetch_snapshot().await?;
    if let Some(previous) = previous
        && previous.as_ref() == snapshot.as_ref()
    {
        return Ok((previous.clone(), false));
    }
    Ok((snapshot, true))
}

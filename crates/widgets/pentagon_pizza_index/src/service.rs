//! The Pentagon Pizza Index protocol, its polling service, and the seams a test
//! drives it through.
//!
//! # A poll, not a subscription
//!
//! The feed publishes one point per hour and answers with the newest one, so there
//! is nothing to subscribe to: the pane re-reads it on an interval and counts down
//! to the next read. One stream task owns that loop, and every round does three
//! things in this order:
//!
//! 1. report that it is fetching, and when the next fetch is due, so the pane can
//!    count down to something it can see;
//! 2. read the feed once and turn the point into the reading the pane draws — a
//!    reading identical to the one already held is **not** republished, so a quiet
//!    hour does not schedule a frame;
//! 3. report readiness *after* the reading it describes, never before, because the
//!    two channels have no order between them and a status that ran ahead of its own
//!    reading would claim a reading the pane does not have.
//!
//! A failed round stops the clock without ending the scope: the task stays alive
//! and waits for a manual refresh, so a feed that is briefly unreachable does not
//! turn into a request every interval until someone notices. Only an explicit load
//! or refresh starts the clock again.
//!
//! # One scope
//!
//! The feed is global — no account, no selection, no venue — so the stream key is a
//! unit-shaped scope rather than a dimension of the request. It is still a key,
//! because the protocol requires one and because a replaced stream has to be told
//! apart from the one it replaced.

use std::{future::Future, sync::Arc, time::Duration};

use gpui_kit::{App, SharedString};
use thiserror::Error;
use tokio::{
    sync::mpsc,
    time::{Instant, timeout},
};
use widget::{
    error::LocalizedError,
    protocol::WidgetProtocol,
    runtime::{ServiceContext, WidgetService},
    stream::{StreamPublishError, StreamPublisher},
    util::deadline_millis,
};

use super::model::{PizzaDataError, PizzaReading, WireEnvelope};

/// How often the feed is re-read on its own.
///
/// The feed publishes one point per hour and every client of it reads the same
/// point, so reading more often would fetch the same reading again and spend
/// requests doing it.
pub(super) const REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// How long one round may take before it is treated as a failure.
pub(super) const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15);

/// The most bytes one answer may carry.
///
/// The whole answer is a few hundred bytes; this bound is what keeps a feed that
/// answers with something else from being read into memory a chunk at a time.
pub(super) const MAX_BODY_BYTES: usize = 64 * 1024;

/// The pending-refresh queue never needs to hold more than the one command a
/// manual refresh sends: a second one while the first is pending is answered as
/// busy rather than queued behind it.
const STREAM_COMMAND_CAPACITY: usize = 1;

/// What the pane asks the service to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PizzaRequest {
    /// Starts the poll over, replacing whatever generation was there.
    ///
    /// One request serves the framework's first load and its recovery alike: an
    /// explicit load is what starts a generation, and a generation reads the feed.
    Load,
    /// Reads the feed once, without waiting for the next interval.
    Refresh,
}

/// What the service answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PizzaResponse {
    StreamStarted,
    RefreshQueued,
}

/// The scope of one poll.
///
/// The feed is global, so there is nothing in it: it exists because the protocol
/// needs a key, and because a replaced stream has to be told apart from the one it
/// replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct PizzaScope;

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
    /// The request never reached the feed, or the answer never came back.
    #[error("the Pentagon Pizza Index request failed: {0}")]
    Transport(#[source] network::transport::error::TransportError),
    /// The feed could not be reached along the route the settings give it.
    ///
    /// The configuration error's own text is a diagnostic about a proxy entry, not
    /// a sentence for a reader, so it stays in the source and the message says what
    /// the reader can act on.
    #[error("the Pentagon Pizza Index route is unavailable: {0}")]
    Route(#[source] app_config::network::ConfigError),
    /// The answer was not one this pane could read.
    #[error("the Pentagon Pizza Index answer was unreadable: {0}")]
    Data(#[source] PizzaDataError),
    /// The answer did not arrive within the round's own bound.
    #[error("the Pentagon Pizza Index did not answer within {0:?}")]
    Timeout(Duration),
}

impl LocalizedError for PizzaFetchError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Transport(error) => error.localized_message(cx),
            Self::Route(_) => i18n::t!(cx, "error.pentagon_pizza_index.route"),
            Self::Data(error) => error.localized_message(cx),
            Self::Timeout(timeout) => i18n::t!(
                cx,
                "error.pentagon_pizza_index.timeout",
                seconds = timeout.as_secs()
            ),
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
            Self::StreamClosed | Self::StreamBusy => {
                i18n::t!(cx, "error.pentagon_pizza_index.unavailable")
            }
        }
    }
}

/// One message of the feed's stream.
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
    type StreamKey = PizzaScope;
    type StreamMessage = PizzaStreamMessage;
    type Error = PizzaServiceError;
}

/// The seam between this widget and the feed, so a test can answer instead.
///
/// One call is one answer: what comes back is the document itself rather than a
/// reading, so that a malformed answer is refused by the model in one place rather
/// than by every source in its own way.
pub(super) trait PizzaSource: Clone + Send + Sync + 'static {
    fn fetch(&self) -> impl Future<Output = Result<WireEnvelope, PizzaFetchError>> + Send;
}

/// The service that polls one pane's feed.
pub(super) struct PizzaService<S = UrlSource> {
    source: S,
    refresh_interval: Duration,
    attempt_timeout: Duration,
}

impl<S> PizzaService<S> {
    pub(super) fn new(source: S) -> Self {
        Self {
            source,
            refresh_interval: REFRESH_INTERVAL,
            attempt_timeout: ATTEMPT_TIMEOUT,
        }
    }

    /// The same service with timings a test can wait out.
    #[cfg(test)]
    pub(super) fn with_timing(
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

impl<S> WidgetService<PizzaProtocol> for PizzaService<S>
where
    S: PizzaSource,
{
    async fn run(self, mut context: ServiceContext<PizzaProtocol>) {
        let mut commands: Option<mpsc::Sender<StreamCommand>> = None;

        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            match request {
                PizzaRequest::Load => {
                    // The answer is worth less than the request it belongs to: a
                    // caller that has already given up is not waiting for it.
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    let (sender, receiver) = mpsc::channel(STREAM_COMMAND_CAPACITY);
                    let source = self.source.clone();
                    let refresh_interval = self.refresh_interval;
                    let attempt_timeout = self.attempt_timeout;
                    // A load always replaces. A stream that answered `StreamStarted`
                    // for a generation nobody replaced would leave the pane waiting
                    // on a schedule that no longer exists: after a failure the loop
                    // is alive but off the clock, and "already reading" is the one
                    // state this cannot tell from "reported a failure and stopped".
                    context.replace_stream(PizzaScope, move |publisher| {
                        run_stream(
                            source,
                            refresh_interval,
                            attempt_timeout,
                            receiver,
                            publisher,
                        )
                    });
                    if reply.respond(Ok(PizzaResponse::StreamStarted)).is_ok() {
                        commands = Some(sender);
                    } else {
                        // Nobody is listening to this stream any more, and the
                        // command channel is gone with the answer.
                        context.cancel_stream(&PizzaScope);
                        commands = None;
                    }
                }
                PizzaRequest::Refresh => {
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    // A refresh is only worth sending to the loop serving the
                    // generation on screen: after a failure the loop is off the
                    // clock, so a command handed to it would be answered as queued
                    // and never run. The next load starts a generation again.
                    let Some(sender) = commands
                        .as_ref()
                        .filter(|_| context.stream_generation(&PizzaScope).is_some())
                    else {
                        let _ = reply.respond(Err(PizzaServiceError::StreamClosed));
                        continue;
                    };
                    // The queue holds one command and the loop takes a round at a
                    // time: a second click while the first read is in flight is
                    // refused here rather than queued behind it.
                    match sender.try_send(StreamCommand::Refresh) {
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

/// The refresh command a manual refresh sends to the poll loop.
enum StreamCommand {
    Refresh,
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
    refresh_interval: Duration,
    attempt_timeout: Duration,
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
    let mut next_refresh = Instant::now() + refresh_interval;
    let mut scheduled = match run_refresh_round(
        &source,
        attempt_timeout,
        next_refresh,
        &mut state,
        &publisher,
    )
    .await
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
        next_refresh = attempted_at + refresh_interval;
        scheduled = match run_refresh_round(
            &source,
            attempt_timeout,
            next_refresh,
            &mut state,
            &publisher,
        )
        .await
        {
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

/// Reads the feed once and reports it.
async fn run_refresh_round<S>(
    source: &S,
    attempt_timeout: Duration,
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

    let outcome = tokio::select! {
        biased;
        () = publisher.clone().cancelled_owned() => return RefreshRoundOutcome::Stop,
        result = timeout(attempt_timeout, source.fetch()) => result,
    };
    let envelope = match outcome {
        Ok(Ok(envelope)) => envelope,
        Ok(Err(error)) => {
            publish_failure(publisher, error).await;
            return RefreshRoundOutcome::Waiting;
        }
        Err(_) => {
            publish_failure(publisher, PizzaFetchError::Timeout(attempt_timeout)).await;
            return RefreshRoundOutcome::Waiting;
        }
    };

    // A refused or unreadable answer is a failure of the read rather than a
    // reading: reporting one as the calmest rung would claim quiet in answer to a
    // question that failed.
    let reading = match envelope.into_reading() {
        Ok(reading) => reading,
        Err(error) => {
            publish_failure(publisher, PizzaFetchError::Data(error)).await;
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

/// The address of the point the pane draws.
///
/// It is the feed's own endpoint rather than a page: the dashboard reads the same
/// one, and it is the only answer that carries a level and the moment it was
/// observed at without the rest of the dashboard around it.
pub(super) const LATEST_POINT_URL: &str = "https://www.pizzint.watch/api/defcon/latest";

/// The production source: one HTTPS read of the feed, along its configured route.
#[derive(Clone)]
pub(super) struct UrlSource {
    transport: Arc<network::transport::hub::TransportHub>,
    config: app_config_provider::notification::ConfigNotification<app_config::network::Config>,
    runtime: widget_tokio_runtime::runtime::Runtime,
    url: url::Url,
}

impl UrlSource {
    /// The feed's live endpoint, read along the route the settings give it.
    pub(super) fn live(
        transport: Arc<network::transport::hub::TransportHub>,
        config: app_config_provider::notification::ConfigNotification<app_config::network::Config>,
        runtime: widget_tokio_runtime::runtime::Runtime,
    ) -> Self {
        Self::at(
            transport,
            config,
            runtime,
            url::Url::parse(LATEST_POINT_URL)
                .expect("the feed's own address is a URL this build can parse"),
        )
    }

    /// The same source against another address, which is what a test drives it
    /// with so it can answer from a server of its own.
    pub(super) fn at(
        transport: Arc<network::transport::hub::TransportHub>,
        config: app_config_provider::notification::ConfigNotification<app_config::network::Config>,
        runtime: widget_tokio_runtime::runtime::Runtime,
        url: url::Url,
    ) -> Self {
        Self {
            transport,
            config,
            runtime,
            url,
        }
    }
}

impl PizzaSource for UrlSource {
    async fn fetch(&self) -> Result<WireEnvelope, PizzaFetchError> {
        use app_config::network::{KytrixServiceCapability, RouteTarget, ServiceTarget};
        use network::transport::http::{HttpClientClass, read_response_body};

        let cancellation = tokio_util::sync::CancellationToken::new();
        // A request is addressed through the Kytrix service that serves it, which is
        // what its line is configured under: this feed has a capability of its own so
        // that a reader behind a firewall can send it down a line of its own.
        let target = RouteTarget::Service(ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::PentagonPizzaIndex,
        });
        let config = self.config.current();
        let route = network::proxy::resolve_async_cancellable(
            &config,
            &target,
            &self.url,
            cancellation.clone(),
            &self.runtime,
        )
        .await
        .map_err(PizzaFetchError::Route)?;
        let client = self
            .transport
            .http()
            .client(&route, HttpClientClass::Public)
            .map_err(PizzaFetchError::Transport)?;
        // The permit keeps a shutdown from closing the transport under a transfer
        // that has not finished, so it is held until the body has been read.
        let _operation = self
            .transport
            .begin_operation()
            .map_err(PizzaFetchError::Transport)?;

        let response = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                return Err(PizzaFetchError::Transport(
                    network::transport::error::TransportError::Cancelled,
                ));
            }
            response = client.get(self.url.clone()).send() => {
                response.map_err(|error| PizzaFetchError::Transport(send_error(&error)))?
            }
        };
        if !response.status().is_success() {
            return Err(PizzaFetchError::Transport(
                network::transport::error::TransportError::ResponseReadFailed,
            ));
        }
        let body = read_response_body(response, HttpClientClass::Public, &cancellation)
            .await
            .map_err(PizzaFetchError::Transport)?;
        // The transport's own bound is the client class's, which is sized for a
        // catalog rather than for this document: an answer larger than the few
        // hundred bytes this feed publishes is not this feed's answer, and it is
        // refused before it is parsed rather than after.
        if body.len() > MAX_BODY_BYTES {
            return Err(PizzaFetchError::Data(PizzaDataError::Unreadable));
        }

        // The answer is read as the type the model wants rather than as a stored
        // document: a body that is not this document is a read that failed, not a
        // reading of nothing.
        serde_json::from_slice(&body).map_err(|_| PizzaFetchError::Data(PizzaDataError::Unreadable))
    }
}

/// Why one request did not reach the feed.
///
/// A transport failure is classified into the two things a reader can act on: a
/// request that timed out, and one that failed for any other reason. The whole
/// `reqwest` error is a diagnostic with an address in it, so it is not the message.
fn send_error(error: &reqwest::Error) -> network::transport::error::TransportError {
    if error.is_timeout() {
        network::transport::error::TransportError::RequestTimedOut
    } else {
        network::transport::error::TransportError::ResponseReadFailed
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use widget::{rpc::RpcCallError, runtime::WidgetRuntime, stream::ReliableStream};

    use super::*;
    use crate::{
        model::PizzaDataError,
        test_support::{FakeSource, answer, reading, unreadable},
    };

    /// The handle of the runtime this test runs on.
    ///
    /// A test drives its subject on its own runtime rather than on a shared one, so
    /// the paused clock it advances is the one the subject reads.
    #[allow(
        clippy::disallowed_methods,
        reason = "a test drives its subject on its own runtime so the paused clock it advances is the one the subject reads"
    )]
    fn current_test_handle() -> &'static tokio::runtime::Handle {
        Box::leak(Box::new(tokio::runtime::Handle::current()))
    }

    fn runtime(source: &FakeSource) -> WidgetRuntime<PizzaProtocol> {
        WidgetRuntime::spawn(
            current_test_handle(),
            PizzaService::with_timing(
                source.clone(),
                Duration::from_secs(60),
                Duration::from_secs(5),
            ),
        )
    }

    async fn load(runtime: &WidgetRuntime<PizzaProtocol>) {
        let response = runtime
            .client()
            .call(PizzaRequest::Load)
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

    async fn receive_status(reliable: &mut ReliableStream<PizzaProtocol>, expected: PizzaStatus) {
        let status = next_reliable(
            reliable,
            |message| matches!(message, PizzaStreamMessage::Status(status) if *status == expected),
        )
        .await;
        assert!(matches!(status, PizzaStreamMessage::Status(_)));
    }

    #[tokio::test]
    async fn a_round_should_publish_the_reading_before_it_reports_ready() {
        let source = FakeSource::default();
        source.answer(answer(3));
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
        let source = FakeSource::default();
        source.answer(answer(3));
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
        assert!(matches!(ready, PizzaStreamMessage::Status(_)));
        assert_eq!(
            source.fetches(),
            2,
            "the same reading was read twice and published once"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_failed_round_should_stop_the_clock_without_ending_the_scope() {
        let source = FakeSource::default();
        source.fail(PizzaFetchError::Timeout(Duration::from_secs(1)));
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
        assert_eq!(error, PizzaFetchError::Timeout(Duration::from_secs(1)));
        assert_eq!(
            source.fetches(),
            1,
            "a failed round is off the clock rather than scheduled again"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_refused_answer_should_fail_the_round_rather_than_report_calm() {
        let source = FakeSource::default();
        source.refuse();
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
        assert!(matches!(
            error,
            PizzaFetchError::Data(PizzaDataError::Refused)
        ));
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_load_should_replace_the_generation_it_replaces() {
        let source = FakeSource::default();
        source.answer(answer(2));
        let mut runtime = runtime(&source);
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");

        load(&runtime).await;
        receive_status(&mut reliable, PizzaStatus::Connecting).await;
        let replaced = runtime
            .client()
            .call(PizzaRequest::Load)
            .await
            .expect("the second load should start")
            .into_value();
        assert_eq!(replaced, PizzaResponse::StreamStarted);
        assert_eq!(
            source.fetches(),
            2,
            "a replacement reads the feed again rather than reusing the old generation"
        );
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

    /// A body the pane is willing to read is one within the bound it states, and the
    /// bound is what keeps a feed answering with something else from being parsed.
    #[test]
    fn a_body_should_be_bounded_by_the_size_this_pane_reads() {
        assert!(
            answer(3).len() < MAX_BODY_BYTES,
            "the answer this feed publishes is far inside the bound"
        );
        assert!(unreadable().len() < MAX_BODY_BYTES);
    }
}

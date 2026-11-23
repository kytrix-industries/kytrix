use std::{future::Future, sync::Arc, time::Duration};

use i18n::{t_smol, translate::Translate};
use kytrix_proto::types::v1 as proto_types;
use kytrix_service_provider::service::{
    connector::KytrixServiceConnector, engine::KytrixServiceError,
};
use smol_str::SmolStr;
use thiserror::Error;
use tokio::{sync::mpsc, time::Instant};
use widget::{
    error::TranslatedError,
    protocol::{StreamGeneration, WidgetProtocol},
    runtime::{ServiceContext, WidgetService},
    stream::{StreamPublishError, StreamPublisher},
    util::deadline_millis,
};

use super::model::{PizzaDataError, PizzaReading};

pub(super) const REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);

const STREAM_COMMAND_CAPACITY: usize = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PizzaRequest {
    Load(PizzaStreamKey),
    Refresh,
    Unload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PizzaResponse {
    StreamStarted,
    RefreshQueued,
    StreamStopped,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct PizzaStreamKey {
    pub account_id: SmolStr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PizzaStatus {
    Connecting,
    Refreshing { next_refresh_at: i64 },
    Ready { revision: u64, next_refresh_at: i64 },
    Failed(PizzaFetchError),
}

impl PizzaStatus {
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

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(super) enum PizzaFetchError {
    #[error("the Pentagon Pizza Index request failed: {0}")]
    Service(#[from] KytrixServiceError),
    #[error("the Pentagon Pizza Index answer was unreadable: {0}")]
    Data(#[from] PizzaDataError),
}

impl TranslatedError for PizzaFetchError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Service(error) => error.translated_message(translate),
            Self::Data(error) => error.translated_message(translate),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub(super) enum PizzaServiceError {
    #[error("the Pentagon Pizza Index stream is closed")]
    StreamClosed,
    #[error("a Pentagon Pizza Index refresh is already in flight")]
    StreamBusy,
}

impl TranslatedError for PizzaServiceError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::StreamClosed => t_smol!(translate, "error.pentagon_pizza_index.unavailable"),
            Self::StreamBusy => t_smol!(translate, "error.pentagon_pizza_index.busy"),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum PizzaStreamMessage {
    Status(PizzaStatus),
    Snapshot(Arc<PizzaSnapshot>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PizzaSnapshot {
    revision: u64,
    reading: PizzaReading,
}

impl PizzaSnapshot {
    pub(super) const fn new(revision: u64, reading: PizzaReading) -> Self {
        Self { revision, reading }
    }

    pub(super) const fn revision(&self) -> u64 {
        self.revision
    }

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

enum StreamCommand {
    Refresh,
}

struct ActiveStream {
    key: PizzaStreamKey,
    generation: StreamGeneration,
    commands: mpsc::Sender<StreamCommand>,
}

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
                    if reply.respond(Ok(PizzaResponse::StreamStarted)).is_ok() {
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
                PizzaRequest::Unload => {
                    if let Some(stream) = active.take() {
                        context.cancel_stream(&stream.key);
                    }
                    let _ = reply.respond(Ok(PizzaResponse::StreamStopped));
                }
                PizzaRequest::Refresh => {
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    let running = active.as_ref().filter(|stream| {
                        context
                            .stream_generation(&stream.key)
                            .is_some_and(|generation| generation == stream.generation)
                    });
                    let Some(stream) = running else {
                        let _ = reply.respond(Err(PizzaServiceError::StreamClosed));
                        continue;
                    };
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

#[derive(Default)]
struct StreamState {
    latest: Option<Arc<PizzaReading>>,
    revision: u64,
}

async fn run_stream<S>(
    source: S,
    mut commands: mpsc::Receiver<StreamCommand>,
    publisher: StreamPublisher<PizzaProtocol>,
) where
    S: PizzaSource,
{
    let mut state = StreamState::default();
    let _ = publish_status(&publisher, PizzaStatus::Connecting).await;

    let mut next_refresh = Instant::now() + REFRESH_INTERVAL;
    let mut scheduled = match run_refresh_round(&source, next_refresh, &mut state, &publisher).await
    {
        RefreshRoundOutcome::Scheduled => true,
        RefreshRoundOutcome::Waiting => false,
        RefreshRoundOutcome::Stop => return,
    };

    loop {
        let attempted_at = if scheduled {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RefreshRoundOutcome {
    Scheduled,
    Waiting,
    Stop,
}

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

    async fn receive_status(reliable: &mut ReliableStream<PizzaProtocol>, expected: PizzaStatus) {
        next_reliable(
            reliable,
            |message| matches!(message, PizzaStreamMessage::Status(status) if *status == expected),
        )
        .await;
    }

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

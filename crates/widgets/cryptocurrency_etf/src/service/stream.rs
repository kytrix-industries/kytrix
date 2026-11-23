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

use super::super::model::{CryptocurrencyEtfParseError, CryptocurrencyEtfSnapshot, parse_etf};

pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(8 * 60 * 60);

pub(crate) const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) const STREAM_COMMAND_CAPACITY: usize = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CryptocurrencyEtfRequest {
    Load(CryptocurrencyEtfStreamKey),
    Retry(CryptocurrencyEtfStreamKey),
    Refresh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CryptocurrencyEtfResponse {
    StreamStarted,
    RefreshQueued,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CryptocurrencyEtfStreamKey {
    pub account_id: SmolStr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CryptocurrencyEtfStatus {
    Connecting,
    Refreshing { next_refresh_at: i64 },
    Ready { revision: u64, next_refresh_at: i64 },
    Failed { error: CryptocurrencyEtfFetchError },
}

impl CryptocurrencyEtfStatus {
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

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum CryptocurrencyEtfFetchError {
    #[error("the ETF request failed: {0}")]
    Service(#[from] KytrixServiceError),
    #[error("the ETF snapshot could not be read: {0}")]
    Parse(#[from] CryptocurrencyEtfParseError),
    #[error("the ETF request did not answer within {0:?}")]
    Timeout(Duration),
}

#[derive(Clone, Debug)]
pub(crate) enum CryptocurrencyEtfStreamMessage {
    Status(CryptocurrencyEtfStatus),
    Snapshot {
        revision: u64,
        snapshot: Arc<CryptocurrencyEtfSnapshot>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum CryptocurrencyEtfServiceError {
    #[error("the ETF refresh queue is closed")]
    StreamClosed,
    #[error("an ETF refresh is already in flight")]
    StreamBusy,
}

pub(crate) struct CryptocurrencyEtfProtocol;

impl WidgetProtocol for CryptocurrencyEtfProtocol {
    type Request = CryptocurrencyEtfRequest;
    type Response = CryptocurrencyEtfResponse;
    type StreamKey = CryptocurrencyEtfStreamKey;
    type StreamMessage = CryptocurrencyEtfStreamMessage;
    type Error = CryptocurrencyEtfServiceError;
}

pub(crate) trait CryptocurrencyEtfSource: Clone + Send + Sync + 'static {
    fn fetch_snapshot(
        &self,
    ) -> impl Future<Output = Result<Arc<CryptocurrencyEtfSnapshot>, CryptocurrencyEtfFetchError>> + Send;
}

impl CryptocurrencyEtfSource for KytrixServiceConnector {
    async fn fetch_snapshot(
        &self,
    ) -> Result<Arc<CryptocurrencyEtfSnapshot>, CryptocurrencyEtfFetchError> {
        parse_etf(self.fetch_etf().await?)
            .map(Arc::new)
            .map_err(Into::into)
    }
}

pub(crate) enum StreamCommand {
    Refresh,
}

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
    key: CryptocurrencyEtfStreamKey,
    generation: StreamGeneration,
    commands: mpsc::Sender<StreamCommand>,
    gate: RefreshGate,
}

#[derive(Default)]
pub(crate) struct StreamState {
    latest: Option<Arc<CryptocurrencyEtfSnapshot>>,
    revision: u64,
}

pub(crate) struct CryptocurrencyEtfService<S = KytrixServiceConnector> {
    source: S,
    refresh_interval: Duration,
    attempt_timeout: Duration,
}

impl<S> CryptocurrencyEtfService<S> {
    pub(crate) const fn new(source: S) -> Self {
        Self {
            source,
            refresh_interval: REFRESH_INTERVAL,
            attempt_timeout: ATTEMPT_TIMEOUT,
        }
    }

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

impl<S> WidgetService<CryptocurrencyEtfProtocol> for CryptocurrencyEtfService<S>
where
    S: CryptocurrencyEtfSource,
{
    async fn run(self, mut context: ServiceContext<CryptocurrencyEtfProtocol>) {
        let mut active: Option<ActiveStream> = None;

        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            match request {
                CryptocurrencyEtfRequest::Load(key) | CryptocurrencyEtfRequest::Retry(key) => {
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    let (commands, stream_commands) = mpsc::channel(STREAM_COMMAND_CAPACITY);
                    let source = self.source.clone();
                    let refresh_interval = self.refresh_interval;
                    let attempt_timeout = self.attempt_timeout;
                    let generation = context.replace_stream(key.clone(), move |publisher| {
                        run_stream(
                            source,
                            refresh_interval,
                            attempt_timeout,
                            stream_commands,
                            publisher,
                        )
                    });
                    if reply
                        .respond(Ok(CryptocurrencyEtfResponse::StreamStarted))
                        .is_ok()
                    {
                        active = Some(ActiveStream {
                            key,
                            generation,
                            commands,
                            gate: RefreshGate::default(),
                        });
                    } else {
                        context.cancel_stream(&key);
                        active = None;
                    }
                }
                CryptocurrencyEtfRequest::Refresh => {
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    let running = active.as_ref().filter(|stream| {
                        context
                            .stream_generation(&stream.key)
                            .is_some_and(|generation| generation == stream.generation)
                    });
                    let Some(stream) = running else {
                        let _ = reply.respond(Err(CryptocurrencyEtfServiceError::StreamClosed));
                        continue;
                    };
                    let Some(permit) = stream.gate.try_acquire() else {
                        let _ = reply.respond(Err(CryptocurrencyEtfServiceError::StreamBusy));
                        continue;
                    };
                    match stream.commands.try_send(StreamCommand::Refresh) {
                        Ok(()) => {
                            let _ = reply.respond(Ok(CryptocurrencyEtfResponse::RefreshQueued));
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            drop(permit);
                            let _ = reply.respond(Err(CryptocurrencyEtfServiceError::StreamBusy));
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            drop(permit);
                            let _ = reply.respond(Err(CryptocurrencyEtfServiceError::StreamClosed));
                        }
                    }
                }
            }
        }
    }
}

async fn run_stream<S>(
    source: S,
    refresh_interval: Duration,
    attempt_timeout: Duration,
    mut commands: mpsc::Receiver<StreamCommand>,
    publisher: StreamPublisher<CryptocurrencyEtfProtocol>,
) where
    S: CryptocurrencyEtfSource,
{
    let mut state = StreamState::default();
    let mut next_refresh = Instant::now() + refresh_interval;
    let _ = publish_status(&publisher, CryptocurrencyEtfStatus::Connecting).await;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefreshCycleOutcome {
    Scheduled,
    Waiting,
    Stop,
}

async fn run_refresh_cycle<S>(
    source: &S,
    attempt_timeout: Duration,
    next_refresh: Instant,
    state: &mut StreamState,
    publisher: &StreamPublisher<CryptocurrencyEtfProtocol>,
) -> RefreshCycleOutcome
where
    S: CryptocurrencyEtfSource,
{
    let next_refresh_at = deadline_millis(next_refresh);

    let status = CryptocurrencyEtfStatus::Refreshing { next_refresh_at };
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
                let published = CryptocurrencyEtfStreamMessage::Snapshot {
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
                CryptocurrencyEtfStatus::Ready {
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
            publish_failure(
                publisher,
                CryptocurrencyEtfFetchError::Timeout(attempt_timeout),
            )
            .await;
            RefreshCycleOutcome::Waiting
        }
    }
}

async fn publish_failure(
    publisher: &StreamPublisher<CryptocurrencyEtfProtocol>,
    error: CryptocurrencyEtfFetchError,
) {
    let _ = publisher
        .publish_reliable(CryptocurrencyEtfStreamMessage::Status(
            CryptocurrencyEtfStatus::Failed { error },
        ))
        .await;
}

async fn publish_status(
    publisher: &StreamPublisher<CryptocurrencyEtfProtocol>,
    status: CryptocurrencyEtfStatus,
) -> Result<(), StreamPublishError> {
    publisher
        .publish_reliable(CryptocurrencyEtfStreamMessage::Status(status))
        .await
}

pub(crate) async fn fetch_changed_snapshot<S>(
    source: &S,
    previous: Option<&Arc<CryptocurrencyEtfSnapshot>>,
) -> Result<(Arc<CryptocurrencyEtfSnapshot>, bool), CryptocurrencyEtfFetchError>
where
    S: CryptocurrencyEtfSource,
{
    let snapshot = source.fetch_snapshot().await?;
    if let Some(previous) = previous
        && previous.as_ref() == snapshot.as_ref()
    {
        return Ok((previous.clone(), false));
    }
    Ok((snapshot, true))
}

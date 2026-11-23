use std::{future::Future, sync::Arc, time::Duration};

use market_participants_connector::market::{
    error::MarketError, provider::MarketCapability, timeframe::StatisticsTimeFrame,
};
use market_participants_model::{
    listing::{route::VenueListingKey, rules::ListingPrecision},
    market::book::{ListingLongShortRatio, ListingOpenInterest, ListingTakerVolume},
};
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

use crate::model::{
    MetricKind, POINT_LIMIT, StatisticsPoints, StatisticsSnapshot, normalize_long_short_ratio,
    normalize_open_interest, normalize_taker_volume,
};

pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

pub(crate) const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) const STREAM_COMMAND_CAPACITY: usize = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatisticsRequest {
    Load {
        metric: MetricKind,
        listing: VenueListingKey,
        timeframe: StatisticsTimeFrame,
    },
    Retry {
        metric: MetricKind,
        listing: VenueListingKey,
        timeframe: StatisticsTimeFrame,
    },
    Refresh,
}

impl StatisticsRequest {
    pub(crate) fn from_scope(
        metric: MetricKind,
        listing: VenueListingKey,
        timeframe: StatisticsTimeFrame,
        retry: bool,
    ) -> Self {
        if retry {
            Self::Retry {
                metric,
                listing,
                timeframe,
            }
        } else {
            Self::Load {
                metric,
                listing,
                timeframe,
            }
        }
    }

    fn into_parts(self) -> Option<(bool, StatisticsStreamKey)> {
        match self {
            Self::Load {
                metric,
                listing,
                timeframe,
            } => Some((false, StatisticsStreamKey::new(metric, listing, timeframe))),
            Self::Retry {
                metric,
                listing,
                timeframe,
            } => Some((true, StatisticsStreamKey::new(metric, listing, timeframe))),
            Self::Refresh => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatisticsResponse {
    StreamStarted,
    RefreshQueued,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StatisticsStreamKey {
    pub metric: MetricKind,
    pub listing: VenueListingKey,
    pub timeframe: StatisticsTimeFrame,
}

impl StatisticsStreamKey {
    pub(crate) fn new(
        metric: MetricKind,
        listing: VenueListingKey,
        timeframe: StatisticsTimeFrame,
    ) -> Self {
        Self {
            metric,
            listing,
            timeframe,
        }
    }

    pub(crate) fn query(&self) -> StatisticsQuery {
        StatisticsQuery {
            listing: self.listing.clone(),
            timeframe: self.timeframe,
            limit: u16::from(POINT_LIMIT),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatisticsQuery {
    pub listing: VenueListingKey,
    pub timeframe: StatisticsTimeFrame,
    pub limit: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatisticsStatus {
    Connecting,
    Refreshing { next_refresh_at: i64 },
    Ready { revision: u64, next_refresh_at: i64 },
    Failed { error: StatisticsFetchError },
}

impl StatisticsStatus {
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
pub enum StatisticsFetchError {
    #[error("the statistics request failed: {0}")]
    MarketParticipant(#[source] MarketError),
    #[error("the statistics request did not answer within {0:?}")]
    Timeout(Duration),
}

#[derive(Clone, Debug)]
pub enum StatisticsStreamMessage {
    Status(StatisticsStatus),
    Snapshot(Arc<StatisticsSnapshot>),
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum StatisticsServiceError {
    #[error("the statistics refresh queue is closed")]
    StreamClosed,
    #[error("a statistics refresh is already in flight")]
    StreamBusy,
}

pub(crate) struct StatisticsProtocol;

impl WidgetProtocol for StatisticsProtocol {
    type Request = StatisticsRequest;
    type Response = StatisticsResponse;
    type StreamKey = StatisticsStreamKey;
    type StreamMessage = StatisticsStreamMessage;
    type Error = StatisticsServiceError;
}

pub trait StatisticsSource: Clone + Send + Sync + 'static {
    fn fetch_listing_metadata(
        &self,
        listing: &VenueListingKey,
    ) -> impl Future<Output = Result<ListingPrecision, MarketError>> + Send;

    fn fetch_open_interest(
        &self,
        query: StatisticsQuery,
    ) -> impl Future<Output = Result<Vec<ListingOpenInterest>, MarketError>> + Send;

    fn fetch_top_trader_account_ratio(
        &self,
        query: StatisticsQuery,
    ) -> impl Future<Output = Result<Vec<ListingLongShortRatio>, MarketError>> + Send;

    fn fetch_top_trader_position_ratio(
        &self,
        query: StatisticsQuery,
    ) -> impl Future<Output = Result<Vec<ListingLongShortRatio>, MarketError>> + Send;

    fn fetch_long_short_account_ratio(
        &self,
        query: StatisticsQuery,
    ) -> impl Future<Output = Result<Vec<ListingLongShortRatio>, MarketError>> + Send;

    fn fetch_taker_volume(
        &self,
        query: StatisticsQuery,
    ) -> impl Future<Output = Result<Vec<ListingTakerVolume>, MarketError>> + Send;
}

impl StatisticsSource for MarketCapability {
    async fn fetch_listing_metadata(
        &self,
        listing: &VenueListingKey,
    ) -> Result<ListingPrecision, MarketError> {
        let resolved = self.resolve_listing(listing)?;
        Ok(ListingPrecision::of(&resolved.rules))
    }

    async fn fetch_open_interest(
        &self,
        query: StatisticsQuery,
    ) -> Result<Vec<ListingOpenInterest>, MarketError> {
        self.market(query.listing.route.clone())
            .analytics()
            .open_interest(query.listing)
            .timeframe(query.timeframe)
            .limit(query.limit)
            .fetch()
            .await
    }

    async fn fetch_top_trader_account_ratio(
        &self,
        query: StatisticsQuery,
    ) -> Result<Vec<ListingLongShortRatio>, MarketError> {
        self.market(query.listing.route.clone())
            .analytics()
            .top_trader_account_ratio(query.listing)
            .timeframe(query.timeframe)
            .limit(query.limit)
            .fetch()
            .await
    }

    async fn fetch_top_trader_position_ratio(
        &self,
        query: StatisticsQuery,
    ) -> Result<Vec<ListingLongShortRatio>, MarketError> {
        self.market(query.listing.route.clone())
            .analytics()
            .top_trader_position_ratio(query.listing)
            .timeframe(query.timeframe)
            .limit(query.limit)
            .fetch()
            .await
    }

    async fn fetch_long_short_account_ratio(
        &self,
        query: StatisticsQuery,
    ) -> Result<Vec<ListingLongShortRatio>, MarketError> {
        self.market(query.listing.route.clone())
            .analytics()
            .long_short_ratio(query.listing)
            .timeframe(query.timeframe)
            .limit(query.limit)
            .fetch()
            .await
    }

    async fn fetch_taker_volume(
        &self,
        query: StatisticsQuery,
    ) -> Result<Vec<ListingTakerVolume>, MarketError> {
        self.market(query.listing.route.clone())
            .analytics()
            .taker_volume(query.listing)
            .timeframe(query.timeframe)
            .limit(query.limit)
            .fetch()
            .await
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
    key: StatisticsStreamKey,
    generation: StreamGeneration,
    commands: mpsc::Sender<StreamCommand>,
    gate: RefreshGate,
}

#[derive(Default)]
pub(crate) struct StreamState {
    metadata: Option<ListingPrecision>,
    latest: Option<StatisticsPoints>,
    revision: u64,
}

pub struct StatisticsService<S = MarketCapability> {
    source: S,
    refresh_interval: Duration,
    attempt_timeout: Duration,
}

impl<S> StatisticsService<S> {
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

impl<S> WidgetService<StatisticsProtocol> for StatisticsService<S>
where
    S: StatisticsSource,
{
    async fn run(self, mut context: ServiceContext<StatisticsProtocol>) {
        let mut active: Option<ActiveStream> = None;

        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            match request {
                request @ (StatisticsRequest::Load { .. } | StatisticsRequest::Retry { .. }) => {
                    let Some((_, key)) = request.into_parts() else {
                        continue;
                    };
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    let (commands, stream_commands) = mpsc::channel(STREAM_COMMAND_CAPACITY);
                    let source = self.source.clone();
                    let refresh_interval = self.refresh_interval;
                    let attempt_timeout = self.attempt_timeout;
                    let stream_key = key.clone();
                    let generation = context.replace_stream(key.clone(), move |publisher| {
                        run_stream(
                            source,
                            stream_key,
                            refresh_interval,
                            attempt_timeout,
                            stream_commands,
                            publisher,
                        )
                    });
                    if reply.respond(Ok(StatisticsResponse::StreamStarted)).is_ok() {
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
                StatisticsRequest::Refresh => {
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    let running = active.as_ref().filter(|stream| {
                        context
                            .stream_generation(&stream.key)
                            .is_some_and(|generation| generation == stream.generation)
                    });
                    let Some(stream) = running else {
                        let _ = reply.respond(Err(StatisticsServiceError::StreamClosed));
                        continue;
                    };
                    let Some(permit) = stream.gate.try_acquire() else {
                        let _ = reply.respond(Err(StatisticsServiceError::StreamBusy));
                        continue;
                    };
                    match stream.commands.try_send(StreamCommand::Refresh) {
                        Ok(()) => {
                            let _ = reply.respond(Ok(StatisticsResponse::RefreshQueued));
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            drop(permit);
                            let _ = reply.respond(Err(StatisticsServiceError::StreamBusy));
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            drop(permit);
                            let _ = reply.respond(Err(StatisticsServiceError::StreamClosed));
                        }
                    }
                }
            }
        }
    }
}

async fn run_stream<S>(
    source: S,
    key: StatisticsStreamKey,
    refresh_interval: Duration,
    attempt_timeout: Duration,
    mut commands: mpsc::Receiver<StreamCommand>,
    publisher: StreamPublisher<StatisticsProtocol>,
) where
    S: StatisticsSource,
{
    let mut state = StreamState::default();
    let mut next_refresh = Instant::now() + refresh_interval;
    let _ = publish_status(&publisher, StatisticsStatus::Connecting).await;

    let mut scheduled = match run_refresh_cycle(
        &source,
        &key,
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
            &key,
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
    key: &StatisticsStreamKey,
    attempt_timeout: Duration,
    next_refresh: Instant,
    state: &mut StreamState,
    publisher: &StreamPublisher<StatisticsProtocol>,
) -> RefreshCycleOutcome
where
    S: StatisticsSource,
{
    let next_refresh_at = deadline_millis(next_refresh);

    let status = StatisticsStatus::Refreshing { next_refresh_at };
    if publish_status(publisher, status).await.is_err() {
        return RefreshCycleOutcome::Stop;
    }

    let fetch = fetch_snapshot(source, key, &mut state.metadata, state.latest.as_ref());
    let outcome = tokio::select! {
        biased;
        () = publisher.clone().cancelled_owned() => return RefreshCycleOutcome::Stop,
        result = timeout(attempt_timeout, fetch) => result,
    };
    match outcome {
        Ok(Ok((points, changed))) => {
            if changed {
                state.revision = state.revision.saturating_add(1);
                let published = StatisticsStreamMessage::Snapshot(Arc::new(StatisticsSnapshot {
                    revision: state.revision,
                    metadata: state.metadata,
                    points: points.clone(),
                }));

                if publisher.publish_latest(published).is_err() {
                    return RefreshCycleOutcome::Stop;
                }
                state.latest = Some(points);
            }
            if publish_status(
                publisher,
                StatisticsStatus::Ready {
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
            publish_failure(publisher, StatisticsFetchError::MarketParticipant(error)).await;
            RefreshCycleOutcome::Waiting
        }
        Err(_) => {
            publish_failure(publisher, StatisticsFetchError::Timeout(attempt_timeout)).await;
            RefreshCycleOutcome::Waiting
        }
    }
}

async fn publish_failure(
    publisher: &StreamPublisher<StatisticsProtocol>,
    error: StatisticsFetchError,
) {
    let _ = publisher
        .publish_reliable(StatisticsStreamMessage::Status(StatisticsStatus::Failed {
            error,
        }))
        .await;
}

async fn publish_status(
    publisher: &StreamPublisher<StatisticsProtocol>,
    status: StatisticsStatus,
) -> Result<(), StreamPublishError> {
    publisher
        .publish_reliable(StatisticsStreamMessage::Status(status))
        .await
}

pub(crate) async fn fetch_snapshot<S>(
    source: &S,
    key: &StatisticsStreamKey,
    metadata: &mut Option<ListingPrecision>,
    previous: Option<&StatisticsPoints>,
) -> Result<(StatisticsPoints, bool), MarketError>
where
    S: StatisticsSource,
{
    if key.metric.needs_instrument_precision() && metadata.is_none() {
        *metadata = source.fetch_listing_metadata(&key.listing).await.ok();
    }
    let query = key.query();
    let fetched = match key.metric {
        MetricKind::OpenInterest => source
            .fetch_open_interest(query)
            .await
            .map(|items| normalize_open_interest(items, previous)),
        MetricKind::TopTraderAccountRatio => source
            .fetch_top_trader_account_ratio(query)
            .await
            .map(|items| normalize_long_short_ratio(items, previous)),
        MetricKind::TopTraderPositionRatio => source
            .fetch_top_trader_position_ratio(query)
            .await
            .map(|items| normalize_long_short_ratio(items, previous)),
        MetricKind::LongShortAccountRatio => source
            .fetch_long_short_account_ratio(query)
            .await
            .map(|items| normalize_long_short_ratio(items, previous)),
        MetricKind::TakerVolume => source
            .fetch_taker_volume(query)
            .await
            .map(|items| normalize_taker_volume(items, previous)),
    }?;
    fetched.map_err(MarketError::from)
}

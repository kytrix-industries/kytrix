//! The background service one statistics chart talks to.
//!
//! These series are a bounded history, not a subscription: the venue answers with
//! the most recent intervals and nothing else, so "live" here means *poll*. One
//! stream task per scope owns the poll loop and does three things, in this order,
//! every time it runs:
//!
//! 1. report that it is fetching, and when the next fetch is due, so the pane can
//!    count down to something it can see;
//! 2. normalize what the venue answered into a window of [`POINT_LIMIT`]
//!    intervals and publish it as the latest value — a window that did not change
//!    keeps its allocation, and the widget above compares by pointer to decide
//!    whether there is anything to redraw;
//! 3. report readiness *after* the window it describes, never before, because the
//!    two channels have no order between them and a status that ran ahead of its
//!    own snapshot would put a chart on screen that is not there yet.
//!
//! A failed fetch is terminal for the refresh loop but not for the scope: the
//! stream stays alive and waits for a manual refresh, so a venue that is briefly
//! unreachable does not turn into a request every minute until someone notices.
//! Only an explicit load or refresh starts the loop again.

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

/// How often a scope is re-fetched on its own.
///
/// The shortest interval these endpoints publish is five minutes, so a minute is
/// already faster than the data can move: it exists to pick up a new interval
/// shortly after the venue has one, not to poll for changes within one.
pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// How long one fetch may take before it is treated as a failure.
pub(crate) const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);

/// The refresh command queue never needs to hold more than the one command a
/// manual refresh sends: a second one while the first is pending is answered as
/// busy rather than queued.
pub(crate) const STREAM_COMMAND_CAPACITY: usize = 1;

/// What the widget asks the service to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatisticsRequest {
    /// Starts polling, or replaces the stream that was polling something else.
    ///
    /// One request carries both what to fetch and which series it is, so there is
    /// no window in which a service has a scope but not a metric.
    Load {
        metric: MetricKind,
        listing: VenueListingKey,
        timeframe: StatisticsTimeFrame,
    },
    /// The same, for a scope the user recovered from a failure.
    Retry {
        metric: MetricKind,
        listing: VenueListingKey,
        timeframe: StatisticsTimeFrame,
    },
    /// Fetches the current scope once, without waiting for the next interval.
    Refresh,
}

impl StatisticsRequest {
    /// A load or a retry of one scope.
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

    /// The parts of a load or a retry.
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

/// What the service answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatisticsResponse {
    StreamStarted,
    RefreshQueued,
}

/// The scope of one chart. Every field invalidates the data on its own: another
/// series, another listing or another interval is another window.
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

    /// The fetch this scope asks for, at the window every chart plots.
    pub(crate) fn query(&self) -> StatisticsQuery {
        StatisticsQuery {
            listing: self.listing.clone(),
            timeframe: self.timeframe,
            limit: u16::from(POINT_LIMIT),
        }
    }
}

/// One statistics fetch: which listing, over which window, and how many
/// intervals.
///
/// It is this crate's own request shape rather than the connector's: the connector's
/// query carries native identifiers a chart has no business naming, and every
/// field here is one the pane actually chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatisticsQuery {
    pub listing: VenueListingKey,
    pub timeframe: StatisticsTimeFrame,
    pub limit: u16,
}

/// Where the poll loop is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatisticsStatus {
    /// The stream is starting.
    Connecting,
    /// A fetch is in flight; `next_refresh_at` is the Unix millisecond the
    /// following automatic one is due.
    Refreshing { next_refresh_at: i64 },
    /// The window of `revision` has been published and the loop is idle until
    /// `next_refresh_at`.
    Ready { revision: u64, next_refresh_at: i64 },
    /// The last fetch produced no window.
    Failed { error: StatisticsFetchError },
}

impl StatisticsStatus {
    /// When the next automatic fetch is due, if one is scheduled.
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

/// Why a fetch produced no window.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum StatisticsFetchError {
    #[error("the statistics request failed: {0}")]
    MarketParticipant(#[source] MarketError),
    #[error("the statistics request did not answer within {0:?}")]
    Timeout(Duration),
}

/// Everything the service publishes: state on the reliable channel, windows on
/// the latest-value one.
#[derive(Clone, Debug)]
pub enum StatisticsStreamMessage {
    Status(StatisticsStatus),
    Snapshot(Arc<StatisticsSnapshot>),
}

/// Why a request could not be accepted.
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

/// The seam between this widget and the venue, so a test can answer instead.
///
/// It is pub(crate) rather than pub(super) because the pane's own test seam —
/// a constructor that takes a source — is a public item on a public widget, and
/// the trait it names has to be at least as visible as the item that uses it.
///
/// Every method is listing-scoped and named after the capability it reads: the
/// listing's own rules, its units and the venue's own limits are resolved inside
/// the connector, so nothing here reads a capability to decide what it may ask
/// for. A listing whose product these endpoints do not serve is refused there, as
/// the typed model error it is.
pub trait StatisticsSource: Clone + Send + Sync + 'static {
    /// The increments the listing's labels are written at.
    ///
    /// Resolving the listing is what loads its route's catalog, and the catalog
    /// has to be loaded before a listing can be resolved against it. So a lookup
    /// that fails is reported as the failure it is — a catalog or transport error
    /// — rather than as a listing that does not exist.
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
    key: StatisticsStreamKey,
    generation: StreamGeneration,
    commands: mpsc::Sender<StreamCommand>,
    gate: RefreshGate,
}

/// What the poll loop carries between cycles.
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

    /// The same service with timings a test can wait out.
    ///
    /// It is crate-visible rather than private to this module because the tests
    /// that use it live in `tests.rs` beside it, which is a module of its own.
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
                    // The answer is worth less than the request it belongs to:
                    // a caller that has already given up is not waiting for it.
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    let (commands, stream_commands) = mpsc::channel(STREAM_COMMAND_CAPACITY);
                    let source = self.source.clone();
                    let refresh_interval = self.refresh_interval;
                    let attempt_timeout = self.attempt_timeout;
                    let stream_key = key.clone();
                    // Replacing is what makes a recovery start over: after a
                    // failure the loop has already ended, and a stream that ran
                    // out its task would otherwise answer `StreamStarted` for a
                    // loop that no longer exists.
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
                        // Nobody is listening to this stream any more, and the
                        // command channel is gone with the answer.
                        context.cancel_stream(&key);
                        active = None;
                    }
                }
                StatisticsRequest::Refresh => {
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
                        let _ = reply.respond(Err(StatisticsServiceError::StreamClosed));
                        continue;
                    };
                    // The gate spans the whole round trip: a second click while
                    // the first fetch is in flight is refused here rather than
                    // queued behind it.
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

/// Runs the poll loop of one scope until the publisher is cancelled.
///
/// The loop has two states, and a failure is what moves it between them. It is
/// either **scheduled** — sleeping until the next automatic fetch — or **waiting**,
/// which is where a failed fetch leaves it: the task stays alive so the scope does
/// not have to be replaced to be recovered, but nothing is on the clock. Only a
/// manual refresh moves it back to scheduled, and a fetch that fails while waiting
/// leaves it waiting.
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

    // The first fetch happens as soon as the stream starts rather than after a
    // whole interval: a pane that has just been opened has nothing to show, and a
    // countdown to its first data would be a countdown to nothing.
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
            // Nothing is on the clock, so the only thing left that can happen is
            // a refresh or the end of the scope.
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

/// What the loop does after one cycle.
///
/// The three answers are the three things a cycle can mean, and a failure is the
/// middle one: the loop keeps running so the scope does not have to be replaced to
/// be recovered, but nothing is on the clock. Collapsing a failure into "the cycle
/// reported its result" would put a refused request back on the schedule and turn a
/// venue that is briefly unreachable into a request every minute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefreshCycleOutcome {
    /// The cycle published its result; the next automatic fetch is due.
    Scheduled,
    /// The cycle reported a failure: nothing is on the clock until a refresh.
    Waiting,
    /// The publisher is gone, so there is nobody left to report to.
    Stop,
}

/// Fetches one window and reports it.
///
/// A failure is published and then the cycle answers [`RefreshCycleOutcome::Waiting`]:
/// the loop is what decides whether to schedule another attempt, and after a
/// failure it does not — a venue that just refused is not worth asking again
/// every minute.
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

/// Publishes a failed cycle, ignoring the outcome: the caller only needs to know
/// the loop is still alive, and a closed publisher is noticed by the next write.
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

/// Fetches and normalizes one window.
///
/// The listing precision is fetched on the first cycle of a chart that needs it
/// and kept; a failure to fetch it is not a failure of the chart, because the
/// window is worth drawing without it — the labels fall back to a compact form.
///
/// What the venue answered is validated as it is normalized: a reading expressed
/// in a unit the chart cannot label is reported as the typed model failure it is
/// rather than drawn as if it were one.
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

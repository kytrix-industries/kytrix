//! The background service one prediction market widget talks to.
//!
//! The chart reads a bounded history once and then follows the venue's live
//! probability stream. The subscription is opened before the history is requested,
//! so a tick that lands while the page is in flight is buffered rather than missed.
//! The stream key is the event, the market and the range: another event, market or
//! range is another chart, and a message from the previous one is refused by the
//! view.

use std::{
    collections::VecDeque,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use futures::StreamExt;
use market_participants_connector::market::{
    domain::DisconnectReason, error::MarketError, provider::MarketCapability,
    streams::PredictionSeriesStream,
};
use market_participants_model::prediction::{
    PredictionEventRef, PredictionEventView, PredictionLeg, PredictionMarket, PredictionMarketRef,
    PredictionRange, PredictionSeries,
};
use thiserror::Error;
use tokio::time::{Instant, timeout};
use widget::{
    protocol::WidgetProtocol,
    runtime::{ServiceContext, WidgetService},
    stream::{StreamPublishError, StreamPublisher},
};

use crate::model::PredictionMarketSnapshot;

/// How long one read may take before it is a failure.
pub(crate) const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

/// How often accumulated live ticks are published to the chart.
///
/// The chart is a full-window repaint, so a venue pushing several ticks a second
/// must not repaint the window at that rate.
pub(crate) const PUBLISH_INTERVAL: Duration = Duration::from_millis(500);

/// The most live updates buffered between opening the stream and the first
/// snapshot.
///
/// This counts updates, not points: one update carries one new observation per
/// leg, so it is separate from the per-line point budget.
pub(crate) const MAX_BUFFERED_UPDATES: usize = 2_000;

/// What the view asks the service to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PredictionMarketRequest {
    Load {
        event: PredictionEventRef,
        market: Option<PredictionMarketRef>,
        range: PredictionRange,
        /// A local stream epoch. Every explicit load increments it, so messages
        /// from a replaced stream can never be mistaken for the new one's.
        epoch: u64,
    },
    Retry {
        event: PredictionEventRef,
        market: Option<PredictionMarketRef>,
        range: PredictionRange,
        epoch: u64,
    },
}

impl PredictionMarketRequest {
    pub(crate) fn key(&self) -> PredictionMarketStreamKey {
        match self {
            Self::Load {
                event,
                market,
                range,
                epoch,
            }
            | Self::Retry {
                event,
                market,
                range,
                epoch,
            } => PredictionMarketStreamKey {
                event: event.clone(),
                market: market.clone(),
                range: *range,
                epoch: *epoch,
            },
        }
    }
}

/// What the service answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PredictionMarketResponse {
    StreamStarted,
}

/// The event, market and range one chart follows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PredictionMarketStreamKey {
    pub event: PredictionEventRef,
    pub market: Option<PredictionMarketRef>,
    pub range: PredictionRange,
    /// The view-local generation that produced this request.
    pub epoch: u64,
}

/// Where the stream is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PredictionMarketStatus {
    Connecting,
    Ready { revision: u64 },
    Failed { error: PredictionMarketFetchError },
}

/// Why one read produced no snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum PredictionMarketFetchError {
    #[error("the prediction request failed: {0}")]
    Market(#[source] MarketError),
    #[error("the prediction event publishes no tradable market")]
    NoMarkets,
    #[error("the prediction request did not answer within {0:?}")]
    Timeout(Duration),
}

impl From<MarketError> for PredictionMarketFetchError {
    fn from(error: MarketError) -> Self {
        Self::Market(error)
    }
}

/// Everything the service publishes.
#[derive(Clone, Debug)]
pub enum PredictionMarketStreamMessage {
    Status(PredictionMarketStatus),
    Snapshot(Arc<PredictionMarketSnapshot>),
}

/// Why a request was refused.
///
/// The service has no refusal of its own: a request always starts or replaces a
/// stream. The type exists because the protocol boundary names one.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum PredictionMarketServiceError {}

/// The seam between the chart and the connector, so a test can answer instead.
pub trait PredictionMarketSource: Clone + Send + Sync + 'static {
    type Subscription: PredictionSeriesEventSubscription;

    fn event(
        &self,
        event: PredictionEventRef,
    ) -> impl Future<Output = Result<PredictionEventView, MarketError>> + Send;

    fn series(
        &self,
        event: PredictionEventRef,
        legs: Vec<PredictionLeg>,
        range: PredictionRange,
    ) -> impl Future<Output = Result<Vec<PredictionSeries>, MarketError>> + Send;

    fn subscribe_series(
        &self,
        event: PredictionEventRef,
        legs: Vec<PredictionLeg>,
    ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send;
}

/// One live probability stream.
pub trait PredictionSeriesEventSubscription: Send + 'static {
    fn recv(
        &mut self,
    ) -> impl Future<Output = Option<Result<Vec<PredictionSeries>, MarketError>>> + Send;
}

impl PredictionMarketSource for MarketCapability {
    type Subscription = ClientPredictionSeriesSubscription;

    async fn event(&self, event: PredictionEventRef) -> Result<PredictionEventView, MarketError> {
        self.prediction_event(event).fetch().await
    }

    async fn series(
        &self,
        event: PredictionEventRef,
        legs: Vec<PredictionLeg>,
        range: PredictionRange,
    ) -> Result<Vec<PredictionSeries>, MarketError> {
        self.prediction_series(event, legs, range).fetch().await
    }

    fn subscribe_series(
        &self,
        event: PredictionEventRef,
        legs: Vec<PredictionLeg>,
    ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send {
        let market = self.clone();
        async move {
            let inner = market.subscribe_prediction_series(event, legs).await?;
            Ok(ClientPredictionSeriesSubscription { inner })
        }
    }
}

pub struct ClientPredictionSeriesSubscription {
    inner: PredictionSeriesStream,
}

impl PredictionSeriesEventSubscription for ClientPredictionSeriesSubscription {
    async fn recv(&mut self) -> Option<Result<Vec<PredictionSeries>, MarketError>> {
        self.inner.next().await
    }
}

pub(crate) struct PredictionMarketProtocol;

impl WidgetProtocol for PredictionMarketProtocol {
    type Request = PredictionMarketRequest;
    type Response = PredictionMarketResponse;
    type StreamKey = PredictionMarketStreamKey;
    type StreamMessage = PredictionMarketStreamMessage;
    type Error = PredictionMarketServiceError;
}

pub struct PredictionMarketService<S = MarketCapability> {
    source: S,
    attempt_timeout: Duration,
    /// The next snapshot revision this service hands out.
    ///
    /// Revisions are service-wide rather than stream-local: replacing a stream
    /// must never restart at a number the chart is still holding from the stream
    /// being replaced, or the first snapshot of the new stream would be refused
    /// as stale.
    revision: Arc<AtomicU64>,
}

impl<S> PredictionMarketService<S> {
    pub(crate) fn new(source: S) -> Self {
        Self {
            source,
            attempt_timeout: ATTEMPT_TIMEOUT,
            revision: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl<S> WidgetService<PredictionMarketProtocol> for PredictionMarketService<S>
where
    S: PredictionMarketSource,
{
    async fn run(self, mut context: ServiceContext<PredictionMarketProtocol>) {
        let mut active_key: Option<PredictionMarketStreamKey> = None;
        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                continue;
            }
            let key = request.key();
            // The supervisor replaces only the exact key it is given. A chart
            // follows one event at a time, so a request for another event or
            // market must retire the previous stream as well.
            if let Some(previous) = active_key
                .as_ref()
                .filter(|previous| *previous != &key)
                .cloned()
            {
                context.cancel_stream(&previous);
            }
            let source = self.source.clone();
            let attempt_timeout = self.attempt_timeout;
            let stream_key = key.clone();
            let revision = Arc::clone(&self.revision);
            context.replace_stream(key.clone(), move |publisher| {
                run_stream(source, stream_key, attempt_timeout, revision, publisher)
            });
            active_key = Some(key.clone());
            if reply
                .respond(Ok(PredictionMarketResponse::StreamStarted))
                .is_err()
            {
                context.cancel_stream(&key);
                if active_key.as_ref() == Some(&key) {
                    active_key = None;
                }
            }
        }
    }
}

async fn run_stream<S>(
    source: S,
    key: PredictionMarketStreamKey,
    attempt_timeout: Duration,
    revisions: Arc<AtomicU64>,
    publisher: StreamPublisher<PredictionMarketProtocol>,
) where
    S: PredictionMarketSource,
{
    if publish_status(&publisher, PredictionMarketStatus::Connecting)
        .await
        .is_err()
    {
        return;
    }
    let event = match timeout(attempt_timeout, source.event(key.event.clone())).await {
        Ok(Ok(event)) => event,
        Ok(Err(error)) => {
            publish_failure(&publisher, PredictionMarketFetchError::Market(error)).await;
            return;
        }
        Err(_) => {
            publish_failure(
                &publisher,
                PredictionMarketFetchError::Timeout(attempt_timeout),
            )
            .await;
            return;
        }
    };
    let Some(market) = selected_market(&event, key.market.as_ref()) else {
        publish_failure(&publisher, PredictionMarketFetchError::NoMarkets).await;
        return;
    };
    let mut subscription = match timeout(
        attempt_timeout,
        source.subscribe_series(event.event.clone(), market.legs.clone()),
    )
    .await
    {
        Ok(Ok(subscription)) => subscription,
        Ok(Err(error)) => {
            publish_failure(&publisher, PredictionMarketFetchError::Market(error)).await;
            return;
        }
        Err(_) => {
            publish_failure(
                &publisher,
                PredictionMarketFetchError::Timeout(attempt_timeout),
            )
            .await;
            return;
        }
    };

    // The live stream is open before history is requested, so a tick that lands
    // while the page is in flight is buffered rather than missed. The buffer is
    // bounded by an update budget: a tick older than that many ticks is a point
    // the chart would drop anyway.
    let mut buffered: VecDeque<Vec<PredictionSeries>> = VecDeque::new();
    let fetch = timeout(
        attempt_timeout,
        source.series(event.event.clone(), market.legs.clone(), key.range),
    );
    tokio::pin!(fetch);
    let history = loop {
        tokio::select! {
            biased;
            () = publisher.cancelled() => return,
            result = &mut fetch => break result,
            update = subscription.recv() => match update {
                Some(Ok(series)) => {
                    buffered.push_back(series);
                    if buffered.len() > MAX_BUFFERED_UPDATES {
                        buffered.pop_front();
                    }
                }
                Some(Err(error)) => {
                    publish_failure(&publisher, PredictionMarketFetchError::Market(error)).await;
                    return;
                }
                None => {
                    publish_failure(
                        &publisher,
                        PredictionMarketFetchError::Market(MarketError::Disconnected(
                            DisconnectReason::Normal,
                        )),
                    )
                    .await;
                    return;
                }
            },
        }
    };
    let history = match history {
        Ok(Ok(series)) => series,
        Ok(Err(error)) => {
            publish_failure(&publisher, PredictionMarketFetchError::Market(error)).await;
            return;
        }
        Err(_) => {
            publish_failure(
                &publisher,
                PredictionMarketFetchError::Timeout(attempt_timeout),
            )
            .await;
            return;
        }
    };

    let mut revision = next_revision(&revisions);
    let mut snapshot = PredictionMarketSnapshot::new(revision, key.range, event, market, history);
    for update in buffered {
        revision = next_revision(&revisions);
        snapshot = snapshot.with_live(update, revision);
    }
    if publisher
        .publish_latest(PredictionMarketStreamMessage::Snapshot(Arc::new(
            snapshot.clone(),
        )))
        .is_err()
    {
        return;
    }
    if publish_status(&publisher, PredictionMarketStatus::Ready { revision })
        .await
        .is_err()
    {
        return;
    }
    // Every tick is folded into the snapshot as it arrives, but the snapshot is
    // published at a bounded cadence: a venue that pushes faster than a screen can
    // redraw would otherwise repaint the whole window on every tick. The timer
    // publishes what has accumulated; a cancelled generation publishes nothing,
    // because the replacement stream starts from its own history.
    let mut flush = tokio::time::interval(PUBLISH_INTERVAL);
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    flush.tick().await;
    let mut pending = false;
    loop {
        tokio::select! {
            biased;
            () = publisher.clone().cancelled_owned() => return,
            _ = flush.tick() => {
                if !pending {
                    continue;
                }
                pending = false;
                if publisher
                    .publish_latest(PredictionMarketStreamMessage::Snapshot(Arc::new(
                        snapshot.clone(),
                    )))
                    .is_err()
                {
                    return;
                }
            }
            update = subscription.recv() => match update {
                Some(Ok(series)) => {
                    revision = next_revision(&revisions);
                    snapshot = snapshot.with_live(series, revision);
                    pending = true;
                }
                Some(Err(error)) => {
                    publish_failure(&publisher, PredictionMarketFetchError::Market(error)).await;
                    return;
                }
                None => {
                    publish_failure(
                        &publisher,
                        PredictionMarketFetchError::Market(MarketError::Disconnected(
                            DisconnectReason::Normal,
                        )),
                    )
                    .await;
                    return;
                }
            },
        }
    }
}

/// The market the caller asked for, or an open headline market, or the first
/// open market the venue published.
fn selected_market(
    event: &PredictionEventView,
    requested: Option<&PredictionMarketRef>,
) -> Option<PredictionMarket> {
    if let Some(requested) = requested
        && let Some(market) = event
            .markets
            .iter()
            .find(|market| &market.market == requested)
    {
        return Some(market.clone());
    }
    event
        .markets
        .iter()
        .filter(|market| market.open)
        .find(|market| market.headline)
        // A grouped event publishes one market per candidate, each with a Yes and
        // a No side. The venue lists the event's main market first, so a fallback
        // that ranked sides could only pick an arbitrary candidate: every
        // candidate's No side is priced near one. The order the venue published
        // is the one a reader sees on the event page.
        .or_else(|| event.markets.iter().find(|market| market.open))
        .or_else(|| event.markets.iter().find(|market| market.headline))
        .or_else(|| event.markets.first())
        .cloned()
}

fn next_revision(revisions: &AtomicU64) -> u64 {
    revisions.fetch_add(1, Ordering::Relaxed).saturating_add(1)
}

async fn publish_status(
    publisher: &StreamPublisher<PredictionMarketProtocol>,
    status: PredictionMarketStatus,
) -> Result<(), StreamPublishError> {
    publisher
        .publish_reliable(PredictionMarketStreamMessage::Status(status))
        .await
}

async fn publish_failure(
    publisher: &StreamPublisher<PredictionMarketProtocol>,
    error: PredictionMarketFetchError,
) {
    let _ = publisher
        .publish_reliable(PredictionMarketStreamMessage::Status(
            PredictionMarketStatus::Failed { error },
        ))
        .await;
}

#[cfg(test)]
mod service_tests;

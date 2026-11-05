//! The background service one prediction market widget talks to.
//!
//! The chart reads a bounded history once and then follows the venue's live
//! probability stream. The subscription is opened before the history is requested,
//! so a tick that lands while the page is in flight is buffered rather than missed.
//! The stream key is the event, the market, the range and the requested content
//! locale: another event, market, range or language is another chart, and a
//! message from the previous one is refused by the view.

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
use smol_str::SmolStr;
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
        locale: Option<SmolStr>,
        /// A local stream epoch. Every explicit load increments it, so messages
        /// from a replaced stream can never be mistaken for the new one's.
        epoch: u64,
    },
    Retry {
        event: PredictionEventRef,
        market: Option<PredictionMarketRef>,
        range: PredictionRange,
        locale: Option<SmolStr>,
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
                locale,
                epoch,
            }
            | Self::Retry {
                event,
                market,
                range,
                locale,
                epoch,
            } => PredictionMarketStreamKey {
                event: event.clone(),
                market: market.clone(),
                range: *range,
                locale: locale.clone(),
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

/// The event, range and locale one chart follows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PredictionMarketStreamKey {
    pub event: PredictionEventRef,
    pub market: Option<PredictionMarketRef>,
    pub range: PredictionRange,
    pub locale: Option<SmolStr>,
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
        locale: Option<SmolStr>,
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

    async fn event(
        &self,
        event: PredictionEventRef,
        locale: Option<SmolStr>,
    ) -> Result<PredictionEventView, MarketError> {
        self.prediction_event(event).locale(locale).fetch().await
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
            // follows one event at a time, so a request for another event,
            // market or locale must retire the previous stream as well.
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
    let event = match timeout(
        attempt_timeout,
        source.event(key.event.clone(), key.locale.clone()),
    )
    .await
    {
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

/// The market the caller asked for, or the event's headline market, or the
/// market with the strongest published side.
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
        .or_else(|| {
            event
                .markets
                .iter()
                .filter(|market| market.open)
                .max_by_key(|market| market.legs.iter().filter_map(|leg| leg.probability).max())
        })
        .or_else(|| event.markets.iter().find(|market| market.headline))
        .or_else(|| {
            event
                .markets
                .iter()
                .max_by_key(|market| market.legs.iter().filter_map(|leg| leg.probability).max())
        })
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
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use market_participants_connector::market::{domain::DisconnectReason, error::MarketError};
    use market_participants_model::{
        decimal::Fraction,
        prediction::{
            PredictionEventRef, PredictionEventView, PredictionLeg, PredictionMarket,
            PredictionPricePoint, PredictionRange, PredictionSeries,
        },
    };
    use smol_str::SmolStr;
    use tokio::sync::{Notify, mpsc, oneshot};
    use widget::{runtime::WidgetRuntime, stream::ReliableStream};

    use super::{
        MAX_BUFFERED_UPDATES, PredictionMarketFetchError, PredictionMarketProtocol,
        PredictionMarketRequest, PredictionMarketService, PredictionMarketSource,
        PredictionMarketStatus, PredictionMarketStreamMessage, PredictionSeriesEventSubscription,
        selected_market,
    };
    use crate::{
        model::{PredictionMarketSeries, PredictionMarketSnapshot},
        test_support::{charted_market, event_ref, event_with, market},
    };

    /// The handle of the runtime this test runs on.
    #[allow(
        clippy::disallowed_methods,
        reason = "a test drives its subject on its own runtime so the paused clock it advances is the one the subject reads"
    )]
    fn current_test_handle() -> &'static tokio::runtime::Handle {
        Box::leak(Box::new(tokio::runtime::Handle::current()))
    }

    /// One headline market of the fixture event.
    fn headline_market(event: &PredictionEventRef, id: &str, open: bool) -> PredictionMarket {
        let mut market = market(event, id, "markets", open);
        market.headline = true;
        market
    }

    #[test]
    fn a_requested_market_wins_over_the_default() {
        let event = event_ref();
        let view = event_with(vec![
            headline_market(&event, "headline", true),
            market(&event, "chosen", "markets", true),
        ]);
        let chosen = view.markets[1].market.clone();

        let selected = selected_market(&view, Some(&chosen)).expect("a market is selected");

        assert_eq!(selected.market, chosen);
    }

    #[test]
    fn an_open_headline_is_the_default() {
        let event = event_ref();
        let view = event_with(vec![
            market(&event, "first", "markets", true),
            headline_market(&event, "headline", true),
        ]);

        let selected = selected_market(&view, None).expect("a market is selected");

        assert_eq!(selected.market.native_market.as_str(), "headline");
    }

    #[test]
    fn an_open_market_wins_over_a_closed_headline() {
        let event = event_ref();
        let view = event_with(vec![
            headline_market(&event, "closed-headline", false),
            market(&event, "open-market", "markets", true),
        ]);

        let selected = selected_market(&view, None).expect("a market is selected");

        assert_eq!(selected.market.native_market.as_str(), "open-market");
    }

    #[test]
    fn a_finished_event_falls_back_to_its_headline() {
        let event = event_ref();
        let view = event_with(vec![
            market(&event, "closed-first", "markets", false),
            headline_market(&event, "closed-headline", false),
        ]);

        let selected = selected_market(&view, None).expect("a market is selected");

        assert_eq!(selected.market.native_market.as_str(), "closed-headline");
    }

    /// One source whose event, history and live stream are supplied by the test.
    #[derive(Clone)]
    struct FakeSource {
        event: Arc<Mutex<Option<oneshot::Receiver<PredictionEventView>>>>,
        history: Arc<Mutex<Option<oneshot::Receiver<Vec<PredictionSeries>>>>>,
        subscription: Arc<Mutex<Option<FakeSubscription>>>,
        history_requested: Arc<Notify>,
    }

    struct FakeSubscription {
        events: mpsc::Receiver<Result<Vec<PredictionSeries>, MarketError>>,
        event_received: Arc<Notify>,
    }

    impl PredictionMarketSource for FakeSource {
        type Subscription = FakeSubscription;

        async fn event(
            &self,
            _event: PredictionEventRef,
            _locale: Option<SmolStr>,
        ) -> Result<PredictionEventView, MarketError> {
            let receiver = self
                .event
                .lock()
                .expect("the fixture lock is not poisoned")
                .take()
                .ok_or(MarketError::ServiceClosed)?;
            receiver.await.map_err(|_| MarketError::ServiceClosed)
        }

        async fn series(
            &self,
            _event: PredictionEventRef,
            _legs: Vec<PredictionLeg>,
            _range: PredictionRange,
        ) -> Result<Vec<PredictionSeries>, MarketError> {
            let receiver = self
                .history
                .lock()
                .expect("the fixture lock is not poisoned")
                .take()
                .ok_or(MarketError::ServiceClosed)?;
            self.history_requested.notify_waiters();
            receiver.await.map_err(|_| MarketError::ServiceClosed)
        }

        async fn subscribe_series(
            &self,
            _event: PredictionEventRef,
            _legs: Vec<PredictionLeg>,
        ) -> Result<Self::Subscription, MarketError> {
            self.subscription
                .lock()
                .expect("the fixture lock is not poisoned")
                .take()
                .ok_or(MarketError::ServiceClosed)
        }
    }

    impl PredictionSeriesEventSubscription for FakeSubscription {
        async fn recv(&mut self) -> Option<Result<Vec<PredictionSeries>, MarketError>> {
            let event = self.events.recv().await;
            if event.is_some() {
                self.event_received.notify_waiters();
            }
            event
        }
    }

    /// One source, one subscription and the handles a test drives them with.
    struct Harness {
        runtime: WidgetRuntime<PredictionMarketProtocol>,
        event: oneshot::Sender<PredictionEventView>,
        history: oneshot::Sender<Vec<PredictionSeries>>,
        events: mpsc::Sender<Result<Vec<PredictionSeries>, MarketError>>,
        history_requested: Arc<Notify>,
        event_received: Arc<Notify>,
    }

    fn harness() -> Harness {
        let (event, event_receiver) = oneshot::channel();
        let (history, history_receiver) = oneshot::channel();
        let (events, event_stream) = mpsc::channel(4);
        let history_requested = Arc::new(Notify::new());
        let event_received = Arc::new(Notify::new());
        let source = FakeSource {
            event: Arc::new(Mutex::new(Some(event_receiver))),
            history: Arc::new(Mutex::new(Some(history_receiver))),
            subscription: Arc::new(Mutex::new(Some(FakeSubscription {
                events: event_stream,
                event_received: event_received.clone(),
            }))),
            history_requested: history_requested.clone(),
        };
        let runtime =
            WidgetRuntime::spawn(current_test_handle(), PredictionMarketService::new(source));
        Harness {
            runtime,
            event,
            history,
            events,
            history_requested,
            event_received,
        }
    }

    fn load() -> PredictionMarketRequest {
        PredictionMarketRequest::Load {
            event: event_ref(),
            market: None,
            range: PredictionRange::default(),
            locale: None,
            epoch: 1,
        }
    }

    fn point(timestamp: i64, probability: u32) -> PredictionPricePoint {
        PredictionPricePoint {
            timestamp,
            probability: Fraction::new(market_participants_model::decimal::Ratio::from_inner(
                u128::from(probability) * 1_000_000_000_000_000,
            ))
            .expect("the fixture probability is a fraction"),
        }
    }

    async fn receive_status(
        reliable: &mut ReliableStream<PredictionMarketProtocol>,
        expected: PredictionMarketStatus,
    ) {
        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(1), reliable.recv())
                .await
                .expect("a status should arrive")
                .expect("the reliable stream stays open");
            let PredictionMarketStreamMessage::Status(status) = envelope.message().as_ref() else {
                continue;
            };
            if *status == expected {
                return;
            }
        }
    }

    fn snapshot_of(message: &PredictionMarketStreamMessage) -> Option<&PredictionMarketSnapshot> {
        match message {
            PredictionMarketStreamMessage::Snapshot(snapshot) => Some(snapshot),
            PredictionMarketStreamMessage::Status(_) => None,
        }
    }

    #[tokio::test]
    async fn a_tick_that_lands_while_history_is_in_flight_should_be_kept() {
        let event = event_ref();
        let (market, legs) = charted_market(&event, "charted");
        let view = event_with(vec![market]);
        let harness = harness();
        let mut latest = harness.runtime.latest_stream();

        harness
            .runtime
            .client()
            .call(load())
            .await
            .expect("the load request starts a stream");
        harness.event.send(view).expect("the event read is waiting");
        harness.history_requested.notified().await;
        harness
            .events
            .send(Ok(vec![PredictionSeries {
                leg: legs[0].clone(),
                points: vec![point(2_000, 60)],
            }]))
            .await
            .expect("the fake stream is open");
        harness.event_received.notified().await;
        harness
            .history
            .send(vec![PredictionSeries {
                leg: legs[0].clone(),
                points: vec![point(1_000, 40)],
            }])
            .expect("the history read is waiting");

        latest
            .changed()
            .await
            .expect("a snapshot should be published");
        let snapshot = latest
            .next_unseen()
            .and_then(|envelope| snapshot_of(envelope.message().as_ref()).cloned())
            .expect("the published message is a snapshot");
        let line: &PredictionMarketSeries = snapshot
            .series
            .iter()
            .find(|line| line.leg == legs[0])
            .expect("the ticked line is drawn");

        assert_eq!(
            line.points
                .iter()
                .map(|point| (point.timestamp, point.probability))
                .collect::<Vec<_>>(),
            vec![
                (1_000, point(1_000, 40).probability),
                (2_000, point(2_000, 60).probability),
            ],
            "a tick that arrived before the history page is appended after it"
        );
        harness.runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_failed_live_stream_should_be_reported_as_a_failure() {
        let event = event_ref();
        let (market, _) = charted_market(&event, "charted");
        let view = event_with(vec![market.clone()]);
        let mut harness = harness();
        let mut reliable = harness
            .runtime
            .take_reliable_stream()
            .expect("a reliable stream");

        harness
            .runtime
            .client()
            .call(load())
            .await
            .expect("the load request starts a stream");
        harness.event.send(view).expect("the event read is waiting");
        harness.history_requested.notified().await;
        harness
            .history
            .send(Vec::new())
            .expect("the history read is waiting");
        receive_status(&mut reliable, PredictionMarketStatus::Ready { revision: 1 }).await;
        harness
            .events
            .send(Err(MarketError::Disconnected(DisconnectReason::Normal)))
            .await
            .expect("the fake stream is open");
        harness.event_received.notified().await;

        receive_status(
            &mut reliable,
            PredictionMarketStatus::Failed {
                error: PredictionMarketFetchError::Market(MarketError::Disconnected(
                    DisconnectReason::Normal,
                )),
            },
        )
        .await;
        harness.runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_closed_live_stream_should_end_the_chart_as_a_failure() {
        let event = event_ref();
        let (market, _) = charted_market(&event, "charted");
        let view = event_with(vec![market.clone()]);
        let mut harness = harness();
        let mut reliable = harness
            .runtime
            .take_reliable_stream()
            .expect("a reliable stream");

        harness
            .runtime
            .client()
            .call(load())
            .await
            .expect("the load request starts a stream");
        harness.event.send(view).expect("the event read is waiting");
        harness.history_requested.notified().await;
        harness
            .history
            .send(Vec::new())
            .expect("the history read is waiting");
        receive_status(&mut reliable, PredictionMarketStatus::Ready { revision: 1 }).await;
        drop(harness.events);

        receive_status(
            &mut reliable,
            PredictionMarketStatus::Failed {
                error: PredictionMarketFetchError::Market(MarketError::Disconnected(
                    DisconnectReason::Normal,
                )),
            },
        )
        .await;
        harness.runtime.shutdown().await;
    }

    /// The stream key of one test load.
    #[test]
    fn the_stream_key_names_every_dimension_that_invalidates_a_chart() {
        let key = load().key();

        assert_eq!(key.event, event_ref());
        assert_eq!(key.market, None);
        assert_eq!(key.range, PredictionRange::default());
        assert_eq!(key.locale, None);
        assert_eq!(key.epoch, 1);
    }

    #[test]
    fn the_live_buffer_is_bounded_separately_from_the_line_point_cap() {
        assert_eq!(MAX_BUFFERED_UPDATES, 2_000);
        assert_ne!(MAX_BUFFERED_UPDATES, crate::model::MAX_POINTS);
    }
}

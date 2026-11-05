//! The background service one prediction chart talks to.
//!
//! The chart reads a bounded history once and then follows the venue's live
//! probability stream. The subscription is opened before the history is requested,
//! so a tick that lands while the page is in flight is buffered rather than missed.
//! The stream key is the event, the market, the window and the requested content
//! locale: another event, market, window or language is another chart, and a
//! message from the previous one is refused by the view.

use std::{collections::VecDeque, future::Future, sync::Arc, time::Duration};

use futures::StreamExt;
use market_participants_connector::market::{
    domain::DisconnectReason, error::MarketError, provider::MarketCapability,
    streams::PredictionSeriesStream,
};
use market_participants_model::prediction::{
    PredictionEventRef, PredictionEventView, PredictionLeg, PredictionMarket, PredictionMarketRef,
    PredictionSeries, PredictionWindow,
};
use smol_str::SmolStr;
use thiserror::Error;
use tokio::time::{Instant, timeout};
use widget::{
    protocol::WidgetProtocol,
    runtime::{ServiceContext, WidgetService},
    stream::{StreamPublishError, StreamPublisher},
};

use crate::model::{MAX_POINTS, PredictionChartSnapshot};

/// How long one read may take before it is a failure.
pub(crate) const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

/// How often accumulated live ticks are published to the chart.
///
/// The chart is a full-window repaint, so a venue pushing several ticks a second
/// must not repaint the window at that rate.
pub(crate) const PUBLISH_INTERVAL: Duration = Duration::from_millis(500);

/// What the view asks the service to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PredictionChartRequest {
    Load {
        event: PredictionEventRef,
        market: Option<PredictionMarketRef>,
        window: PredictionWindow,
        locale: Option<SmolStr>,
    },
    Retry {
        event: PredictionEventRef,
        market: Option<PredictionMarketRef>,
        window: PredictionWindow,
        locale: Option<SmolStr>,
    },
}

impl PredictionChartRequest {
    pub(crate) fn key(&self) -> PredictionChartStreamKey {
        match self {
            Self::Load {
                event,
                market,
                window,
                locale,
            }
            | Self::Retry {
                event,
                market,
                window,
                locale,
            } => PredictionChartStreamKey {
                event: event.clone(),
                market: market.clone(),
                window: *window,
                locale: locale.clone(),
            },
        }
    }
}

/// What the service answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PredictionChartResponse {
    StreamStarted,
}

/// The event, window and locale one chart follows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PredictionChartStreamKey {
    pub event: PredictionEventRef,
    pub market: Option<PredictionMarketRef>,
    pub window: PredictionWindow,
    pub locale: Option<SmolStr>,
}

/// Where the stream is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PredictionChartStatus {
    Connecting,
    Ready { revision: u64 },
    Failed { error: PredictionChartFetchError },
}

/// Why one read produced no snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum PredictionChartFetchError {
    #[error("the prediction request failed: {0}")]
    Market(#[source] MarketError),
    #[error("the prediction event publishes no tradable market")]
    NoMarkets,
    #[error("the prediction request did not answer within {0:?}")]
    Timeout(Duration),
}

impl From<MarketError> for PredictionChartFetchError {
    fn from(error: MarketError) -> Self {
        Self::Market(error)
    }
}

/// Everything the service publishes.
#[derive(Clone, Debug)]
pub enum PredictionChartStreamMessage {
    Status(PredictionChartStatus),
    Snapshot(Arc<PredictionChartSnapshot>),
}

/// Why a request was refused.
///
/// The service has no refusal of its own: a request always starts or replaces a
/// stream. The type exists because the protocol boundary names one.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum PredictionChartServiceError {}

/// The seam between the chart and the connector, so a test can answer instead.
pub trait PredictionChartSource: Clone + Send + Sync + 'static {
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
        window: PredictionWindow,
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

impl PredictionChartSource for MarketCapability {
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
        window: PredictionWindow,
    ) -> Result<Vec<PredictionSeries>, MarketError> {
        self.prediction_series(event, legs, window).fetch().await
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

pub(crate) struct PredictionChartProtocol;

impl WidgetProtocol for PredictionChartProtocol {
    type Request = PredictionChartRequest;
    type Response = PredictionChartResponse;
    type StreamKey = PredictionChartStreamKey;
    type StreamMessage = PredictionChartStreamMessage;
    type Error = PredictionChartServiceError;
}

pub struct PredictionChartService<S = MarketCapability> {
    source: S,
    attempt_timeout: Duration,
}

impl<S> PredictionChartService<S> {
    pub(crate) const fn new(source: S) -> Self {
        Self {
            source,
            attempt_timeout: ATTEMPT_TIMEOUT,
        }
    }
}

impl<S> WidgetService<PredictionChartProtocol> for PredictionChartService<S>
where
    S: PredictionChartSource,
{
    async fn run(self, mut context: ServiceContext<PredictionChartProtocol>) {
        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                continue;
            }
            let key = request.key();
            let source = self.source.clone();
            let attempt_timeout = self.attempt_timeout;
            let stream_key = key.clone();
            let generation = context.replace_stream(key.clone(), move |publisher| {
                run_stream(source, stream_key, attempt_timeout, publisher)
            });
            if reply
                .respond(Ok(PredictionChartResponse::StreamStarted))
                .is_err()
            {
                context.cancel_stream(&key);
                let _ = generation;
            }
        }
    }
}

async fn run_stream<S>(
    source: S,
    key: PredictionChartStreamKey,
    attempt_timeout: Duration,
    publisher: StreamPublisher<PredictionChartProtocol>,
) where
    S: PredictionChartSource,
{
    if publish_status(&publisher, PredictionChartStatus::Connecting)
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
            publish_failure(&publisher, PredictionChartFetchError::Market(error)).await;
            return;
        }
        Err(_) => {
            publish_failure(
                &publisher,
                PredictionChartFetchError::Timeout(attempt_timeout),
            )
            .await;
            return;
        }
    };
    let Some(market) = selected_market(&event, key.market.as_ref()) else {
        publish_failure(&publisher, PredictionChartFetchError::NoMarkets).await;
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
            publish_failure(&publisher, PredictionChartFetchError::Market(error)).await;
            return;
        }
        Err(_) => {
            publish_failure(
                &publisher,
                PredictionChartFetchError::Timeout(attempt_timeout),
            )
            .await;
            return;
        }
    };

    // The live stream is open before history is requested, so a tick that lands
    // while the page is in flight is buffered rather than missed. The buffer is
    // bounded by the point budget a line keeps: a tick older than that many ticks
    // is a point the chart would drop anyway.
    let mut buffered: VecDeque<Vec<PredictionSeries>> = VecDeque::new();
    let fetch = timeout(
        attempt_timeout,
        source.series(event.event.clone(), market.legs.clone(), key.window),
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
                    if buffered.len() > MAX_POINTS {
                        buffered.pop_front();
                    }
                }
                Some(Err(error)) => {
                    publish_failure(&publisher, PredictionChartFetchError::Market(error)).await;
                    return;
                }
                None => {
                    publish_failure(
                        &publisher,
                        PredictionChartFetchError::Market(MarketError::Disconnected(
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
            publish_failure(&publisher, PredictionChartFetchError::Market(error)).await;
            return;
        }
        Err(_) => {
            publish_failure(
                &publisher,
                PredictionChartFetchError::Timeout(attempt_timeout),
            )
            .await;
            return;
        }
    };

    let mut revision = 1_u64;
    let mut snapshot = PredictionChartSnapshot::new(1, event, market, history);
    for update in buffered {
        revision = revision.saturating_add(1);
        snapshot = snapshot.with_live(update, revision);
    }
    if publisher
        .publish_latest(PredictionChartStreamMessage::Snapshot(Arc::new(
            snapshot.clone(),
        )))
        .is_err()
    {
        return;
    }
    if publish_status(&publisher, PredictionChartStatus::Ready { revision })
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
                    .publish_latest(PredictionChartStreamMessage::Snapshot(Arc::new(
                        snapshot.clone(),
                    )))
                    .is_err()
                {
                    return;
                }
            }
            update = subscription.recv() => match update {
                Some(Ok(series)) => {
                    revision = revision.saturating_add(1);
                    snapshot = snapshot.with_live(series, revision);
                    pending = true;
                }
                Some(Err(error)) => {
                    publish_failure(&publisher, PredictionChartFetchError::Market(error)).await;
                    return;
                }
                None => {
                    publish_failure(
                        &publisher,
                        PredictionChartFetchError::Market(MarketError::Disconnected(
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

async fn publish_status(
    publisher: &StreamPublisher<PredictionChartProtocol>,
    status: PredictionChartStatus,
) -> Result<(), StreamPublishError> {
    publisher
        .publish_reliable(PredictionChartStreamMessage::Status(status))
        .await
}

async fn publish_failure(
    publisher: &StreamPublisher<PredictionChartProtocol>,
    error: PredictionChartFetchError,
) {
    let _ = publisher
        .publish_reliable(PredictionChartStreamMessage::Status(
            PredictionChartStatus::Failed { error },
        ))
        .await;
}

#[cfg(test)]
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
            PredictionPricePoint, PredictionSeries, PredictionWindow,
        },
    };
    use smol_str::SmolStr;
    use tokio::sync::{Notify, mpsc, oneshot};
    use widget::{runtime::WidgetRuntime, stream::ReliableStream};

    use super::{
        PredictionChartFetchError, PredictionChartProtocol, PredictionChartRequest,
        PredictionChartService, PredictionChartSource, PredictionChartStatus,
        PredictionChartStreamMessage, PredictionSeriesEventSubscription, selected_market,
    };
    use crate::{
        model::{PredictionChartSeries, PredictionChartSnapshot},
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

    impl PredictionChartSource for FakeSource {
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
            _window: PredictionWindow,
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
        runtime: WidgetRuntime<PredictionChartProtocol>,
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
            WidgetRuntime::spawn(current_test_handle(), PredictionChartService::new(source));
        Harness {
            runtime,
            event,
            history,
            events,
            history_requested,
            event_received,
        }
    }

    fn load() -> PredictionChartRequest {
        PredictionChartRequest::Load {
            event: event_ref(),
            market: None,
            window: PredictionWindow::default(),
            locale: None,
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
        reliable: &mut ReliableStream<PredictionChartProtocol>,
        expected: PredictionChartStatus,
    ) {
        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(1), reliable.recv())
                .await
                .expect("a status should arrive")
                .expect("the reliable stream stays open");
            let PredictionChartStreamMessage::Status(status) = envelope.message().as_ref() else {
                continue;
            };
            if *status == expected {
                return;
            }
        }
    }

    fn snapshot_of(message: &PredictionChartStreamMessage) -> Option<&PredictionChartSnapshot> {
        match message {
            PredictionChartStreamMessage::Snapshot(snapshot) => Some(snapshot),
            PredictionChartStreamMessage::Status(_) => None,
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
        let line: &PredictionChartSeries = snapshot
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
        receive_status(&mut reliable, PredictionChartStatus::Ready { revision: 1 }).await;
        harness
            .events
            .send(Err(MarketError::Disconnected(DisconnectReason::Normal)))
            .await
            .expect("the fake stream is open");
        harness.event_received.notified().await;

        receive_status(
            &mut reliable,
            PredictionChartStatus::Failed {
                error: PredictionChartFetchError::Market(MarketError::Disconnected(
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
        receive_status(&mut reliable, PredictionChartStatus::Ready { revision: 1 }).await;
        drop(harness.events);

        receive_status(
            &mut reliable,
            PredictionChartStatus::Failed {
                error: PredictionChartFetchError::Market(MarketError::Disconnected(
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
        assert_eq!(key.window, PredictionWindow::default());
        assert_eq!(key.locale, None);
    }
}

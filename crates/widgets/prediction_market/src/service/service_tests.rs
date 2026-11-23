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

#[allow(
    clippy::disallowed_methods,
    reason = "a test drives its subject on its own runtime so the paused clock it advances is the one the subject reads"
)]
fn current_test_handle() -> &'static tokio::runtime::Handle {
    Box::leak(Box::new(tokio::runtime::Handle::current()))
}

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
fn a_grouped_event_defaults_to_the_first_open_market_not_the_least_likely_candidate() {
    let event = event_ref();
    let (mut main, _) = charted_market(&event, "main");
    main.headline = false;
    main.legs[0].probability = Some("0.99".parse().expect("a fraction"));
    main.legs[1].probability = Some("0.01".parse().expect("a fraction"));
    let (mut long_shot, _) = charted_market(&event, "long-shot");
    long_shot.headline = false;
    long_shot.legs[0].probability = Some("0.001".parse().expect("a fraction"));
    long_shot.legs[1].probability = Some("0.999".parse().expect("a fraction"));
    let view = event_with(vec![main, long_shot]);

    let selected = selected_market(&view, None).expect("a market is selected");

    assert_eq!(
        selected.market.native_market.as_str(),
        "main",
        "the venue's first market is the event's main one, not the candidate \
         whose No side is priced highest"
    );
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

    async fn event(&self, _event: PredictionEventRef) -> Result<PredictionEventView, MarketError> {
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
    let runtime = WidgetRuntime::spawn(current_test_handle(), PredictionMarketService::new(source));
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

#[test]
fn the_stream_key_names_every_dimension_that_invalidates_a_chart() {
    let key = load().key();

    assert_eq!(key.event, event_ref());
    assert_eq!(key.market, None);
    assert_eq!(key.range, PredictionRange::default());
    assert_eq!(key.epoch, 1);
}

#[test]
fn the_live_buffer_is_bounded_separately_from_the_line_point_cap() {
    assert_eq!(MAX_BUFFERED_UPDATES, 2_000);
    assert_ne!(MAX_BUFFERED_UPDATES, crate::model::MAX_POINTS);
}

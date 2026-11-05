//! Unit tests of the book service: the page, the channel and the order the two
//! are applied in.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use market_participants_connector::{
    error::MarketParticipantError,
    market::{
        domain::DisconnectReason,
        error::{MarketError, MarketNetworkError},
    },
    market_participants::model::query::PredictionOrderBookQuery,
};
use market_participants_model::{
    decimal::{Fraction, Quantity, Tick, UnixMillis},
    outcome::OutcomeLegRef,
    prediction::{
        PredictionBookLevel, PredictionEventRef, PredictionEventView, PredictionLeg,
        PredictionOrderBook, PredictionOrderBookUpdate, PredictionRange, PredictionSeries,
    },
};
use smol_str::SmolStr;
use tokio::sync::{mpsc, oneshot};
use widget::{
    runtime::WidgetRuntime,
    stream::{LatestStream, ReliableStream},
};

use super::{
    ATTEMPT_TIMEOUT, MAX_BUFFERED_UPDATES, PredictionOrderBookError, PredictionOrderBookProtocol,
    PredictionOrderBookRequest, PredictionOrderBookResponse, PredictionOrderBookService,
    PredictionOrderBookStatus, PredictionOrderBookStreamKey, PredictionOrderBookStreamMessage,
};
use crate::{
    service::{
        PredictionMarketSource, PredictionOrderBookEventSubscription,
        PredictionSeriesEventSubscription,
    },
    test_support::{change, charted_market, event_ref, level, order_book},
};

/// The handle of the runtime this test runs on.
#[allow(
    clippy::disallowed_methods,
    reason = "a test drives its subject on its own runtime so the paused clock it advances is the one the subject reads"
)]
fn current_test_handle() -> &'static tokio::runtime::Handle {
    Box::leak(Box::new(tokio::runtime::Handle::current()))
}

/// The legs every fixture request names.
fn legs() -> Vec<PredictionLeg> {
    charted_market(&event_ref(), "charted").0.legs
}

/// The leg the fixture book is for.
fn leg() -> OutcomeLegRef {
    legs()[0].leg.clone()
}

/// One book with a bid, an ask, a last trade and a stated instant.
fn book(timestamp: UnixMillis) -> PredictionOrderBook {
    let mut book = order_book(
        leg(),
        "0.001",
        vec![level("0.34", "50")],
        vec![level("0.35", "100")],
    );
    book.last_trade_price = Some(price("0.345"));
    book.timestamp = timestamp;
    book
}

fn load(epoch: u64) -> PredictionOrderBookRequest {
    PredictionOrderBookRequest::Load {
        event: event_ref(),
        legs: legs(),
        leg: leg(),
        epoch,
    }
}

fn retry(epoch: u64) -> PredictionOrderBookRequest {
    PredictionOrderBookRequest::Retry {
        event: event_ref(),
        legs: legs(),
        leg: leg(),
        epoch,
    }
}

fn stop() -> PredictionOrderBookRequest {
    PredictionOrderBookRequest::Stop
}

/// The refusal Polymarket answers a token that has no CLOB book at all with.
fn no_book_error() -> MarketError {
    MarketError::Network(MarketNetworkError::from(MarketParticipantError::Rejected {
        code: SmolStr::new_static("404"),
        message: SmolStr::new_static("No orderbook exists for the requested token id"),
        retryable: false,
        raw_response: None,
    }))
}

/// The queued answers of the fake source: one page and one channel per call.
#[derive(Clone, Default)]
struct FakeSource {
    fetches: Arc<Mutex<VecDeque<oneshot::Receiver<Result<PredictionOrderBook, MarketError>>>>>,
    subscriptions: Arc<Mutex<VecDeque<FakeSubscription>>>,
}

impl PredictionMarketSource for FakeSource {
    type Subscription = PendingSeriesSubscription;
    type OrderBookSubscription = FakeSubscription;

    async fn event(
        &self,
        _event: PredictionEventRef,
        _locale: Option<SmolStr>,
    ) -> Result<PredictionEventView, MarketError> {
        Err(MarketError::ServiceClosed)
    }

    async fn series(
        &self,
        _event: PredictionEventRef,
        _legs: Vec<PredictionLeg>,
        _range: PredictionRange,
    ) -> Result<Vec<PredictionSeries>, MarketError> {
        Err(MarketError::ServiceClosed)
    }

    async fn subscribe_series(
        &self,
        _event: PredictionEventRef,
        _legs: Vec<PredictionLeg>,
    ) -> Result<Self::Subscription, MarketError> {
        Ok(PendingSeriesSubscription)
    }

    async fn fetch_order_book(
        &self,
        _query: PredictionOrderBookQuery,
    ) -> Result<PredictionOrderBook, MarketError> {
        let receiver = self
            .fetches
            .lock()
            .expect("the fixture lock is not poisoned")
            .pop_front();
        match receiver {
            Some(receiver) => receiver.await.map_err(|_| MarketError::ServiceClosed)?,
            None => std::future::pending().await,
        }
    }

    async fn subscribe_order_book(
        &self,
        _query: PredictionOrderBookQuery,
    ) -> Result<Self::OrderBookSubscription, MarketError> {
        let subscription = self
            .subscriptions
            .lock()
            .expect("the fixture lock is not poisoned")
            .pop_front();
        match subscription {
            Some(subscription) => Ok(subscription),
            None => std::future::pending().await,
        }
    }
}

/// One live book channel the test drives.
struct FakeSubscription {
    updates: mpsc::Receiver<Result<Vec<PredictionOrderBookUpdate>, MarketError>>,
    /// Fires once per message the service took from this channel, so a test can
    /// tell that a frame was buffered before it let the page answer.
    taken: mpsc::UnboundedSender<()>,
}

impl PredictionOrderBookEventSubscription for FakeSubscription {
    async fn recv(&mut self) -> Option<Result<Vec<PredictionOrderBookUpdate>, MarketError>> {
        let updates = self.updates.recv().await;
        let _ = self.taken.send(());
        updates
    }
}

/// The chart half of the source, which the book service never touches.
struct PendingSeriesSubscription;

impl PredictionSeriesEventSubscription for PendingSeriesSubscription {
    async fn recv(&mut self) -> Option<Result<Vec<PredictionSeries>, MarketError>> {
        std::future::pending().await
    }
}

/// One service, its fake source, and the handles a test drives them with.
struct Harness {
    runtime: WidgetRuntime<PredictionOrderBookProtocol>,
    source: FakeSource,
}

impl Harness {
    fn new() -> Self {
        let source = FakeSource::default();
        let runtime = WidgetRuntime::spawn(
            current_test_handle(),
            PredictionOrderBookService::new(source.clone()),
        );
        Self { runtime, source }
    }

    /// Queues one page answer and gives the test the handle that sends it.
    fn fetch(&self) -> oneshot::Sender<Result<PredictionOrderBook, MarketError>> {
        let (sender, receiver) = oneshot::channel();
        self.source
            .fetches
            .lock()
            .expect("the fixture lock is not poisoned")
            .push_back(receiver);
        sender
    }

    /// Queues one live channel and gives the test the handles that feed and
    /// watch it.
    fn subscribe(
        &self,
    ) -> (
        mpsc::Sender<Result<Vec<PredictionOrderBookUpdate>, MarketError>>,
        mpsc::UnboundedReceiver<()>,
    ) {
        let (updates, receiver) = mpsc::channel(8);
        let (taken, taken_receiver) = mpsc::unbounded_channel();
        self.source
            .subscriptions
            .lock()
            .expect("the fixture lock is not poisoned")
            .push_back(FakeSubscription {
                updates: receiver,
                taken,
            });
        (updates, taken_receiver)
    }

    fn take_reliable(&mut self) -> ReliableStream<PredictionOrderBookProtocol> {
        self.runtime
            .take_reliable_stream()
            .expect("a reliable stream is taken once per test")
    }
}

/// The next snapshot a book stream publishes, with the key it was published
/// under.
async fn next_snapshot_keyed(
    latest: &mut LatestStream<PredictionOrderBookProtocol>,
) -> (PredictionOrderBookStreamKey, Arc<PredictionOrderBook>) {
    latest
        .changed()
        .await
        .expect("the latest channel stays open");
    let envelope = latest
        .next_unseen()
        .expect("a snapshot should be published");
    let key = envelope.key().clone();
    match envelope.message().as_ref() {
        PredictionOrderBookStreamMessage::Snapshot(book) => (key, book.clone()),
        PredictionOrderBookStreamMessage::Status(status) => {
            panic!("expected a snapshot, got {status:?}")
        }
    }
}

/// The next snapshot a book stream publishes.
async fn next_snapshot(
    latest: &mut LatestStream<PredictionOrderBookProtocol>,
) -> Arc<PredictionOrderBook> {
    next_snapshot_keyed(latest).await.1
}

/// Reads the reliable channel until `expected` arrives.
async fn receive_status(
    reliable: &mut ReliableStream<PredictionOrderBookProtocol>,
    expected: PredictionOrderBookStatus,
) {
    loop {
        let envelope = tokio::time::timeout(Duration::from_secs(1), reliable.recv())
            .await
            .expect("a status should arrive")
            .expect("the reliable stream stays open");
        let PredictionOrderBookStreamMessage::Status(status) = envelope.message().as_ref() else {
            continue;
        };
        if *status == expected {
            return;
        }
    }
}

fn prices(levels: &[PredictionBookLevel]) -> Vec<Fraction> {
    levels.iter().map(|level| level.price).collect()
}

fn sizes(levels: &[PredictionBookLevel]) -> Vec<Quantity> {
    levels.iter().map(|level| level.size).collect()
}

fn price(value: &str) -> Fraction {
    value.parse().expect("the fixture price is a fraction")
}

fn size(value: &str) -> Quantity {
    value.parse().expect("the fixture size is a quantity")
}

#[tokio::test]
async fn a_page_should_become_the_first_snapshot_and_then_ready() {
    let mut harness = Harness::new();
    let (_updates, _taken) = harness.subscribe();
    let fetch = harness.fetch();
    let mut latest = harness.runtime.latest_stream();
    let mut reliable = harness.take_reliable();

    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the load request starts a stream");
    receive_status(&mut reliable, PredictionOrderBookStatus::Connecting).await;
    fetch.send(Ok(book(1_000))).expect("the fetch is waiting");

    let snapshot = next_snapshot(&mut latest).await;

    assert_eq!(
        snapshot.leg,
        leg(),
        "the published book names the leg it was read for"
    );
    assert_eq!(snapshot.timestamp, 1_000);
    assert_eq!(prices(&snapshot.asks), vec![price("0.35")]);
    assert_eq!(prices(&snapshot.bids), vec![price("0.34")]);
    assert_eq!(snapshot.last_trade_price, Some(price("0.345")));
    receive_status(&mut reliable, PredictionOrderBookStatus::Ready).await;
    harness.runtime.shutdown().await;
}

#[tokio::test]
async fn a_buffered_frame_newer_than_the_page_should_be_applied_and_an_older_one_dropped() {
    let harness = Harness::new();
    let (updates, mut taken) = harness.subscribe();
    let fetch = harness.fetch();
    let mut latest = harness.runtime.latest_stream();
    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the load request starts a stream");
    // Both frames land while the page is in flight: the service takes the
    // channel before it asks for the page.
    updates
        .send(Ok(vec![
            PredictionOrderBookUpdate::Replace {
                bids: vec![level("0.1", "1")],
                asks: Vec::new(),
                timestamp: 900,
            },
            PredictionOrderBookUpdate::Levels {
                bids: vec![change("0.32", "7")],
                asks: Vec::new(),
                timestamp: 1_100,
            },
        ]))
        .await
        .expect("the fake channel is open");
    taken
        .recv()
        .await
        .expect("the frame was taken from the channel");
    fetch.send(Ok(book(1_000))).expect("the fetch is waiting");

    let snapshot = next_snapshot(&mut latest).await;

    assert_eq!(
        prices(&snapshot.bids),
        vec![price("0.34"), price("0.32")],
        "the newer frame is applied on top of the page, and the older replace \
         does not roll the page back"
    );
    assert_eq!(sizes(&snapshot.bids), vec![size("50"), size("7")]);
    assert_eq!(
        prices(&snapshot.asks),
        vec![price("0.35")],
        "the old replace does not empty the ask side either"
    );
    assert_eq!(snapshot.timestamp, 1_100);
    harness.runtime.shutdown().await;
}

#[tokio::test]
async fn levels_should_add_update_and_remove_a_price() {
    let harness = Harness::new();
    let (updates, _taken) = harness.subscribe();
    let fetch = harness.fetch();
    let mut latest = harness.runtime.latest_stream();
    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the load request starts a stream");
    fetch.send(Ok(book(1_000))).expect("the fetch is waiting");
    let _ = next_snapshot(&mut latest).await;

    updates
        .send(Ok(vec![PredictionOrderBookUpdate::Levels {
            bids: vec![change("0.34", "80")],
            asks: vec![change("0.36", "20"), change("0.35", "0")],
            timestamp: 2_000,
        }]))
        .await
        .expect("the fake channel is open");
    let snapshot = next_snapshot(&mut latest).await;

    assert_eq!(
        sizes(&snapshot.bids),
        vec![size("80")],
        "a level of the same price takes its new absolute size"
    );
    assert_eq!(
        prices(&snapshot.asks),
        vec![price("0.36")],
        "a zero size removes its level and a new price is added"
    );
    harness.runtime.shutdown().await;
}

#[tokio::test]
async fn replace_should_reset_both_sides() {
    let harness = Harness::new();
    let (updates, _taken) = harness.subscribe();
    let fetch = harness.fetch();
    let mut latest = harness.runtime.latest_stream();
    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the load request starts a stream");
    fetch.send(Ok(book(1_000))).expect("the fetch is waiting");
    let _ = next_snapshot(&mut latest).await;

    updates
        .send(Ok(vec![PredictionOrderBookUpdate::Replace {
            bids: vec![level("0.3", "5")],
            asks: vec![level("0.4", "6")],
            timestamp: 2_000,
        }]))
        .await
        .expect("the fake channel is open");
    let snapshot = next_snapshot(&mut latest).await;

    assert_eq!(prices(&snapshot.bids), vec![price("0.3")]);
    assert_eq!(prices(&snapshot.asks), vec![price("0.4")]);
    assert_eq!(
        snapshot.last_trade_price,
        Some(price("0.345")),
        "a replace is about the ladder; the last trade is its own update"
    );
    harness.runtime.shutdown().await;
}

#[tokio::test]
async fn a_tick_size_and_a_last_trade_should_be_carried_into_the_next_snapshot() {
    let harness = Harness::new();
    let (updates, _taken) = harness.subscribe();
    let fetch = harness.fetch();
    let mut latest = harness.runtime.latest_stream();
    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the load request starts a stream");
    fetch.send(Ok(book(1_000))).expect("the fetch is waiting");
    let _ = next_snapshot(&mut latest).await;

    updates
        .send(Ok(vec![
            PredictionOrderBookUpdate::TickSize {
                tick_size: "0.01".parse::<Tick>().expect("the fixture tick is valid"),
                timestamp: 2_000,
            },
            PredictionOrderBookUpdate::LastTrade {
                price: price("0.355"),
                timestamp: 3_000,
            },
        ]))
        .await
        .expect("the fake channel is open");
    let snapshot = next_snapshot(&mut latest).await;

    assert_eq!(snapshot.tick_size, "0.01".parse::<Tick>().unwrap());
    assert_eq!(snapshot.last_trade_price, Some(price("0.355")));
    assert_eq!(snapshot.timestamp, 3_000);
    harness.runtime.shutdown().await;
}

#[tokio::test]
async fn a_frame_older_than_the_book_should_be_dropped() {
    let harness = Harness::new();
    let (updates, _taken) = harness.subscribe();
    let fetch = harness.fetch();
    let mut latest = harness.runtime.latest_stream();
    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the load request starts a stream");
    fetch.send(Ok(book(1_000))).expect("the fetch is waiting");
    let _ = next_snapshot(&mut latest).await;

    updates
        .send(Ok(vec![PredictionOrderBookUpdate::Levels {
            bids: vec![change("0.3", "9")],
            asks: Vec::new(),
            timestamp: 500,
        }]))
        .await
        .expect("the fake channel is open");
    // A stale batch publishes nothing at all, so the next reading is the one
    // the following (accepted) frame produces.
    updates
        .send(Ok(vec![PredictionOrderBookUpdate::Levels {
            bids: vec![change("0.33", "4")],
            asks: Vec::new(),
            timestamp: 2_000,
        }]))
        .await
        .expect("the fake channel is open");
    let snapshot = next_snapshot(&mut latest).await;

    assert_eq!(
        prices(&snapshot.bids),
        vec![price("0.34"), price("0.33")],
        "the stale frame left no level behind"
    );
    harness.runtime.shutdown().await;
}

#[tokio::test]
async fn the_published_book_should_be_best_first_on_both_sides() {
    let harness = Harness::new();
    let (_updates, _taken) = harness.subscribe();
    let fetch = harness.fetch();
    let mut latest = harness.runtime.latest_stream();
    let unsorted = order_book(
        leg(),
        "0.01",
        vec![level("0.3", "1"), level("0.34", "2"), level("0.32", "3")],
        vec![level("0.4", "1"), level("0.36", "2"), level("0.38", "3")],
    );
    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the load request starts a stream");
    fetch.send(Ok(unsorted)).expect("the fetch is waiting");

    let snapshot = next_snapshot(&mut latest).await;

    assert_eq!(
        prices(&snapshot.bids),
        vec![price("0.34"), price("0.32"), price("0.3")],
        "bids are published best-first, so they run downwards"
    );
    assert_eq!(
        prices(&snapshot.asks),
        vec![price("0.36"), price("0.38"), price("0.4")],
        "asks are published best-first, so they run upwards"
    );
    harness.runtime.shutdown().await;
}

#[tokio::test]
async fn a_failed_fetch_should_fail_the_stream() {
    let mut harness = Harness::new();
    let (_updates, _taken) = harness.subscribe();
    let fetch = harness.fetch();
    let mut reliable = harness.take_reliable();
    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the load request starts a stream");
    fetch
        .send(Err(MarketError::ServiceClosed))
        .expect("the fetch is waiting");

    receive_status(
        &mut reliable,
        PredictionOrderBookStatus::Failed {
            error: PredictionOrderBookError::Market(MarketError::ServiceClosed),
        },
    )
    .await;
    harness.runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_fetch_that_never_answers_should_time_out() {
    let mut harness = Harness::new();
    let (_updates, _taken) = harness.subscribe();
    // No page is queued, so the fetch never answers.
    let mut reliable = harness.take_reliable();
    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the load request starts a stream");

    tokio::time::advance(ATTEMPT_TIMEOUT + Duration::from_secs(1)).await;

    receive_status(
        &mut reliable,
        PredictionOrderBookStatus::Failed {
            error: PredictionOrderBookError::Timeout(ATTEMPT_TIMEOUT),
        },
    )
    .await;
    harness.runtime.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_subscription_that_never_opens_should_time_out() {
    let mut harness = Harness::new();
    // No channel is queued, so the subscription never opens.
    let mut reliable = harness.take_reliable();
    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the load request starts a stream");

    tokio::time::advance(ATTEMPT_TIMEOUT + Duration::from_secs(1)).await;

    receive_status(
        &mut reliable,
        PredictionOrderBookStatus::Failed {
            error: PredictionOrderBookError::Timeout(ATTEMPT_TIMEOUT),
        },
    )
    .await;
    harness.runtime.shutdown().await;
}

#[tokio::test]
async fn a_failing_channel_should_fail_the_stream() {
    let mut harness = Harness::new();
    let (updates, _taken) = harness.subscribe();
    let fetch = harness.fetch();
    let mut reliable = harness.take_reliable();
    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the load request starts a stream");
    fetch.send(Ok(book(1_000))).expect("the fetch is waiting");
    receive_status(&mut reliable, PredictionOrderBookStatus::Ready).await;

    updates
        .send(Err(MarketError::Disconnected(
            DisconnectReason::HeartbeatTimeout,
        )))
        .await
        .expect("the fake channel is open");

    receive_status(
        &mut reliable,
        PredictionOrderBookStatus::Failed {
            error: PredictionOrderBookError::Market(MarketError::Disconnected(
                DisconnectReason::HeartbeatTimeout,
            )),
        },
    )
    .await;
    harness.runtime.shutdown().await;
}

#[tokio::test]
async fn a_closed_channel_should_fail_the_stream_as_a_normal_disconnect() {
    let mut harness = Harness::new();
    let (updates, _taken) = harness.subscribe();
    let fetch = harness.fetch();
    let mut reliable = harness.take_reliable();
    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the load request starts a stream");
    fetch.send(Ok(book(1_000))).expect("the fetch is waiting");
    receive_status(&mut reliable, PredictionOrderBookStatus::Ready).await;
    drop(updates);

    receive_status(
        &mut reliable,
        PredictionOrderBookStatus::Failed {
            error: PredictionOrderBookError::Market(MarketError::Disconnected(
                DisconnectReason::Normal,
            )),
        },
    )
    .await;
    harness.runtime.shutdown().await;
}

#[tokio::test]
async fn a_channel_error_during_the_page_should_fail_the_stream() {
    let mut harness = Harness::new();
    let (updates, mut taken) = harness.subscribe();
    let _fetch = harness.fetch();
    let mut reliable = harness.take_reliable();
    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the load request starts a stream");
    updates
        .send(Err(MarketError::Disconnected(
            DisconnectReason::HeartbeatTimeout,
        )))
        .await
        .expect("the fake channel is open");
    taken
        .recv()
        .await
        .expect("the frame was taken from the channel");

    receive_status(
        &mut reliable,
        PredictionOrderBookStatus::Failed {
            error: PredictionOrderBookError::Market(MarketError::Disconnected(
                DisconnectReason::HeartbeatTimeout,
            )),
        },
    )
    .await;
    harness.runtime.shutdown().await;
}

#[tokio::test]
async fn a_new_epoch_should_replace_the_old_stream() {
    let mut harness = Harness::new();
    let (first_updates, _first_taken) = harness.subscribe();
    let first_fetch = harness.fetch();
    let mut latest = harness.runtime.latest_stream();
    let mut reliable = harness.take_reliable();
    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the first load starts a stream");
    first_fetch
        .send(Ok(book(1_000)))
        .expect("the first fetch is waiting");
    receive_status(&mut reliable, PredictionOrderBookStatus::Ready).await;
    let _ = next_snapshot(&mut latest).await;

    let (_second_updates, _second_taken) = harness.subscribe();
    let second_fetch = harness.fetch();
    harness
        .runtime
        .client()
        .call(retry(2))
        .await
        .expect("the retry starts a replacing stream");
    second_fetch
        .send(Ok(book(5_000)))
        .expect("the second fetch is waiting");
    receive_status(&mut reliable, PredictionOrderBookStatus::Ready).await;
    let (key, snapshot) = next_snapshot_keyed(&mut latest).await;

    assert_eq!(
        key.epoch, 2,
        "the snapshot belongs to the generation that asked for it"
    );
    assert_eq!(
        snapshot.timestamp, 5_000,
        "the replaced stream's page is the one that is drawn"
    );

    // The old channel is no longer followed: the task that read it is gone, and
    // with it the receiver, so nothing can be published under the retired key.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(
        first_updates
            .send(Ok(vec![PredictionOrderBookUpdate::LastTrade {
                price: price("0.01"),
                timestamp: 9_000,
            }]))
            .await
            .is_err(),
        "the replaced stream took its channel with it"
    );
    assert!(
        latest.next_unseen().is_none(),
        "the replaced stream publishes nothing more"
    );
    harness.runtime.shutdown().await;
}

#[tokio::test]
async fn a_buffered_levels_at_the_pages_own_instant_should_be_applied() {
    let harness = Harness::new();
    let (updates, mut taken) = harness.subscribe();
    let fetch = harness.fetch();
    let mut latest = harness.runtime.latest_stream();
    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the load request starts a stream");
    // Both frames land while the page is in flight. The replace states the whole
    // book as it was queued before the page was read, so it loses to the page;
    // the absolute sizes are newer by construction, even at the same instant.
    updates
        .send(Ok(vec![
            PredictionOrderBookUpdate::Replace {
                bids: vec![level("0.1", "1")],
                asks: Vec::new(),
                timestamp: 1_000,
            },
            PredictionOrderBookUpdate::Levels {
                bids: vec![change("0.34", "80")],
                asks: Vec::new(),
                timestamp: 1_000,
            },
        ]))
        .await
        .expect("the fake channel is open");
    taken
        .recv()
        .await
        .expect("the frame was taken from the channel");
    fetch.send(Ok(book(1_000))).expect("the fetch is waiting");

    let snapshot = next_snapshot(&mut latest).await;

    assert_eq!(
        prices(&snapshot.bids),
        vec![price("0.34")],
        "the whole-book replace from the page's own instant is the dump that \
         predates the page"
    );
    assert_eq!(
        sizes(&snapshot.bids),
        vec![size("80")],
        "an absolute size stated at the page's own instant happened after it"
    );
    assert_eq!(snapshot.timestamp, 1_000);
    harness.runtime.shutdown().await;
}

#[tokio::test]
async fn a_live_replace_at_the_pages_own_instant_should_be_refused() {
    let harness = Harness::new();
    let (updates, _taken) = harness.subscribe();
    let fetch = harness.fetch();
    let mut latest = harness.runtime.latest_stream();
    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the load request starts a stream");
    fetch.send(Ok(book(1_000))).expect("the fetch is waiting");
    let first = next_snapshot(&mut latest).await;
    assert_eq!(prices(&first.bids), vec![price("0.34")]);

    updates
        .send(Ok(vec![PredictionOrderBookUpdate::Replace {
            bids: vec![level("0.1", "1")],
            asks: Vec::new(),
            timestamp: 1_000,
        }]))
        .await
        .expect("the fake channel is open");
    // A refused frame publishes nothing, so the next reading is the one the
    // following accepted frame produces.
    updates
        .send(Ok(vec![PredictionOrderBookUpdate::Levels {
            bids: vec![change("0.33", "4")],
            asks: Vec::new(),
            timestamp: 2_000,
        }]))
        .await
        .expect("the fake channel is open");
    let snapshot = next_snapshot(&mut latest).await;

    assert_eq!(
        prices(&snapshot.bids),
        vec![price("0.34"), price("0.33")],
        "the same-instant replace did not roll the page back"
    );
    assert_eq!(snapshot.timestamp, 2_000);
    harness.runtime.shutdown().await;
}

#[tokio::test]
async fn a_replace_tick_and_trade_at_the_same_newer_instant_should_all_apply() {
    let harness = Harness::new();
    let (updates, _taken) = harness.subscribe();
    let fetch = harness.fetch();
    let mut latest = harness.runtime.latest_stream();
    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the load request starts a stream");
    fetch.send(Ok(book(1_000))).expect("the fetch is waiting");
    let _ = next_snapshot(&mut latest).await;

    updates
        .send(Ok(vec![
            PredictionOrderBookUpdate::Replace {
                bids: vec![level("0.2", "5")],
                asks: vec![level("0.3", "6")],
                timestamp: 2_000,
            },
            PredictionOrderBookUpdate::TickSize {
                tick_size: "0.01".parse::<Tick>().expect("the fixture tick is valid"),
                timestamp: 2_000,
            },
            PredictionOrderBookUpdate::LastTrade {
                price: price("0.25"),
                timestamp: 2_000,
            },
        ]))
        .await
        .expect("the fake channel is open");
    let snapshot = next_snapshot(&mut latest).await;

    assert_eq!(prices(&snapshot.bids), vec![price("0.2")]);
    assert_eq!(prices(&snapshot.asks), vec![price("0.3")]);
    assert_eq!(
        snapshot.tick_size,
        "0.01".parse::<Tick>().expect("the fixture tick is valid"),
        "a tick stated at the same instant as the replace is applied"
    );
    assert_eq!(
        snapshot.last_trade_price,
        Some(price("0.25")),
        "so is a last trade at that instant"
    );
    assert_eq!(snapshot.timestamp, 2_000);
    harness.runtime.shutdown().await;
}

#[tokio::test]
async fn a_token_with_no_book_should_publish_an_empty_page_and_then_follow_the_channel() {
    let mut harness = Harness::new();
    let (updates, _taken) = harness.subscribe();
    let fetch = harness.fetch();
    let mut latest = harness.runtime.latest_stream();
    let mut reliable = harness.take_reliable();
    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the load request starts a stream");
    fetch
        .send(Err(no_book_error()))
        .expect("the fetch is waiting");

    let empty = next_snapshot(&mut latest).await;
    assert_eq!(empty.leg, leg(), "the page names the leg it was read for");
    assert_eq!(
        empty.tick_size,
        "0.01".parse::<Tick>().expect("the fixture tick is valid"),
        "an empty page is stated at the venue's cent tick"
    );
    assert!(empty.bids.is_empty() && empty.asks.is_empty());
    assert_eq!(empty.last_trade_price, None);
    assert_eq!(empty.timestamp, 0);
    // A Ready can only be read after the snapshot; a failure would have ended the
    // stream before either was published.
    receive_status(&mut reliable, PredictionOrderBookStatus::Ready).await;

    updates
        .send(Ok(vec![PredictionOrderBookUpdate::Replace {
            bids: vec![level("0.34", "50")],
            asks: vec![level("0.35", "100")],
            timestamp: 1_000,
        }]))
        .await
        .expect("the fake channel is open");
    let snapshot = next_snapshot(&mut latest).await;

    assert_eq!(
        prices(&snapshot.bids),
        vec![price("0.34")],
        "a book that appears later replaces the empty page"
    );
    assert_eq!(prices(&snapshot.asks), vec![price("0.35")]);
    harness.runtime.shutdown().await;
}

#[tokio::test]
async fn a_404_that_is_not_the_missing_book_answer_should_fail_the_stream() {
    let mut harness = Harness::new();
    let (_updates, _taken) = harness.subscribe();
    let fetch = harness.fetch();
    let mut reliable = harness.take_reliable();
    // A missing token and a missing book are both 404s; only the venue's own
    // sentence says the book is what does not exist.
    let refusal =
        MarketError::Network(MarketNetworkError::from(MarketParticipantError::Rejected {
            code: SmolStr::new_static("404"),
            message: SmolStr::new_static("the requested token id is unknown"),
            retryable: false,
            raw_response: None,
        }));
    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the load request starts a stream");
    fetch
        .send(Err(refusal.clone()))
        .expect("the fetch is waiting");

    receive_status(
        &mut reliable,
        PredictionOrderBookStatus::Failed {
            error: PredictionOrderBookError::Market(refusal),
        },
    )
    .await;
    harness.runtime.shutdown().await;
}

#[tokio::test]
async fn a_stop_should_cancel_the_active_stream_and_keep_serving() {
    let harness = Harness::new();
    assert_eq!(
        harness
            .runtime
            .client()
            .call(stop())
            .await
            .expect("a stop is answered")
            .into_value(),
        PredictionOrderBookResponse::Stopped,
        "a stop with nothing to stop is still answered"
    );

    let (first_updates, _first_taken) = harness.subscribe();
    let _first_fetch = harness.fetch();
    harness
        .runtime
        .client()
        .call(load(1))
        .await
        .expect("the load request starts a stream");
    assert_eq!(
        harness
            .runtime
            .client()
            .call(stop())
            .await
            .expect("the stop is answered")
            .into_value(),
        PredictionOrderBookResponse::Stopped
    );

    // The stream the stop retired is gone, and with it the channel it read.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(
        first_updates
            .send(Ok(vec![PredictionOrderBookUpdate::LastTrade {
                price: price("0.01"),
                timestamp: 1,
            }]))
            .await
            .is_err(),
        "the stopped stream took its channel with it"
    );

    // The service is still waiting for requests.
    let (_second_updates, _second_taken) = harness.subscribe();
    let second_fetch = harness.fetch();
    let mut latest = harness.runtime.latest_stream();
    harness
        .runtime
        .client()
        .call(retry(2))
        .await
        .expect("a request after a stop starts a stream");
    second_fetch
        .send(Ok(book(5_000)))
        .expect("the second fetch is waiting");
    let (key, snapshot) = next_snapshot_keyed(&mut latest).await;

    assert_eq!(key.epoch, 2);
    assert_eq!(snapshot.timestamp, 5_000);
    harness.runtime.shutdown().await;
}

#[test]
fn the_live_buffer_is_bounded_separately_from_the_page() {
    assert_eq!(MAX_BUFFERED_UPDATES, 2_000);
}

//! The background service the prediction order book panel talks to.
//!
//! One book is one leg of one market, and the panel follows it in two parts: a
//! REST page that states the book at an instant, and the venue's live channel
//! that carries the changes. The subscription is opened before the page is
//! requested, so a frame that lands while the page is in flight is buffered
//! rather than missed — and the page is authoritative at its own timestamp, so a
//! buffered frame older than it is dropped instead of rolling the book back.
//!
//! The stream key is the event, the leg and a local epoch: switching to another
//! leg is another book, and a request for one replaces the stream the previous
//! one was reading.

use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    sync::Arc,
    time::Duration,
};

use futures::StreamExt;
use gpui_kit::{App, SharedString};
use i18n::t;
use market_participants_connector::{
    error::MarketParticipantError,
    market::{
        domain::DisconnectReason, error::MarketError, provider::MarketCapability,
        streams::PredictionOrderBookStream,
    },
};
use market_participants_model::{
    decimal::{Fraction, Quantity, Tick, UnixMillis},
    outcome::OutcomeLegRef,
    prediction::{
        PredictionBookLevel, PredictionBookLevelChange, PredictionEventRef, PredictionOrderBook,
        PredictionOrderBookUpdate,
    },
};
use thiserror::Error;
use tokio::time::{Instant, timeout};
use widget::{
    error::LocalizedError,
    protocol::WidgetProtocol,
    rpc::RpcCallError,
    runtime::{ServiceContext, WidgetService},
    stream::{StreamPublishError, StreamPublisher},
};

use crate::model::PredictionOrderBookTarget;

/// How long one read may take before it is a failure.
pub(crate) const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

/// The most live batches buffered between opening the stream and the first
/// snapshot.
///
/// A frame older than this many frames is one the panel would overwrite anyway.
pub(crate) const MAX_BUFFERED_UPDATES: usize = 2_000;

/// What the view asks the book service to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PredictionOrderBookRequest {
    Load {
        target: PredictionOrderBookTarget,
        epoch: u64,
    },
    Retry {
        target: PredictionOrderBookTarget,
        epoch: u64,
    },
    /// Retires the stream the panel follows and leaves the service waiting for
    /// another request.
    ///
    /// A hidden panel holds no lease on the venue: the stream is cancelled
    /// instead of publishing into a channel nobody draws.
    Stop,
}

/// What the service answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PredictionOrderBookResponse {
    StreamStarted,
    Stopped,
}

/// The event, leg and generation one book follows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PredictionOrderBookStreamKey {
    pub event: PredictionEventRef,
    pub leg: OutcomeLegRef,
    /// The view-local generation that produced this request.
    pub epoch: u64,
}

/// Where the stream is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PredictionOrderBookStatus {
    Connecting,
    Ready,
    Failed { error: PredictionOrderBookError },
}

/// Everything the book service publishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PredictionOrderBookStreamMessage {
    Status(PredictionOrderBookStatus),
    Snapshot(Arc<PredictionOrderBook>),
}

/// Why one book produced no reading.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum PredictionOrderBookError {
    #[error("the order book request failed: {0}")]
    Market(#[source] MarketError),
    #[error("the order book request did not answer within {0:?}")]
    Timeout(Duration),
    #[error("the order book command failed: {0}")]
    Rpc(#[source] RpcCallError<PredictionOrderBookServiceError>),
}

impl LocalizedError for PredictionOrderBookError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Market(error) => error.localized_message(cx),
            Self::Timeout(_) => t!(cx, "widget.prediction_market.book.timed_out"),
            Self::Rpc(error) => error.localized_message(cx),
        }
    }
}

/// Why a request was refused.
///
/// The service never refuses one: a request always starts or replaces a stream.
/// The type exists because the protocol boundary names one.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum PredictionOrderBookServiceError {}

impl LocalizedError for PredictionOrderBookServiceError {
    fn localized_message(&self, _cx: &App) -> SharedString {
        match *self {}
    }
}

pub(crate) struct PredictionOrderBookProtocol;

impl WidgetProtocol for PredictionOrderBookProtocol {
    type Request = PredictionOrderBookRequest;
    type Response = PredictionOrderBookResponse;
    type StreamKey = PredictionOrderBookStreamKey;
    type StreamMessage = PredictionOrderBookStreamMessage;
    type Error = PredictionOrderBookServiceError;
}

/// The seam between the book panel and the connector, so a test can answer
/// instead.
///
/// One book is one leg of one market: the target names the event, every leg of
/// the market — the payload a venue's market channel subscribes with — and the
/// leg the ladder is drawn for.
pub(crate) trait PredictionOrderBookSource: Clone + Send + Sync + 'static {
    type Subscription: PredictionOrderBookEventSubscription;

    fn fetch_order_book(
        &self,
        target: PredictionOrderBookTarget,
    ) -> impl Future<Output = Result<PredictionOrderBook, MarketError>> + Send;

    fn subscribe_order_book(
        &self,
        target: PredictionOrderBookTarget,
    ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send;
}

/// One live order book stream.
pub(crate) trait PredictionOrderBookEventSubscription: Send + 'static {
    fn recv(
        &mut self,
    ) -> impl Future<Output = Option<Result<Vec<PredictionOrderBookUpdate>, MarketError>>> + Send;
}

impl PredictionOrderBookSource for MarketCapability {
    type Subscription = ClientPredictionOrderBookSubscription;

    async fn fetch_order_book(
        &self,
        target: PredictionOrderBookTarget,
    ) -> Result<PredictionOrderBook, MarketError> {
        self.fetch_prediction_order_book(target.event, target.legs, target.leg)
            .await
    }

    async fn subscribe_order_book(
        &self,
        target: PredictionOrderBookTarget,
    ) -> Result<Self::Subscription, MarketError> {
        let inner = self
            .subscribe_prediction_order_book(target.event, target.legs, target.leg)
            .await?;
        Ok(ClientPredictionOrderBookSubscription { inner })
    }
}

pub(crate) struct ClientPredictionOrderBookSubscription {
    inner: PredictionOrderBookStream,
}

impl PredictionOrderBookEventSubscription for ClientPredictionOrderBookSubscription {
    async fn recv(&mut self) -> Option<Result<Vec<PredictionOrderBookUpdate>, MarketError>> {
        self.inner.next().await
    }
}

pub(crate) struct PredictionOrderBookService<S = MarketCapability> {
    source: S,
    attempt_timeout: Duration,
}

impl<S> PredictionOrderBookService<S> {
    pub(crate) fn new(source: S) -> Self {
        Self {
            source,
            attempt_timeout: ATTEMPT_TIMEOUT,
        }
    }
}

impl<S> WidgetService<PredictionOrderBookProtocol> for PredictionOrderBookService<S>
where
    S: PredictionOrderBookSource,
{
    async fn run(self, mut context: ServiceContext<PredictionOrderBookProtocol>) {
        let mut active_key: Option<PredictionOrderBookStreamKey> = None;
        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                continue;
            }
            let (key, target) = match &request {
                PredictionOrderBookRequest::Load { target, epoch }
                | PredictionOrderBookRequest::Retry { target, epoch } => (
                    PredictionOrderBookStreamKey {
                        event: target.event.clone(),
                        leg: target.leg.clone(),
                        epoch: *epoch,
                    },
                    target.clone(),
                ),
                // A stop names no stream of its own: it retires whatever the
                // panel was following and answers, leaving the service ready for
                // the next request.
                PredictionOrderBookRequest::Stop => {
                    if let Some(previous) = active_key.take() {
                        context.cancel_stream(&previous);
                    }
                    let _ = reply.respond(Ok(PredictionOrderBookResponse::Stopped));
                    continue;
                }
            };
            // A panel follows one leg at a time, so a request for another leg —
            // or for a new epoch of the same one — retires the previous stream
            // instead of leaving it publishing into a channel nobody reads.
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
            context.replace_stream(key.clone(), move |publisher| {
                run_stream(source, stream_key, target, attempt_timeout, publisher)
            });
            active_key = Some(key.clone());
            if reply
                .respond(Ok(PredictionOrderBookResponse::StreamStarted))
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
    key: PredictionOrderBookStreamKey,
    target: PredictionOrderBookTarget,
    attempt_timeout: Duration,
    publisher: StreamPublisher<PredictionOrderBookProtocol>,
) where
    S: PredictionOrderBookSource,
{
    if publish_status(&publisher, PredictionOrderBookStatus::Connecting)
        .await
        .is_err()
    {
        return;
    }
    let mut subscription =
        match timeout(attempt_timeout, source.subscribe_order_book(target.clone())).await {
            Ok(Ok(subscription)) => subscription,
            Ok(Err(error)) => {
                fail(&publisher, PredictionOrderBookError::Market(error)).await;
                return;
            }
            Err(_) => {
                fail(
                    &publisher,
                    PredictionOrderBookError::Timeout(attempt_timeout),
                )
                .await;
                return;
            }
        };

    // The channel is open before the page is requested, so a frame that lands
    // while the page is in flight is buffered instead of missed. The buffer is
    // bounded: a panel that fell this far behind redraws the newest state
    // anyway.
    let mut buffered: VecDeque<Vec<PredictionOrderBookUpdate>> = VecDeque::new();
    let fetch = timeout(attempt_timeout, source.fetch_order_book(target.clone()));
    tokio::pin!(fetch);
    let book = loop {
        tokio::select! {
            biased;
            () = publisher.cancelled() => return,
            result = &mut fetch => break result,
            updates = subscription.recv() => match updates {
                Some(Ok(updates)) => {
                    buffered.push_back(updates);
                    if buffered.len() > MAX_BUFFERED_UPDATES {
                        buffered.pop_front();
                    }
                }
                Some(Err(error)) => {
                    fail(&publisher, PredictionOrderBookError::Market(error)).await;
                    return;
                }
                None => {
                    fail(&publisher, disconnected()).await;
                    return;
                }
            },
        }
    };
    let book = match book {
        Ok(Ok(book)) => book,
        // A token no CLOB book exists for answers 404. Many placeholder markets
        // have no resting level at all, and that is an empty page rather than a
        // failure: the panel says there is no data and the channel stays open,
        // because a book can still appear later.
        Ok(Err(error)) if is_missing_order_book(&error) => missing_order_book(&target.leg),
        Ok(Err(error)) => {
            fail(&publisher, PredictionOrderBookError::Market(error)).await;
            return;
        }
        Err(_) => {
            fail(
                &publisher,
                PredictionOrderBookError::Timeout(attempt_timeout),
            )
            .await;
            return;
        }
    };

    // The page is authoritative at its own timestamp: the channel usually opens
    // with a dump of the whole book that was queued before the page was read, so
    // applying it would roll the reading back to an older instant. A whole-book
    // replace from the page's own instant is such a dump too — the page was read
    // at that instant and the frame may describe an earlier moment — while an
    // absolute change at that instant happened after the page by construction.
    let page_timestamp = book.timestamp;
    let mut state = BookState::from_snapshot(&book);
    for batch in buffered {
        for update in &batch {
            let timestamp = timestamp_of(update);
            let applies = match update {
                PredictionOrderBookUpdate::Replace { .. } => timestamp > page_timestamp,
                PredictionOrderBookUpdate::Levels { .. }
                | PredictionOrderBookUpdate::TickSize { .. }
                | PredictionOrderBookUpdate::LastTrade { .. } => timestamp >= page_timestamp,
            };
            if applies {
                state.apply(update);
            }
        }
    }
    if publisher
        .publish_latest(PredictionOrderBookStreamMessage::Snapshot(Arc::new(
            state.snapshot(&key.leg),
        )))
        .is_err()
    {
        return;
    }
    if publish_status(&publisher, PredictionOrderBookStatus::Ready)
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            biased;
            () = publisher.clone().cancelled_owned() => return,
            updates = subscription.recv() => match updates {
                Some(Ok(updates)) => {
                    let mut accepted = false;
                    for update in &updates {
                        accepted |= state.apply(update);
                    }
                    if !accepted {
                        continue;
                    }
                    if publisher
                        .publish_latest(PredictionOrderBookStreamMessage::Snapshot(Arc::new(
                            state.snapshot(&key.leg),
                        )))
                        .is_err()
                    {
                        return;
                    }
                }
                Some(Err(error)) => {
                    fail(&publisher, PredictionOrderBookError::Market(error)).await;
                    return;
                }
                None => {
                    fail(&publisher, disconnected()).await;
                    return;
                }
            },
        }
    }
}

/// The state one book is drawn from: the normalized book the page and the
/// channel agree on.
struct BookState {
    tick: Tick,
    /// Ascending by price; the bid side is read in reverse.
    bids: BTreeMap<Fraction, Quantity>,
    asks: BTreeMap<Fraction, Quantity>,
    last_trade: Option<Fraction>,
    timestamp: UnixMillis,
}

impl BookState {
    fn from_snapshot(book: &PredictionOrderBook) -> Self {
        Self {
            tick: book.tick_size,
            bids: levels(&book.bids),
            asks: levels(&book.asks),
            last_trade: book.last_trade_price,
            timestamp: book.timestamp,
        }
    }

    /// Applies one update, dropping it when the book cannot carry it: a
    /// whole-book replace states an instant that has to be strictly newer than
    /// the one the state already holds, while a tick change, a last trade and a
    /// set of absolute sizes are idempotent and may land on that same instant.
    /// Answers whether the state changed.
    fn apply(&mut self, update: &PredictionOrderBookUpdate) -> bool {
        let timestamp = timestamp_of(update);
        let ordered = match update {
            PredictionOrderBookUpdate::Replace { .. } => timestamp > self.timestamp,
            PredictionOrderBookUpdate::Levels { .. }
            | PredictionOrderBookUpdate::TickSize { .. }
            | PredictionOrderBookUpdate::LastTrade { .. } => timestamp >= self.timestamp,
        };
        if !ordered {
            return false;
        }
        match update {
            PredictionOrderBookUpdate::Replace { bids, asks, .. } => {
                self.bids = levels(bids);
                self.asks = levels(asks);
            }
            PredictionOrderBookUpdate::Levels { bids, asks, .. } => {
                apply_changes(&mut self.bids, bids);
                apply_changes(&mut self.asks, asks);
            }
            PredictionOrderBookUpdate::TickSize { tick_size, .. } => self.tick = *tick_size,
            PredictionOrderBookUpdate::LastTrade { price, .. } => self.last_trade = Some(*price),
        }
        self.timestamp = timestamp;
        true
    }

    fn snapshot(&self, leg: &OutcomeLegRef) -> PredictionOrderBook {
        PredictionOrderBook {
            leg: leg.clone(),
            tick_size: self.tick,
            // The map runs from the worst price upwards; a book is read from
            // the best one, and the bid side is the side that runs downwards.
            bids: self
                .bids
                .iter()
                .rev()
                .map(|(price, size)| PredictionBookLevel {
                    price: *price,
                    size: *size,
                })
                .collect(),
            asks: self
                .asks
                .iter()
                .map(|(price, size)| PredictionBookLevel {
                    price: *price,
                    size: *size,
                })
                .collect(),
            last_trade_price: self.last_trade,
            timestamp: self.timestamp,
        }
    }
}

/// The instantaneous timestamp an update states.
fn timestamp_of(update: &PredictionOrderBookUpdate) -> UnixMillis {
    match update {
        PredictionOrderBookUpdate::Replace { timestamp, .. }
        | PredictionOrderBookUpdate::Levels { timestamp, .. }
        | PredictionOrderBookUpdate::TickSize { timestamp, .. }
        | PredictionOrderBookUpdate::LastTrade { timestamp, .. } => *timestamp,
    }
}

/// The levels of one side, dropping the ones that hold no shares.
fn levels(levels: &[PredictionBookLevel]) -> BTreeMap<Fraction, Quantity> {
    levels
        .iter()
        .filter(|level| !level.size.is_zero())
        .map(|level| (level.price, level.size))
        .collect()
}

/// Applies one side's absolute size changes; a zero size removes its level.
fn apply_changes(side: &mut BTreeMap<Fraction, Quantity>, changes: &[PredictionBookLevelChange]) {
    for change in changes {
        if change.size.is_zero() {
            side.remove(&change.price);
        } else {
            side.insert(change.price, change.size);
        }
    }
}

/// The tick a token with no book at all is read at.
///
/// Polymarket quotes its CLOB in cents, so a cent is the fallback precision an
/// empty page is stated at. The panel hides its grouping control because the
/// book has no level to group.
fn missing_book_tick() -> Tick {
    // A hundredth of one share, in the model's eighteen-digit fixed point: the
    // value is not zero, so it is always a valid tick.
    Tick::from_inner(10_000_000_000_000_000).expect("a cent is a valid tick")
}

/// The empty page a token with no CLOB book is drawn from.
fn missing_order_book(leg: &OutcomeLegRef) -> PredictionOrderBook {
    PredictionOrderBook {
        leg: leg.clone(),
        tick_size: missing_book_tick(),
        bids: Vec::new(),
        asks: Vec::new(),
        last_trade_price: None,
        timestamp: 0,
    }
}

/// Whether the venue answered that the requested token has no CLOB book.
///
/// Polymarket returns `404 No orderbook exists for the requested token id` for a
/// token nothing rests on; every other refusal — a rate limit, an unknown token,
/// a transport failure — stays a failure.
fn is_missing_order_book(error: &MarketError) -> bool {
    matches!(
        error,
        MarketError::Network(network)
            if matches!(
                network.error(),
                MarketParticipantError::Rejected { code, message, .. }
                    if code == "404" && message == "No orderbook exists for the requested token id"
            )
    )
}

fn disconnected() -> PredictionOrderBookError {
    PredictionOrderBookError::Market(MarketError::Disconnected(DisconnectReason::Normal))
}

async fn publish_status(
    publisher: &StreamPublisher<PredictionOrderBookProtocol>,
    status: PredictionOrderBookStatus,
) -> Result<(), StreamPublishError> {
    publisher
        .publish_reliable(PredictionOrderBookStreamMessage::Status(status))
        .await
}

async fn fail(
    publisher: &StreamPublisher<PredictionOrderBookProtocol>,
    error: PredictionOrderBookError,
) {
    let _ = publisher
        .publish_reliable(PredictionOrderBookStreamMessage::Status(
            PredictionOrderBookStatus::Failed { error },
        ))
        .await;
}

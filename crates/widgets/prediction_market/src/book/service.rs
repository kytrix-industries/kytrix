use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    sync::Arc,
    time::Duration,
};

use futures::StreamExt;
use i18n::{t_smol, translate::Translate};
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
use smol_str::SmolStr;
use thiserror::Error;
use tokio::time::{Instant, timeout};
use widget::{
    error::TranslatedError,
    protocol::WidgetProtocol,
    rpc::RpcCallError,
    runtime::{ServiceContext, WidgetService},
    stream::{StreamPublishError, StreamPublisher},
};

use crate::model::PredictionOrderBookTarget;

pub(crate) const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) const MAX_BUFFERED_UPDATES: usize = 2_000;

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
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PredictionOrderBookResponse {
    StreamStarted,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PredictionOrderBookStreamKey {
    pub event: PredictionEventRef,
    pub leg: OutcomeLegRef,
    pub epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PredictionOrderBookStatus {
    Connecting,
    Ready,
    Failed { error: PredictionOrderBookError },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PredictionOrderBookStreamMessage {
    Status(PredictionOrderBookStatus),
    Snapshot(Arc<PredictionOrderBook>),
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum PredictionOrderBookError {
    #[error("the order book request failed: {0}")]
    Market(#[source] MarketError),
    #[error("the order book request did not answer within {0:?}")]
    Timeout(Duration),
    #[error("the order book command failed: {0}")]
    Rpc(#[source] RpcCallError<PredictionOrderBookServiceError>),
}

impl TranslatedError for PredictionOrderBookError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Market(error) => error.translated_message(translate),
            Self::Timeout(_) => t_smol!(translate, "widget.prediction_market.book.timed_out"),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum PredictionOrderBookServiceError {}

impl TranslatedError for PredictionOrderBookServiceError {
    fn translated_message(&self, _translate: &Translate) -> SmolStr {
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
                PredictionOrderBookRequest::Stop => {
                    if let Some(previous) = active_key.take() {
                        context.cancel_stream(&previous);
                    }
                    let _ = reply.respond(Ok(PredictionOrderBookResponse::Stopped));
                    continue;
                }
            };
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

struct BookState {
    tick: Tick,
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

fn timestamp_of(update: &PredictionOrderBookUpdate) -> UnixMillis {
    match update {
        PredictionOrderBookUpdate::Replace { timestamp, .. }
        | PredictionOrderBookUpdate::Levels { timestamp, .. }
        | PredictionOrderBookUpdate::TickSize { timestamp, .. }
        | PredictionOrderBookUpdate::LastTrade { timestamp, .. } => *timestamp,
    }
}

fn levels(levels: &[PredictionBookLevel]) -> BTreeMap<Fraction, Quantity> {
    levels
        .iter()
        .filter(|level| !level.size.is_zero())
        .map(|level| (level.price, level.size))
        .collect()
}

fn apply_changes(side: &mut BTreeMap<Fraction, Quantity>, changes: &[PredictionBookLevelChange]) {
    for change in changes {
        if change.size.is_zero() {
            side.remove(&change.price);
        } else {
            side.insert(change.price, change.size);
        }
    }
}

fn missing_book_tick() -> Tick {
    Tick::from_inner(10_000_000_000_000_000).expect("a cent is a valid tick")
}

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

const MISSING_ORDER_BOOK_SENTENCE: &str = "No orderbook exists for the requested token id";

fn is_missing_order_book(error: &MarketError) -> bool {
    matches!(
        error,
        MarketError::Network(network)
            if matches!(
                network.error(),
                MarketParticipantError::Rejected { code, message, .. }
                    if code == "404" && message.contains(MISSING_ORDER_BOOK_SENTENCE)
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

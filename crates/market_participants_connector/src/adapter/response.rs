use std::pin::Pin;

use futures::Stream;
use market_participants_model::{
    decimal::{Price, Quantity, Ratio, UnixMillis},
    listing::{
        identifiers::VenueListingIdentifiers, listing_impl::ListingStatus, route::VenueListingKey,
        rules::TradingRules,
    },
    market::{
        book::MarketSequence,
        units::{MarketTradeId, PositionSide},
    },
    prediction::{
        PredictionEventView, PredictionEventsPage, PredictionOrderBook, PredictionOrderBookUpdate,
        PredictionSeries,
    },
    reference::PriceReferenceTerms,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::{
    catalog::{display::publication::StagedAssetCode, registry::asset::AssetTerms},
    connection::ConnectionSubscription,
    error::MarketParticipantError,
    market_participants::model::{
        NativeAccountTrade, NativeBalance, NativeHistoricalPosition,
        NativeHyperliquidHistoricalOrder, NativeHyperliquidLedgerEvent, NativeHyperliquidOpenOrder,
        NativeHyperliquidPerpetualState, NativeHyperliquidSpotPair, NativeHyperliquidSpotState,
        NativeHyperliquidUserFill, NativeHyperliquidWalletUpdate, NativeOrder,
        NativeOrderBookLevel, NativePosition,
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeListing<T> {
    pub listing: VenueListingKey,
    pub data: T,
}

impl<T> NativeListing<T> {
    pub const fn new(listing: VenueListingKey, data: T) -> Self {
        Self { listing, data }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeCandlestick {
    pub timestamp: UnixMillis,
    pub open: Price,
    pub high: Price,
    pub low: Price,
    pub close: Price,
    pub volume: Quantity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeTrade {
    pub id: MarketTradeId,
    pub price: Price,
    pub quantity: Quantity,
    pub side: market_participants_model::market::units::Side,
    pub timestamp: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeTicker {
    pub monotonic_id: Option<i64>,
    pub open: Price,
    pub high: Option<Price>,
    pub low: Option<Price>,
    pub close: Price,
    pub traded_quantity: Option<Quantity>,
    pub traded_notional: Option<Quantity>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeOrderBook {
    pub sequence: MarketSequence,
    pub timestamp: Option<UnixMillis>,
    pub bids: Vec<NativeOrderBookLevel>,
    pub asks: Vec<NativeOrderBookLevel>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeOrderBookUpdate {
    pub snapshot: bool,
    pub first_sequence: MarketSequence,
    pub last_sequence: MarketSequence,
    pub previous_sequence: Option<MarketSequence>,
    pub timestamp: Option<UnixMillis>,
    pub bids: Vec<NativeOrderBookLevel>,
    pub asks: Vec<NativeOrderBookLevel>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeOpenInterest {
    pub quantity: Quantity,
    pub notional: Option<Quantity>,
    pub timestamp: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeLongShortRatio {
    pub ratio: Ratio,
    pub timestamp: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeTakerVolume {
    pub buy: Quantity,
    pub sell: Quantity,
    pub timestamp: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeForceOrder {
    pub position_side: PositionSide,
    pub price: Price,
    pub quantity: Quantity,
    pub timestamp: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeCatalogEntry {
    pub listing: VenueListingKey,
    pub native_symbol: market_participants_model::identifier::NativeSymbol,
    pub aliases: Vec<market_participants_model::identifier::NativeSymbol>,
    pub identifiers: VenueListingIdentifiers,
    pub rules: TradingRules,
    pub status: ListingStatus,
    pub available_from: Option<UnixMillis>,
    pub available_until: Option<UnixMillis>,
    pub instrument: market_participants_model::instrument::Instrument,
    pub asset_terms: Vec<AssetTerms>,
    pub reference_terms: Vec<PriceReferenceTerms>,
    pub asset_codes: Vec<StagedAssetCode>,
}

#[derive(Debug)]
pub enum ProtocolResponse {
    Markets(Vec<NativeCatalogEntry>),
    Candlesticks(Vec<NativeListing<NativeCandlestick>>),
    Ticker(NativeListing<NativeTicker>),
    Tickers(Vec<NativeListing<NativeTicker>>),
    PredictionEvents(PredictionEventsPage),
    PredictionEvent(PredictionEventView),
    PredictionSeries(Vec<PredictionSeries>),
    PredictionOrderBook(PredictionOrderBook),
    OrderBook(NativeListing<NativeOrderBook>),
    Trades(Vec<NativeListing<NativeTrade>>),
    OrderPlaced(crate::account::shared::OrderId),
    OrderCancelled(crate::account::shared::OrderId),
    OpenInterest(Vec<NativeListing<NativeOpenInterest>>),
    LongShortRatios(Vec<NativeListing<NativeLongShortRatio>>),
    TakerVolumes(Vec<NativeListing<NativeTakerVolume>>),
    Orders(Vec<NativeOrder>),
    Positions(Vec<NativePosition>),
    AccountTrades(Vec<NativeAccountTrade>),
    HistoricalPositions(Vec<NativeHistoricalPosition>),
    Balances(Vec<NativeBalance>),
    TradingSettings(crate::account::shared::TradingSettings),
    HyperliquidSpotState(NativeHyperliquidSpotState),
    HyperliquidPerpetualState(NativeHyperliquidPerpetualState),
    HyperliquidSpotPairs(Vec<NativeHyperliquidSpotPair>),
    HyperliquidOpenOrders(Vec<NativeHyperliquidOpenOrder>),
    HyperliquidHistoricalOrders(Vec<NativeHyperliquidHistoricalOrder>),
    HyperliquidUserFills(Vec<NativeHyperliquidUserFill>),
    HyperliquidLedger(Vec<NativeHyperliquidLedgerEvent>),
}

pub enum ProtocolEvent {
    Candlestick(NativeListing<NativeCandlestick>),
    Ticker(NativeListing<NativeTicker>),
    OrderBook(NativeListing<NativeOrderBookUpdate>),
    Trade(NativeListing<NativeTrade>),
    Order(NativeOrder),
    Position(NativePosition),
    ForceOrder(NativeListing<NativeForceOrder>),
    PredictionSeries(Vec<PredictionSeries>),
    PredictionOrderBookUpdate(Vec<PredictionOrderBookUpdate>),
    HyperliquidWalletUpdate(NativeHyperliquidWalletUpdate),
}

pub type ProtocolStream =
    Pin<Box<dyn Stream<Item = Result<ProtocolEvent, MarketParticipantError>> + Send>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubscriptionStatus {
    Connecting,
    Active,
    Reconnecting,
    Closed,
    Failed(MarketParticipantError),
}

pub struct SubscriptionHandle {
    stream: Option<ProtocolStream>,
    cancellation: CancellationToken,
    connection_subscription: Option<ConnectionSubscription>,
    status: watch::Receiver<SubscriptionStatus>,
    status_sender: watch::Sender<SubscriptionStatus>,
}

impl std::fmt::Debug for SubscriptionHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubscriptionHandle")
            .finish_non_exhaustive()
    }
}

impl SubscriptionHandle {
    pub fn new(stream: ProtocolStream, cancellation: CancellationToken) -> Self {
        let (status_sender, status) = watch::channel(SubscriptionStatus::Active);
        Self {
            stream: Some(stream),
            cancellation,
            connection_subscription: None,
            status,
            status_sender,
        }
    }

    pub(crate) fn with_connection_subscription(
        stream: ProtocolStream,
        cancellation: CancellationToken,
        connection_subscription: ConnectionSubscription,
    ) -> Self {
        let mut handle = Self::new(stream, cancellation);
        handle.connection_subscription = Some(connection_subscription);
        handle
    }

    pub fn into_stream(mut self) -> ProtocolStream {
        let stream = self
            .stream
            .take()
            .unwrap_or_else(|| Box::pin(futures::stream::empty()));
        Box::pin(OwnedProtocolStream {
            inner: stream,
            cancellation: self.cancellation.clone(),
            connection_subscription: self.connection_subscription.take(),
            status_sender: self.status_sender.clone(),
        })
    }

    pub fn observe_status(&self) -> watch::Receiver<SubscriptionStatus> {
        self.status.clone()
    }

    #[cfg(test)]
    pub(crate) fn cancel(&mut self) {
        self.cancellation.cancel();
        if let Some(subscription) = self.connection_subscription.take() {
            subscription.cancel();
        }
        self.stream = None;
        self.status_sender.send_replace(SubscriptionStatus::Closed);
    }
}

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        if self.stream.is_some() {
            self.cancellation.cancel();
            if let Some(subscription) = &self.connection_subscription {
                subscription.cancel();
            }
        }
    }
}

struct OwnedProtocolStream {
    inner: ProtocolStream,
    cancellation: CancellationToken,
    connection_subscription: Option<ConnectionSubscription>,
    status_sender: watch::Sender<SubscriptionStatus>,
}

impl Stream for OwnedProtocolStream {
    type Item = Result<ProtocolEvent, MarketParticipantError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let result = self.inner.as_mut().poll_next(context);
        if matches!(result, std::task::Poll::Ready(None)) {
            self.cancellation.cancel();
            if let Some(subscription) = &self.connection_subscription {
                subscription.cancel();
            }
            self.status_sender.send_replace(SubscriptionStatus::Closed);
        }
        result
    }
}

impl Drop for OwnedProtocolStream {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(subscription) = &self.connection_subscription {
            subscription.cancel();
        }
        self.status_sender.send_replace(SubscriptionStatus::Closed);
    }
}

#[cfg(test)]
mod tests {
    use app_config::network::ResolvedProxy;
    use bytes::Bytes;
    use futures::{StreamExt, stream};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use url::Url;

    use super::{ProtocolStream, SubscriptionHandle};
    use crate::connection::{
        Connection, ConnectionKey, ConnectionState, PendingRequest, SubscriptionTopic,
        no_response_id, physical::ConnectionProtocol,
    };

    fn registered_handle(
        stream: ProtocolStream,
    ) -> (
        SubscriptionHandle,
        std::sync::Arc<Connection>,
        mpsc::UnboundedReceiver<PendingRequest>,
    ) {
        let (outbound, _outbound_receiver) = mpsc::channel(1);
        let (subscription_outbound, mut subscription_receiver) = mpsc::unbounded_channel();
        let connection = Connection::with_outbound(
            ConnectionKey::new(
                Url::parse("wss://example.test/ws").expect("test endpoint should parse"),
                ResolvedProxy::NoProxy,
                None,
                0,
                &ConnectionProtocol::new(no_response_id),
            ),
            ConnectionState::stateless(),
            outbound,
            subscription_outbound,
        );
        let subscription = connection
            .subscribe(
                SubscriptionTopic::from(Bytes::from_static(b"candles:1m")),
                Bytes::from_static(b"subscribe:1m"),
                Bytes::from_static(b"unsubscribe:1m"),
            )
            .expect("topic lease should be acquired");
        subscription_receiver
            .try_recv()
            .expect("subscribe should be enqueued");
        let handle = SubscriptionHandle::with_connection_subscription(
            stream,
            CancellationToken::new(),
            subscription,
        );
        (handle, connection, subscription_receiver)
    }

    #[test]
    fn explicit_cancel_releases_the_connection_subscription_once() {
        let (mut handle, connection, mut subscription_receiver) =
            registered_handle(Box::pin(stream::pending()));

        handle.cancel();
        handle.cancel();

        assert_eq!(
            subscription_receiver
                .try_recv()
                .expect("cancel should enqueue unsubscribe")
                .payload,
            Bytes::from_static(b"unsubscribe:1m")
        );
        assert!(subscription_receiver.try_recv().is_err());
        assert!(!connection.is_closed());
    }

    #[test]
    fn dropping_the_handle_releases_the_connection_subscription() {
        let (handle, connection, mut subscription_receiver) =
            registered_handle(Box::pin(stream::pending()));

        drop(handle);

        assert_eq!(
            subscription_receiver
                .try_recv()
                .expect("handle drop should enqueue unsubscribe")
                .payload,
            Bytes::from_static(b"unsubscribe:1m")
        );
        assert!(!connection.is_closed());
    }

    #[test]
    fn dropping_the_owned_stream_releases_the_connection_subscription() {
        let (handle, connection, mut subscription_receiver) =
            registered_handle(Box::pin(stream::pending()));
        let owned_stream = handle.into_stream();

        drop(owned_stream);

        assert_eq!(
            subscription_receiver
                .try_recv()
                .expect("stream drop should enqueue unsubscribe")
                .payload,
            Bytes::from_static(b"unsubscribe:1m")
        );
        assert!(!connection.is_closed());
    }

    #[tokio::test]
    async fn natural_stream_completion_releases_the_connection_subscription_once() {
        let (handle, connection, mut subscription_receiver) =
            registered_handle(Box::pin(stream::empty()));
        let mut owned_stream = handle.into_stream();

        assert!(owned_stream.next().await.is_none());
        drop(owned_stream);

        assert_eq!(
            subscription_receiver
                .try_recv()
                .expect("stream completion should enqueue unsubscribe")
                .payload,
            Bytes::from_static(b"unsubscribe:1m")
        );
        assert!(subscription_receiver.try_recv().is_err());
        assert!(!connection.is_closed());
    }
}

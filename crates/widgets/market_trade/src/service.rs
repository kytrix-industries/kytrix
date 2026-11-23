use std::{collections::BTreeMap, convert::Infallible, future::Future, sync::Arc};

use futures::StreamExt;
use market_participants_connector::market::{
    domain::DisconnectReason, error::MarketError, provider::MarketCapability, streams::TradeStream,
};
use market_participants_model::{
    decimal::UnixMillis,
    listing::{route::VenueListingKey, rules::ListingPrecision},
    market::{book::ListingTrade, units::MarketTradeId},
};
use tokio::time::Instant;
use widget::{
    protocol::WidgetProtocol,
    runtime::{ServiceContext, WidgetService},
    stream::StreamPublisher,
};

use super::model::{MAX_TRADES, MarketTradeSnapshot, MarketTradeState};

#[derive(Debug)]
pub(super) enum MarketTradeRequest {
    Load { listing: VenueListingKey },
    Retry { listing: VenueListingKey },
}

#[derive(Debug)]
pub(super) enum MarketTradeResponse {
    StreamStarted,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct MarketTradeStreamKey {
    listing: VenueListingKey,
}

impl MarketTradeStreamKey {
    pub(super) const fn new(listing: VenueListingKey) -> Self {
        Self { listing }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum MarketTradeStatus {
    Connecting,
    LoadingHistory,
    Ready,
    Error(MarketError),
}

#[derive(Clone, Debug)]
pub(super) enum MarketTradeStreamMessage {
    Status(MarketTradeStatus),
    Snapshot(MarketTradeSnapshot),
}

pub(super) struct MarketTradeProtocol;

impl WidgetProtocol for MarketTradeProtocol {
    type Request = MarketTradeRequest;
    type Response = MarketTradeResponse;
    type StreamKey = MarketTradeStreamKey;
    type StreamMessage = MarketTradeStreamMessage;
    type Error = Infallible;
}

pub(super) trait MarketTradeSource: Clone + Send + Sync + 'static {
    type Subscription: MarketTradeEventSubscription;

    fn fetch_market_trade(
        &self,
        listing: VenueListingKey,
        limit: u16,
    ) -> impl Future<Output = Result<Vec<ListingTrade>, MarketError>> + Send;

    fn subscribe_market_trade(
        &self,
        listing: VenueListingKey,
    ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send;
}

pub(super) trait MarketTradeEventSubscription: Send + 'static {
    fn precision(&self) -> ListingPrecision;

    fn recv(
        &mut self,
    ) -> impl Future<Output = Option<Result<Arc<ListingTrade>, MarketError>>> + Send;
}

impl MarketTradeSource for MarketCapability {
    type Subscription = ClientMarketTradeSubscription;

    fn fetch_market_trade(
        &self,
        listing: VenueListingKey,
        limit: u16,
    ) -> impl Future<Output = Result<Vec<ListingTrade>, MarketError>> + Send {
        let market = self.clone();
        async move {
            let route = listing.route.clone();
            market
                .market(route)
                .trades(listing)
                .limit(limit)
                .fetch()
                .await
        }
    }

    fn subscribe_market_trade(
        &self,
        listing: VenueListingKey,
    ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send {
        let market = self.clone();
        async move {
            let resolved = market.resolve_listing(&listing)?;
            let route = resolved.key.route.clone();
            let inner = market.market(route).trades(listing).subscribe().await?;
            Ok(ClientMarketTradeSubscription {
                inner,
                precision: ListingPrecision::of(&resolved.rules),
            })
        }
    }
}

pub(super) struct ClientMarketTradeSubscription {
    inner: TradeStream,
    precision: ListingPrecision,
}

impl MarketTradeEventSubscription for ClientMarketTradeSubscription {
    fn precision(&self) -> ListingPrecision {
        self.precision
    }

    async fn recv(&mut self) -> Option<Result<Arc<ListingTrade>, MarketError>> {
        self.inner.next().await.map(|event| event.map(Arc::new))
    }
}

pub(super) struct MarketTradeService<S = MarketCapability> {
    source: S,
    history_limit: u16,
    maximum_trades: usize,
}

impl<S> MarketTradeService<S> {
    pub(super) fn new(source: S, history_limit: u16, maximum_trades: usize) -> Self {
        Self {
            source,
            history_limit: history_limit.max(1),
            maximum_trades: maximum_trades.clamp(1, MAX_TRADES),
        }
    }
}

impl<S> WidgetService<MarketTradeProtocol> for MarketTradeService<S>
where
    S: MarketTradeSource,
{
    async fn run(self, mut context: ServiceContext<MarketTradeProtocol>) {
        let mut active_key: Option<MarketTradeStreamKey> = None;

        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            let (listing, retry) = match request {
                MarketTradeRequest::Load { listing } => (listing, false),
                MarketTradeRequest::Retry { listing } => (listing, true),
            };
            let key = MarketTradeStreamKey::new(listing.clone());
            if !retry && active_key.as_ref() == Some(&key) {
                let _ = reply.respond(Ok(MarketTradeResponse::StreamStarted));
                continue;
            }
            if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                continue;
            }
            if active_key.as_ref() != Some(&key)
                && let Some(previous) = active_key.take()
            {
                context.cancel_stream(&previous);
            }

            let source = self.source.clone();
            let spec = MarketTradeSpec {
                listing,
                history_limit: self.history_limit,
                maximum_trades: self.maximum_trades,
            };
            if retry {
                context.replace_stream(key.clone(), move |publisher| async move {
                    run_stream(source, spec, publisher).await;
                });
            } else {
                context.start_stream(key.clone(), move |publisher| async move {
                    run_stream(source, spec, publisher).await;
                });
            }
            active_key = Some(key);
            if reply
                .respond(Ok(MarketTradeResponse::StreamStarted))
                .is_err()
                && let Some(previous) = active_key.take()
            {
                context.cancel_stream(&previous);
            }
        }
    }
}

#[derive(Clone)]
struct MarketTradeSpec {
    listing: VenueListingKey,
    history_limit: u16,
    maximum_trades: usize,
}

async fn run_stream<S>(
    source: S,
    spec: MarketTradeSpec,
    publisher: StreamPublisher<MarketTradeProtocol>,
) where
    S: MarketTradeSource,
{
    if !publish_status(&publisher, MarketTradeStatus::Connecting).await {
        return;
    }
    if let Err(error) = run_stream_once(&source, &spec, &publisher).await
        && publish_status(&publisher, MarketTradeStatus::Error(error)).await
    {
        publisher.cancelled().await;
    }
}

async fn run_stream_once<S>(
    source: &S,
    spec: &MarketTradeSpec,
    publisher: &StreamPublisher<MarketTradeProtocol>,
) -> Result<(), MarketError>
where
    S: MarketTradeSource,
{
    let subscription = source.subscribe_market_trade(spec.listing.clone());
    tokio::pin!(subscription);
    let mut subscription = tokio::select! {
        biased;
        () = publisher.cancelled() => return Ok(()),
        result = &mut subscription => result?,
    };

    if !publish_status(publisher, MarketTradeStatus::LoadingHistory).await {
        return Ok(());
    }

    let (history, buffered) = fetch_while_buffering(
        source,
        spec.listing.clone(),
        spec.history_limit,
        &mut subscription,
        spec.maximum_trades,
        publisher,
    )
    .await?;

    let mut trades = MarketTradeState::new(spec.maximum_trades, subscription.precision());
    trades.merge(buffered.into_values().map(|trade| trade.as_ref().clone()));
    trades.merge(history);

    if !publish_snapshot(publisher, trades.snapshot()) {
        return Ok(());
    }
    if !publish_status(publisher, MarketTradeStatus::Ready).await {
        return Ok(());
    }

    loop {
        tokio::select! {
            biased;
            () = publisher.cancelled() => return Ok(()),
            event = subscription.recv() => {
                let Some(event) = event else {
                    return Err(MarketError::Disconnected(DisconnectReason::Normal));
                };
                let trade = event?;
                validate_trade(&trade)?;
                if !trades.insert(&trade) {
                    continue;
                }
                if !publish_snapshot(publisher, trades.snapshot()) {
                    return Ok(());
                }
            }
        }
    }
}

async fn fetch_while_buffering<S>(
    source: &S,
    listing: VenueListingKey,
    limit: u16,
    subscription: &mut S::Subscription,
    maximum_trades: usize,
    publisher: &StreamPublisher<MarketTradeProtocol>,
) -> Result<
    (
        Vec<ListingTrade>,
        BTreeMap<(UnixMillis, MarketTradeId), Arc<ListingTrade>>,
    ),
    MarketError,
>
where
    S: MarketTradeSource,
{
    let fetch = source.fetch_market_trade(listing, limit);
    tokio::pin!(fetch);
    let mut buffered = BTreeMap::new();

    loop {
        tokio::select! {
            biased;
            () = publisher.cancelled() => return Err(MarketError::ServiceClosed),
            result = &mut fetch => {
                let history = result?;
                for trade in &history {
                    validate_trade(trade)?;
                }
                return Ok((history, buffered));
            }
            event = subscription.recv() => {
                let Some(event) = event else {
                    return Err(MarketError::Disconnected(DisconnectReason::Normal));
                };
                let trade = event?;
                validate_trade(&trade)?;
                buffer_trade(&mut buffered, trade, maximum_trades);            }
        }
    }
}

fn buffer_trade(
    buffered: &mut BTreeMap<(UnixMillis, MarketTradeId), Arc<ListingTrade>>,
    trade: Arc<ListingTrade>,
    maximum_trades: usize,
) {
    buffered.insert((trade.timestamp, trade.id.clone()), trade);
    while buffered.len() > maximum_trades.max(1) {
        buffered.pop_first();
    }
}

fn validate_trade(trade: &ListingTrade) -> Result<(), MarketError> {
    trade.validate().map_err(MarketError::from)
}

async fn publish_status(
    publisher: &StreamPublisher<MarketTradeProtocol>,
    status: MarketTradeStatus,
) -> bool {
    publisher
        .publish_reliable(MarketTradeStreamMessage::Status(status))
        .await
        .is_ok()
}

fn publish_snapshot(
    publisher: &StreamPublisher<MarketTradeProtocol>,
    snapshot: MarketTradeSnapshot,
) -> bool {
    publisher
        .publish_latest(MarketTradeStreamMessage::Snapshot(snapshot))
        .is_ok()
}

#[cfg(test)]
mod tests {

    #[allow(
        clippy::disallowed_methods,
        reason = "a test drives its subject on its own runtime so the paused clock it advances is the one the subject reads"
    )]
    fn current_test_handle() -> &'static tokio::runtime::Handle {
        Box::leak(Box::new(tokio::runtime::Handle::current()))
    }
    use std::{
        collections::VecDeque,
        num::NonZeroUsize,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use market_participants_connector::catalog::{error::SourceError, test_support};
    use market_participants_model::{
        asset::AssetId,
        decimal::{Price, Quantity, Tick},
        listing::{route::VenueListingKey, venue::PriceBasis},
        market::{
            price::MarketPrice,
            units::{MarketQuantity, QuantityUnit, Side},
        },
    };
    use smol_str::SmolStr;
    use tokio::sync::{Notify, mpsc, oneshot};
    use widget::runtime::{WidgetRuntime, WidgetRuntimeConfig};

    use super::*;

    fn listing() -> VenueListingKey {
        test_support::listing_key(&test_support::spot_route(), "BTCUSDT")
    }

    fn other_listing() -> VenueListingKey {
        test_support::listing_key(&test_support::spot_route(), "ETHUSDT")
    }

    fn quote() -> AssetId {
        test_support::currency("USD")
    }

    fn trade(id: u64, timestamp: UnixMillis, price: u64) -> ListingTrade {
        ListingTrade {
            listing: listing(),
            id: MarketTradeId::Numeric(id),
            price: MarketPrice {
                value: Price::from(price),
                quote_asset: quote(),
                basis: PriceBasis::PerAssetUnit,
            },
            quantity: MarketQuantity {
                value: Quantity::from(1_u64),
                unit: QuantityUnit::AssetUnits {
                    asset: test_support::token("binance", "BTC"),
                },
            },
            side: Side::Buy,
            timestamp,
        }
    }

    type FakeEvent = Result<Arc<ListingTrade>, MarketError>;
    type SetupReceiver = oneshot::Receiver<Result<FakeSubscription, MarketError>>;

    fn precision() -> ListingPrecision {
        ListingPrecision {
            price_tick: "0.01".parse().expect("a price tick should parse"),
            quantity_tick: "0.001".parse().expect("a quantity tick should parse"),
        }
    }

    #[derive(Debug)]
    struct FakeSubscription {
        events: mpsc::Receiver<FakeEvent>,
        events_received: Arc<Notify>,
        dropped: Option<oneshot::Sender<()>>,
    }

    impl Drop for FakeSubscription {
        fn drop(&mut self) {
            if let Some(dropped) = self.dropped.take() {
                let _ = dropped.send(());
            }
        }
    }

    impl MarketTradeEventSubscription for FakeSubscription {
        fn precision(&self) -> ListingPrecision {
            precision()
        }

        async fn recv(&mut self) -> Option<FakeEvent> {
            let event = self.events.recv().await;
            if event.is_some() {
                self.events_received.notify_one();
            }
            event
        }
    }

    struct Fixture {
        subscription: FakeSubscription,
        events: mpsc::Sender<FakeEvent>,
        events_received: Arc<Notify>,
        dropped: oneshot::Receiver<()>,
    }

    fn subscription() -> Fixture {
        let (events, receiver) = mpsc::channel(8);
        let (dropped, drop_receiver) = oneshot::channel();
        let events_received = Arc::new(Notify::new());
        Fixture {
            subscription: FakeSubscription {
                events: receiver,
                events_received: events_received.clone(),
                dropped: Some(dropped),
            },
            events,
            events_received,
            dropped: drop_receiver,
        }
    }

    fn setup() -> (
        oneshot::Sender<Result<FakeSubscription, MarketError>>,
        SetupReceiver,
    ) {
        oneshot::channel()
    }

    fn page() -> (
        oneshot::Sender<Vec<ListingTrade>>,
        oneshot::Receiver<Vec<ListingTrade>>,
    ) {
        oneshot::channel()
    }

    #[derive(Clone)]
    struct FakeSource {
        setups: Arc<Mutex<VecDeque<SetupReceiver>>>,
        pages: Arc<Mutex<VecDeque<oneshot::Receiver<Vec<ListingTrade>>>>>,
        fetch_started: Arc<Notify>,
        attempts: Arc<AtomicUsize>,
    }

    impl MarketTradeSource for FakeSource {
        type Subscription = FakeSubscription;

        async fn fetch_market_trade(
            &self,
            _listing: VenueListingKey,
            _limit: u16,
        ) -> Result<Vec<ListingTrade>, MarketError> {
            let page = self
                .pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .ok_or(MarketError::ServiceClosed)?;
            self.fetch_started.notify_one();
            page.await.map_err(|_| MarketError::ServiceClosed)
        }

        async fn subscribe_market_trade(
            &self,
            _listing: VenueListingKey,
        ) -> Result<Self::Subscription, MarketError> {
            self.attempts.fetch_add(1, Ordering::AcqRel);
            let setup = self
                .setups
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .ok_or(MarketError::ServiceClosed)?;
            setup.await.map_err(|_| MarketError::ServiceClosed)?
        }
    }

    #[derive(Clone)]
    struct ResolvingSource {
        tick: Arc<Mutex<Tick>>,
        subscriptions: Arc<AtomicUsize>,
        senders: Arc<Mutex<Vec<mpsc::Sender<FakeEvent>>>>,
    }

    impl ResolvingSource {
        fn publish_at(&self, price_tick: &str) {
            *self
                .tick
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = price_tick
                .parse()
                .expect("the restated price tick should parse");
        }

        fn subscriptions(&self) -> usize {
            self.subscriptions.load(Ordering::Acquire)
        }
    }

    struct ResolvingSubscription {
        events: mpsc::Receiver<FakeEvent>,
        precision: ListingPrecision,
    }

    impl MarketTradeEventSubscription for ResolvingSubscription {
        fn precision(&self) -> ListingPrecision {
            self.precision
        }

        async fn recv(&mut self) -> Option<FakeEvent> {
            self.events.recv().await
        }
    }

    impl MarketTradeSource for ResolvingSource {
        type Subscription = ResolvingSubscription;

        async fn fetch_market_trade(
            &self,
            _listing: VenueListingKey,
            _limit: u16,
        ) -> Result<Vec<ListingTrade>, MarketError> {
            Ok(vec![trade(1, 1, 100)])
        }

        async fn subscribe_market_trade(
            &self,
            _listing: VenueListingKey,
        ) -> Result<Self::Subscription, MarketError> {
            self.subscriptions.fetch_add(1, Ordering::AcqRel);
            let (sender, events) = mpsc::channel(8);
            self.senders
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(sender);
            Ok(ResolvingSubscription {
                events,
                precision: ListingPrecision {
                    price_tick: *self
                        .tick
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                    quantity_tick: precision().quantity_tick,
                },
            })
        }
    }

    fn published_prices(
        envelope: &widget::protocol::StreamEnvelope<MarketTradeStreamKey, MarketTradeStreamMessage>,
    ) -> Vec<String> {
        let MarketTradeStreamMessage::Snapshot(snapshot) = envelope.message().as_ref() else {
            panic!("the latest stream should carry a snapshot");
        };
        snapshot
            .rows()
            .iter()
            .map(|row| row.display(time::UtcOffset::UTC).price.to_string())
            .collect()
    }

    fn source(
        setups: impl IntoIterator<Item = SetupReceiver>,
        pages: impl IntoIterator<Item = oneshot::Receiver<Vec<ListingTrade>>>,
    ) -> FakeSource {
        FakeSource {
            setups: Arc::new(Mutex::new(setups.into_iter().collect())),
            pages: Arc::new(Mutex::new(pages.into_iter().collect())),
            fetch_started: Arc::new(Notify::new()),
            attempts: Arc::new(AtomicUsize::new(0)),
        }
    }

    async fn wait_for_attempts(source: &FakeSource, expected: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while source.attempts.load(Ordering::Acquire) < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("subscription attempts should start");
    }

    async fn receive_status(
        reliable: &mut widget::stream::ReliableStream<MarketTradeProtocol>,
        expected: MarketTradeStatus,
    ) {
        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(1), reliable.recv())
                .await
                .expect("status should arrive")
                .expect("reliable stream should remain open");
            if let MarketTradeStreamMessage::Status(status) = envelope.message().as_ref()
                && status == &expected
            {
                return;
            }
        }
    }

    fn snapshot_of(
        envelope: &widget::protocol::StreamEnvelope<MarketTradeStreamKey, MarketTradeStreamMessage>,
    ) -> Vec<(MarketTradeId, Price)> {
        let MarketTradeStreamMessage::Snapshot(snapshot) = envelope.message().as_ref() else {
            panic!("latest value should be a market-trade snapshot");
        };
        snapshot
            .rows()
            .iter()
            .map(|row| (row.trade().id.clone(), row.trade().price.value))
            .collect()
    }

    fn expected_rows(rows: &[(u64, u64)]) -> Vec<(MarketTradeId, Price)> {
        rows.iter()
            .map(|&(id, price)| (MarketTradeId::Numeric(id), Price::from(price)))
            .collect()
    }

    #[tokio::test]
    async fn loading_status_should_follow_subscription_setup() {
        let Fixture {
            subscription,
            dropped: _dropped,
            ..
        } = subscription();
        let (setup_sender, setup) = setup();
        setup_sender
            .send(Ok(subscription))
            .expect("first subscription setup should be waiting");
        let (_page_sender, page) = page();
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            MarketTradeService::new(source([setup], [page]), 200, MAX_TRADES),
        );
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("reliable stream should be available");

        runtime
            .client()
            .call(MarketTradeRequest::Load { listing: listing() })
            .await
            .expect("load request should start the stream");

        receive_status(&mut reliable, MarketTradeStatus::Connecting).await;
        receive_status(&mut reliable, MarketTradeStatus::LoadingHistory).await;
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn initialization_should_merge_buffered_realtime_with_the_history_page() {
        let Fixture {
            subscription,
            events,
            events_received,
            dropped: _dropped,
        } = subscription();
        let (setup_sender, setup) = setup();
        setup_sender
            .send(Ok(subscription))
            .expect("subscription setup should be waiting");
        let (page_sender, page) = page();
        let source = source([setup], [page]);
        let fetch_started = source.fetch_started.clone();
        let runtime = WidgetRuntime::spawn(
            current_test_handle(),
            MarketTradeService::new(source, 200, MAX_TRADES),
        );
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(MarketTradeRequest::Load { listing: listing() })
            .await
            .expect("load request should start the stream");
        fetch_started.notified().await;
        events
            .send(Ok(Arc::new(trade(201, 25, 25))))
            .await
            .expect("fake subscription should be open");
        events_received.notified().await;
        page_sender
            .send(vec![
                trade(201, 20, 20),
                trade(199, 19, 19),
                trade(200, 20, 20),
            ])
            .expect("the page request should be waiting");

        latest
            .changed()
            .await
            .expect("merged snapshot should be published");
        let envelope = latest.next_unseen().expect("snapshot should be unseen");

        assert_eq!(
            snapshot_of(&envelope),
            expected_rows(&[(199, 19), (200, 20), (201, 25)])
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn initialization_should_keep_the_newest_trades_when_the_buffer_overflows() {
        let Fixture {
            subscription,
            events,
            events_received,
            dropped: _dropped,
        } = subscription();
        let (setup_sender, setup) = setup();
        setup_sender
            .send(Ok(subscription))
            .expect("subscription setup should be waiting");
        let (page_sender, page) = page();
        let source = source([setup], [page]);
        let fetch_started = source.fetch_started.clone();
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            MarketTradeService::new(source, 200, 2),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(MarketTradeRequest::Load { listing: listing() })
            .await
            .unwrap();
        fetch_started.notified().await;
        for id in 1..=5_u64 {
            events
                .send(Ok(Arc::new(trade(id, id as i64, id))))
                .await
                .expect("fake subscription should be open");
            events_received.notified().await;
        }
        page_sender
            .send(vec![trade(0, 0, 0)])
            .expect("the page request should be waiting");

        latest.changed().await.unwrap();
        let envelope = latest.next_unseen().expect("snapshot should be unseen");
        assert_eq!(snapshot_of(&envelope), expected_rows(&[(4, 4), (5, 5)]));
        loop {
            let status = reliable.recv().await.expect("a status should be published");
            match status.message().as_ref() {
                MarketTradeStreamMessage::Status(MarketTradeStatus::Ready) => break,
                MarketTradeStreamMessage::Status(MarketTradeStatus::Error(error)) => {
                    panic!("a buffered overflow must not fail the stream: {error:?}");
                }
                _ => {}
            }
        }
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn events_should_publish_as_they_arrive() {
        let Fixture {
            subscription,
            events,
            events_received,
            dropped: _dropped,
        } = subscription();
        let (setup_sender, setup) = setup();
        setup_sender
            .send(Ok(subscription))
            .expect("subscription setup should be waiting");
        let (page_sender, page) = page();
        page_sender
            .send(vec![trade(1, 1, 1)])
            .expect("the page request should be waiting");
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            MarketTradeService::new(source([setup], [page]), 200, MAX_TRADES),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(MarketTradeRequest::Load { listing: listing() })
            .await
            .unwrap();
        receive_status(&mut reliable, MarketTradeStatus::Ready).await;
        while latest.next_unseen().is_some() {}

        for (id, expected) in [
            (2_u64, expected_rows(&[(1, 1), (2, 2)])),
            (3, expected_rows(&[(1, 1), (2, 2), (3, 3)])),
        ] {
            events
                .send(Ok(Arc::new(trade(id, id as i64, id))))
                .await
                .expect("fake subscription should be open");
            events_received.notified().await;
            tokio::time::timeout(Duration::from_secs(1), latest.changed())
                .await
                .expect("a trade should publish as it arrives")
                .unwrap();
            let envelope = latest.next_unseen().expect("snapshot should be unseen");
            assert_eq!(snapshot_of(&envelope), expected);
        }
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn events_that_cannot_change_retained_rows_should_not_publish_a_snapshot() {
        let Fixture {
            subscription,
            events,
            events_received,
            dropped: _dropped,
        } = subscription();
        let (setup_sender, setup) = setup();
        setup_sender
            .send(Ok(subscription))
            .expect("subscription setup should be waiting");
        let (page_sender, page) = page();
        page_sender
            .send(vec![trade(10, 10, 10), trade(11, 11, 11)])
            .expect("the page request should be waiting");
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            MarketTradeService::new(source([setup], [page]), 200, 2),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(MarketTradeRequest::Load { listing: listing() })
            .await
            .unwrap();
        receive_status(&mut reliable, MarketTradeStatus::Ready).await;
        while latest.next_unseen().is_some() {}

        for duplicate in [trade(11, 999, 999), trade(9, 9, 9)] {
            events
                .send(Ok(Arc::new(duplicate)))
                .await
                .expect("the trade should reach the service");
            events_received.notified().await;
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), latest.changed())
                .await
                .is_err(),
            "a trade that changes nothing should not publish a snapshot"
        );

        events
            .send(Ok(Arc::new(trade(12, 12, 12))))
            .await
            .expect("the trade should reach the service");
        events_received.notified().await;
        tokio::time::timeout(Duration::from_secs(1), latest.changed())
            .await
            .expect("the stream should still publish what does change")
            .unwrap();
        let envelope = latest.next_unseen().expect("snapshot should be unseen");

        assert_eq!(snapshot_of(&envelope), expected_rows(&[(11, 11), (12, 12)]));
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_closed_subscription_should_be_terminal_without_retrying() {
        let Fixture {
            subscription,
            events,
            dropped: _dropped,
            ..
        } = subscription();
        let (setup_sender, setup) = setup();
        setup_sender
            .send(Ok(subscription))
            .expect("subscription setup should be waiting");
        let (page_sender, page) = page();
        page_sender
            .send(Vec::new())
            .expect("the page request should be waiting");
        let source = source([setup], [page]);
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            MarketTradeService::new(source.clone(), 200, MAX_TRADES),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();

        runtime
            .client()
            .call(MarketTradeRequest::Load { listing: listing() })
            .await
            .unwrap();
        receive_status(&mut reliable, MarketTradeStatus::Ready).await;

        drop(events);
        receive_status(
            &mut reliable,
            MarketTradeStatus::Error(MarketError::Disconnected(DisconnectReason::Normal)),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(source.attempts.load(Ordering::Acquire), 1);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn loading_a_new_listing_should_cancel_a_pending_subscription_request() {
        let Fixture {
            subscription,
            dropped: _dropped,
            ..
        } = subscription();
        let (mut first_setup_sender, first_setup) = setup();
        let (second_setup_sender, second_setup) = setup();
        second_setup_sender
            .send(Ok(subscription))
            .expect("the replacement subscription setup should be waiting");
        let (_page_sender, page) = page();
        let source = source([first_setup, second_setup], [page]);
        let runtime = WidgetRuntime::spawn(
            current_test_handle(),
            MarketTradeService::new(source.clone(), 200, MAX_TRADES),
        );
        let client = runtime.client();

        client
            .call(MarketTradeRequest::Load { listing: listing() })
            .await
            .expect("the first load should start a stream");
        wait_for_attempts(&source, 1).await;
        client
            .call(MarketTradeRequest::Load {
                listing: other_listing(),
            })
            .await
            .expect("the replacement stream should start");

        tokio::time::timeout(Duration::from_secs(1), first_setup_sender.closed())
            .await
            .expect("a pending subscription should be cancelled promptly");
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn two_listings_should_keep_their_own_streams() {
        let Fixture {
            subscription: first_subscription,
            events: first_events,
            dropped: _first_dropped,
            ..
        } = subscription();
        let Fixture {
            subscription: second_subscription,
            dropped: _second_dropped,
            ..
        } = subscription();
        let (first_setup_sender, first_setup) = setup();
        let (second_setup_sender, second_setup) = setup();
        first_setup_sender.send(Ok(first_subscription)).unwrap();
        second_setup_sender.send(Ok(second_subscription)).unwrap();
        let (first_page_sender, first_page) = page();
        let (second_page_sender, second_page) = page();
        first_page_sender.send(vec![trade(1, 1, 1)]).unwrap();
        second_page_sender.send(vec![trade(2, 2, 2)]).unwrap();
        let source = source([first_setup, second_setup], [first_page, second_page]);
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            MarketTradeService::new(source.clone(), 200, MAX_TRADES),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();
        let client = runtime.client();

        client
            .call(MarketTradeRequest::Load { listing: listing() })
            .await
            .unwrap();
        receive_status(&mut reliable, MarketTradeStatus::Ready).await;
        client
            .call(MarketTradeRequest::Load {
                listing: other_listing(),
            })
            .await
            .unwrap();
        assert_eq!(
            source.attempts.load(Ordering::Acquire),
            2,
            "the second listing is its own scope, not a reload of the first"
        );

        assert!(
            first_events
                .send(Ok(Arc::new(trade(3, 3, 3))))
                .await
                .is_err()
        );
        latest.changed().await.unwrap();
        let envelope = latest.next_unseen().expect("snapshot should be unseen");
        assert_eq!(envelope.key().listing, other_listing());
        assert_eq!(snapshot_of(&envelope), expected_rows(&[(2, 2)]));
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn retry_should_replace_the_stream_and_suppress_late_events() {
        let Fixture {
            subscription: first_subscription,
            events: first_events,
            dropped: first_dropped,
            ..
        } = subscription();
        let Fixture {
            subscription: second_subscription,
            events: second_events,
            events_received: second_events_received,
            dropped: _second_dropped,
        } = subscription();
        let (first_setup_sender, first_setup) = setup();
        let (second_setup_sender, second_setup) = setup();
        first_setup_sender.send(Ok(first_subscription)).unwrap();
        second_setup_sender.send(Ok(second_subscription)).unwrap();
        let (first_page_sender, first_page) = page();
        let (second_page_sender, second_page) = page();
        first_page_sender.send(vec![trade(1, 1, 1)]).unwrap();
        second_page_sender.send(vec![trade(2, 2, 2)]).unwrap();
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            MarketTradeService::new(
                source([first_setup, second_setup], [first_page, second_page]),
                200,
                MAX_TRADES,
            ),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(MarketTradeRequest::Load { listing: listing() })
            .await
            .unwrap();
        receive_status(&mut reliable, MarketTradeStatus::Ready).await;
        latest.changed().await.unwrap();
        let _ = latest.next_unseen();

        runtime
            .client()
            .call(MarketTradeRequest::Retry { listing: listing() })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), first_dropped)
            .await
            .expect("the replaced subscription should be dropped")
            .unwrap();
        assert!(
            first_events
                .send(Ok(Arc::new(trade(3, 3, 3))))
                .await
                .is_err()
        );
        receive_status(&mut reliable, MarketTradeStatus::Ready).await;
        latest.changed().await.unwrap();
        let envelope = latest.next_unseen().expect("snapshot should be unseen");
        assert_eq!(snapshot_of(&envelope), expected_rows(&[(2, 2)]));

        second_events
            .send(Ok(Arc::new(trade(4, 4, 4))))
            .await
            .unwrap();
        second_events_received.notified().await;
        tokio::time::timeout(Duration::from_secs(1), latest.changed())
            .await
            .expect("the replacement stream should publish")
            .unwrap();
        let envelope = latest.next_unseen().expect("snapshot should be unseen");

        assert_eq!(snapshot_of(&envelope), expected_rows(&[(2, 2), (4, 4)]));
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn retry_should_resolve_the_listing_again_while_a_load_keeps_the_live_stream() {
        let source = ResolvingSource {
            tick: Arc::new(Mutex::new(
                "0.01".parse().expect("a price tick should parse"),
            )),
            subscriptions: Arc::new(AtomicUsize::new(0)),
            senders: Arc::new(Mutex::new(Vec::new())),
        };
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            MarketTradeService::new(source.clone(), 200, MAX_TRADES),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(MarketTradeRequest::Load { listing: listing() })
            .await
            .unwrap();
        receive_status(&mut reliable, MarketTradeStatus::Ready).await;
        latest.changed().await.unwrap();
        let envelope = latest.next_unseen().expect("snapshot should be unseen");
        assert_eq!(published_prices(&envelope), vec!["100.00"]);
        assert_eq!(source.subscriptions(), 1);

        source.publish_at("0.5");
        runtime
            .client()
            .call(MarketTradeRequest::Load { listing: listing() })
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert_eq!(source.subscriptions(), 1);

        runtime
            .client()
            .call(MarketTradeRequest::Retry { listing: listing() })
            .await
            .unwrap();
        receive_status(&mut reliable, MarketTradeStatus::Ready).await;
        latest.changed().await.unwrap();
        let envelope = latest.next_unseen().expect("snapshot should be unseen");
        assert_eq!(published_prices(&envelope), vec!["100.0"]);
        assert_eq!(source.subscriptions(), 2);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_should_cancel_a_stream_blocked_by_reliable_backpressure() {
        let Fixture {
            subscription,
            dropped: _dropped,
            ..
        } = subscription();
        let (setup_sender, setup) = setup();
        setup_sender.send(Ok(subscription)).unwrap();
        let (_page_sender, page) = page();
        let source = source([setup], [page]);
        let config = WidgetRuntimeConfig::new(
            NonZeroUsize::new(1).unwrap(),
            NonZeroUsize::new(1).unwrap(),
            Duration::from_secs(1),
        );
        let runtime = WidgetRuntime::spawn_with_config(
            current_test_handle(),
            MarketTradeService::new(source.clone(), 200, MAX_TRADES),
            config,
        );
        runtime
            .client()
            .call(MarketTradeRequest::Load { listing: listing() })
            .await
            .unwrap();
        wait_for_attempts(&source, 1).await;

        tokio::time::timeout(Duration::from_secs(1), runtime.shutdown())
            .await
            .expect("shutdown should cancel the blocked reliable publish");
    }

    #[tokio::test]
    async fn a_trade_that_is_not_a_traded_quantity_should_fail_the_stream() {
        let Fixture {
            subscription,
            events,
            dropped: _dropped,
            ..
        } = subscription();
        let (setup_sender, setup) = setup();
        setup_sender
            .send(Ok(subscription))
            .expect("subscription setup should be waiting");
        let (page_sender, page) = page();
        page_sender
            .send(vec![trade(1, 1, 1)])
            .expect("the page request should be waiting");
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            MarketTradeService::new(source([setup], [page]), 200, MAX_TRADES),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(MarketTradeRequest::Load { listing: listing() })
            .await
            .unwrap();
        receive_status(&mut reliable, MarketTradeStatus::Ready).await;
        while latest.next_unseen().is_some() {}

        let mut amount = trade(2, 2, 2);
        amount.quantity = MarketQuantity {
            value: Quantity::from(2_u64),
            unit: QuantityUnit::Notional {
                asset: test_support::currency("USD"),
            },
        };
        events
            .send(Ok(Arc::new(amount)))
            .await
            .expect("the trade should reach the service");

        let reported = loop {
            let envelope = reliable.recv().await.expect("a status should arrive");
            if let MarketTradeStreamMessage::Status(MarketTradeStatus::Error(error)) =
                envelope.message().as_ref()
            {
                break error.clone();
            }
        };

        assert!(matches!(
            reported,
            MarketError::Model(boxed)
                if matches!(
                    *boxed,
                    market_participants_model::error::ModelError::QuantityUnitMismatch { .. }
                )
        ));
        assert!(
            latest.next_unseen().is_none(),
            "a trade the tape cannot measure is never drawn"
        );
        runtime.shutdown().await;
    }

    #[derive(Clone)]
    struct UnresolvableSource {
        failure: MarketError,
    }

    impl MarketTradeSource for UnresolvableSource {
        type Subscription = FakeSubscription;

        async fn fetch_market_trade(
            &self,
            _listing: VenueListingKey,
            _limit: u16,
        ) -> Result<Vec<ListingTrade>, MarketError> {
            std::future::pending().await
        }

        async fn subscribe_market_trade(
            &self,
            _listing: VenueListingKey,
        ) -> Result<Self::Subscription, MarketError> {
            Err(self.failure.clone())
        }
    }

    #[tokio::test]
    async fn a_catalog_failure_on_the_subscription_seam_should_be_reported_as_itself() {
        let failure = MarketError::CatalogSource(SourceError::Network {
            reason: SmolStr::new_static("the test catalog is unreachable"),
        });
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            MarketTradeService::new(
                UnresolvableSource {
                    failure: failure.clone(),
                },
                200,
                MAX_TRADES,
            ),
        );
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("reliable stream should be available");
        runtime
            .client()
            .call(MarketTradeRequest::Load { listing: listing() })
            .await
            .expect("load request should start a stream");

        let reported = loop {
            let envelope = tokio::time::timeout(Duration::from_secs(1), reliable.recv())
                .await
                .expect("the failure should be published")
                .expect("the reliable stream should remain open");
            if let MarketTradeStreamMessage::Status(MarketTradeStatus::Error(error)) =
                envelope.message().as_ref()
            {
                break error.clone();
            }
        };

        assert_eq!(reported, failure);
        runtime.shutdown().await;
    }
}

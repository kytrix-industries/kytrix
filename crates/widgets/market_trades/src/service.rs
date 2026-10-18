//! The market-trade protocol, its background service, and the seam the service
//! reads market data through.
//!
//! One service instance owns at most one live generation, keyed by the exact
//! listing it was started for. Everything that crosses the boundary is expressed
//! in that one dimension, so a message that belongs to a listing the user has
//! already left can be recognized and dropped by both sides.
//!
//! The service keeps the whole tape it publishes. The opening page and every live
//! trade end up in the same [`MarketTradesState`], which is why the table never
//! sees the seam between history and realtime: by the time a snapshot is
//! published, the two are already merged and deduplicated.

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

use super::model::{MAX_TRADES, MarketTradesSnapshot, MarketTradesState};

#[derive(Debug)]
pub(super) enum MarketTradesRequest {
    /// Starts the stream for this listing. Idempotent while one is already live.
    Load { listing: VenueListingKey },
    /// Replaces the current generation, for a listing the user is retrying.
    Retry { listing: VenueListingKey },
}

#[derive(Debug)]
pub(super) enum MarketTradesResponse {
    StreamStarted,
}

/// The listing one live tape belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct MarketTradesStreamKey {
    listing: VenueListingKey,
}

impl MarketTradesStreamKey {
    pub(super) const fn new(listing: VenueListingKey) -> Self {
        Self { listing }
    }
}

/// What the stream is doing, as far as the tape needs to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum MarketTradesStatus {
    /// Subscribing to the live trade stream.
    Connecting,
    /// Subscribed, and waiting for the opening page.
    LoadingHistory,
    /// A snapshot is published and the stream is keeping it current.
    Ready,
    /// Terminal. The stream task stays alive until the generation is replaced.
    Error(MarketError),
}

#[derive(Clone, Debug)]
pub(super) enum MarketTradesStreamMessage {
    Status(MarketTradesStatus),
    /// The whole tape, merged. Replaces whatever the table was holding.
    Snapshot(MarketTradesSnapshot),
}

pub(super) struct MarketTradesProtocol;

impl WidgetProtocol for MarketTradesProtocol {
    type Request = MarketTradesRequest;
    type Response = MarketTradesResponse;
    type StreamKey = MarketTradesStreamKey;
    type StreamMessage = MarketTradesStreamMessage;
    type Error = Infallible;
}

/// The market data one market-trades service reads through.
///
/// What a route *offers* is a different question, asked by the widget through its
/// own directory seam: the service subscribes and fetches, while the widget only
/// needs to know whether a stored listing still exists. Both calls are
/// listing-scoped, and the market participants connector resolves the listing — its rules, its
/// status and its native identifiers — inside the generation it is served from.
pub(super) trait MarketTradesSource: Clone + Send + Sync + 'static {
    type Subscription: MarketTradeEventSubscription;

    /// The opening page of `listing`, as the venue printed it.
    fn fetch_market_trades(
        &self,
        listing: VenueListingKey,
        limit: u16,
    ) -> impl Future<Output = Result<Vec<ListingTrade>, MarketError>> + Send;

    fn subscribe_market_trades(
        &self,
        listing: VenueListingKey,
    ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send;
}

/// One live trade stream.
pub(super) trait MarketTradeEventSubscription: Send + 'static {
    /// The increments of the listing this stream carries trades of.
    ///
    /// They are read before the stream is opened, from the same resolution that
    /// rejected a listing the route no longer publishes, and a tape labels every
    /// trade it holds at them.
    fn precision(&self) -> ListingPrecision;

    fn recv(
        &mut self,
    ) -> impl Future<Output = Option<Result<Arc<ListingTrade>, MarketError>>> + Send;
}

impl MarketTradesSource for MarketCapability {
    type Subscription = ClientMarketTradeSubscription;

    fn fetch_market_trades(
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

    fn subscribe_market_trades(
        &self,
        listing: VenueListingKey,
    ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send {
        let market = self.clone();
        async move {
            // The listing's own rules carry the increments every label of the tape
            // is written at, and resolving the listing is what rejects one this
            // route no longer publishes before a stream is opened for it.
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

/// One live trade stream from the market participants connector.
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

pub(super) struct MarketTradesService<S = MarketCapability> {
    source: S,
    history_limit: u16,
    maximum_trades: usize,
}

impl<S> MarketTradesService<S> {
    /// Creates a service that opens each tape with `history_limit` trades and
    /// keeps at most `maximum_trades` of them.
    ///
    /// A tape of zero rows could not hold the trade it is printing, so the
    /// capacity is clamped here rather than trusted.
    pub(super) fn new(source: S, history_limit: u16, maximum_trades: usize) -> Self {
        Self {
            source,
            history_limit: history_limit.max(1),
            maximum_trades: maximum_trades.clamp(1, MAX_TRADES),
        }
    }
}

impl<S> WidgetService<MarketTradesProtocol> for MarketTradesService<S>
where
    S: MarketTradesSource,
{
    async fn run(self, mut context: ServiceContext<MarketTradesProtocol>) {
        let mut active_key: Option<MarketTradesStreamKey> = None;

        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            let (listing, retry) = match request {
                MarketTradesRequest::Load { listing } => (listing, false),
                MarketTradesRequest::Retry { listing } => (listing, true),
            };
            let key = MarketTradesStreamKey::new(listing.clone());
            // A repeated load for the listing that is already running is answered
            // without touching the live stream: the caller has not asked for
            // anything new.
            if !retry && active_key.as_ref() == Some(&key) {
                let _ = reply.respond(Ok(MarketTradesResponse::StreamStarted));
                continue;
            }
            // `next_rpc` already dropped expired requests; re-check before
            // starting anything, so a caller that went away cannot leave a stream
            // behind.
            if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                continue;
            }
            // Streams are keyed, so a new listing would otherwise leave the previous
            // one running and publishing into the same widget.
            if active_key.as_ref() != Some(&key)
                && let Some(previous) = active_key.take()
            {
                context.cancel_stream(&previous);
            }

            let source = self.source.clone();
            let spec = MarketTradesSpec {
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
                .respond(Ok(MarketTradesResponse::StreamStarted))
                .is_err()
                && let Some(previous) = active_key.take()
            {
                context.cancel_stream(&previous);
            }
        }
    }
}

/// Everything one tape generation was started for.
#[derive(Clone)]
struct MarketTradesSpec {
    listing: VenueListingKey,
    history_limit: u16,
    maximum_trades: usize,
}

/// Runs one stream generation until it fails or is cancelled.
async fn run_stream<S>(
    source: S,
    spec: MarketTradesSpec,
    publisher: StreamPublisher<MarketTradesProtocol>,
) where
    S: MarketTradesSource,
{
    if !publish_status(&publisher, MarketTradesStatus::Connecting).await {
        return;
    }
    if let Err(error) = run_stream_once(&source, &spec, &publisher).await {
        // A terminal failure must not retry on its own: the view decides when to
        // ask for a replacement generation, so the task stays alive until then.
        if publish_status(&publisher, MarketTradesStatus::Error(error)).await {
            publisher.cancelled().await;
        }
    }
}

async fn run_stream_once<S>(
    source: &S,
    spec: &MarketTradesSpec,
    publisher: &StreamPublisher<MarketTradesProtocol>,
) -> Result<(), MarketError>
where
    S: MarketTradesSource,
{
    // The request that opens the stream is also what resolves the listing: a
    // catalog or network failure therefore reaches the user as the failure it is
    // instead of being dressed up as a listing nobody lists.
    let subscription = source.subscribe_market_trades(spec.listing.clone());
    tokio::pin!(subscription);
    let mut subscription = tokio::select! {
        biased;
        () = publisher.cancelled() => return Ok(()),
        result = &mut subscription => result?,
    };

    if !publish_status(publisher, MarketTradesStatus::LoadingHistory).await {
        return Ok(());
    }

    // The live stream is open before the page is requested, so trades that print
    // while it is in flight are buffered rather than missed.
    let (history, buffered) = fetch_while_buffering(
        source,
        spec.listing.clone(),
        spec.history_limit,
        &mut subscription,
        spec.maximum_trades,
        publisher,
    )
    .await?;

    let mut trades = MarketTradesState::new(spec.maximum_trades, subscription.precision());
    // The buffered live trades go in first, so when the page and the stream both
    // report one execution the live copy is the one the tape keeps.
    trades.merge(buffered.into_values().map(|trade| trade.as_ref().clone()));
    trades.merge(history);

    if !publish_snapshot(publisher, trades.snapshot()) {
        return Ok(());
    }
    if !publish_status(publisher, MarketTradesStatus::Ready).await {
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
                // A trade is published as it arrives: the latest stream keeps only
                // the newest snapshot per key, so batching in time would add
                // latency without saving anything. A trade the tape already holds
                // — or one older than everything retained — leaves nothing to
                // redraw, so it publishes nothing.
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

/// Fetches the opening page while keeping the live stream drained.
///
/// A page takes seconds on a slow venue. Every trade printed while it is in
/// flight is newer than anything the page can hold, so it is buffered instead of
/// dropped, and the caller merges the buffer once the page has landed.
async fn fetch_while_buffering<S>(
    source: &S,
    listing: VenueListingKey,
    limit: u16,
    subscription: &mut S::Subscription,
    maximum_trades: usize,
    publisher: &StreamPublisher<MarketTradesProtocol>,
) -> Result<
    (
        Vec<ListingTrade>,
        BTreeMap<(UnixMillis, MarketTradeId), Arc<ListingTrade>>,
    ),
    MarketError,
>
where
    S: MarketTradesSource,
{
    let fetch = source.fetch_market_trades(listing, limit);
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

/// Buffers one trade, keeping the newest `maximum_trades` of them.
///
/// The tape itself can never show more than that, so a longer buffer would only
/// hold rows the merge is guaranteed to evict again.
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

/// Rejects a trade whose quantity is not a traded quantity.
///
/// A venue that reports an amount where a traded quantity belongs is not a venue
/// this tape can draw: the value would be shown in a column that measures
/// something else, so it fails as the model rejection it is.
fn validate_trade(trade: &ListingTrade) -> Result<(), MarketError> {
    trade.validate().map_err(MarketError::from)
}

/// Publishes a status, reporting whether the stream is still alive.
async fn publish_status(
    publisher: &StreamPublisher<MarketTradesProtocol>,
    status: MarketTradesStatus,
) -> bool {
    publisher
        .publish_reliable(MarketTradesStreamMessage::Status(status))
        .await
        .is_ok()
}

/// Publishes a snapshot, reporting whether the stream is still alive.
fn publish_snapshot(
    publisher: &StreamPublisher<MarketTradesProtocol>,
    snapshot: MarketTradesSnapshot,
) -> bool {
    publisher
        .publish_latest(MarketTradesStreamMessage::Snapshot(snapshot))
        .is_ok()
}

#[cfg(test)]
mod tests {

    /// The handle of the runtime this test runs on.
    ///
    /// A test drives its subject on its own runtime rather than on a shared one. The
    /// clock these tests read is the runtime's: `#[tokio::test(start_paused = true)]`
    /// pauses it and `tokio::time::advance` moves it, and both only affect the runtime
    /// whose clock was paused. Work started on another runtime keeps waiting on real
    /// time while the test advances a clock it never reads.
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

    /// The increments the fake listing publishes its numbers at.
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

    /// One live subscription and the handles a test drives it with.
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

    /// A source that hands out one subscription and one page per generation.
    #[derive(Clone)]
    struct FakeSource {
        setups: Arc<Mutex<VecDeque<SetupReceiver>>>,
        pages: Arc<Mutex<VecDeque<oneshot::Receiver<Vec<ListingTrade>>>>>,
        fetch_started: Arc<Notify>,
        attempts: Arc<AtomicUsize>,
    }

    impl MarketTradesSource for FakeSource {
        type Subscription = FakeSubscription;

        async fn fetch_market_trades(
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

        async fn subscribe_market_trades(
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

    /// A source that reports the increments the listing is published at when a subscription is
    /// asked for, and can be republished between two of them.
    ///
    /// A listing's rules are resolved inside the subscription, so this is what a market that was
    /// loaded again looks like from the service's side: the next stream reports other increments.
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

    /// One live stream of a resolving source, writing its rows at the increments it resolved.
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

    impl MarketTradesSource for ResolvingSource {
        type Subscription = ResolvingSubscription;

        async fn fetch_market_trades(
            &self,
            _listing: VenueListingKey,
            _limit: u16,
        ) -> Result<Vec<ListingTrade>, MarketError> {
            Ok(vec![trade(1, 1, 100)])
        }

        async fn subscribe_market_trades(
            &self,
            _listing: VenueListingKey,
        ) -> Result<Self::Subscription, MarketError> {
            self.subscriptions.fetch_add(1, Ordering::AcqRel);
            let (sender, events) = mpsc::channel(8);
            // The stream is live until the service drops it, so the sender is kept here rather than
            // dropped with this call.
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

    /// The prices of one published snapshot, written at the increments its stream resolved.
    fn published_prices(
        envelope: &widget::protocol::StreamEnvelope<
            MarketTradesStreamKey,
            MarketTradesStreamMessage,
        >,
    ) -> Vec<String> {
        let MarketTradesStreamMessage::Snapshot(snapshot) = envelope.message().as_ref() else {
            panic!("the latest stream should carry a snapshot");
        };
        snapshot
            .rows()
            .iter()
            .map(|row| row.display().price.to_string())
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
        reliable: &mut widget::stream::ReliableStream<MarketTradesProtocol>,
        expected: MarketTradesStatus,
    ) {
        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(1), reliable.recv())
                .await
                .expect("status should arrive")
                .expect("reliable stream should remain open");
            if let MarketTradesStreamMessage::Status(status) = envelope.message().as_ref()
                && status == &expected
            {
                return;
            }
        }
    }

    fn snapshot_of(
        envelope: &widget::protocol::StreamEnvelope<
            MarketTradesStreamKey,
            MarketTradesStreamMessage,
        >,
    ) -> Vec<(MarketTradeId, Price)> {
        let MarketTradesStreamMessage::Snapshot(snapshot) = envelope.message().as_ref() else {
            panic!("latest value should be a market-trades snapshot");
        };
        snapshot
            .rows()
            .iter()
            .map(|row| (row.trade().id.clone(), row.trade().price.value))
            .collect()
    }

    /// The `(id, price)` rows a snapshot is expected to hold.
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
            MarketTradesService::new(source([setup], [page]), 200, MAX_TRADES),
        );
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("reliable stream should be available");

        runtime
            .client()
            .call(MarketTradesRequest::Load { listing: listing() })
            .await
            .expect("load request should start the stream");

        receive_status(&mut reliable, MarketTradesStatus::Connecting).await;
        receive_status(&mut reliable, MarketTradesStatus::LoadingHistory).await;
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
            MarketTradesService::new(source, 200, MAX_TRADES),
        );
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(MarketTradesRequest::Load { listing: listing() })
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

        // The live copy of trade 201 wins over the page's copy, and the page fills
        // in the trades that printed before it.
        assert_eq!(
            snapshot_of(&envelope),
            expected_rows(&[(199, 19), (200, 20), (201, 25)])
        );
        runtime.shutdown().await;
    }

    /// The buffer that keeps the live stream drained is bounded, and what it drops
    /// it drops silently: the tape can never show more than its own capacity, so
    /// overflowing it is not a failure the user has to hear about.
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
            MarketTradesService::new(source, 200, 2),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(MarketTradesRequest::Load { listing: listing() })
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
        // The stream reaches its ready boundary without reporting a failure in
        // between: an overflowing buffer is not something the user has to hear
        // about.
        loop {
            let status = reliable.recv().await.expect("a status should be published");
            match status.message().as_ref() {
                MarketTradesStreamMessage::Status(MarketTradesStatus::Ready) => break,
                MarketTradesStreamMessage::Status(MarketTradesStatus::Error(error)) => {
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
            MarketTradesService::new(source([setup], [page]), 200, MAX_TRADES),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(MarketTradesRequest::Load { listing: listing() })
            .await
            .unwrap();
        receive_status(&mut reliable, MarketTradesStatus::Ready).await;
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
            MarketTradesService::new(source([setup], [page]), 200, 2),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(MarketTradesRequest::Load { listing: listing() })
            .await
            .unwrap();
        receive_status(&mut reliable, MarketTradesStatus::Ready).await;
        while latest.next_unseen().is_some() {}

        // A trade the tape already holds, and one older than everything it holds.
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
            MarketTradesService::new(source.clone(), 200, MAX_TRADES),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();

        runtime
            .client()
            .call(MarketTradesRequest::Load { listing: listing() })
            .await
            .unwrap();
        receive_status(&mut reliable, MarketTradesStatus::Ready).await;

        drop(events);
        receive_status(
            &mut reliable,
            MarketTradesStatus::Error(MarketError::Disconnected(DisconnectReason::Normal)),
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
            MarketTradesService::new(source.clone(), 200, MAX_TRADES),
        );
        let client = runtime.client();

        client
            .call(MarketTradesRequest::Load { listing: listing() })
            .await
            .expect("the first load should start a stream");
        wait_for_attempts(&source, 1).await;
        client
            .call(MarketTradesRequest::Load {
                listing: other_listing(),
            })
            .await
            .expect("the replacement stream should start");

        tokio::time::timeout(Duration::from_secs(1), first_setup_sender.closed())
            .await
            .expect("a pending subscription should be cancelled promptly");
        runtime.shutdown().await;
    }

    /// Two listings of one route are two scopes: the tape that follows one of them
    /// neither replaces nor accepts the other's stream.
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
            MarketTradesService::new(source.clone(), 200, MAX_TRADES),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();
        let client = runtime.client();

        client
            .call(MarketTradesRequest::Load { listing: listing() })
            .await
            .unwrap();
        receive_status(&mut reliable, MarketTradesStatus::Ready).await;
        client
            .call(MarketTradesRequest::Load {
                listing: other_listing(),
            })
            .await
            .unwrap();
        assert_eq!(
            source.attempts.load(Ordering::Acquire),
            2,
            "the second listing is its own scope, not a reload of the first"
        );

        // The first stream was cancelled with its listing, so its events reach a
        // widget that is no longer following it.
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
            MarketTradesService::new(
                source([first_setup, second_setup], [first_page, second_page]),
                200,
                MAX_TRADES,
            ),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(MarketTradesRequest::Load { listing: listing() })
            .await
            .unwrap();
        receive_status(&mut reliable, MarketTradesStatus::Ready).await;
        latest.changed().await.unwrap();
        let _ = latest.next_unseen();

        runtime
            .client()
            .call(MarketTradesRequest::Retry { listing: listing() })
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
        receive_status(&mut reliable, MarketTradesStatus::Ready).await;
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

    /// The replacing request is the one that resolves the listing again: a stream a retry opens
    /// reports the increments the listing is published at when it is asked, and the rows it
    /// publishes are written at them — while a load for the scope that is already live is answered
    /// without the source being asked for anything at all.
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
            MarketTradesService::new(source.clone(), 200, MAX_TRADES),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(MarketTradesRequest::Load { listing: listing() })
            .await
            .unwrap();
        receive_status(&mut reliable, MarketTradesStatus::Ready).await;
        latest.changed().await.unwrap();
        let envelope = latest.next_unseen().expect("snapshot should be unseen");
        assert_eq!(published_prices(&envelope), vec!["100.00"]);
        assert_eq!(source.subscriptions(), 1);

        // The market is loaded again and publishes the listing at another tick. A load for the
        // scope that is already live never reaches the source, which is why it cannot be the
        // request a reload sends.
        source.publish_at("0.5");
        runtime
            .client()
            .call(MarketTradesRequest::Load { listing: listing() })
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert_eq!(source.subscriptions(), 1);

        // A retry is the request that opens a stream, and the stream it opens is the resolution
        // this load produced: its rows are written at the tick that load published.
        runtime
            .client()
            .call(MarketTradesRequest::Retry { listing: listing() })
            .await
            .unwrap();
        receive_status(&mut reliable, MarketTradesStatus::Ready).await;
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
            MarketTradesService::new(source.clone(), 200, MAX_TRADES),
            config,
        );
        runtime
            .client()
            .call(MarketTradesRequest::Load { listing: listing() })
            .await
            .unwrap();
        wait_for_attempts(&source, 1).await;

        tokio::time::timeout(Duration::from_secs(1), runtime.shutdown())
            .await
            .expect("shutdown should cancel the blocked reliable publish");
    }

    /// A trade whose quantity is an amount rather than a traded quantity is not a
    /// trade this tape can draw, so it fails the stream as the model rejection it
    /// is instead of filling a column that measures something else.
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
            MarketTradesService::new(source([setup], [page]), 200, MAX_TRADES),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(MarketTradesRequest::Load { listing: listing() })
            .await
            .unwrap();
        receive_status(&mut reliable, MarketTradesStatus::Ready).await;
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
            if let MarketTradesStreamMessage::Status(MarketTradesStatus::Error(error)) =
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

    /// A source whose subscribe seam reports what a catalog that could not be
    /// fetched reports.
    #[derive(Clone)]
    struct UnresolvableSource {
        failure: MarketError,
    }

    impl MarketTradesSource for UnresolvableSource {
        type Subscription = FakeSubscription;

        async fn fetch_market_trades(
            &self,
            _listing: VenueListingKey,
            _limit: u16,
        ) -> Result<Vec<ListingTrade>, MarketError> {
            std::future::pending().await
        }

        async fn subscribe_market_trades(
            &self,
            _listing: VenueListingKey,
        ) -> Result<Self::Subscription, MarketError> {
            Err(self.failure.clone())
        }
    }

    /// The seam fails with whatever the route's catalog failed with — a fetch that
    /// never completed, a venue that refused, a catalog that would not decode. That
    /// failure is what the tape is told about, and it is never dressed up as a
    /// listing that could not be named.
    #[tokio::test]
    async fn a_catalog_failure_on_the_subscription_seam_should_be_reported_as_itself() {
        let failure = MarketError::CatalogSource(SourceError::Network {
            reason: SmolStr::new_static("the test catalog is unreachable"),
        });
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            MarketTradesService::new(
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
            .call(MarketTradesRequest::Load { listing: listing() })
            .await
            .expect("load request should start a stream");

        let reported = loop {
            let envelope = tokio::time::timeout(Duration::from_secs(1), reliable.recv())
                .await
                .expect("the failure should be published")
                .expect("the reliable stream should remain open");
            if let MarketTradesStreamMessage::Status(MarketTradesStatus::Error(error)) =
                envelope.message().as_ref()
            {
                break error.clone();
            }
        };

        assert_eq!(reported, failure);
        runtime.shutdown().await;
    }
}

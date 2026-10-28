//! The background service one force-order pane talks to.
//!
//! Which routes report liquidations, and which of their listings exist at all, are
//! two questions the route registry and the published catalog answer: the pane
//! hands this service the routes the registry offers, and the service asks each of
//! them for the perpetual listings it publishes before it subscribes to anything.
//! A route that publishes no perpetual listing has no liquidation to report, and
//! an event naming a listing the route does not publish is not one this pane may
//! show.

use std::{collections::VecDeque, convert::Infallible, future::Future, sync::Arc};

use ahash::AHashMap;
use futures::{
    StreamExt,
    future::{select_all, try_join_all},
};
use gpui_kit::SharedString;
use market_participants_connector::{
    catalog::store::Catalog,
    market::{
        domain::DisconnectReason, error::MarketError, provider::MarketCapability,
        streams::ForceOrderStream,
    },
};
use market_participants_model::{
    instrument::InstrumentKind,
    listing::route::{VenueListingKey, VenueRoute},
    market::book::ListingForceOrder,
};
use smol_str::SmolStr;
use tokio::time::Instant;
use widget::{
    protocol::WidgetProtocol,
    runtime::{ServiceContext, WidgetService},
    stream::{StreamPublishError, StreamPublisher},
};

use super::model::{
    ForceOrderEntry, ForceOrderListing, ForceOrderSnapshot, ForceOrderState, MAX_FORCE_ORDERS,
};

#[derive(Debug)]
pub(super) enum ForceOrderRequest {
    /// Starts the stream for these routes. Idempotent while one is already live.
    Load { routes: Arc<[VenueRoute]> },
    /// Replaces the current generation, for a route set that changed under the
    /// pane or for a user's own retry.
    Retry { routes: Arc<[VenueRoute]> },
}

#[derive(Debug)]
pub(super) enum ForceOrderResponse {
    StreamStarted,
}

/// Every route that reports liquidations publishes into one stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ForceOrderStreamKey {
    AllMarkets,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ForceOrderStatus {
    Connecting,
    Ready,
    Error(MarketError),
}

#[derive(Clone, Debug)]
pub(super) enum ForceOrderStreamMessage {
    Status(ForceOrderStatus),
    Snapshot(ForceOrderSnapshot),
}

pub(super) struct ForceOrderProtocol;

impl WidgetProtocol for ForceOrderProtocol {
    type Request = ForceOrderRequest;
    type Response = ForceOrderResponse;
    type StreamKey = ForceOrderStreamKey;
    type StreamMessage = ForceOrderStreamMessage;
    type Error = Infallible;
}

pub(super) trait ForceOrderSource: Clone + Send + Sync + 'static {
    type Subscription: ForceOrderEventSubscription;

    /// The perpetual listings one route publishes, as their canonical identities.
    ///
    /// A liquidation is only ever published for a perpetual listing, so this is
    /// what decides whether a route is worth subscribing to. What a listing is
    /// *called* is not a catalog fact this seam answers: the service reads the name
    /// from the display generation it captured, so a venue's own symbol can never
    /// become a row's name.
    fn perpetual_listings(
        &self,
        route: &VenueRoute,
    ) -> impl Future<Output = Result<Vec<VenueListingKey>, MarketError>> + Send;

    fn subscribe_force_order(
        &self,
        route: VenueRoute,
    ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send;
}

pub(super) trait ForceOrderEventSubscription: Send + 'static {
    fn recv(
        &mut self,
    ) -> impl Future<Output = Option<Result<Arc<Vec<ListingForceOrder>>, MarketError>>> + Send;
}

impl ForceOrderSource for MarketCapability {
    type Subscription = ClientForceOrderSubscription;

    async fn perpetual_listings(
        &self,
        route: &VenueRoute,
    ) -> Result<Vec<VenueListingKey>, MarketError> {
        let ensured = self.ensure(route)?;
        let entries = ensured.listings();
        let mut listings = Vec::with_capacity(entries.len());
        for entry in entries.iter() {
            if entry.instrument.kind() != InstrumentKind::Perpetual {
                continue;
            }
            listings.push(entry.listing.key.clone());
        }
        Ok(listings)
    }

    fn subscribe_force_order(
        &self,
        route: VenueRoute,
    ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send {
        let market = self.clone();
        async move {
            let inner = market.subscribe_force_order_on(route).await?;
            Ok(ClientForceOrderSubscription { inner })
        }
    }
}

pub(super) struct ClientForceOrderSubscription {
    inner: ForceOrderStream,
}

impl ForceOrderEventSubscription for ClientForceOrderSubscription {
    async fn recv(&mut self) -> Option<Result<Arc<Vec<ListingForceOrder>>, MarketError>> {
        self.inner
            .next()
            .await
            .map(|event| event.map(|order| Arc::new(vec![order])))
    }
}

pub(super) struct ForceOrderService<S = MarketCapability> {
    source: S,
    /// The display context every row's listing name is read from.
    ///
    /// It is injected rather than reached for, so the service names its listings
    /// from the generation its own caller published, and so a test can hand it the
    /// generation it wants the rows named by.
    display: Arc<Catalog>,
    maximum_orders: usize,
}

impl<S> ForceOrderService<S> {
    pub(super) fn new(source: S, display: Arc<Catalog>, maximum_orders: usize) -> Self {
        Self {
            source,
            display,
            maximum_orders: maximum_orders.clamp(1, MAX_FORCE_ORDERS),
        }
    }
}

impl<S> WidgetService<ForceOrderProtocol> for ForceOrderService<S>
where
    S: ForceOrderSource,
{
    async fn run(self, mut context: ServiceContext<ForceOrderProtocol>) {
        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            let (routes, retry) = match request {
                ForceOrderRequest::Load { routes } => (routes, false),
                ForceOrderRequest::Retry { routes } => (routes, true),
            };
            let key = ForceOrderStreamKey::AllMarkets;
            // A repeated load of what is already running is answered without
            // touching the live stream: the caller has not asked for anything new.
            // A route set that changed arrives as a retry, because another venue's
            // liquidations are another scope rather than a repeat of this one.
            if !retry && context.stream_generation(&key).is_some() {
                let _ = reply.respond(Ok(ForceOrderResponse::StreamStarted));
                continue;
            }
            // `next_rpc` already dropped expired requests; re-check before
            // starting anything, so a caller that went away cannot leave a stream
            // behind.
            if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                continue;
            }
            let source = self.source.clone();
            let display = self.display.clone();
            let maximum_orders = self.maximum_orders;
            let streamed = routes.clone();
            if retry {
                context.replace_stream(key, move |publisher| async move {
                    run_stream(source, streamed, display, maximum_orders, publisher).await;
                });
            } else {
                context.start_stream(key, move |publisher| async move {
                    run_stream(source, streamed, display, maximum_orders, publisher).await;
                });
            }
            if reply.deadline() <= Instant::now()
                || reply.is_cancelled()
                || reply
                    .respond(Ok(ForceOrderResponse::StreamStarted))
                    .is_err()
            {
                context.cancel_stream(&key);
            }
        }
    }
}

async fn run_stream<S>(
    source: S,
    routes: Arc<[VenueRoute]>,
    display: Arc<Catalog>,
    maximum_orders: usize,
    publisher: StreamPublisher<ForceOrderProtocol>,
) where
    S: ForceOrderSource,
{
    if publish_status(&publisher, ForceOrderStatus::Connecting)
        .await
        .is_err()
    {
        return;
    }
    if let Err(error) =
        run_stream_once(&source, &routes, &display, maximum_orders, &publisher).await
    {
        publish_error(&publisher, error).await;
        // A terminal failure must not retry on its own: the view decides when to
        // ask for a replacement stream, so the task stays alive until then.
        publisher.cancelled().await;
    }
}

async fn run_stream_once<S>(
    source: &S,
    routes: &[VenueRoute],
    display: &Catalog,
    maximum_orders: usize,
    publisher: &StreamPublisher<ForceOrderProtocol>,
) -> Result<(), MarketError>
where
    S: ForceOrderSource,
{
    // What each route actually liquidates is the first question: a route whose
    // published catalog holds no perpetual listing has nothing to subscribe to.
    let discovered = tokio::select! {
        biased;
        () = publisher.cancelled() => return Ok(()),
        result = try_join_all(routes.iter().cloned().map(|route| async move {
            let listings = source.perpetual_listings(&route).await?;
            Ok::<_, MarketError>((route, listings))
        })) => result?,
    };
    // Every row is named and written by one generation: the labels and the
    // increments are read from a single capture of the published catalog, so a
    // publication that lands while they are read cannot put two generations' names
    // or two generations' precision on one screen. A listing the captured
    // generation does not name keeps its structured key and no name at all, which
    // the view draws as the localized unknown.
    let display = display.cached_display();
    let mut listings: AHashMap<VenueListingKey, ForceOrderListing> =
        AHashMap::with_capacity(discovered.iter().map(|(_, listings)| listings.len()).sum());
    let mut subscribed = Vec::with_capacity(discovered.len());
    for (route, route_listings) in discovered {
        if route_listings.is_empty() {
            continue;
        }
        for listing in route_listings {
            let named = ForceOrderListing {
                label: display
                    .listing_instrument(&listing)
                    .ok()
                    .map(|name| SharedString::new(name.as_str())),
                precision: display.listing_precision(&listing),
            };
            listings.insert(listing, named);
        }
        subscribed.push(route);
    }
    if subscribed.is_empty() {
        return Err(MarketError::Configuration(SmolStr::new_static(
            "no market-participant route publishes a perpetual listing",
        )));
    }

    let subscriptions = tokio::select! {
        biased;
        () = publisher.cancelled() => return Ok(()),
        result = try_join_all(
            subscribed
                .into_iter()
                .map(|route| source.subscribe_force_order(route)),
        ) => result?,
    };
    let mut subscription_queue = VecDeque::with_capacity(subscriptions.len());
    subscription_queue.extend(subscriptions);

    let mut orders = ForceOrderState::new(maximum_orders);
    if publisher
        .publish_latest(ForceOrderStreamMessage::Snapshot(orders.snapshot()))
        .is_err()
    {
        return Ok(());
    }
    if publish_status(publisher, ForceOrderStatus::Ready)
        .await
        .is_err()
    {
        return Ok(());
    }

    loop {
        tokio::select! {
            biased;
            () = publisher.cancelled() => return Ok(()),
            event = async {
                let mut pending = Vec::with_capacity(subscription_queue.len());
                for subscription in &mut subscription_queue {
                    pending.push(Box::pin(subscription.recv()));
                }
                let (event, _, _) = select_all(pending).await;
                if let Some(subscription) = subscription_queue.pop_front() {
                    subscription_queue.push_back(subscription);
                }
                event
            } => {
                // A batch is published as it arrives: the latest stream keeps only
                // the newest snapshot per key, so batching in time would add
                // latency without saving anything. A batch that leaves the
                // retained orders unchanged — every order in it was evicted as
                // stale — publishes nothing, because nothing drawn would change.
                if !merge_event(event, &listings, &mut orders)? {
                    continue;
                }
                if publisher
                    .publish_latest(ForceOrderStreamMessage::Snapshot(orders.snapshot()))
                    .is_err()
                {
                    return Ok(());
                }
            }
        }
    }
}

fn merge_event(
    event: Option<Result<Arc<Vec<ListingForceOrder>>, MarketError>>,
    listings: &AHashMap<VenueListingKey, ForceOrderListing>,
    orders: &mut ForceOrderState,
) -> Result<bool, MarketError> {
    match event {
        Some(Ok(batch)) => {
            // An event naming a listing its own route does not publish is not a
            // liquidation this pane may show, so it is left out rather than shown
            // under a listing that does not exist.
            let entries = batch
                .iter()
                .filter_map(|order| {
                    listings.get(&order.listing).map(|listing| ForceOrderEntry {
                        order: order.clone(),
                        listing: listing.clone(),
                    })
                })
                .collect::<Vec<_>>();
            Ok(orders.merge(&entries) > 0)
        }
        Some(Err(error)) => Err(error),
        None => Err(MarketError::Disconnected(DisconnectReason::Normal)),
    }
}

async fn publish_status(
    publisher: &StreamPublisher<ForceOrderProtocol>,
    status: ForceOrderStatus,
) -> Result<(), StreamPublishError> {
    publisher
        .publish_reliable(ForceOrderStreamMessage::Status(status))
        .await
}

async fn publish_error(publisher: &StreamPublisher<ForceOrderProtocol>, error: MarketError) {
    let _ = publish_status(publisher, ForceOrderStatus::Error(error)).await;
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

    use market_participants_connector::catalog::test_support;
    use market_participants_model::{
        decimal::{Price, Quantity},
        listing::route::VenueRoute,
        market::{
            price::MarketPrice,
            units::{MarketQuantity, PositionSide, QuantityUnit},
        },
    };
    use tokio::sync::{mpsc, oneshot};
    use widget::runtime::{WidgetRuntime, WidgetRuntimeConfig};

    use super::*;

    type FakeEvent = Result<Arc<Vec<ListingForceOrder>>, MarketError>;
    type SetupReceiver = oneshot::Receiver<Result<FakeSubscription, MarketError>>;
    type SubscriptionFixture = (
        FakeSubscription,
        mpsc::Sender<FakeEvent>,
        oneshot::Receiver<()>,
    );

    fn binance_route() -> VenueRoute {
        test_support::perp_route()
    }

    fn okx_route() -> VenueRoute {
        test_support::route("okx", "swap")
    }

    fn routes() -> Arc<[VenueRoute]> {
        Arc::from([binance_route(), okx_route()])
    }

    fn listing(route: &VenueRoute, symbol: &str) -> VenueListingKey {
        test_support::listing_key(route, symbol)
    }

    /// A display context that names nothing: the tests that are not about the
    /// label drive the rows they assert through the listing keys alone.
    fn unnamed_display() -> Arc<Catalog> {
        Arc::new(Catalog::new())
    }

    fn force_order(route: &VenueRoute, symbol: &str, timestamp: i64) -> ListingForceOrder {
        ListingForceOrder {
            listing: test_support::listing_key(route, symbol),
            price: MarketPrice {
                value: Price::from(100_u64),
                quote_asset: test_support::token("fixture", "USDT"),
                basis: market_participants_model::listing::venue::PriceBasis::PerAssetUnit,
            },
            quantity: MarketQuantity::traded(Quantity::from(1_u64), QuantityUnit::Contracts)
                .expect("a contract count is a traded quantity"),
            position_side: PositionSide::Long,
            timestamp: (timestamp),
        }
    }

    #[derive(Clone)]
    struct FakeSource {
        binance: Arc<Mutex<VecDeque<SetupReceiver>>>,
        okx: Arc<Mutex<VecDeque<SetupReceiver>>>,
        listings: Arc<Vec<(VenueRoute, Vec<VenueListingKey>)>>,
        attempts: Arc<AtomicUsize>,
    }

    #[derive(Debug)]
    struct FakeSubscription {
        events: mpsc::Receiver<FakeEvent>,
        dropped: Option<oneshot::Sender<()>>,
    }

    impl Drop for FakeSubscription {
        fn drop(&mut self) {
            if let Some(dropped) = self.dropped.take() {
                let _ = dropped.send(());
            }
        }
    }

    impl ForceOrderSource for FakeSource {
        type Subscription = FakeSubscription;

        async fn perpetual_listings(
            &self,
            route: &VenueRoute,
        ) -> Result<Vec<VenueListingKey>, MarketError> {
            Ok(self
                .listings
                .iter()
                .find(|(candidate, _)| candidate == route)
                .map(|(_, listings)| listings.clone())
                .unwrap_or_default())
        }

        async fn subscribe_force_order(
            &self,
            route: VenueRoute,
        ) -> Result<Self::Subscription, MarketError> {
            self.attempts.fetch_add(1, Ordering::AcqRel);
            let setups = if route == binance_route() {
                &self.binance
            } else if route == okx_route() {
                &self.okx
            } else {
                return Err(MarketError::UnsupportedCapability {
                    route,
                    capability: market_participants_model::routing::CapabilityId::ForceOrder,
                });
            };
            let setup = setups
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .ok_or(MarketError::ServiceClosed)?;
            setup.await.map_err(|_| MarketError::ServiceClosed)?
        }
    }

    impl ForceOrderEventSubscription for FakeSubscription {
        async fn recv(&mut self) -> Option<FakeEvent> {
            self.events.recv().await
        }
    }

    fn source(
        binance: impl IntoIterator<Item = SetupReceiver>,
        okx: impl IntoIterator<Item = SetupReceiver>,
    ) -> FakeSource {
        FakeSource {
            binance: Arc::new(Mutex::new(binance.into_iter().collect())),
            okx: Arc::new(Mutex::new(okx.into_iter().collect())),
            listings: Arc::new(vec![
                (binance_route(), vec![listing(&binance_route(), "BTCUSDT")]),
                (okx_route(), vec![listing(&okx_route(), "BTCUSDT")]),
            ]),
            attempts: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn setup() -> (
        oneshot::Sender<Result<FakeSubscription, MarketError>>,
        SetupReceiver,
    ) {
        oneshot::channel()
    }

    fn subscription() -> SubscriptionFixture {
        let (events, receiver) = mpsc::channel(8);
        let (dropped, drop_receiver) = oneshot::channel();
        (
            FakeSubscription {
                events: receiver,
                dropped: Some(dropped),
            },
            events,
            drop_receiver,
        )
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
        reliable: &mut widget::stream::ReliableStream<ForceOrderProtocol>,
        expected: ForceOrderStatus,
    ) {
        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(1), reliable.recv())
                .await
                .expect("status should arrive")
                .expect("reliable stream should remain open");
            if let ForceOrderStreamMessage::Status(status) = envelope.message().as_ref()
                && status == &expected
            {
                return;
            }
        }
    }

    #[tokio::test]
    async fn retry_should_cancel_a_pending_setup_before_starting_the_replacement() {
        let (mut first_binance, first_binance_setup) = setup();
        let (first_okx, first_okx_setup) = setup();
        let (second_binance, second_binance_setup) = setup();
        let (second_okx, second_okx_setup) = setup();
        let (first_okx_subscription, _first_okx_events, _first_okx_dropped) = subscription();
        first_okx
            .send(Ok(first_okx_subscription))
            .expect("first OKX setup should be waiting");
        let (second_binance_subscription, _second_binance_events, _second_binance_dropped) =
            subscription();
        let (second_okx_subscription, _second_okx_events, _second_okx_dropped) = subscription();
        second_binance
            .send(Ok(second_binance_subscription))
            .expect("replacement Binance setup should be waiting");
        second_okx
            .send(Ok(second_okx_subscription))
            .expect("replacement OKX setup should be waiting");
        let source = source(
            [first_binance_setup, second_binance_setup],
            [first_okx_setup, second_okx_setup],
        );
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            ForceOrderService::new(source.clone(), unnamed_display(), 10),
        );
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("reliable stream should be available");

        runtime
            .client()
            .call(ForceOrderRequest::Load { routes: routes() })
            .await
            .expect("initial stream should start");
        wait_for_attempts(&source, 2).await;
        runtime
            .client()
            .call(ForceOrderRequest::Retry { routes: routes() })
            .await
            .expect("replacement stream should start");

        tokio::time::timeout(Duration::from_secs(1), first_binance.closed())
            .await
            .expect("pending setup should be cancelled promptly");
        wait_for_attempts(&source, 4).await;
        receive_status(&mut reliable, ForceOrderStatus::Ready).await;
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn retry_should_drop_old_subscriptions_and_suppress_late_events() {
        let (first_binance_setup_sender, first_binance_setup) = setup();
        let (first_okx_setup_sender, first_okx_setup) = setup();
        let (second_binance_setup_sender, second_binance_setup) = setup();
        let (second_okx_setup_sender, second_okx_setup) = setup();
        let (first_binance, first_binance_events, first_binance_dropped) = subscription();
        let (first_okx, _first_okx_events, first_okx_dropped) = subscription();
        let (second_binance, second_binance_events, _second_binance_dropped) = subscription();
        let (second_okx, _second_okx_events, _second_okx_dropped) = subscription();
        first_binance_setup_sender.send(Ok(first_binance)).unwrap();
        first_okx_setup_sender.send(Ok(first_okx)).unwrap();
        second_binance_setup_sender
            .send(Ok(second_binance))
            .unwrap();
        second_okx_setup_sender.send(Ok(second_okx)).unwrap();
        let source = source(
            [first_binance_setup, second_binance_setup],
            [first_okx_setup, second_okx_setup],
        );
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            ForceOrderService::new(source, unnamed_display(), 10),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(ForceOrderRequest::Load { routes: routes() })
            .await
            .unwrap();
        receive_status(&mut reliable, ForceOrderStatus::Ready).await;
        latest.changed().await.unwrap();
        let _ = latest.next_unseen();
        runtime
            .client()
            .call(ForceOrderRequest::Retry { routes: routes() })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), first_binance_dropped)
            .await
            .expect("old Binance subscription should be dropped")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), first_okx_dropped)
            .await
            .expect("old OKX subscription should be dropped")
            .unwrap();
        assert!(
            first_binance_events
                .send(Ok(Arc::new(vec![force_order(
                    &binance_route(),
                    "BTCUSDT",
                    1
                )])))
                .await
                .is_err()
        );
        receive_status(&mut reliable, ForceOrderStatus::Ready).await;
        latest.changed().await.unwrap();
        let _ = latest.next_unseen();

        second_binance_events
            .send(Ok(Arc::new(vec![force_order(
                &binance_route(),
                "BTCUSDT",
                2,
            )])))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), latest.changed())
            .await
            .expect("replacement event should publish")
            .unwrap();
        let envelope = latest.next_unseen().expect("snapshot should be unseen");
        let ForceOrderStreamMessage::Snapshot(snapshot) = envelope.message().as_ref() else {
            panic!("latest value should be a force-order snapshot");
        };
        assert_eq!(snapshot.orders()[0].order().timestamp, 2);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn stream_should_enforce_the_configured_order_capacity() {
        let (binance_setup_sender, binance_setup) = setup();
        let (okx_setup_sender, okx_setup) = setup();
        let (binance, binance_events, _binance_dropped) = subscription();
        let (okx, _okx_events, _okx_dropped) = subscription();
        binance_setup_sender.send(Ok(binance)).unwrap();
        okx_setup_sender.send(Ok(okx)).unwrap();
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            ForceOrderService::new(source([binance_setup], [okx_setup]), unnamed_display(), 2),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();
        runtime
            .client()
            .call(ForceOrderRequest::Load { routes: routes() })
            .await
            .unwrap();
        receive_status(&mut reliable, ForceOrderStatus::Ready).await;
        latest.changed().await.unwrap();
        let _ = latest.next_unseen();

        binance_events
            .send(Ok(Arc::new(vec![
                force_order(&binance_route(), "BTCUSDT", 1),
                force_order(&binance_route(), "BTCUSDT", 3),
                force_order(&binance_route(), "BTCUSDT", 2),
            ])))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), latest.changed())
            .await
            .unwrap()
            .unwrap();
        let envelope = latest.next_unseen().unwrap();
        let ForceOrderStreamMessage::Snapshot(snapshot) = envelope.message().as_ref() else {
            panic!("latest value should be a force-order snapshot");
        };
        assert_eq!(
            snapshot
                .orders()
                .iter()
                .map(|row| row.order().timestamp)
                .collect::<Vec<_>>(),
            [2, 3]
        );
        runtime.shutdown().await;
    }

    /// Every batch that changes what the widget retains is published as it arrives.
    ///
    /// A batch whose orders are all evicted as stale changes nothing, so publishing
    /// it would only hand the view a snapshot identical to the one it already
    /// shows. The liveness check at the end keeps the silent-batch assertion
    /// honest: the stream is still there and still publishes what does change.
    #[tokio::test]
    async fn a_batch_that_changes_nothing_should_not_publish_a_snapshot() {
        let (binance_setup_sender, binance_setup) = setup();
        let (okx_setup_sender, okx_setup) = setup();
        let (binance, binance_events, _binance_dropped) = subscription();
        let (okx, _okx_events, _okx_dropped) = subscription();
        binance_setup_sender.send(Ok(binance)).unwrap();
        okx_setup_sender.send(Ok(okx)).unwrap();
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            ForceOrderService::new(source([binance_setup], [okx_setup]), unnamed_display(), 2),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();
        runtime
            .client()
            .call(ForceOrderRequest::Load { routes: routes() })
            .await
            .unwrap();
        receive_status(&mut reliable, ForceOrderStatus::Ready).await;
        while latest.next_unseen().is_some() {}

        binance_events
            .send(Ok(Arc::new(vec![
                force_order(&binance_route(), "BTCUSDT", 2),
                force_order(&binance_route(), "BTCUSDT", 3),
            ])))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), latest.changed())
            .await
            .expect("a batch that changes the retained orders should publish")
            .unwrap();
        while latest.next_unseen().is_some() {}

        // Both retained slots hold newer orders, so this batch is evicted whole.
        binance_events
            .send(Ok(Arc::new(vec![force_order(
                &binance_route(),
                "BTCUSDT",
                1,
            )])))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), latest.changed())
                .await
                .is_err(),
            "a batch that changed nothing should not publish a snapshot"
        );

        binance_events
            .send(Ok(Arc::new(vec![force_order(
                &binance_route(),
                "BTCUSDT",
                4,
            )])))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), latest.changed())
            .await
            .expect("the stream should still publish what does change")
            .unwrap();
        let envelope = latest.next_unseen().expect("snapshot should be unseen");
        let ForceOrderStreamMessage::Snapshot(snapshot) = envelope.message().as_ref() else {
            panic!("latest value should be a force-order snapshot");
        };
        assert_eq!(
            snapshot
                .orders()
                .iter()
                .map(|row| row.order().timestamp)
                .collect::<Vec<_>>(),
            [3, 4]
        );
        runtime.shutdown().await;
    }

    /// An event naming a listing its own route does not publish is not a
    /// liquidation this pane may show.
    #[tokio::test]
    async fn an_event_of_an_unpublished_listing_should_not_be_retained() {
        let (binance_setup_sender, binance_setup) = setup();
        let (okx_setup_sender, okx_setup) = setup();
        let (binance, binance_events, _binance_dropped) = subscription();
        let (okx, _okx_events, _okx_dropped) = subscription();
        binance_setup_sender.send(Ok(binance)).unwrap();
        okx_setup_sender.send(Ok(okx)).unwrap();
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            ForceOrderService::new(source([binance_setup], [okx_setup]), unnamed_display(), 10),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();
        runtime
            .client()
            .call(ForceOrderRequest::Load { routes: routes() })
            .await
            .unwrap();
        receive_status(&mut reliable, ForceOrderStatus::Ready).await;
        while latest.next_unseen().is_some() {}

        binance_events
            .send(Ok(Arc::new(vec![
                force_order(&binance_route(), "BTCUSDT", 1),
                force_order(&binance_route(), "DOGEUSDT", 2),
            ])))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), latest.changed())
            .await
            .expect("the published listing's event should publish")
            .unwrap();
        let envelope = latest.next_unseen().expect("snapshot should be unseen");
        let ForceOrderStreamMessage::Snapshot(snapshot) = envelope.message().as_ref() else {
            panic!("latest value should be a force-order snapshot");
        };
        assert_eq!(snapshot.len(), 1);
        assert_eq!(
            snapshot.orders()[0].order().listing,
            test_support::listing_key(&binance_route(), "BTCUSDT")
        );
        runtime.shutdown().await;
    }

    /// A source that publishes exactly the listings a test names and answers with
    /// exactly one batch, so a test can hold every other dimension still.
    #[derive(Clone)]
    struct NamingSource {
        listings: Arc<Vec<VenueListingKey>>,
        batch: Arc<Vec<ListingForceOrder>>,
    }

    struct NamingSubscription {
        batch: Option<Arc<Vec<ListingForceOrder>>>,
    }

    impl ForceOrderSource for NamingSource {
        type Subscription = NamingSubscription;

        async fn perpetual_listings(
            &self,
            _route: &VenueRoute,
        ) -> Result<Vec<VenueListingKey>, MarketError> {
            Ok(self.listings.as_ref().clone())
        }

        async fn subscribe_force_order(
            &self,
            _route: VenueRoute,
        ) -> Result<Self::Subscription, MarketError> {
            Ok(NamingSubscription {
                batch: Some(Arc::clone(&self.batch)),
            })
        }
    }

    impl ForceOrderEventSubscription for NamingSubscription {
        async fn recv(&mut self) -> Option<FakeEvent> {
            match self.batch.take() {
                Some(batch) => Some(Ok(batch)),
                None => std::future::pending().await,
            }
        }
    }

    /// Rows are named by the generation the service was *given*, not by the
    /// venue's own symbol: the same listing key the venue wrote as `BTCUSDT` is
    /// drawn as the name the injected catalog published.
    #[tokio::test]
    async fn a_row_should_be_named_by_the_generation_the_service_was_given() {
        let route = test_support::spot_route();
        let key = test_support::listing_key(&route, "BTCUSDT");
        let fixture = test_support::Fixture::new(vec![test_support::entry(
            &route,
            test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
            "BTCUSDT",
        )]);
        fixture
            .ensure()
            .await
            .expect("the fixture spot route publishes its catalog");
        let published = fixture
            .catalog()
            .cached_listing_instrument(&key)
            .expect("the published generation names the listing");
        let display = Arc::clone(fixture.catalog_arc());

        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            ForceOrderService::new(
                NamingSource {
                    listings: Arc::new(vec![key.clone()]),
                    batch: Arc::new(vec![force_order(&route, "BTCUSDT", 1)]),
                },
                display,
                10,
            ),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(ForceOrderRequest::Load { routes: routes() })
            .await
            .unwrap();
        receive_status(&mut reliable, ForceOrderStatus::Ready).await;
        latest.changed().await.unwrap();
        let envelope = latest.next_unseen().expect("snapshot should be unseen");
        let ForceOrderStreamMessage::Snapshot(snapshot) = envelope.message().as_ref() else {
            panic!("latest value should be a force-order snapshot");
        };
        let label = snapshot.orders()[0]
            .display(time::UtcOffset::UTC)
            .listing
            .clone()
            .expect("the injected generation names the row");

        assert_eq!(label.as_ref(), published.as_str());
        assert_ne!(
            label.as_ref(),
            "BTCUSDT",
            "the venue's own symbol is not the name of the listing"
        );
        runtime.shutdown().await;
    }

    /// The numbers of a row are written at the increments of the listing the
    /// injected generation publishes, so a whole price keeps the decimals its own
    /// tick states instead of being read as the canonical form.
    #[tokio::test]
    async fn a_row_should_be_written_at_the_precision_of_its_published_listing() {
        let route = test_support::spot_route();
        let key = test_support::listing_key(&route, "BTCUSDT");
        let mut published = test_support::entry(
            &route,
            test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
            "BTCUSDT",
        );
        published.snapshot.rules.common.price.tick = "0.01"
            .parse()
            .expect("the published price tick should parse");
        published.snapshot.rules.common.quantity.step = "0.001"
            .parse()
            .expect("the published quantity step should parse");
        let fixture = test_support::Fixture::new(vec![published]);
        fixture
            .ensure()
            .await
            .expect("the fixture spot route publishes its catalog");
        let display = Arc::clone(fixture.catalog_arc());

        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            ForceOrderService::new(
                NamingSource {
                    listings: Arc::new(vec![key]),
                    batch: Arc::new(vec![force_order(&route, "BTCUSDT", 1)]),
                },
                display,
                10,
            ),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(ForceOrderRequest::Load { routes: routes() })
            .await
            .unwrap();
        receive_status(&mut reliable, ForceOrderStatus::Ready).await;
        latest.changed().await.unwrap();
        let envelope = latest.next_unseen().expect("snapshot should be unseen");
        let ForceOrderStreamMessage::Snapshot(snapshot) = envelope.message().as_ref() else {
            panic!("latest value should be a force-order snapshot");
        };
        let display = snapshot.orders()[0].display(time::UtcOffset::UTC);

        assert_eq!(
            [display.price.as_ref(), display.quantity.as_ref()],
            ["100.00", "1.000"]
        );
        runtime.shutdown().await;
    }

    /// A listing the generation never named keeps no name at all, so the row is
    /// drawn as the localized unknown instead of a symbol or a digest.
    #[tokio::test]
    async fn a_row_the_generation_does_not_name_should_carry_no_label() {
        let route = test_support::spot_route();
        let key = test_support::listing_key(&route, "BTCUSDT");
        let unnamed = test_support::listing_key(&route, "ETHUSDT");
        let fixture = test_support::Fixture::new(vec![test_support::entry(
            &route,
            test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
            "BTCUSDT",
        )]);
        fixture
            .ensure()
            .await
            .expect("the fixture spot route publishes its catalog");
        let display = Arc::clone(fixture.catalog_arc());

        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            ForceOrderService::new(
                NamingSource {
                    listings: Arc::new(vec![key.clone(), unnamed.clone()]),
                    batch: Arc::new(vec![force_order(&route, "ETHUSDT", 1)]),
                },
                display,
                10,
            ),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(ForceOrderRequest::Load { routes: routes() })
            .await
            .unwrap();
        receive_status(&mut reliable, ForceOrderStatus::Ready).await;
        latest.changed().await.unwrap();
        let envelope = latest.next_unseen().expect("snapshot should be unseen");
        let ForceOrderStreamMessage::Snapshot(snapshot) = envelope.message().as_ref() else {
            panic!("latest value should be a force-order snapshot");
        };

        assert!(
            fixture
                .catalog()
                .cached_listing_instrument(&unnamed)
                .is_err(),
            "the fixture really published no name for the listing"
        );
        assert!(
            snapshot.orders()[0]
                .display(time::UtcOffset::UTC)
                .listing
                .is_none(),
            "a listing the generation does not name keeps no label"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn subscription_error_should_be_terminal_without_retrying() {
        let (binance_setup_sender, binance_setup) = setup();
        let (okx_setup_sender, okx_setup) = setup();
        let (binance, binance_events, _binance_dropped) = subscription();
        let (okx, _okx_events, _okx_dropped) = subscription();
        binance_setup_sender.send(Ok(binance)).unwrap();
        okx_setup_sender.send(Ok(okx)).unwrap();
        let source = source([binance_setup], [okx_setup]);
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            ForceOrderService::new(source.clone(), unnamed_display(), 10),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        runtime
            .client()
            .call(ForceOrderRequest::Load { routes: routes() })
            .await
            .unwrap();
        receive_status(&mut reliable, ForceOrderStatus::Ready).await;

        binance_events
            .send(Err(MarketError::Disconnected(DisconnectReason::Normal)))
            .await
            .unwrap();
        receive_status(
            &mut reliable,
            ForceOrderStatus::Error(MarketError::Disconnected(DisconnectReason::Normal)),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(source.attempts.load(Ordering::Acquire), 2);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_should_cancel_a_stream_blocked_by_reliable_backpressure() {
        let (binance_setup_sender, binance_setup) = setup();
        let (okx_setup_sender, okx_setup) = setup();
        let (binance, _binance_events, _binance_dropped) = subscription();
        let (okx, _okx_events, _okx_dropped) = subscription();
        binance_setup_sender.send(Ok(binance)).unwrap();
        okx_setup_sender.send(Ok(okx)).unwrap();
        let source = source([binance_setup], [okx_setup]);
        let config = WidgetRuntimeConfig::new(
            NonZeroUsize::new(1).unwrap(),
            NonZeroUsize::new(1).unwrap(),
            Duration::from_secs(1),
        );
        let runtime = WidgetRuntime::spawn_with_config(
            current_test_handle(),
            ForceOrderService::new(source.clone(), unnamed_display(), 10),
            config,
        );
        runtime
            .client()
            .call(ForceOrderRequest::Load { routes: routes() })
            .await
            .unwrap();
        wait_for_attempts(&source, 2).await;

        tokio::time::timeout(Duration::from_secs(1), runtime.shutdown())
            .await
            .expect("shutdown should cancel the blocked reliable publish");
    }
}

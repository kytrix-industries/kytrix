use std::{collections::VecDeque, convert::Infallible, future::Future};

use futures::StreamExt;
use i18n::{t_smol, translate::Translate};
use market_participants_connector::market::{
    error::MarketError, provider::MarketCapability, streams::OrderBookStream,
};
use market_participants_model::{
    listing::route::VenueListingKey,
    market::book::{
        ListingOrderBook, ListingOrderBookLevel, ListingOrderBookUpdate, MarketSequence,
    },
};
use smol_str::SmolStr;
use thiserror::Error;
use tokio::time::Instant;
use widget::{
    error::TranslatedError,
    protocol::WidgetProtocol,
    runtime::{ServiceContext, WidgetService},
    stream::StreamPublisher,
};

use super::model::{MAX_LEVELS_PER_SIDE, OrderBookMetadata, OrderBookSnapshot, is_after};

const SNAPSHOT_DEPTH: u16 = 5_000;
const SNAPSHOT_SYNC_BUFFER_CAPACITY: usize = 64;
const SNAPSHOT_SYNC_ATTEMPTS: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum OrderBookRequest {
    Load(OrderBookStreamKey),
    Retry(OrderBookStreamKey),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OrderBookResponse {
    StreamStarted,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct OrderBookStreamKey {
    listing: VenueListingKey,
    price_multiplier: u32,
    window_size: usize,
}

impl OrderBookStreamKey {
    pub(super) const fn new(
        listing: VenueListingKey,
        price_multiplier: u32,
        window_size: usize,
    ) -> Self {
        Self {
            listing,
            price_multiplier,
            window_size,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum OrderBookStatus {
    Connecting,
    LoadingSnapshot(OrderBookMetadata),
    Ready { revision: MarketSequence },
    Error(OrderBookStreamError),
}

#[derive(Clone, Debug)]
pub(super) enum OrderBookStreamMessage {
    Status(OrderBookStatus),
    Snapshot(OrderBookSnapshot),
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub(super) enum OrderBookStreamError {
    #[error(transparent)]
    MarketParticipant(#[from] MarketError),
    #[error("the order-book snapshot could not be synchronized with the live updates")]
    SnapshotSynchronizationFailed,
    #[error("the order-book snapshot stayed behind the venue's updates")]
    SnapshotBehindUpdates,
    #[error("the order-book snapshot is unavailable")]
    SnapshotUnavailable,
    #[error("the order-book subscription closed")]
    SubscriptionClosed,
}

impl TranslatedError for OrderBookStreamError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::MarketParticipant(error) => error.translated_message(translate),
            Self::SnapshotSynchronizationFailed => {
                t_smol!(translate, "error.order_book.snapshot_sync_failed")
            }
            Self::SnapshotBehindUpdates => {
                t_smol!(translate, "error.order_book.snapshot_behind_updates")
            }
            Self::SnapshotUnavailable => {
                t_smol!(translate, "error.order_book.snapshot_unavailable")
            }
            Self::SubscriptionClosed => t_smol!(translate, "error.order_book.subscription_closed"),
        }
    }
}

pub(super) struct OrderBookProtocol;

impl WidgetProtocol for OrderBookProtocol {
    type Request = OrderBookRequest;
    type Response = OrderBookResponse;
    type StreamKey = OrderBookStreamKey;
    type StreamMessage = OrderBookStreamMessage;
    type Error = Infallible;
}

pub(super) trait OrderBookSource: Clone + Send + Sync + 'static {
    type Subscription: OrderBookEventSubscription;

    fn subscribe_order_book(
        &self,
        listing: VenueListingKey,
        depth: u16,
    ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send;
}

pub(super) trait OrderBookEventSubscription: Send + 'static {
    fn metadata(&self) -> OrderBookMetadata;

    fn synchronize(&mut self) -> impl Future<Output = Result<(), OrderBookStreamError>> + Send;

    fn recv(&mut self) -> impl Future<Output = Option<Result<(), OrderBookStreamError>>> + Send;

    fn snapshot(&self) -> Option<OrderBookSnapshot>;
}

impl OrderBookSource for MarketCapability {
    type Subscription = ClientOrderBookSubscription;

    async fn subscribe_order_book(
        &self,
        listing: VenueListingKey,
        depth: u16,
    ) -> Result<Self::Subscription, MarketError> {
        let resolved = self.resolve_listing(&listing)?;
        let venue_depth = self.order_book_depth(&listing.route)?;
        let metadata = OrderBookMetadata::of(&resolved.rules, venue_depth);
        let depth = depth.clamp(1, SNAPSHOT_DEPTH).min(venue_depth);
        let inner = self
            .market(listing.route.clone())
            .order_book(listing.clone())
            .depth(depth)
            .subscribe()
            .await?;
        Ok(ClientOrderBookSubscription {
            inner,
            market: self.clone(),
            listing,
            metadata,
            depth,
            maximum_levels: usize::from(depth).min(MAX_LEVELS_PER_SIDE),
            book: None,
            initial_pending: false,
        })
    }
}

pub(super) struct ClientOrderBookSubscription {
    inner: OrderBookStream,
    market: MarketCapability,
    listing: VenueListingKey,
    metadata: OrderBookMetadata,
    depth: u16,
    maximum_levels: usize,
    book: Option<ListingOrderBook>,
    initial_pending: bool,
}

impl OrderBookEventSubscription for ClientOrderBookSubscription {
    fn metadata(&self) -> OrderBookMetadata {
        self.metadata.clone()
    }

    fn snapshot(&self) -> Option<OrderBookSnapshot> {
        self.book.as_ref().map(|book| {
            OrderBookSnapshot::from_market_participant(
                book,
                self.metadata.clone(),
                self.maximum_levels,
            )
        })
    }

    async fn synchronize(&mut self) -> Result<(), OrderBookStreamError> {
        self.book = None;
        self.initial_pending = false;

        let mut buffered = VecDeque::with_capacity(SNAPSHOT_SYNC_BUFFER_CAPACITY);
        let mut attempts = 0;
        loop {
            attempts += 1;
            if attempts > SNAPSHOT_SYNC_ATTEMPTS {
                return Err(OrderBookStreamError::SnapshotBehindUpdates);
            }
            let mut snapshot = self.fetch_opening_book(&mut buffered).await?;
            truncate_order_book(&mut snapshot, self.maximum_levels);

            loop {
                match merge_buffered_updates(&mut snapshot, &mut buffered, self.maximum_levels)? {
                    MergeOutcome::SnapshotOutdated => break,
                    MergeOutcome::Synchronized => {
                        self.book = Some(snapshot);
                        self.initial_pending = true;
                        return Ok(());
                    }
                    MergeOutcome::AwaitingUpdate => {
                        let Some(event) = self.inner.next().await else {
                            return Err(OrderBookStreamError::SubscriptionClosed);
                        };
                        push_buffered_update(&mut buffered, event?)?;
                    }
                }
            }
        }
    }

    async fn recv(&mut self) -> Option<Result<(), OrderBookStreamError>> {
        if self.initial_pending {
            self.initial_pending = false;
            return Some(Ok(()));
        }

        loop {
            let event = self.inner.next().await?;
            let update = match event {
                Ok(update) => update,
                Err(error) => return Some(Err(error.into())),
            };
            match apply_live_update(&mut self.book, update, self.maximum_levels) {
                Ok(Applied::Unchanged) => {}
                Ok(Applied::Changed) => return Some(Ok(())),
                Ok(Applied::NeedsSnapshot) => return Some(self.resynchronize().await),
                Err(error) => return Some(Err(error)),
            }
        }
    }
}

impl ClientOrderBookSubscription {
    async fn fetch_opening_book(
        &mut self,
        buffered: &mut VecDeque<ListingOrderBookUpdate>,
    ) -> Result<ListingOrderBook, OrderBookStreamError> {
        let fetch = self
            .market
            .market(self.listing.route.clone())
            .order_book(self.listing.clone())
            .depth(self.depth)
            .fetch();
        tokio::pin!(fetch);
        loop {
            tokio::select! {
                result = &mut fetch => return result.map_err(OrderBookStreamError::from),
                event = self.inner.next() => {
                    let Some(event) = event else {
                        return Err(OrderBookStreamError::SubscriptionClosed);
                    };
                    push_buffered_update(buffered, event?)?;
                }
            }
        }
    }

    async fn resynchronize(&mut self) -> Result<(), OrderBookStreamError> {
        self.synchronize().await?;
        self.initial_pending = false;
        Ok(())
    }
}

fn push_buffered_update(
    buffered: &mut VecDeque<ListingOrderBookUpdate>,
    update: ListingOrderBookUpdate,
) -> Result<(), OrderBookStreamError> {
    if buffered.len() == SNAPSHOT_SYNC_BUFFER_CAPACITY {
        return Err(OrderBookStreamError::SnapshotSynchronizationFailed);
    }
    buffered.push_back(update);
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MergeOutcome {
    Synchronized,
    SnapshotOutdated,
    AwaitingUpdate,
}

fn merge_buffered_updates(
    snapshot: &mut ListingOrderBook,
    buffered: &mut VecDeque<ListingOrderBookUpdate>,
    maximum_levels: usize,
) -> Result<MergeOutcome, OrderBookStreamError> {
    while buffered
        .front()
        .is_some_and(|update| !is_after(&update.last_sequence, &snapshot.sequence))
    {
        buffered.pop_front();
    }
    let Some(oldest) = buffered.front() else {
        return Ok(MergeOutcome::AwaitingUpdate);
    };
    if !oldest.snapshot {
        match oldest.first_sequence.is_after(&snapshot.sequence) {
            Some(true) if !range_covers_successor(&snapshot.sequence, oldest) => {
                return Ok(MergeOutcome::SnapshotOutdated);
            }
            None => return Ok(MergeOutcome::SnapshotOutdated),
            _ => {}
        }
        if numeric_range(oldest).is_none() {
            return Ok(MergeOutcome::SnapshotOutdated);
        }
    }

    let boundary = buffered
        .pop_front()
        .expect("a non-empty buffer has an oldest update");
    apply_update(snapshot, boundary, maximum_levels);
    while let Some(update) = buffered.pop_front() {
        match apply_order_book_update(snapshot, update, maximum_levels)? {
            Applied::Unchanged | Applied::Changed => {}
            Applied::NeedsSnapshot => return Ok(MergeOutcome::SnapshotOutdated),
        }
    }
    Ok(MergeOutcome::Synchronized)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Applied {
    Unchanged,
    Changed,
    NeedsSnapshot,
}

fn apply_order_book_update(
    book: &mut ListingOrderBook,
    update: ListingOrderBookUpdate,
    maximum_levels: usize,
) -> Result<Applied, OrderBookStreamError> {
    match update.last_sequence.is_after(&book.sequence) {
        Some(false) => return Ok(Applied::Unchanged),
        None if !update.snapshot => return Ok(Applied::NeedsSnapshot),
        _ => {}
    }
    if !update.snapshot && !update_continues(book, &update) {
        return Ok(Applied::NeedsSnapshot);
    }

    apply_update(book, update, maximum_levels);
    Ok(Applied::Changed)
}

fn apply_live_update(
    book: &mut Option<ListingOrderBook>,
    update: ListingOrderBookUpdate,
    maximum_levels: usize,
) -> Result<Applied, OrderBookStreamError> {
    let Some(book) = book.as_mut() else {
        return Ok(Applied::NeedsSnapshot);
    };
    apply_order_book_update(book, update, maximum_levels)
}

fn update_continues(book: &ListingOrderBook, update: &ListingOrderBookUpdate) -> bool {
    match &update.previous_sequence {
        Some(previous) => previous == &book.sequence,
        None => {
            let Some(next) = numeric_successor(&book.sequence) else {
                return false;
            };
            numeric_range(update).is_some_and(|(first, last)| first <= next && last >= next)
        }
    }
}

fn range_covers_successor(sequence: &MarketSequence, update: &ListingOrderBookUpdate) -> bool {
    let Some(next) = numeric_successor(sequence) else {
        return false;
    };
    numeric_range(update).is_some_and(|(first, _)| first <= next)
}

fn numeric_range(update: &ListingOrderBookUpdate) -> Option<(u64, u64)> {
    match (&update.first_sequence, &update.last_sequence) {
        (MarketSequence::Numeric(first), MarketSequence::Numeric(last)) => Some((*first, *last)),
        _ => None,
    }
}

fn numeric_successor(sequence: &MarketSequence) -> Option<u64> {
    match sequence {
        MarketSequence::Numeric(value) => value.checked_add(1),
        MarketSequence::Native(_) => None,
    }
}

fn apply_update(
    book: &mut ListingOrderBook,
    update: ListingOrderBookUpdate,
    maximum_levels: usize,
) {
    if update.snapshot {
        book.bids = update.bids;
        book.asks = update.asks;
    } else {
        apply_levels(&mut book.bids, update.bids, true);
        apply_levels(&mut book.asks, update.asks, false);
    }
    book.sequence = update.last_sequence;
    book.timestamp = update.timestamp.or(book.timestamp);
    truncate_order_book(book, maximum_levels);
}

fn truncate_order_book(book: &mut ListingOrderBook, maximum_levels: usize) {
    book.bids.truncate(maximum_levels);
    book.asks.truncate(maximum_levels);
}

fn apply_levels(
    levels: &mut Vec<ListingOrderBookLevel>,
    updates: Vec<ListingOrderBookLevel>,
    descending: bool,
) {
    for update in updates {
        if let Some(existing) = levels
            .iter_mut()
            .find(|level| level.price.value == update.price.value)
        {
            if update.quantity.value.is_zero() {
                levels.retain(|level| level.price.value != update.price.value);
            } else {
                existing.quantity = update.quantity;
            }
        } else if !update.quantity.value.is_zero() {
            levels.push(update);
        }
    }
    levels.sort_unstable_by(|left, right| {
        if descending {
            right.price.value.cmp(&left.price.value)
        } else {
            left.price.value.cmp(&right.price.value)
        }
    });
}

pub(super) struct OrderBookService<S = MarketCapability> {
    source: S,
    snapshot_depth: u16,
}

impl<S> OrderBookService<S> {
    pub(super) fn new(source: S, snapshot_depth: u16) -> Self {
        Self {
            source,
            snapshot_depth: snapshot_depth.clamp(1, SNAPSHOT_DEPTH),
        }
    }
}

impl<S> WidgetService<OrderBookProtocol> for OrderBookService<S>
where
    S: OrderBookSource,
{
    async fn run(self, mut context: ServiceContext<OrderBookProtocol>) {
        let mut active_key: Option<OrderBookStreamKey> = None;

        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            let (key, retry) = match request {
                OrderBookRequest::Load(key) => (key, false),
                OrderBookRequest::Retry(key) => (key, true),
            };
            if !retry && active_key.as_ref() == Some(&key) {
                let _ = reply.respond(Ok(OrderBookResponse::StreamStarted));
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
            let spec = OrderBookSpec {
                listing: key.listing.clone(),
                snapshot_depth: self.snapshot_depth,
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
            if reply.respond(Ok(OrderBookResponse::StreamStarted)).is_err()
                && let Some(previous) = active_key.take()
            {
                context.cancel_stream(&previous);
            }
        }
    }
}

#[derive(Clone)]
struct OrderBookSpec {
    listing: VenueListingKey,
    snapshot_depth: u16,
}

async fn run_stream<S>(
    source: S,
    spec: OrderBookSpec,
    publisher: StreamPublisher<OrderBookProtocol>,
) where
    S: OrderBookSource,
{
    if !publish_status(&publisher, OrderBookStatus::Connecting).await {
        return;
    }
    if let Err(error) = run_stream_once(&source, &spec, &publisher).await
        && publish_status(&publisher, OrderBookStatus::Error(error)).await
    {
        publisher.cancelled().await;
    }
}

async fn run_stream_once<S>(
    source: &S,
    spec: &OrderBookSpec,
    publisher: &StreamPublisher<OrderBookProtocol>,
) -> Result<(), OrderBookStreamError>
where
    S: OrderBookSource,
{
    let subscription = source.subscribe_order_book(spec.listing.clone(), spec.snapshot_depth);
    tokio::pin!(subscription);
    let mut subscription = tokio::select! {
        biased;
        () = publisher.cancelled() => return Ok(()),
        result = &mut subscription => result?,
    };

    if !publish_status(
        publisher,
        OrderBookStatus::LoadingSnapshot(subscription.metadata()),
    )
    .await
    {
        return Ok(());
    }
    if !synchronize(&mut subscription, publisher).await? {
        return Ok(());
    }

    let mut awaiting_ready = true;
    loop {
        tokio::select! {
            biased;
            () = publisher.cancelled() => return Ok(()),
            event = subscription.recv() => {
                let Some(event) = event else {
                    return Err(OrderBookStreamError::SubscriptionClosed);
                };
                event?;
                let Some(revision) = publish_snapshot(publisher, &subscription)? else {
                    return Ok(());
                };
                if awaiting_ready {
                    awaiting_ready = false;
                    if !publish_status(publisher, OrderBookStatus::Ready { revision }).await {
                        return Ok(());
                    }
                }
            }
        }
    }
}

async fn synchronize<S>(
    subscription: &mut S,
    publisher: &StreamPublisher<OrderBookProtocol>,
) -> Result<bool, OrderBookStreamError>
where
    S: OrderBookEventSubscription,
{
    let synchronization = subscription.synchronize();
    tokio::pin!(synchronization);
    tokio::select! {
        biased;
        () = publisher.cancelled() => Ok(false),
        result = &mut synchronization => result.map(|()| true),
    }
}

fn publish_snapshot<S>(
    publisher: &StreamPublisher<OrderBookProtocol>,
    subscription: &S,
) -> Result<Option<MarketSequence>, OrderBookStreamError>
where
    S: OrderBookEventSubscription,
{
    let snapshot = subscription
        .snapshot()
        .ok_or(OrderBookStreamError::SnapshotUnavailable)?;
    let revision = snapshot.sequence.clone();
    let published = publisher
        .publish_latest(OrderBookStreamMessage::Snapshot(snapshot))
        .is_ok();
    Ok(published.then_some(revision))
}

async fn publish_status(
    publisher: &StreamPublisher<OrderBookProtocol>,
    status: OrderBookStatus,
) -> bool {
    publisher
        .publish_reliable(OrderBookStreamMessage::Status(status))
        .await
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
        str::FromStr,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use market_participants_connector::{
        catalog::error::SourceError, market::domain::DisconnectReason,
    };
    use market_participants_model::{
        decimal::{Price, Quantity, Tick},
        identifier::IdentifierValue,
        listing::{route::VenueListingKey, rules::ListingPrecision},
        market::{
            price::MarketPrice,
            units::{MarketQuantity, QuantityUnit},
        },
    };
    use smol_str::SmolStr;
    use tokio::sync::{Notify, mpsc, oneshot};
    use widget::runtime::{WidgetRuntime, WidgetRuntimeConfig};

    use super::*;
    use crate::model::{PRICE_MULTIPLIERS, WINDOW_SIZES};

    type SetupReceiver = oneshot::Receiver<Result<FakeSubscription, MarketError>>;

    enum FakeEvent {
        Book(ListingOrderBook),
        Live(Result<ListingOrderBookUpdate, MarketError>),
    }

    #[derive(Clone)]
    struct FakeSource {
        setups: Arc<Mutex<VecDeque<SetupReceiver>>>,
        attempts: Arc<AtomicUsize>,
        depths: Arc<Mutex<Vec<u16>>>,
        started: Arc<Notify>,
    }

    struct FakeSubscription {
        metadata: OrderBookMetadata,
        maximum_levels: usize,
        book: Option<ListingOrderBook>,
        initial_pending: bool,
        events: mpsc::Receiver<FakeEvent>,
        synchronizations: Arc<AtomicUsize>,
        dropped: Option<oneshot::Sender<()>>,
    }

    struct FakeSubscriptionFixture {
        subscription: FakeSubscription,
        events: mpsc::Sender<FakeEvent>,
        synchronizations: Arc<AtomicUsize>,
        dropped: oneshot::Receiver<()>,
    }

    impl Drop for FakeSubscription {
        fn drop(&mut self) {
            if let Some(dropped) = self.dropped.take() {
                let _ = dropped.send(());
            }
        }
    }

    impl std::fmt::Debug for FakeSubscription {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("FakeSubscription")
        }
    }

    impl OrderBookSource for FakeSource {
        type Subscription = FakeSubscription;

        async fn subscribe_order_book(
            &self,
            _listing: VenueListingKey,
            depth: u16,
        ) -> Result<Self::Subscription, MarketError> {
            self.attempts.fetch_add(1, Ordering::AcqRel);
            self.depths
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(depth);
            self.started.notify_one();
            let setup = self
                .setups
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .ok_or(MarketError::ServiceClosed)?;
            setup.await.map_err(|_| MarketError::ServiceClosed)?
        }
    }

    impl OrderBookEventSubscription for FakeSubscription {
        fn metadata(&self) -> OrderBookMetadata {
            self.metadata.clone()
        }

        fn snapshot(&self) -> Option<OrderBookSnapshot> {
            self.book.as_ref().map(|book| {
                OrderBookSnapshot::from_market_participant(
                    book,
                    self.metadata.clone(),
                    self.maximum_levels,
                )
            })
        }

        async fn synchronize(&mut self) -> Result<(), OrderBookStreamError> {
            self.synchronizations.fetch_add(1, Ordering::AcqRel);
            loop {
                let event = self
                    .events
                    .recv()
                    .await
                    .ok_or(OrderBookStreamError::SubscriptionClosed)?;
                if let FakeEvent::Book(book) = event {
                    self.book = Some(book);
                    self.initial_pending = true;
                    return Ok(());
                }
            }
        }

        async fn recv(&mut self) -> Option<Result<(), OrderBookStreamError>> {
            if self.initial_pending {
                self.initial_pending = false;
                return Some(Ok(()));
            }
            loop {
                let FakeEvent::Live(event) = self.events.recv().await? else {
                    continue;
                };
                let update = match event {
                    Ok(update) => update,
                    Err(error) => return Some(Err(error.into())),
                };
                match apply_live_update(&mut self.book, update, self.maximum_levels) {
                    Ok(Applied::Unchanged) => {}
                    Ok(Applied::Changed) => return Some(Ok(())),
                    Ok(Applied::NeedsSnapshot) => {
                        return match self.synchronize().await {
                            Ok(()) => {
                                self.initial_pending = false;
                                Some(Ok(()))
                            }
                            Err(error) => Some(Err(error)),
                        };
                    }
                    Err(error) => return Some(Err(error)),
                }
            }
        }
    }

    fn source(setups: impl IntoIterator<Item = SetupReceiver>) -> FakeSource {
        FakeSource {
            setups: Arc::new(Mutex::new(setups.into_iter().collect())),
            attempts: Arc::new(AtomicUsize::new(0)),
            depths: Arc::new(Mutex::new(Vec::new())),
            started: Arc::new(Notify::new()),
        }
    }

    fn requested_depths(source: &FakeSource) -> Vec<u16> {
        source
            .depths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn setup() -> (
        oneshot::Sender<Result<FakeSubscription, MarketError>>,
        SetupReceiver,
    ) {
        oneshot::channel()
    }

    fn metadata() -> OrderBookMetadata {
        OrderBookMetadata {
            precision: ListingPrecision {
                price_tick: Tick::from_str("0.01").expect("price tick should parse"),
                quantity_tick: Tick::from_str("0.001").expect("quantity tick should parse"),
            },
            quote_asset: market_participants_connector::catalog::test_support::currency("USD"),
            quantity_unit: QuantityUnit::AssetUnits {
                asset: market_participants_connector::catalog::test_support::token(
                    "binance", "BTC",
                ),
            },
            depth: 5_000,
        }
    }

    fn subscription() -> FakeSubscriptionFixture {
        let (events, receiver) = mpsc::channel(8);
        let (dropped, drop_receiver) = oneshot::channel();
        let synchronizations = Arc::new(AtomicUsize::new(0));
        FakeSubscriptionFixture {
            subscription: FakeSubscription {
                metadata: metadata(),
                maximum_levels: 20,
                book: None,
                initial_pending: false,
                events: receiver,
                synchronizations: Arc::clone(&synchronizations),
                dropped: Some(dropped),
            },
            events,
            synchronizations,
            dropped: drop_receiver,
        }
    }

    fn listing(base: &str) -> VenueListingKey {
        market_participants_connector::catalog::test_support::listing_key(
            &market_participants_connector::catalog::test_support::spot_route(),
            &format!("{base}USDT"),
        )
    }

    fn book(sequence: u64, levels: usize) -> ListingOrderBook {
        ListingOrderBook {
            listing: listing("BTC"),
            sequence: MarketSequence::Numeric(sequence),
            timestamp: None,
            bids: (0..levels)
                .map(|offset| level(100_u64.saturating_sub(offset as u64), 1))
                .collect(),
            asks: (0..levels)
                .map(|offset| level(101_u64.saturating_add(offset as u64), 1))
                .collect(),
        }
    }

    fn native_book(sequence: &str, levels: usize) -> ListingOrderBook {
        ListingOrderBook {
            listing: listing("BTC"),
            sequence: native_sequence(sequence),
            timestamp: None,
            bids: (0..levels)
                .map(|offset| level(100 - offset as u64, 1))
                .collect(),
            asks: (0..levels)
                .map(|offset| level(101 + offset as u64, 1))
                .collect(),
        }
    }

    fn native_sequence(value: &str) -> MarketSequence {
        MarketSequence::Native(
            IdentifierValue::new(value).expect("a native watermark should be valid"),
        )
    }

    fn delta(
        first_sequence: u64,
        last_sequence: u64,
        previous_sequence: Option<u64>,
        bids: Vec<ListingOrderBookLevel>,
        asks: Vec<ListingOrderBookLevel>,
    ) -> ListingOrderBookUpdate {
        ListingOrderBookUpdate {
            listing: listing("BTC"),
            snapshot: false,
            first_sequence: MarketSequence::Numeric(first_sequence),
            last_sequence: MarketSequence::Numeric(last_sequence),
            previous_sequence: previous_sequence.map(MarketSequence::Numeric),
            timestamp: None,
            bids,
            asks,
        }
    }

    fn complete_update(
        sequence: MarketSequence,
        bids: Vec<ListingOrderBookLevel>,
        asks: Vec<ListingOrderBookLevel>,
    ) -> ListingOrderBookUpdate {
        ListingOrderBookUpdate {
            listing: listing("BTC"),
            snapshot: true,
            first_sequence: sequence.clone(),
            last_sequence: sequence,
            previous_sequence: None,
            timestamp: None,
            bids,
            asks,
        }
    }

    fn level(price_value: u64, quantity_value: u64) -> ListingOrderBookLevel {
        ListingOrderBookLevel {
            price: MarketPrice {
                value: Price::from(price_value),
                quote_asset: market_participants_connector::catalog::test_support::currency("USD"),
                basis: market_participants_model::listing::venue::PriceBasis::PerAssetUnit,
            },
            quantity: MarketQuantity {
                value: Quantity::from(quantity_value),
                unit: QuantityUnit::AssetUnits {
                    asset: market_participants_connector::catalog::test_support::token(
                        "binance", "BTC",
                    ),
                },
            },
        }
    }

    #[test]
    fn stale_delta_should_be_ignored_without_changing_the_book() {
        let mut current = book(10, 2);
        let original = current.clone();
        let applied = apply_order_book_update(
            &mut current,
            delta(9, 10, Some(8), vec![level(100, 99)], Vec::new()),
            20,
        )
        .expect("stale delta should not fail");

        assert_eq!(applied, Applied::Unchanged);
        assert_eq!(current, original);
    }

    #[test]
    fn stale_complete_snapshot_should_be_ignored_without_rolling_back_the_book() {
        let mut current = book(10, 1);
        let original = current.clone();
        let stale = complete_update(MarketSequence::Numeric(9), Vec::new(), Vec::new());

        let applied = apply_order_book_update(&mut current, stale, 20)
            .expect("stale complete snapshot should not fail");

        assert_eq!(applied, Applied::Unchanged);
        assert_eq!(current, original);
    }

    #[test]
    fn a_range_that_leaves_a_hole_should_ask_for_a_whole_snapshot() {
        let mut current = book(10, 2);
        let original = current.clone();
        let applied = apply_order_book_update(
            &mut current,
            delta(12, 12, None, Vec::new(), Vec::new()),
            20,
        )
        .expect("a hole is answered with a snapshot rather than a failure");

        assert_eq!(applied, Applied::NeedsSnapshot);
        assert_eq!(current, original);
    }

    #[test]
    fn a_contiguous_range_should_be_accepted_without_a_named_previous_update() {
        let mut current = book(10, 1);
        let applied = apply_order_book_update(
            &mut current,
            delta(11, 12, None, vec![level(99, 2)], Vec::new()),
            20,
        )
        .expect("a range that covers the next sequence should apply");

        assert_eq!(applied, Applied::Changed);
        assert_eq!(current.sequence, MarketSequence::Numeric(12));
        assert_eq!(current.bids.len(), 2);
    }

    #[test]
    fn a_zero_quantity_update_should_remove_the_price_level() {
        let mut current = book(10, 2);
        let applied = apply_order_book_update(
            &mut current,
            delta(11, 11, Some(10), vec![level(100, 0)], Vec::new()),
            20,
        )
        .expect("a removal is not a failure");

        assert_eq!(applied, Applied::Changed);
        assert_eq!(
            current
                .bids
                .iter()
                .map(|level| level.price.value)
                .collect::<Vec<_>>(),
            [Price::from(99_u64)]
        );
    }

    #[test]
    fn a_futures_range_should_follow_the_update_it_names() {
        let mut current = book(905, 1);
        let applied = apply_order_book_update(
            &mut current,
            delta(1_039, 2_000, Some(905), vec![level(99, 2)], Vec::new()),
            20,
        )
        .expect("an update naming the book's own sequence should apply");

        assert_eq!(applied, Applied::Changed);
        assert_eq!(current.sequence, MarketSequence::Numeric(2_000));
        assert_eq!(current.bids.len(), 2);
    }

    #[test]
    fn a_futures_range_naming_another_update_should_ask_for_a_whole_snapshot() {
        let mut current = book(905, 1);
        let applied = apply_order_book_update(
            &mut current,
            delta(1_039, 2_000, Some(906), Vec::new(), Vec::new()),
            20,
        )
        .expect("an update naming an update the book does not hold is not applied");

        assert_eq!(applied, Applied::NeedsSnapshot);
        assert_eq!(current.sequence, MarketSequence::Numeric(905));
    }

    #[test]
    fn a_native_increment_should_never_be_replayed_as_an_increment() {
        let mut current = native_book("1000", 1);
        let original = current.clone();
        let increment = ListingOrderBookUpdate {
            listing: listing("BTC"),
            snapshot: false,
            first_sequence: native_sequence("2000"),
            last_sequence: native_sequence("2000"),
            previous_sequence: None,
            timestamp: None,
            bids: vec![level(99, 2)],
            asks: Vec::new(),
        };

        let applied = apply_order_book_update(&mut current, increment, 20)
            .expect("an unorderable increment is not a failure");

        assert_eq!(applied, Applied::NeedsSnapshot);
        assert_eq!(current, original, "nothing was replayed");
    }

    #[test]
    fn a_native_watermark_should_only_ever_replace_the_whole_book() {
        let mut current = native_book("1000", 1);
        let complete = complete_update(native_sequence("2000"), vec![level(98, 2)], Vec::new());

        let applied =
            apply_order_book_update(&mut current, complete, 20).expect("a native snapshot applies");

        assert_eq!(applied, Applied::Changed);
        assert_eq!(current.sequence, native_sequence("2000"));
        assert_eq!(current.bids.len(), 1);
        assert_eq!(current.bids[0].price.value, Price::from(98_u64));
        assert!(current.asks.is_empty(), "a snapshot replaces both sides");
    }

    #[test]
    fn an_update_numbered_differently_should_ask_for_a_whole_snapshot() {
        let mut current = book(10, 1);
        let original = current.clone();
        let mut mismatched = delta(11, 11, None, vec![level(99, 2)], Vec::new());
        mismatched.first_sequence = native_sequence("11");
        mismatched.last_sequence = native_sequence("11");

        let applied = apply_order_book_update(&mut current, mismatched, 20)
            .expect("an uncomparable increment is not a failure");

        assert_eq!(applied, Applied::NeedsSnapshot);
        assert_eq!(current, original);
    }

    #[test]
    fn applied_delta_should_keep_both_sides_within_capacity() {
        let mut current = book(10, 3);
        let applied = apply_order_book_update(
            &mut current,
            delta(11, 11, Some(10), vec![level(97, 1)], vec![level(104, 1)]),
            2,
        )
        .expect("contiguous delta should apply");

        assert_eq!(applied, Applied::Changed);
        assert_eq!(current.sequence, MarketSequence::Numeric(11));
        assert_eq!(current.bids.len(), 2);
        assert_eq!(current.asks.len(), 2);
        assert_eq!(current.bids[0].price.value, Price::from(100_u64));
        assert_eq!(current.asks[0].price.value, Price::from(101_u64));
    }

    fn buffered(
        deltas: impl IntoIterator<Item = ListingOrderBookUpdate>,
    ) -> VecDeque<ListingOrderBookUpdate> {
        deltas.into_iter().collect()
    }

    #[test]
    fn updates_the_snapshot_already_contains_should_be_dropped() {
        let mut current = book(10, 1);
        let mut buffered = buffered([
            delta(9, 10, Some(9), vec![level(100, 99)], Vec::new()),
            delta(11, 11, Some(10), vec![level(99, 2)], Vec::new()),
        ]);

        let outcome = merge_buffered_updates(&mut current, &mut buffered, 20)
            .expect("stale updates should be discarded");

        assert_eq!(outcome, MergeOutcome::Synchronized);
        assert_eq!(current.sequence, MarketSequence::Numeric(11));
        assert_eq!(current.bids.len(), 2);
    }

    #[test]
    fn an_update_ending_at_the_snapshot_should_contribute_nothing() {
        let mut current = book(10, 1);
        let mut buffered = buffered([
            delta(8, 10, Some(7), vec![level(100, 99)], Vec::new()),
            delta(11, 11, Some(10), vec![level(99, 2)], Vec::new()),
        ]);

        let outcome = merge_buffered_updates(&mut current, &mut buffered, 20)
            .expect("the boundary update should be absorbed");

        assert_eq!(outcome, MergeOutcome::Synchronized);
        assert_eq!(current.sequence, MarketSequence::Numeric(11));
        assert_eq!(current.bids[0].price.value, Price::from(100_u64));
        assert_eq!(current.bids.len(), 2);
    }

    #[test]
    fn an_update_spanning_the_snapshot_should_bridge_it() {
        let mut current = book(10, 1);
        let mut buffered = buffered([delta(8, 12, Some(7), vec![level(99, 2)], Vec::new())]);

        let outcome = merge_buffered_updates(&mut current, &mut buffered, 20)
            .expect("a spanning update should bridge the snapshot");

        assert_eq!(outcome, MergeOutcome::Synchronized);
        assert_eq!(current.sequence, MarketSequence::Numeric(12));
        assert_eq!(current.bids.len(), 2);
    }

    #[test]
    fn a_snapshot_newer_than_every_buffered_update_should_wait_for_the_next_one() {
        let mut current = book(20, 1);
        let mut buffered = buffered([delta(10, 10, Some(9), Vec::new(), Vec::new())]);

        let outcome = merge_buffered_updates(&mut current, &mut buffered, 20)
            .expect("a newer snapshot is not a failure");

        assert_eq!(outcome, MergeOutcome::AwaitingUpdate);
        assert!(
            buffered.is_empty(),
            "an update the snapshot contains is gone"
        );
        assert_eq!(current.sequence, MarketSequence::Numeric(20));
    }

    #[test]
    fn a_snapshot_the_updates_have_already_passed_should_ask_for_another_one() {
        let mut current = book(905, 1);
        let mut buffered = buffered([delta(
            907,
            40_839,
            Some(906),
            vec![level(99, 2)],
            Vec::new(),
        )]);

        let outcome = merge_buffered_updates(&mut current, &mut buffered, 20)
            .expect("a snapshot behind its own updates is not a failure");

        assert_eq!(outcome, MergeOutcome::SnapshotOutdated);
        assert_eq!(
            current.sequence,
            MarketSequence::Numeric(905),
            "nothing was applied to the snapshot"
        );
        assert_eq!(
            buffered.len(),
            1,
            "the update is kept for the next snapshot"
        );
    }

    #[test]
    fn a_gap_after_the_boundary_should_ask_for_another_snapshot() {
        let mut current = book(10, 1);
        let mut buffered = buffered([
            delta(11, 11, Some(10), vec![level(99, 2)], Vec::new()),
            delta(13, 13, Some(12), vec![level(98, 2)], Vec::new()),
        ]);

        let outcome = merge_buffered_updates(&mut current, &mut buffered, 20)
            .expect("a gap after the boundary is not a failure");

        assert_eq!(outcome, MergeOutcome::SnapshotOutdated);
        assert_eq!(current.sequence, MarketSequence::Numeric(11));
        assert_eq!(current.bids.len(), 2);
    }

    #[test]
    fn buffering_should_stop_at_the_capacity_it_was_given() {
        let mut buffered = VecDeque::with_capacity(SNAPSHOT_SYNC_BUFFER_CAPACITY);
        for sequence in 0..SNAPSHOT_SYNC_BUFFER_CAPACITY as u64 {
            push_buffered_update(
                &mut buffered,
                delta(sequence, sequence, None, Vec::new(), Vec::new()),
            )
            .expect("a buffer within capacity should accept updates");
        }

        let error = push_buffered_update(
            &mut buffered,
            delta(u64::MAX, u64::MAX, None, Vec::new(), Vec::new()),
        )
        .expect_err("a full buffer must refuse another update");

        assert_eq!(error, OrderBookStreamError::SnapshotSynchronizationFailed);
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
        reliable: &mut widget::stream::ReliableStream<OrderBookProtocol>,
        expected: OrderBookStatus,
    ) {
        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(1), reliable.recv())
                .await
                .expect("status should arrive")
                .expect("reliable stream should remain open");
            if let OrderBookStreamMessage::Status(status) = envelope.message().as_ref()
                && *status == expected
            {
                return;
            }
        }
    }

    async fn receive_error(
        reliable: &mut widget::stream::ReliableStream<OrderBookProtocol>,
        expected: OrderBookStreamError,
    ) {
        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(1), reliable.recv())
                .await
                .expect("error should arrive")
                .expect("reliable stream should remain open");
            if let OrderBookStreamMessage::Status(OrderBookStatus::Error(error)) =
                envelope.message().as_ref()
            {
                assert_eq!(*error, expected);
                return;
            }
        }
    }

    #[tokio::test]
    async fn a_new_scope_should_cancel_the_previous_subscription_setup() {
        let (mut first_setup_sender, first_setup) = setup();
        let (second_setup_sender, second_setup) = setup();
        let FakeSubscriptionFixture {
            subscription: second,
            events: second_events,
            ..
        } = subscription();
        second_events
            .send(FakeEvent::Book(book(2, 1)))
            .await
            .unwrap();
        second_setup_sender.send(Ok(second)).unwrap();
        let source = source([first_setup, second_setup]);
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            OrderBookService::new(source.clone(), 20),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();

        let first_client = runtime.client();
        let first = tokio::spawn(async move {
            first_client
                .call(OrderBookRequest::Load(OrderBookStreamKey::new(
                    listing("BTC"),
                    1,
                    WINDOW_SIZES[0],
                )))
                .await
        });
        source.started.notified().await;
        runtime
            .client()
            .call(OrderBookRequest::Load(OrderBookStreamKey::new(
                listing("ETH"),
                1,
                WINDOW_SIZES[0],
            )))
            .await
            .unwrap();

        assert!(first.await.expect("first load task should finish").is_ok());
        tokio::time::timeout(Duration::from_secs(1), first_setup_sender.closed())
            .await
            .expect("pending setup should be cancelled promptly");
        wait_for_attempts(&source, 2).await;
        receive_status(
            &mut reliable,
            OrderBookStatus::Ready {
                revision: MarketSequence::Numeric(2),
            },
        )
        .await;
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_changed_grouping_should_start_the_stream_of_the_new_scope() {
        let (first_setup_sender, first_setup) = setup();
        let (second_setup_sender, second_setup) = setup();
        let FakeSubscriptionFixture {
            subscription: first,
            events: first_events,
            ..
        } = subscription();
        let FakeSubscriptionFixture {
            subscription: second,
            events: second_events,
            ..
        } = subscription();
        first_events
            .send(FakeEvent::Book(book(1, 1)))
            .await
            .unwrap();
        second_events
            .send(FakeEvent::Book(book(2, 1)))
            .await
            .unwrap();
        first_setup_sender.send(Ok(first)).unwrap();
        second_setup_sender.send(Ok(second)).unwrap();
        let source = source([first_setup, second_setup]);
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            OrderBookService::new(source.clone(), 20),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        runtime
            .client()
            .call(OrderBookRequest::Load(OrderBookStreamKey::new(
                listing("BTC"),
                1,
                WINDOW_SIZES[0],
            )))
            .await
            .unwrap();
        receive_status(
            &mut reliable,
            OrderBookStatus::Ready {
                revision: MarketSequence::Numeric(1),
            },
        )
        .await;

        runtime
            .client()
            .call(OrderBookRequest::Load(OrderBookStreamKey::new(
                listing("BTC"),
                PRICE_MULTIPLIERS[1],
                WINDOW_SIZES[0],
            )))
            .await
            .expect("a new grouping is a new scope");

        wait_for_attempts(&source, 2).await;
        receive_status(
            &mut reliable,
            OrderBookStatus::Ready {
                revision: MarketSequence::Numeric(2),
            },
        )
        .await;
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_load_should_not_start_a_stream() {
        let source = source([]);
        let runtime = WidgetRuntime::spawn(
            current_test_handle(),
            OrderBookService::new(source.clone(), 20),
        );
        let mut latest = runtime.latest_stream();

        let error = runtime
            .client()
            .call_with_timeout(
                OrderBookRequest::Load(OrderBookStreamKey::new(listing("BTC"), 1, WINDOW_SIZES[0])),
                Duration::ZERO,
            )
            .await
            .expect_err("a request whose deadline has passed should not be answered");

        assert!(matches!(
            error,
            widget::rpc::RpcCallError::DeadlineExceeded { .. }
        ));
        assert_eq!(source.attempts.load(Ordering::Acquire), 0);
        assert!(latest.next_unseen().is_none());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn the_service_should_ask_the_venue_for_the_configured_depth() {
        let (setup_sender, setup) = setup();
        let FakeSubscriptionFixture {
            subscription,
            events,
            ..
        } = subscription();
        events.send(FakeEvent::Book(book(1, 1))).await.unwrap();
        setup_sender.send(Ok(subscription)).unwrap();
        let source = source([setup]);
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            OrderBookService::new(source.clone(), 7),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        runtime
            .client()
            .call(OrderBookRequest::Load(OrderBookStreamKey::new(
                listing("BTC"),
                1,
                WINDOW_SIZES[0],
            )))
            .await
            .unwrap();
        receive_status(
            &mut reliable,
            OrderBookStatus::Ready {
                revision: MarketSequence::Numeric(1),
            },
        )
        .await;

        assert_eq!(requested_depths(&source), [7]);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn replacement_should_drop_the_old_subscription_and_suppress_late_books() {
        let (first_setup_sender, first_setup) = setup();
        let (second_setup_sender, second_setup) = setup();
        let FakeSubscriptionFixture {
            subscription: first,
            events: first_events,
            dropped: first_dropped,
            ..
        } = subscription();
        let FakeSubscriptionFixture {
            subscription: second,
            events: second_events,
            ..
        } = subscription();
        first_events
            .send(FakeEvent::Book(book(1, 1)))
            .await
            .unwrap();
        second_events
            .send(FakeEvent::Book(book(10, 1)))
            .await
            .unwrap();
        first_setup_sender.send(Ok(first)).unwrap();
        second_setup_sender.send(Ok(second)).unwrap();
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            OrderBookService::new(source([first_setup, second_setup]), 20),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(OrderBookRequest::Load(OrderBookStreamKey::new(
                listing("BTC"),
                1,
                WINDOW_SIZES[0],
            )))
            .await
            .unwrap();
        receive_status(
            &mut reliable,
            OrderBookStatus::Ready {
                revision: MarketSequence::Numeric(1),
            },
        )
        .await;
        latest.changed().await.unwrap();
        let _ = latest.next_unseen();
        runtime
            .client()
            .call(OrderBookRequest::Load(OrderBookStreamKey::new(
                listing("ETH"),
                1,
                WINDOW_SIZES[0],
            )))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), first_dropped)
            .await
            .expect("old subscription should be dropped")
            .unwrap();
        assert!(
            first_events
                .send(FakeEvent::Book(book(2, 1)))
                .await
                .is_err()
        );
        receive_status(
            &mut reliable,
            OrderBookStatus::Ready {
                revision: MarketSequence::Numeric(10),
            },
        )
        .await;
        latest.changed().await.unwrap();
        let initial = latest
            .next_unseen()
            .expect("replacement snapshot should exist");
        let OrderBookStreamMessage::Snapshot(initial) = initial.message().as_ref() else {
            panic!("latest value should be an order-book snapshot");
        };
        assert_eq!(initial.sequence, MarketSequence::Numeric(10));

        second_events
            .send(FakeEvent::Live(Ok(delta(
                11,
                11,
                Some(10),
                vec![level(99, 2)],
                Vec::new(),
            ))))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), latest.changed())
            .await
            .expect("replacement update should publish")
            .unwrap();
        let updated = latest.next_unseen().unwrap();
        let OrderBookStreamMessage::Snapshot(updated) = updated.message().as_ref() else {
            panic!("latest value should be an order-book snapshot");
        };
        assert_eq!(updated.sequence, MarketSequence::Numeric(11));
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_gap_in_the_live_stream_should_fall_back_to_a_whole_snapshot() {
        let (setup_sender, setup) = setup();
        let FakeSubscriptionFixture {
            subscription,
            events,
            synchronizations,
            ..
        } = subscription();
        events.send(FakeEvent::Book(book(1, 1))).await.unwrap();
        setup_sender.send(Ok(subscription)).unwrap();
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            OrderBookService::new(source([setup]), 20),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();
        runtime
            .client()
            .call(OrderBookRequest::Load(OrderBookStreamKey::new(
                listing("BTC"),
                1,
                WINDOW_SIZES[0],
            )))
            .await
            .unwrap();
        receive_status(
            &mut reliable,
            OrderBookStatus::Ready {
                revision: MarketSequence::Numeric(1),
            },
        )
        .await;
        latest.changed().await.unwrap();
        assert_eq!(published_revision(&mut latest), MarketSequence::Numeric(1));
        assert_eq!(synchronizations.load(Ordering::Acquire), 1);

        events
            .send(FakeEvent::Live(Ok(delta(
                30,
                30,
                None,
                vec![level(99, 2)],
                Vec::new(),
            ))))
            .await
            .unwrap();
        events.send(FakeEvent::Book(book(20, 2))).await.unwrap();

        tokio::time::timeout(Duration::from_secs(1), latest.changed())
            .await
            .expect("the fresh snapshot should publish")
            .unwrap();
        assert_eq!(published_revision(&mut latest), MarketSequence::Numeric(20));
        assert_eq!(
            synchronizations.load(Ordering::Acquire),
            2,
            "the book was replaced by a whole snapshot"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), reliable.recv())
                .await
                .is_err(),
            "a gap publishes no failure of its own"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_repeated_load_should_not_replace_the_running_stream() {
        let (setup_sender, setup) = setup();
        let FakeSubscriptionFixture {
            subscription,
            events,
            ..
        } = subscription();
        events.send(FakeEvent::Book(book(1, 1))).await.unwrap();
        setup_sender.send(Ok(subscription)).unwrap();
        let source = source([setup]);
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            OrderBookService::new(source.clone(), 20),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let key = OrderBookStreamKey::new(listing("BTC"), 1, WINDOW_SIZES[0]);
        runtime
            .client()
            .call(OrderBookRequest::Load(key.clone()))
            .await
            .unwrap();
        receive_status(
            &mut reliable,
            OrderBookStatus::Ready {
                revision: MarketSequence::Numeric(1),
            },
        )
        .await;

        runtime
            .client()
            .call(OrderBookRequest::Load(key))
            .await
            .expect("a repeated load should be accepted");
        tokio::time::sleep(Duration::from_millis(25)).await;

        assert_eq!(source.attempts.load(Ordering::Acquire), 1);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn every_applied_update_should_be_published() {
        let (setup_sender, setup) = setup();
        let FakeSubscriptionFixture {
            subscription,
            events,
            ..
        } = subscription();
        events.send(FakeEvent::Book(book(1, 1))).await.unwrap();
        setup_sender.send(Ok(subscription)).unwrap();
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            OrderBookService::new(source([setup]), 20),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let mut latest = runtime.latest_stream();
        runtime
            .client()
            .call(OrderBookRequest::Load(OrderBookStreamKey::new(
                listing("BTC"),
                1,
                WINDOW_SIZES[0],
            )))
            .await
            .unwrap();
        receive_status(
            &mut reliable,
            OrderBookStatus::Ready {
                revision: MarketSequence::Numeric(1),
            },
        )
        .await;
        latest.changed().await.unwrap();
        assert_eq!(published_revision(&mut latest), MarketSequence::Numeric(1));

        for sequence in 2..=4 {
            events
                .send(FakeEvent::Live(Ok(complete_update(
                    MarketSequence::Numeric(sequence),
                    vec![level(99, sequence)],
                    Vec::new(),
                ))))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(1), latest.changed())
                .await
                .expect("an accepted update should publish")
                .unwrap();
            assert_eq!(
                published_revision(&mut latest),
                MarketSequence::Numeric(sequence)
            );
        }
        runtime.shutdown().await;
    }

    fn published_revision(
        latest: &mut widget::stream::LatestStream<OrderBookProtocol>,
    ) -> MarketSequence {
        let envelope = latest
            .next_unseen()
            .expect("a snapshot should be published");
        let OrderBookStreamMessage::Snapshot(snapshot) = envelope.message().as_ref() else {
            panic!("latest value should be an order-book snapshot");
        };
        snapshot.sequence.clone()
    }

    #[tokio::test]
    async fn setup_error_should_be_terminal_until_the_same_key_is_loaded_again() {
        let (setup_sender, initial_setup) = setup();
        let (retry_setup_sender, retry_setup) = setup();
        let FakeSubscriptionFixture {
            subscription: retry_subscription,
            events: retry_events,
            ..
        } = subscription();
        retry_events
            .send(FakeEvent::Book(book(2, 1)))
            .await
            .unwrap();
        retry_setup_sender.send(Ok(retry_subscription)).unwrap();
        setup_sender
            .send(Err(MarketError::Timeout {
                operation: SmolStr::new_static("subscribe order book"),
                timeout_ms: 1_000,
            }))
            .unwrap();
        let source = source([initial_setup, retry_setup]);
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            OrderBookService::new(source.clone(), 20),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        let key = OrderBookStreamKey::new(listing("BTC"), 1, WINDOW_SIZES[0]);
        runtime
            .client()
            .call(OrderBookRequest::Load(key.clone()))
            .await
            .unwrap();

        receive_error(
            &mut reliable,
            MarketError::Timeout {
                operation: SmolStr::new_static("subscribe order book"),
                timeout_ms: 1_000,
            }
            .into(),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(source.attempts.load(Ordering::Acquire), 1);

        runtime
            .client()
            .call(OrderBookRequest::Retry(key))
            .await
            .expect("an explicit retry should replace the failed stream");
        receive_status(
            &mut reliable,
            OrderBookStatus::Ready {
                revision: MarketSequence::Numeric(2),
            },
        )
        .await;
        assert_eq!(source.attempts.load(Ordering::Acquire), 2);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn event_error_should_be_terminal_without_retrying() {
        let (setup_sender, setup) = setup();
        let FakeSubscriptionFixture {
            subscription,
            events,
            ..
        } = subscription();
        events.send(FakeEvent::Book(book(1, 1))).await.unwrap();
        setup_sender.send(Ok(subscription)).unwrap();
        let source = source([setup]);
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            OrderBookService::new(source.clone(), 20),
        );
        let mut reliable = runtime.take_reliable_stream().unwrap();
        runtime
            .client()
            .call(OrderBookRequest::Load(OrderBookStreamKey::new(
                listing("BTC"),
                1,
                WINDOW_SIZES[0],
            )))
            .await
            .unwrap();
        receive_status(
            &mut reliable,
            OrderBookStatus::Ready {
                revision: MarketSequence::Numeric(1),
            },
        )
        .await;

        events
            .send(FakeEvent::Live(Err(MarketError::Disconnected(
                DisconnectReason::Normal,
            ))))
            .await
            .unwrap();
        receive_error(
            &mut reliable,
            MarketError::Disconnected(DisconnectReason::Normal).into(),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(source.attempts.load(Ordering::Acquire), 1);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_should_cancel_a_stream_blocked_by_reliable_backpressure() {
        let (setup_sender, setup) = setup();
        let FakeSubscriptionFixture {
            subscription,
            events,
            ..
        } = subscription();
        events.send(FakeEvent::Book(book(1, 1))).await.unwrap();
        setup_sender.send(Ok(subscription)).unwrap();
        let source = source([setup]);
        let config = WidgetRuntimeConfig::new(
            NonZeroUsize::new(1).unwrap(),
            NonZeroUsize::new(1).unwrap(),
            Duration::from_secs(1),
        );
        let runtime = WidgetRuntime::spawn_with_config(
            current_test_handle(),
            OrderBookService::new(source.clone(), 20),
            config,
        );
        runtime
            .client()
            .call(OrderBookRequest::Load(OrderBookStreamKey::new(
                listing("BTC"),
                1,
                WINDOW_SIZES[0],
            )))
            .await
            .unwrap();
        wait_for_attempts(&source, 1).await;

        tokio::time::timeout(Duration::from_secs(1), runtime.shutdown())
            .await
            .expect("shutdown should cancel the blocked reliable publish");
    }

    #[derive(Clone)]
    struct UnresolvableSource {
        failure: MarketError,
    }

    impl OrderBookSource for UnresolvableSource {
        type Subscription = FakeSubscription;

        async fn subscribe_order_book(
            &self,
            _listing: VenueListingKey,
            _depth: u16,
        ) -> Result<Self::Subscription, MarketError> {
            Err(self.failure.clone())
        }
    }

    #[tokio::test]
    async fn a_catalog_failure_on_the_opening_seam_should_be_reported_as_itself() {
        let failure = MarketError::CatalogSource(SourceError::Network {
            reason: SmolStr::new_static("the test catalog is unreachable"),
        });
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            OrderBookService::new(
                UnresolvableSource {
                    failure: failure.clone(),
                },
                20,
            ),
        );
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("reliable stream should be available");
        runtime
            .client()
            .call(OrderBookRequest::Load(OrderBookStreamKey::new(
                listing("BTC"),
                1,
                WINDOW_SIZES[0],
            )))
            .await
            .expect("load request should start a stream");

        let reported = loop {
            let envelope = tokio::time::timeout(Duration::from_secs(1), reliable.recv())
                .await
                .expect("the failure should be published")
                .expect("the reliable stream should remain open");
            if let OrderBookStreamMessage::Status(OrderBookStatus::Error(error)) =
                envelope.message().as_ref()
            {
                break error.clone();
            }
        };

        assert_eq!(reported, OrderBookStreamError::MarketParticipant(failure));
        assert!(
            matches!(
                reported,
                OrderBookStreamError::MarketParticipant(MarketError::CatalogSource(_))
            ),
            "a catalog that could not be fetched is reported as the catalog failure it is"
        );
        runtime.shutdown().await;
    }
}

//! The watch-list protocol, its background service, and the seam the service
//! reads tickers through.
//!
//! One service instance follows the whole list the widget configures. A
//! `Replace` request states the complete set of listings to follow and is diffed
//! against the streams that are already open, so adding or removing one listing
//! neither reopens the others nor loses the snapshots they hold. A `Retry`
//! request reopens exactly one listing's stream, which is how a row whose stream
//! failed is brought back. A `MarketReloaded` request states a complete set to
//! follow the way `Replace` does and opens every one of its streams again, which
//! is what the market they are on being loaded again needs: every stream resolved
//! its increments from the generation that load replaced.
//!
//! # Stream generations
//!
//! Which messages are still current belongs to the framework, not to this
//! protocol: `start_stream` keeps the generation of a key that is already live,
//! `replace_stream` opens a new one, and the framework drops everything a retired
//! generation still had in flight. So a message carries nothing but what it says,
//! and the widget never sees one that belongs to a stream it has left.
//!
//! The service's own state follows the same rule at the request boundary: it
//! changes the followed set only after the caller has the response, so a request
//! whose answer was lost leaves no half-applied list behind.

use std::{future::Future, sync::Arc};

use ahash::{HashSet, HashSetExt};
use futures::StreamExt;
use gpui_kit::{App, SharedString};
use i18n::t;
use market_participants_connector::market::{
    domain::DisconnectReason, error::MarketError, provider::MarketCapability, streams::TickerStream,
};
use market_participants_model::{
    listing::{route::VenueListingKey, rules::ListingPrecision},
    market::selection::ListingTicker,
};
use thiserror::Error;
use tokio::{task::JoinSet, time::Instant};
use widget::{
    error::LocalizedError,
    protocol::WidgetProtocol,
    rpc::RpcRequestContext,
    runtime::{ServiceContext, WidgetService},
    stream::{StreamPublishError, StreamPublisher},
};

use super::model::{
    MAX_WATCH_LIST_ENTRIES, WatchListStatus, WatchListStreamError, WatchListStreamEvent,
};

/// What the widget asks the service to do with the followed set.
///
/// The listings a request names are the ones the widget can read: a followed listing whose
/// market participant is not loaded has no stream for this service to hold, and the widget
/// keeps its row itself. A request therefore states a subset of what the user follows, and
/// the widget is the one that decides which part of the list this is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum WatchListRequest {
    /// Follows exactly these listings, keeping the streams that are already open
    /// for the listings that stay.
    Replace { entries: Vec<VenueListingKey> },
    /// Reopens one listing's stream in place.
    Retry { key: VenueListingKey },
    /// Follows exactly these listings, opening every one of their streams again.
    ///
    /// The market they are on was loaded again: every one of those streams resolved its
    /// increments from the generation that load replaced, so each of them has to resolve
    /// again. Unlike `Replace` it keeps nothing open — every listing it names has a stream
    /// that is stale, whether or not this service was already following it — and unlike
    /// `Retry` it is one request for all of them, because a venue commit moves every listing
    /// of that venue at once and the widget may have only one request in flight.
    MarketReloaded { entries: Vec<VenueListingKey> },
}

/// The service accepted the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WatchListResponse {
    Applied,
}

/// Why the service refused a request.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub(super) enum WatchListServiceError {
    #[error("the watch list follows {actual} listings; the maximum is {maximum}")]
    TooManyEntries { actual: usize, maximum: usize },
    #[error("the watch list already follows {key:?}")]
    DuplicateEntry { key: VenueListingKey },
    #[error("the listing is not followed")]
    EntryNotActive,
}

impl LocalizedError for WatchListServiceError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::TooManyEntries { maximum, .. } => {
                t!(cx, "error.watch_list.capacity", maximum = maximum)
            }
            Self::DuplicateEntry { .. } => t!(cx, "error.watch_list.duplicate_entry"),
            Self::EntryNotActive => t!(cx, "error.watch_list.entry_not_active"),
        }
    }
}

pub(super) struct WatchListProtocol;

impl WidgetProtocol for WatchListProtocol {
    type Request = WatchListRequest;
    type Response = WatchListResponse;
    type StreamKey = VenueListingKey;
    type StreamMessage = WatchListStreamEvent;
    type Error = WatchListServiceError;
}

/// The market data one watch list reads through.
///
/// The service is written against this seam rather than against the market participants
/// connector, so a test can drive every ordering the real venues can produce
/// without a venue.
pub(super) trait TickerSource: Clone + Send + Sync + 'static {
    type Subscription: TickerEventSubscription;

    /// The latest ticker of `listing`.
    ///
    /// Resolving the listing is part of both calls, so a lookup that fails is
    /// reported as the failure it is — a catalog or transport error — rather than
    /// as a listing that does not exist.
    fn fetch_ticker(
        &self,
        listing: VenueListingKey,
    ) -> impl Future<Output = Result<ListingTicker, MarketError>> + Send;

    fn subscribe_ticker(
        &self,
        listing: VenueListingKey,
    ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send;
}

/// One live ticker stream.
pub(super) trait TickerEventSubscription: Send + 'static {
    /// The increments of the listing this stream carries tickers of.
    ///
    /// They are read from the resolution that opened the stream, so every row the
    /// stream publishes is written at the precision of the listing it follows.
    fn precision(&self) -> ListingPrecision;

    fn recv(
        &mut self,
    ) -> impl Future<Output = Option<Result<Arc<ListingTicker>, MarketError>>> + Send;
}

impl TickerSource for MarketCapability {
    type Subscription = ClientTickerSubscription;

    fn fetch_ticker(
        &self,
        listing: VenueListingKey,
    ) -> impl Future<Output = Result<ListingTicker, MarketError>> + Send {
        let capability = self.clone();
        async move {
            let route = listing.route.clone();
            capability.market(route).ticker(listing).fetch().await
        }
    }

    fn subscribe_ticker(
        &self,
        listing: VenueListingKey,
    ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send {
        let capability = self.clone();
        async move {
            // The listing's own rules carry the increments its price is written at,
            // and resolving the listing is what rejects one this route no longer
            // publishes before a stream is opened for it.
            let resolved = capability.resolve_listing(&listing)?;
            let route = resolved.key.route.clone();
            let inner = capability.market(route).ticker(listing).subscribe().await?;
            Ok(ClientTickerSubscription {
                inner,
                precision: ListingPrecision::of(&resolved.rules),
            })
        }
    }
}

/// One ticker stream from the market participants connector.
pub(super) struct ClientTickerSubscription {
    inner: TickerStream,
    precision: ListingPrecision,
}

impl TickerEventSubscription for ClientTickerSubscription {
    fn precision(&self) -> ListingPrecision {
        self.precision
    }

    async fn recv(&mut self) -> Option<Result<Arc<ListingTicker>, MarketError>> {
        self.inner.next().await.map(|event| event.map(Arc::new))
    }
}

/// The service that follows one watch list.
pub(super) struct WatchListService<S = MarketCapability> {
    source: S,
}

impl<S> WatchListService<S> {
    pub(super) const fn new(source: S) -> Self {
        Self { source }
    }
}

impl<S> WidgetService<WatchListProtocol> for WatchListService<S>
where
    S: TickerSource,
{
    async fn run(self, mut context: ServiceContext<WatchListProtocol>) {
        let mut active = HashSet::with_capacity(MAX_WATCH_LIST_ENTRIES);

        while let Some(request) = context.next_rpc().await {
            let (mut reply, request) = request.into_parts();
            match request {
                WatchListRequest::Replace { entries } => {
                    let replacement = match validate_replacement(&entries) {
                        Ok(replacement) => replacement,
                        Err(error) => {
                            let _ = reply.respond(Err(error));
                            continue;
                        }
                    };
                    let mut removed = Vec::with_capacity(active.len());
                    removed.extend(
                        active
                            .iter()
                            .filter(|key| !replacement.contains(*key))
                            .cloned(),
                    );
                    let mut added = Vec::with_capacity(replacement.len());
                    added.extend(
                        replacement
                            .iter()
                            .filter(|key| !active.contains(*key))
                            .cloned()
                            .map(|key| (key, self.source.clone())),
                    );

                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    let Some(added) = prepare_subscriptions(added, &mut reply).await else {
                        continue;
                    };
                    if reply.respond(Ok(WatchListResponse::Applied)).is_err() {
                        continue;
                    }
                    for key in &removed {
                        context.cancel_stream(key);
                        active.remove(key);
                    }
                    for prepared in added {
                        let key = prepared.key.clone();
                        start_stream(&mut context, prepared);
                        active.insert(key);
                    }
                }
                WatchListRequest::Retry { key } => {
                    if !active.contains(&key) {
                        let _ = reply.respond(Err(WatchListServiceError::EntryNotActive));
                        continue;
                    }

                    let source = self.source.clone();
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    let deadline = reply.deadline();
                    let subscription = {
                        let setup = source.subscribe_ticker(key.clone());
                        tokio::pin!(setup);
                        tokio::select! {
                            biased;
                            () = reply.cancelled() => continue,
                            () = tokio::time::sleep_until(deadline) => continue,
                            result = &mut setup => result,
                        }
                    };
                    if reply.respond(Ok(WatchListResponse::Applied)).is_ok() {
                        replace_stream(
                            &mut context,
                            PreparedTicker {
                                key,
                                source,
                                subscription,
                            },
                        );
                    }
                }
                WatchListRequest::MarketReloaded { entries } => {
                    // The listings are the set to follow, exactly as `Replace` follows the set
                    // it is given: a reload is asked for by a widget that may have had a
                    // request of its own in flight, and cancelling that request is only
                    // harmless if this one carries the whole set rather than the difference
                    // from what this service happens to hold.
                    let replacement = match validate_replacement(&entries) {
                        Ok(replacement) => replacement,
                        Err(error) => {
                            let _ = reply.respond(Err(error));
                            continue;
                        }
                    };
                    let mut removed = Vec::with_capacity(active.len());
                    removed.extend(
                        active
                            .iter()
                            .filter(|key| !replacement.contains(*key))
                            .cloned(),
                    );
                    // Unlike `Replace`, every listing is subscribed to again: the streams that
                    // stay are the ones whose resolution this request is about.
                    let reopened = replacement
                        .iter()
                        .cloned()
                        .map(|key| (key, self.source.clone()))
                        .collect::<Vec<_>>();

                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }
                    // Every stream is prepared before any of them is opened, so a subscription
                    // that fails or a caller that went away leaves the streams that are running
                    // exactly as they were.
                    let Some(reopened) = prepare_subscriptions(reopened, &mut reply).await else {
                        continue;
                    };
                    if reply.respond(Ok(WatchListResponse::Applied)).is_err() {
                        continue;
                    }
                    for key in &removed {
                        context.cancel_stream(key);
                        active.remove(key);
                    }
                    for prepared in reopened {
                        let key = prepared.key.clone();
                        replace_stream(&mut context, prepared);
                        active.insert(key);
                    }
                }
            }
        }
    }
}

/// One listing whose stream is prepared and can be opened.
struct PreparedTicker<S>
where
    S: TickerSource,
{
    key: VenueListingKey,
    source: S,
    /// The subscription, or the failure that stands in its place. A listing the
    /// source cannot serve is reported the way a failed subscription is — through
    /// the stream the retry opens — so the row explains itself instead of the whole
    /// request failing with nothing to show.
    subscription: Result<S::Subscription, MarketError>,
}

/// Subscribes to every newly followed listing, all at once.
///
/// The request's deadline and its cancellation both end the wait, and whatever
/// was subscribed to by then is dropped with the tasks: a request the caller no
/// longer wants must not leave streams running behind it.
async fn prepare_subscriptions<S>(
    added: Vec<(VenueListingKey, S)>,
    reply: &mut RpcRequestContext<WatchListProtocol>,
) -> Option<Vec<PreparedTicker<S>>>
where
    S: TickerSource,
{
    let mut tasks = JoinSet::new();
    for (key, source) in added {
        tasks.spawn(async move {
            let subscription = source.subscribe_ticker(key.clone()).await;
            PreparedTicker {
                key,
                source,
                subscription,
            }
        });
    }

    let mut prepared = Vec::with_capacity(tasks.len());
    let deadline = reply.deadline();
    while !tasks.is_empty() {
        let result = tokio::select! {
            biased;
            () = reply.cancelled() => {
                abort_setup_tasks(&mut tasks).await;
                return None;
            }
            () = tokio::time::sleep_until(deadline) => {
                abort_setup_tasks(&mut tasks).await;
                return None;
            }
            result = tasks.join_next() => result,
        };
        match result {
            Some(Ok(setup)) => prepared.push(setup),
            Some(Err(error)) => {
                log::error!("watch-list subscription setup failed: {error}");
                abort_setup_tasks(&mut tasks).await;
                return None;
            }
            None => break,
        }
    }

    Some(prepared)
}

async fn abort_setup_tasks<T>(tasks: &mut JoinSet<T>)
where
    T: Send + 'static,
{
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

/// Checks a replacement and returns it as the set the streams are keyed by.
pub(super) fn validate_replacement(
    entries: &[VenueListingKey],
) -> Result<HashSet<VenueListingKey>, WatchListServiceError> {
    if entries.len() > MAX_WATCH_LIST_ENTRIES {
        return Err(WatchListServiceError::TooManyEntries {
            actual: entries.len(),
            maximum: MAX_WATCH_LIST_ENTRIES,
        });
    }

    let mut replacement = HashSet::with_capacity(entries.len());
    for key in entries {
        if !replacement.insert(key.clone()) {
            return Err(WatchListServiceError::DuplicateEntry { key: key.clone() });
        }
    }

    Ok(replacement)
}

/// Opens the stream of a listing that is not followed yet.
///
/// Repeating the same request is idempotent: a key that already has a live
/// generation keeps it, and neither its publisher nor its snapshot is touched.
fn start_stream<S>(context: &mut ServiceContext<WatchListProtocol>, prepared: PreparedTicker<S>)
where
    S: TickerSource,
{
    let PreparedTicker {
        key,
        source,
        subscription,
    } = prepared;
    let stream_key = key.clone();
    context.start_stream(key, move |publisher| async move {
        run_stream(source, stream_key, subscription, publisher).await;
    });
}

/// Reopens one listing's stream, retiring the generation it replaces.
fn replace_stream<S>(context: &mut ServiceContext<WatchListProtocol>, prepared: PreparedTicker<S>)
where
    S: TickerSource,
{
    let PreparedTicker {
        key,
        source,
        subscription,
    } = prepared;
    let stream_key = key.clone();
    context.replace_stream(key, move |publisher| async move {
        run_stream(source, stream_key, subscription, publisher).await;
    });
}

/// Reports what one stream is doing until it ends.
async fn run_stream<S>(
    source: S,
    key: VenueListingKey,
    subscription: Result<S::Subscription, MarketError>,
    publisher: StreamPublisher<WatchListProtocol>,
) where
    S: TickerSource,
{
    if publish_status(&publisher, WatchListStatus::Connecting)
        .await
        .is_err()
    {
        return;
    }
    let result = match subscription {
        Ok(subscription) => run_stream_once(&source, &key, subscription, &publisher).await,
        Err(error) => Err(error.into()),
    };
    if let Err(error) = result {
        publish_error(&publisher, error).await;
        // A failed stream is terminal for its generation: it stays parked until a
        // retry replaces it, so nothing reconnects behind the user's back.
        publisher.cancelled().await;
    }
}

/// Publishes one listing's snapshots until the stream ends.
async fn run_stream_once<S>(
    source: &S,
    key: &VenueListingKey,
    mut subscription: S::Subscription,
    publisher: &StreamPublisher<WatchListProtocol>,
) -> Result<(), WatchListStreamError>
where
    S: TickerSource,
{
    if publish_status(publisher, WatchListStatus::LoadingSnapshot)
        .await
        .is_err()
    {
        return Ok(());
    }

    // The increments are read once per stream generation: whichever source reports
    // the opening ticker — the buffered stream or the REST page — the row is written
    // at the precision of the listing the subscription resolved.
    let precision = subscription.precision();
    let initial = match select_initial_snapshot(source, key, &mut subscription, publisher).await {
        Ok(Some(ticker)) => ticker,
        Ok(None) => return Ok(()),
        Err(error) => return Err(error),
    };

    let mut last_ticker = initial.clone();
    if publish_snapshot(publisher, initial, precision).is_err() {
        return Ok(());
    }
    if publish_status(publisher, WatchListStatus::Ready)
        .await
        .is_err()
    {
        return Ok(());
    }

    loop {
        let event = tokio::select! {
            biased;
            () = publisher.cancelled() => return Ok(()),
            event = subscription.recv() => event,
        };
        match event {
            Some(Ok(ticker)) => {
                validate_ticker_listing(key, &ticker)?;
                if !should_publish_ticker(&last_ticker, &ticker) {
                    continue;
                }
                last_ticker = ticker.clone();
                if publish_snapshot(publisher, ticker, precision).is_err() {
                    return Ok(());
                }
            }
            Some(Err(error)) => return Err(error.into()),
            None => {
                return Err(MarketError::Disconnected(DisconnectReason::Normal).into());
            }
        }
    }
}

/// Whether an incoming ticker says anything the row does not already show.
///
/// A ticker whose identifier went backwards is older than the one that was
/// accepted, and one that is equal says nothing at all. A venue that publishes no
/// ordering identifier at all says nothing about which of two tickers is newer, so
/// a changed ticker is taken as it comes.
fn should_publish_ticker(current: &ListingTicker, incoming: &ListingTicker) -> bool {
    if incoming == current {
        return false;
    }
    match (incoming.monotonic_id, current.monotonic_id) {
        (Some(incoming), Some(current)) => incoming >= current,
        _ => true,
    }
}

/// Takes the opening snapshot from whichever source reports it first.
///
/// A buffered stream event is newer than a REST response that was started before
/// it, so the subscription is polled first.
async fn select_initial_snapshot<S>(
    source: &S,
    key: &VenueListingKey,
    subscription: &mut S::Subscription,
    publisher: &StreamPublisher<WatchListProtocol>,
) -> Result<Option<Arc<ListingTicker>>, WatchListStreamError>
where
    S: TickerSource,
{
    let fetch = source.fetch_ticker(key.clone());
    tokio::pin!(fetch);
    tokio::select! {
        biased;
        () = publisher.cancelled() => Ok(None),
        event = subscription.recv() => {
            match event {
                Some(Ok(ticker)) => {
                    validate_ticker_listing(key, &ticker)?;
                    Ok(Some(ticker))
                }
                Some(Err(error)) => Err(error.into()),
                None => Err(MarketError::Disconnected(DisconnectReason::Normal).into()),
            }
        }
        result = &mut fetch => {
            let fetched = Arc::new(result?);
            validate_ticker_listing(key, &fetched)?;
            Ok(Some(fetched))
        }
    }
}

/// Rejects a ticker that arrived for a listing other than the subscribed one.
fn validate_ticker_listing(
    expected: &VenueListingKey,
    ticker: &ListingTicker,
) -> Result<(), WatchListStreamError> {
    if &ticker.listing == expected {
        return Ok(());
    }

    Err(WatchListStreamError::TickerListingMismatch {
        expected: Box::new(expected.clone()),
        actual: Box::new(ticker.listing.clone()),
    })
}

fn publish_snapshot(
    publisher: &StreamPublisher<WatchListProtocol>,
    ticker: Arc<ListingTicker>,
    precision: ListingPrecision,
) -> Result<(), StreamPublishError> {
    publisher.publish_latest(WatchListStreamEvent::Snapshot { ticker, precision })
}

async fn publish_status(
    publisher: &StreamPublisher<WatchListProtocol>,
    status: WatchListStatus,
) -> Result<(), StreamPublishError> {
    publisher
        .publish_reliable(WatchListStreamEvent::Status(status))
        .await
}

async fn publish_error(
    publisher: &StreamPublisher<WatchListProtocol>,
    error: WatchListStreamError,
) {
    let _ = publisher
        .publish_reliable(WatchListStreamEvent::Error(error))
        .await;
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
        future::pending,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use market_participants_connector::catalog::test_support;
    use market_participants_model::{
        asset::AssetId,
        decimal::{Price, Quantity},
        listing::{route::VenueListingKey, venue::PriceBasis},
        market::{
            price::MarketPrice,
            units::{MarketQuantity, QuantityUnit},
        },
    };
    use tokio::sync::{Mutex, Notify, mpsc, oneshot};
    use widget::{
        protocol::StreamEnvelope,
        rpc::RpcCallError,
        runtime::WidgetRuntime,
        stream::{LatestStream, ReliableStream},
    };

    use super::*;

    /// A subscription whose events the test pushes by hand.
    struct FakeSubscription {
        events: mpsc::Receiver<Result<Arc<ListingTicker>, MarketError>>,
        event_received: Arc<Notify>,
        dropped: Arc<Notify>,
        disconnected: Option<Arc<Notify>>,
    }

    impl Drop for FakeSubscription {
        fn drop(&mut self) {
            self.dropped.notify_one();
        }
    }

    impl TickerEventSubscription for FakeSubscription {
        fn precision(&self) -> ListingPrecision {
            precision()
        }

        async fn recv(&mut self) -> Option<Result<Arc<ListingTicker>, MarketError>> {
            let event = self.events.recv().await;
            if event.is_some() {
                self.event_received.notify_one();
            } else if let Some(disconnected) = &self.disconnected {
                disconnected.notify_one();
            }
            event
        }
    }

    /// A source that hands out one prepared subscription.
    #[derive(Clone)]
    struct FakeSource {
        subscription: Arc<Mutex<Option<FakeSubscription>>>,
        fetched: Arc<ListingTicker>,
    }

    type FakeSourceHarness = (
        FakeSource,
        mpsc::Sender<Result<Arc<ListingTicker>, MarketError>>,
        Arc<Notify>,
        Arc<Notify>,
    );

    impl TickerSource for FakeSource {
        type Subscription = FakeSubscription;

        async fn fetch_ticker(
            &self,
            _listing: VenueListingKey,
        ) -> Result<ListingTicker, MarketError> {
            Ok(self.fetched.as_ref().clone())
        }

        async fn subscribe_ticker(
            &self,
            _listing: VenueListingKey,
        ) -> Result<Self::Subscription, MarketError> {
            self.subscription
                .lock()
                .await
                .take()
                .ok_or(MarketError::ServiceClosed)
        }
    }

    /// A source that hands out the subscription prepared for each followed listing and
    /// answers each fetch with a ticker of the listing that was asked for.
    #[derive(Clone)]
    struct MultiListingSource {
        subscriptions: Arc<Mutex<ahash::HashMap<VenueListingKey, FakeSubscription>>>,
        subscription_calls: Arc<AtomicUsize>,
    }

    impl TickerSource for MultiListingSource {
        type Subscription = FakeSubscription;

        async fn fetch_ticker(
            &self,
            listing: VenueListingKey,
        ) -> Result<ListingTicker, MarketError> {
            Ok(ticker_for(&listing, 1, 100))
        }

        async fn subscribe_ticker(
            &self,
            listing: VenueListingKey,
        ) -> Result<Self::Subscription, MarketError> {
            self.subscription_calls.fetch_add(1, Ordering::AcqRel);
            self.subscriptions
                .lock()
                .await
                .remove(&listing)
                .ok_or(MarketError::ServiceClosed)
        }
    }

    /// A source whose opening snapshot never arrives.
    #[derive(Clone)]
    struct PendingFetchSource {
        subscription: Arc<Mutex<Option<FakeSubscription>>>,
        fetch_started: Arc<Notify>,
    }

    impl TickerSource for PendingFetchSource {
        type Subscription = FakeSubscription;

        async fn fetch_ticker(
            &self,
            _listing: VenueListingKey,
        ) -> Result<ListingTicker, MarketError> {
            self.fetch_started.notify_one();
            pending().await
        }

        async fn subscribe_ticker(
            &self,
            _listing: VenueListingKey,
        ) -> Result<Self::Subscription, MarketError> {
            self.subscription
                .lock()
                .await
                .take()
                .ok_or(MarketError::ServiceClosed)
        }
    }

    /// A source that subscribes only when the test says so.
    #[derive(Clone)]
    struct ManualRetrySource {
        attempts: Arc<AtomicUsize>,
        attempted: Arc<Notify>,
        subscriptions: Arc<Mutex<VecDeque<Result<FakeSubscription, MarketError>>>>,
        fetched: Arc<Mutex<VecDeque<ListingTicker>>>,
    }

    impl TickerSource for ManualRetrySource {
        type Subscription = FakeSubscription;

        async fn fetch_ticker(
            &self,
            _listing: VenueListingKey,
        ) -> Result<ListingTicker, MarketError> {
            self.fetched
                .lock()
                .await
                .pop_front()
                .ok_or(MarketError::ServiceClosed)
        }

        async fn subscribe_ticker(
            &self,
            _listing: VenueListingKey,
        ) -> Result<Self::Subscription, MarketError> {
            self.attempts.fetch_add(1, Ordering::AcqRel);
            self.attempted.notify_one();
            self.subscriptions
                .lock()
                .await
                .pop_front()
                .unwrap_or(Err(MarketError::ServiceClosed))
        }
    }

    /// A source whose subscription can be made to fail on demand.
    ///
    /// It is what a route that cannot serve the listing reports: the subscription
    /// itself fails, before anything is asked of a venue.
    #[derive(Clone)]
    struct UnservableListingSource {
        unservable: Arc<AtomicBool>,
        subscription: Arc<Mutex<Option<FakeSubscription>>>,
        fetched: Arc<ListingTicker>,
    }

    impl TickerSource for UnservableListingSource {
        type Subscription = FakeSubscription;

        async fn fetch_ticker(
            &self,
            _listing: VenueListingKey,
        ) -> Result<ListingTicker, MarketError> {
            Ok(self.fetched.as_ref().clone())
        }

        async fn subscribe_ticker(
            &self,
            _listing: VenueListingKey,
        ) -> Result<Self::Subscription, MarketError> {
            if self.unservable.load(Ordering::Acquire) {
                return Err(MarketError::ServiceClosed);
            }
            self.subscription
                .lock()
                .await
                .take()
                .ok_or(MarketError::ServiceClosed)
        }
    }

    /// A source that only counts what was asked of it.
    #[derive(Clone)]
    struct CountingSource {
        subscription_calls: Arc<AtomicUsize>,
        fetch_calls: Arc<AtomicUsize>,
    }

    /// A subscription that never reports anything.
    struct PendingSubscription;

    impl TickerSource for CountingSource {
        type Subscription = PendingSubscription;

        async fn fetch_ticker(
            &self,
            _listing: VenueListingKey,
        ) -> Result<ListingTicker, MarketError> {
            self.fetch_calls.fetch_add(1, Ordering::AcqRel);
            pending().await
        }

        async fn subscribe_ticker(
            &self,
            _listing: VenueListingKey,
        ) -> Result<Self::Subscription, MarketError> {
            self.subscription_calls.fetch_add(1, Ordering::AcqRel);
            Ok(PendingSubscription)
        }
    }

    impl TickerEventSubscription for PendingSubscription {
        fn precision(&self) -> ListingPrecision {
            precision()
        }

        async fn recv(&mut self) -> Option<Result<Arc<ListingTicker>, MarketError>> {
            pending().await
        }
    }

    /// What one subscription of [`GatedTickerSource`] does when it is opened.
    enum SubscriptionPlan {
        Ready,
        Pending {
            started: oneshot::Sender<()>,
            dropped: oneshot::Sender<()>,
        },
    }

    /// Reports that a setup future was dropped before it finished.
    struct SetupDropSignal(Option<oneshot::Sender<()>>);

    impl Drop for SetupDropSignal {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    /// A source that blocks inside subscription setup.
    #[derive(Clone)]
    struct GatedTickerSource {
        plans: Arc<Mutex<VecDeque<SubscriptionPlan>>>,
        subscription_calls: Arc<AtomicUsize>,
        fetch_calls: Arc<AtomicUsize>,
    }

    impl TickerSource for GatedTickerSource {
        type Subscription = PendingSubscription;

        async fn fetch_ticker(
            &self,
            _listing: VenueListingKey,
        ) -> Result<ListingTicker, MarketError> {
            self.fetch_calls.fetch_add(1, Ordering::AcqRel);
            pending().await
        }

        async fn subscribe_ticker(
            &self,
            _listing: VenueListingKey,
        ) -> Result<Self::Subscription, MarketError> {
            self.subscription_calls.fetch_add(1, Ordering::AcqRel);
            match self
                .plans
                .lock()
                .await
                .pop_front()
                .ok_or(MarketError::ServiceClosed)?
            {
                SubscriptionPlan::Ready => Ok(PendingSubscription),
                SubscriptionPlan::Pending { started, dropped } => {
                    let _drop_signal = SetupDropSignal(Some(dropped));
                    let _ = started.send(());
                    pending().await
                }
            }
        }
    }

    fn route() -> market_participants_model::listing::route::VenueRoute {
        test_support::spot_route()
    }

    fn key() -> VenueListingKey {
        test_support::listing_key(&route(), "BTCUSDT")
    }

    fn other_key() -> VenueListingKey {
        test_support::listing_key(&route(), "ETHUSDT")
    }

    fn quote() -> AssetId {
        test_support::token("binance", "USDT")
    }

    fn base() -> AssetId {
        test_support::token("binance", "BTC")
    }

    fn price(value: u64) -> MarketPrice {
        MarketPrice {
            value: Price::from(value),
            quote_asset: quote(),
            basis: PriceBasis::PerAssetUnit,
        }
    }

    /// The increments every fake subscription reports for its listing.
    fn precision() -> ListingPrecision {
        ListingPrecision {
            price_tick: "0.01".parse().expect("a price tick should parse"),
            quantity_tick: "0.001".parse().expect("a quantity step should parse"),
        }
    }

    fn ticker_for(listing: &VenueListingKey, monotonic_id: i64, close: u64) -> ListingTicker {
        ListingTicker {
            listing: listing.clone(),
            monotonic_id: Some(monotonic_id),
            open: price(100),
            high: Some(price(close)),
            low: Some(price(close)),
            close: price(close),
            traded_quantity: Some(MarketQuantity {
                value: Quantity::from(10_u64),
                unit: QuantityUnit::AssetUnits { asset: base() },
            }),
            traded_notional: Some(MarketQuantity {
                value: Quantity::from(1_000_u64),
                unit: QuantityUnit::Notional { asset: quote() },
            }),
        }
    }

    fn ticker(monotonic_id: i64, close: u64) -> ListingTicker {
        ticker_for(&key(), monotonic_id, close)
    }

    fn indexed_key(index: usize) -> VenueListingKey {
        test_support::listing_key(&route(), &format!("A{index}"))
    }

    fn replace(entries: Vec<VenueListingKey>) -> WatchListRequest {
        WatchListRequest::Replace { entries }
    }

    fn fake_source(fetched: ListingTicker) -> FakeSourceHarness {
        let (events, event_receiver) = mpsc::channel(8);
        let event_received = Arc::new(Notify::new());
        let dropped = Arc::new(Notify::new());
        (
            FakeSource {
                subscription: Arc::new(Mutex::new(Some(FakeSubscription {
                    events: event_receiver,
                    event_received: event_received.clone(),
                    dropped: dropped.clone(),
                    disconnected: None,
                }))),
                fetched: Arc::new(fetched),
            },
            events,
            event_received,
            dropped,
        )
    }

    fn counting_source() -> (CountingSource, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let subscription_calls = Arc::new(AtomicUsize::new(0));
        let fetch_calls = Arc::new(AtomicUsize::new(0));
        (
            CountingSource {
                subscription_calls: subscription_calls.clone(),
                fetch_calls: fetch_calls.clone(),
            },
            subscription_calls,
            fetch_calls,
        )
    }

    fn gated_ticker_source(plans: Vec<SubscriptionPlan>) -> GatedTickerSource {
        GatedTickerSource {
            plans: Arc::new(Mutex::new(plans.into())),
            subscription_calls: Arc::new(AtomicUsize::new(0)),
            fetch_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn pending_subscription_plan() -> (
        SubscriptionPlan,
        oneshot::Receiver<()>,
        oneshot::Receiver<()>,
    ) {
        let (started, started_receiver) = oneshot::channel();
        let (dropped, dropped_receiver) = oneshot::channel();
        (
            SubscriptionPlan::Pending { started, dropped },
            started_receiver,
            dropped_receiver,
        )
    }

    fn event(
        envelope: &StreamEnvelope<VenueListingKey, WatchListStreamEvent>,
    ) -> &WatchListStreamEvent {
        envelope.message().as_ref()
    }

    async fn wait_for_count(counter: &AtomicUsize, expected: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while counter.load(Ordering::Acquire) != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("source calls should reach the expected count");
    }

    async fn load_initial_snapshot(
        runtime: &WidgetRuntime<WatchListProtocol>,
        latest: &mut LatestStream<WatchListProtocol>,
    ) {
        runtime
            .client()
            .call(replace(vec![key()]))
            .await
            .expect("watch-list replacement should start the stream");
        tokio::time::timeout(Duration::from_secs(1), latest.changed())
            .await
            .expect("initial ticker should be published")
            .expect("latest stream should remain open");
        let _ = latest.next_unseen();
    }

    async fn receive_error(
        reliable: &mut ReliableStream<WatchListProtocol>,
    ) -> WatchListStreamError {
        loop {
            let envelope = reliable
                .recv()
                .await
                .expect("error stream should remain open");
            if let WatchListStreamEvent::Error(error) = event(&envelope) {
                return error.clone();
            }
        }
    }

    /// The statuses a stream published before it reported anything else.
    async fn receive_statuses_until_ready(
        reliable: &mut ReliableStream<WatchListProtocol>,
    ) -> Vec<WatchListStatus> {
        let mut statuses = Vec::with_capacity(3);
        loop {
            let envelope = reliable
                .recv()
                .await
                .expect("the stream should remain deliverable");
            match event(&envelope) {
                WatchListStreamEvent::Status(status) => {
                    statuses.push(*status);
                    if *status == WatchListStatus::Ready {
                        return statuses;
                    }
                }
                WatchListStreamEvent::Error(error) => panic!("unexpected stream error: {error}"),
                WatchListStreamEvent::Snapshot { .. } => {
                    panic!("snapshots must use the latest stream")
                }
            }
        }
    }

    #[tokio::test]
    async fn identical_realtime_ticker_should_not_publish_again() {
        let initial = ticker(1, 100);
        let (source, events, event_received, _dropped) = fake_source(initial.clone());
        let runtime = WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source));
        let mut latest = runtime.latest_stream();
        load_initial_snapshot(&runtime, &mut latest).await;

        events
            .send(Ok(Arc::new(initial)))
            .await
            .expect("the fake subscription should remain open");
        event_received.notified().await;
        events
            .send(Ok(Arc::new(ticker(2, 101))))
            .await
            .expect("a changed ticker should remain publishable");
        event_received.notified().await;
        tokio::time::timeout(Duration::from_secs(1), latest.changed())
            .await
            .expect("the changed ticker should be published")
            .expect("latest stream should remain open");
        let envelope = latest
            .next_unseen()
            .expect("the changed ticker should be unseen");

        assert_eq!(envelope.sequence(), 2);
        assert!(matches!(
            event(&envelope),
            WatchListStreamEvent::Snapshot { ticker, .. }
                if ticker.close.value == Price::from(101_u64)
        ));
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn changed_equal_id_ticker_should_publish_immediately() {
        let (source, events, event_received, _dropped) = fake_source(ticker(1, 100));
        let runtime = WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source));
        let mut latest = runtime.latest_stream();
        load_initial_snapshot(&runtime, &mut latest).await;

        events
            .send(Ok(Arc::new(ticker(1, 101))))
            .await
            .expect("the fake subscription should remain open");
        event_received.notified().await;
        tokio::time::timeout(Duration::from_secs(1), latest.changed())
            .await
            .expect("the changed ticker should publish without a cadence delay")
            .expect("latest stream should remain open");
        let envelope = latest
            .next_unseen()
            .expect("the changed ticker should be unseen");
        let WatchListStreamEvent::Snapshot { ticker, .. } = event(&envelope) else {
            panic!("latest stream should contain a ticker snapshot");
        };

        assert_eq!(ticker.close.value, Price::from(101_u64));
        runtime.shutdown().await;
    }

    /// A ticker whose identifier went backwards is older than the one already
    /// accepted, so it does not replace it.
    #[tokio::test]
    async fn a_ticker_that_went_backwards_should_not_replace_a_newer_one() {
        let (source, events, event_received, _dropped) = fake_source(ticker(2, 100));
        let runtime = WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source));
        let mut latest = runtime.latest_stream();
        load_initial_snapshot(&runtime, &mut latest).await;

        events
            .send(Ok(Arc::new(ticker(1, 999))))
            .await
            .expect("the older ticker should reach the service");
        event_received.notified().await;

        assert!(
            tokio::time::timeout(Duration::from_millis(50), latest.changed())
                .await
                .is_err(),
            "an older ticker must not replace the accepted one"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn mismatched_rest_ticker_should_latch_error_without_publishing_snapshot() {
        let mut mismatched = ticker(1, 100);
        mismatched.listing = other_key();
        let (source, _events, _event_received, _dropped) = fake_source(mismatched);
        let mut runtime =
            WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source));
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("reliable stream should be available");
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(replace(vec![key()]))
            .await
            .expect("a valid watch-list replacement should be confirmed");
        let error = receive_error(&mut reliable).await;

        assert!(matches!(
            error,
            WatchListStreamError::TickerListingMismatch { expected, actual }
                if expected.as_ref() == &key() && actual.as_ref() == &other_key()
        ));
        assert!(latest.next_unseen().is_none());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn mismatched_websocket_ticker_should_latch_error_without_replacing_last_snapshot() {
        let (source, events, event_received, _dropped) = fake_source(ticker(1, 100));
        let mut runtime =
            WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source));
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("reliable stream should be available");
        let mut latest = runtime.latest_stream();
        load_initial_snapshot(&runtime, &mut latest).await;
        let mut mismatched = ticker(2, 200);
        mismatched.listing = other_key();

        events
            .send(Ok(Arc::new(mismatched)))
            .await
            .expect("the fake subscription should remain open");
        event_received.notified().await;
        let error = receive_error(&mut reliable).await;

        assert!(matches!(
            error,
            WatchListStreamError::TickerListingMismatch { actual, .. }
                if actual.as_ref() == &other_key()
        ));
        assert!(latest.next_unseen().is_none());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn buffered_websocket_mismatch_should_fail_while_rest_snapshot_is_pending() {
        let (events, event_receiver) = mpsc::channel(1);
        let event_received = Arc::new(Notify::new());
        let fetch_started = Arc::new(Notify::new());
        let source = PendingFetchSource {
            subscription: Arc::new(Mutex::new(Some(FakeSubscription {
                events: event_receiver,
                event_received: event_received.clone(),
                dropped: Arc::new(Notify::new()),
                disconnected: None,
            }))),
            fetch_started: fetch_started.clone(),
        };
        let mut runtime =
            WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source));
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("reliable stream should be available");
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(replace(vec![key()]))
            .await
            .expect("a valid watch-list replacement should be confirmed");
        fetch_started.notified().await;
        let mut mismatched = ticker(2, 200);
        mismatched.listing = other_key();
        events
            .send(Ok(Arc::new(mismatched)))
            .await
            .expect("the buffered ticker should reach the subscription");
        event_received.notified().await;

        let error = receive_error(&mut reliable).await;

        assert!(matches!(
            error,
            WatchListStreamError::TickerListingMismatch { expected, actual }
                if expected.as_ref() == &key() && actual.as_ref() == &other_key()
        ));
        assert!(latest.next_unseen().is_none());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn ready_rest_and_buffered_websocket_should_commit_one_websocket_snapshot() {
        let (source, events, _event_received, _dropped) = fake_source(ticker(1, 100));
        events
            .send(Ok(Arc::new(ticker(2, 200))))
            .await
            .expect("the websocket ticker should be buffered before setup");
        let mut runtime =
            WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source));
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("reliable stream should be available");
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(replace(vec![key()]))
            .await
            .expect("a valid watch-list replacement should be confirmed");
        tokio::time::timeout(Duration::from_secs(1), latest.changed())
            .await
            .expect("the initial websocket ticker should be published")
            .expect("latest stream should remain open");
        let envelope = latest
            .next_unseen()
            .expect("the websocket ticker should be unseen");
        let statuses = receive_statuses_until_ready(&mut reliable).await;
        let WatchListStreamEvent::Snapshot { ticker, .. } = event(&envelope) else {
            panic!("latest stream should contain a ticker snapshot");
        };

        assert_eq!(ticker.close.value, Price::from(200_u64));
        assert_eq!(envelope.sequence(), 1);
        assert_eq!(
            statuses,
            [
                WatchListStatus::Connecting,
                WatchListStatus::LoadingSnapshot,
                WatchListStatus::Ready,
            ]
        );
        assert!(latest.next_unseen().is_none());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn ready_rest_without_buffered_websocket_should_commit_rest_snapshot() {
        let (source, _events, _event_received, _dropped) = fake_source(ticker(1, 100));
        let mut runtime =
            WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source));
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("reliable stream should be available");
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(replace(vec![key()]))
            .await
            .expect("a valid watch-list replacement should be confirmed");
        tokio::time::timeout(Duration::from_secs(1), latest.changed())
            .await
            .expect("the initial REST ticker should be published")
            .expect("latest stream should remain open");
        let envelope = latest
            .next_unseen()
            .expect("the REST ticker should be unseen");
        let statuses = receive_statuses_until_ready(&mut reliable).await;
        let WatchListStreamEvent::Snapshot { ticker, .. } = event(&envelope) else {
            panic!("latest stream should contain a ticker snapshot");
        };

        assert_eq!(ticker.close.value, Price::from(100_u64));
        assert_eq!(envelope.sequence(), 1);
        assert_eq!(
            statuses,
            [
                WatchListStatus::Connecting,
                WatchListStatus::LoadingSnapshot,
                WatchListStatus::Ready,
            ]
        );
        runtime.shutdown().await;
    }

    /// Two listings of one route are two streams of their own: each is subscribed
    /// once and publishes under its own key, so neither replaces the other.
    #[tokio::test]
    async fn reloading_a_market_should_reopen_every_listings_stream() {
        let (first_events, first_receiver) = mpsc::channel(8);
        let (second_events, second_receiver) = mpsc::channel(8);
        let notifications = Arc::new(Notify::new());
        let source = MultiListingSource {
            subscriptions: Arc::new(Mutex::new(ahash::HashMap::from_iter([
                (
                    key(),
                    FakeSubscription {
                        events: first_receiver,
                        event_received: Arc::clone(&notifications),
                        dropped: Arc::new(Notify::new()),
                        disconnected: None,
                    },
                ),
                (
                    other_key(),
                    FakeSubscription {
                        events: second_receiver,
                        event_received: Arc::clone(&notifications),
                        dropped: Arc::new(Notify::new()),
                        disconnected: None,
                    },
                ),
            ]))),
            subscription_calls: Arc::new(AtomicUsize::new(0)),
        };
        let runtime =
            WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source.clone()));
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(replace(vec![key(), other_key()]))
            .await
            .expect("both listings should be followed");
        assert_eq!(source.subscription_calls.load(Ordering::Acquire), 2);

        // The same set again is a replacement of nothing: the streams that are open are the ones
        // the service keeps, which is what makes `Replace` the wrong request for a market that was
        // loaded again.
        runtime
            .client()
            .call(replace(vec![key(), other_key()]))
            .await
            .expect("a repeated replacement should be applied");
        assert_eq!(source.subscription_calls.load(Ordering::Acquire), 2);

        // The market those listings are on is loaded again, so every one of their streams is
        // opened again: the source is asked for both of them once more.
        let (reloaded_first_events, reloaded_first_receiver) = mpsc::channel(8);
        let (reloaded_second_events, reloaded_second_receiver) = mpsc::channel(8);
        {
            let mut subscriptions = source.subscriptions.lock().await;
            subscriptions.insert(
                key(),
                FakeSubscription {
                    events: reloaded_first_receiver,
                    event_received: Arc::clone(&notifications),
                    dropped: Arc::new(Notify::new()),
                    disconnected: None,
                },
            );
            subscriptions.insert(
                other_key(),
                FakeSubscription {
                    events: reloaded_second_receiver,
                    event_received: Arc::clone(&notifications),
                    dropped: Arc::new(Notify::new()),
                    disconnected: None,
                },
            );
        }
        runtime
            .client()
            .call(WatchListRequest::MarketReloaded {
                entries: vec![key(), other_key()],
            })
            .await
            .expect("a reload of the market should be applied");
        wait_for_count(&source.subscription_calls, 4).await;

        // The generations the reload retired are gone: the streams it replaced are the ones that
        // were dropped with the subscriptions they held.
        for (events, listing) in [(&first_events, key()), (&second_events, other_key())] {
            assert!(
                events
                    .send(Ok(Arc::new(ticker_for(&listing, 2, 200))))
                    .await
                    .is_err(),
                "the stream of the retired generation should not be reachable any more"
            );
        }

        // The streams the reload opened are the live ones, and they are the resolution this load
        // produced: what the new subscription publishes is what the pane reads.
        for (events, listing, close) in [
            (&reloaded_first_events, key(), 200_u64),
            (&reloaded_second_events, other_key(), 300),
        ] {
            // Whatever a stream published while it was opening is not what this reads: the ticker
            // below is what the resolution this load produced is asked to carry.
            while latest.next_unseen().is_some() {}
            events
                .send(Ok(Arc::new(ticker_for(&listing, 3, close))))
                .await
                .expect("the stream the reload opened should remain open");
            tokio::time::timeout(Duration::from_secs(1), latest.changed())
                .await
                .expect("the reopened listing should publish")
                .expect("latest stream should remain open");
            let envelope = latest.next_unseen().expect("a snapshot should be unseen");

            assert_eq!(envelope.key(), &listing);
            let WatchListStreamEvent::Snapshot {
                ticker: published, ..
            } = event(&envelope)
            else {
                panic!("latest stream should contain a ticker snapshot");
            };
            assert_eq!(published.close.value, Price::from(close));
        }

        // The listings are the set to follow rather than a set this service is assumed to hold:
        // a request that names one it does not follow follows it, because the request it stands
        // in for — the widget's own, still in flight — may never have been applied at all.
        let (third_events, third_receiver) = mpsc::channel(8);
        source.subscriptions.lock().await.insert(
            indexed_key(9),
            FakeSubscription {
                events: third_receiver,
                event_received: Arc::clone(&notifications),
                dropped: Arc::new(Notify::new()),
                disconnected: None,
            },
        );
        runtime
            .client()
            .call(WatchListRequest::MarketReloaded {
                entries: vec![indexed_key(9)],
            })
            .await
            .expect("a reload should follow the set it names");
        wait_for_count(&source.subscription_calls, 5).await;
        assert!(
            reloaded_first_events
                .send(Ok(Arc::new(ticker_for(&key(), 4, 200))))
                .await
                .is_err(),
            "a listing the reload left out should not be followed any more"
        );
        while latest.next_unseen().is_some() {}
        third_events
            .send(Ok(Arc::new(ticker_for(&indexed_key(9), 4, 400))))
            .await
            .expect("the listing the reload named should be followed");
        tokio::time::timeout(Duration::from_secs(1), latest.changed())
            .await
            .expect("the listing the reload named should publish")
            .expect("latest stream should remain open");
        assert_eq!(
            latest
                .next_unseen()
                .expect("a snapshot should be unseen")
                .key(),
            &indexed_key(9)
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn two_listings_should_keep_their_own_streams() {
        let (first_events, first_receiver) = mpsc::channel(8);
        let (second_events, second_receiver) = mpsc::channel(8);
        let first_received = Arc::new(Notify::new());
        let second_received = Arc::new(Notify::new());
        let source = MultiListingSource {
            subscriptions: Arc::new(Mutex::new(ahash::HashMap::from_iter([
                (
                    key(),
                    FakeSubscription {
                        events: first_receiver,
                        event_received: first_received.clone(),
                        dropped: Arc::new(Notify::new()),
                        disconnected: None,
                    },
                ),
                (
                    other_key(),
                    FakeSubscription {
                        events: second_receiver,
                        event_received: second_received.clone(),
                        dropped: Arc::new(Notify::new()),
                        disconnected: None,
                    },
                ),
            ]))),
            subscription_calls: Arc::new(AtomicUsize::new(0)),
        };
        let runtime =
            WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source.clone()));
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(replace(vec![key(), other_key()]))
            .await
            .expect("both listings should be followed");
        assert_eq!(source.subscription_calls.load(Ordering::Acquire), 2);
        tokio::time::timeout(Duration::from_secs(1), latest.changed())
            .await
            .expect("the fixture listings should publish")
            .expect("latest stream should remain open");
        while latest.next_unseen().is_some() {}

        // Each stream publishes a ticker of the listing it was opened for under
        // that listing's own key.
        for (events, received, listing, close) in [
            (&first_events, &first_received, key(), 200_u64),
            (&second_events, &second_received, other_key(), 300),
        ] {
            events
                .send(Ok(Arc::new(ticker_for(&listing, 2, close))))
                .await
                .expect("the listing's stream should remain open");
            received.notified().await;
            tokio::time::timeout(Duration::from_secs(1), latest.changed())
                .await
                .expect("the listing should publish")
                .expect("latest stream should remain open");
            let envelope = latest.next_unseen().expect("a snapshot should be unseen");

            assert_eq!(envelope.key(), &listing);
            let WatchListStreamEvent::Snapshot {
                ticker: published, ..
            } = event(&envelope)
            else {
                panic!("latest stream should contain a ticker snapshot");
            };
            assert_eq!(published.close.value, Price::from(close));
        }
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn exact_capacity_should_start_each_source_and_capacity_plus_one_should_start_none() {
        let (source, subscription_calls, _fetch_calls) = counting_source();
        let runtime = WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source));
        let accepted = (0..MAX_WATCH_LIST_ENTRIES)
            .map(indexed_key)
            .collect::<Vec<_>>();

        runtime
            .client()
            .call(replace(accepted.clone()))
            .await
            .expect("the exact watch-list capacity should be accepted");
        wait_for_count(&subscription_calls, MAX_WATCH_LIST_ENTRIES).await;
        let error = runtime
            .client()
            .call(replace(
                accepted
                    .into_iter()
                    .chain(std::iter::once(indexed_key(MAX_WATCH_LIST_ENTRIES)))
                    .collect(),
            ))
            .await
            .expect_err("capacity plus one must be rejected");

        assert!(matches!(
            error,
            RpcCallError::Service {
                source: WatchListServiceError::TooManyEntries {
                    actual,
                    maximum: MAX_WATCH_LIST_ENTRIES
                },
                ..
            } if actual == MAX_WATCH_LIST_ENTRIES + 1
        ));
        assert_eq!(
            subscription_calls.load(Ordering::Acquire),
            MAX_WATCH_LIST_ENTRIES
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn cancelled_replace_should_have_no_side_effect_and_not_poison_following_replacement() {
        let (pending_plan, started, dropped) = pending_subscription_plan();
        let source = gated_ticker_source(vec![pending_plan, SubscriptionPlan::Ready]);
        let runtime =
            WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source.clone()));
        let client = runtime.client();
        let dropped_client = client.clone();
        let request = tokio::spawn(async move { dropped_client.call(replace(vec![key()])).await });

        started
            .await
            .expect("the replacement setup should start before cancellation");
        request.abort();
        assert!(
            request
                .await
                .expect_err("a cancelled replacement task should abort")
                .is_cancelled()
        );
        dropped
            .await
            .expect("caller cancellation should drop the replacement setup");

        assert_eq!(source.subscription_calls.load(Ordering::Acquire), 1);
        assert_eq!(source.fetch_calls.load(Ordering::Acquire), 0);

        client
            .call(replace(vec![other_key()]))
            .await
            .expect("the following replacement should be confirmed");
        wait_for_count(&source.subscription_calls, 2).await;
        wait_for_count(&source.fetch_calls, 1).await;
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn expired_replace_should_have_no_side_effect_and_not_cancel_following_replacement() {
        let (pending_plan, started, dropped) = pending_subscription_plan();
        let source = gated_ticker_source(vec![pending_plan, SubscriptionPlan::Ready]);
        let runtime =
            WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source.clone()));
        let client = runtime.client();
        let expired_client = client.clone();
        let expired = tokio::spawn(async move {
            expired_client
                .call_with_timeout(replace(vec![key()]), Duration::from_secs(1))
                .await
        });

        started
            .await
            .expect("the replacement setup should start before its deadline");
        tokio::time::advance(Duration::from_secs(1)).await;
        let error = expired
            .await
            .expect("the expired call task should finish")
            .expect_err("an expired replacement must not be confirmed");
        dropped
            .await
            .expect("deadline expiry should drop the replacement setup");

        assert!(matches!(error, RpcCallError::DeadlineExceeded { .. }));
        assert_eq!(source.subscription_calls.load(Ordering::Acquire), 1);
        assert_eq!(source.fetch_calls.load(Ordering::Acquire), 0);

        client
            .call(replace(vec![other_key()]))
            .await
            .expect("the following replacement should be confirmed");
        wait_for_count(&source.subscription_calls, 2).await;
        wait_for_count(&source.fetch_calls, 1).await;
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn cancelled_retry_should_not_replace_the_active_stream_or_poison_the_following_retry() {
        let (pending_plan, started, dropped) = pending_subscription_plan();
        let source = gated_ticker_source(vec![
            SubscriptionPlan::Ready,
            pending_plan,
            SubscriptionPlan::Ready,
        ]);
        let runtime =
            WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source.clone()));
        let client = runtime.client();
        client
            .call(replace(vec![key()]))
            .await
            .expect("the initial replacement should establish the active key");
        wait_for_count(&source.fetch_calls, 1).await;

        let dropped_client = client.clone();
        let request = tokio::spawn(async move {
            dropped_client
                .call(WatchListRequest::Retry { key: key() })
                .await
        });
        started
            .await
            .expect("the retry setup should start before cancellation");
        request.abort();
        assert!(
            request
                .await
                .expect_err("a cancelled retry task should abort")
                .is_cancelled()
        );
        dropped
            .await
            .expect("caller cancellation should drop the retry setup");

        assert_eq!(source.subscription_calls.load(Ordering::Acquire), 2);
        assert_eq!(source.fetch_calls.load(Ordering::Acquire), 1);

        client
            .call(WatchListRequest::Retry { key: key() })
            .await
            .expect("the following retry should be confirmed");
        wait_for_count(&source.subscription_calls, 3).await;
        wait_for_count(&source.fetch_calls, 2).await;
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn expired_retry_should_not_replace_the_active_stream_or_poison_the_following_retry() {
        let (pending_plan, started, dropped) = pending_subscription_plan();
        let source = gated_ticker_source(vec![
            SubscriptionPlan::Ready,
            pending_plan,
            SubscriptionPlan::Ready,
        ]);
        let runtime =
            WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source.clone()));
        let client = runtime.client();
        client
            .call(replace(vec![key()]))
            .await
            .expect("the initial replacement should establish the active key");
        wait_for_count(&source.fetch_calls, 1).await;

        let expired_client = client.clone();
        let expired = tokio::spawn(async move {
            expired_client
                .call_with_timeout(
                    WatchListRequest::Retry { key: key() },
                    Duration::from_secs(1),
                )
                .await
        });
        started
            .await
            .expect("the retry setup should start before its deadline");
        tokio::time::advance(Duration::from_secs(1)).await;
        let error = expired
            .await
            .expect("the expired retry task should finish")
            .expect_err("an expired retry must not be confirmed");
        dropped
            .await
            .expect("deadline expiry should drop the retry setup");

        assert!(matches!(error, RpcCallError::DeadlineExceeded { .. }));
        assert_eq!(source.subscription_calls.load(Ordering::Acquire), 2);
        assert_eq!(source.fetch_calls.load(Ordering::Acquire), 1);

        client
            .call(WatchListRequest::Retry { key: key() })
            .await
            .expect("the following retry should be confirmed");
        wait_for_count(&source.subscription_calls, 3).await;
        wait_for_count(&source.fetch_calls, 2).await;
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn removing_a_key_should_drop_its_subscription() {
        let (source, _events, _event_received, dropped) = fake_source(ticker(1, 100));
        let runtime = WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source));
        let mut latest = runtime.latest_stream();
        load_initial_snapshot(&runtime, &mut latest).await;

        runtime
            .client()
            .call(replace(Vec::new()))
            .await
            .expect("an empty replacement should cancel the ticker stream");

        tokio::time::timeout(Duration::from_secs(1), dropped.notified())
            .await
            .expect("the cancelled ticker subscription should be dropped");
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn an_unchanged_replacement_should_leave_the_running_subscription_untouched() {
        let (source, _events, _event_received, _dropped) = fake_source(ticker(1, 100));
        let runtime = WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source));
        let mut latest = runtime.latest_stream();
        load_initial_snapshot(&runtime, &mut latest).await;

        runtime
            .client()
            .call(replace(vec![key()]))
            .await
            .expect("an unchanged replacement should stay valid");

        assert!(
            latest.next_unseen().is_none(),
            "an unchanged replacement must not reopen the stream it already has"
        );
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn subscribe_failure_should_wait_for_an_explicit_retry() {
        let (recovered_events, recovered_event_receiver) = mpsc::channel(1);
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempted = Arc::new(Notify::new());
        let source = ManualRetrySource {
            attempts: attempts.clone(),
            attempted: attempted.clone(),
            subscriptions: Arc::new(Mutex::new(VecDeque::from([
                Err(MarketError::ServiceClosed),
                Ok(FakeSubscription {
                    events: recovered_event_receiver,
                    event_received: Arc::new(Notify::new()),
                    dropped: Arc::new(Notify::new()),
                    disconnected: None,
                }),
            ]))),
            fetched: Arc::new(Mutex::new(VecDeque::from([ticker(2, 200)]))),
        };
        let mut runtime =
            WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source));
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("reliable stream should be available");
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(replace(vec![key()]))
            .await
            .expect("the watch-list replacement should start the stream");
        attempted.notified().await;
        assert_eq!(attempts.load(Ordering::Acquire), 1);

        let failed_generation = loop {
            let envelope = reliable
                .recv()
                .await
                .expect("the failure should remain deliverable");
            if matches!(
                event(&envelope),
                WatchListStreamEvent::Error(WatchListStreamError::MarketParticipant(
                    MarketError::ServiceClosed
                ))
            ) {
                break envelope.generation();
            }
        };

        tokio::time::advance(Duration::from_secs(3_600)).await;
        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::Acquire), 1);

        runtime
            .client()
            .call(replace(vec![key()]))
            .await
            .expect("an unchanged replacement should remain valid");
        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::Acquire), 1);

        runtime
            .client()
            .call(WatchListRequest::Retry { key: key() })
            .await
            .expect("an explicit retry should replace the failed generation");
        attempted.notified().await;
        assert_eq!(attempts.load(Ordering::Acquire), 2);
        let retry_generation = loop {
            let envelope = reliable
                .recv()
                .await
                .expect("the retry status should remain deliverable");
            if matches!(
                event(&envelope),
                WatchListStreamEvent::Status(WatchListStatus::Connecting)
            ) {
                break envelope.generation();
            }
        };
        assert!(retry_generation > failed_generation);

        let recovered = loop {
            latest
                .changed()
                .await
                .expect("the recovered ticker should be published");
            let Some(envelope) = latest.next_unseen() else {
                continue;
            };
            let WatchListStreamEvent::Snapshot { ticker, .. } = event(&envelope) else {
                continue;
            };
            if ticker.monotonic_id == Some(2) {
                break ticker.clone();
            }
        };

        assert_eq!(recovered.close.value, Price::from(200_u64));
        drop(recovered_events);
        runtime.shutdown().await;
    }

    /// A retry whose listing cannot be subscribed fails the row's own stream, and
    /// nothing else: the request is still applied, because the row — not the whole
    /// watch list — is what has something to report.
    #[tokio::test]
    async fn a_retry_whose_listing_cannot_be_subscribed_should_fail_only_that_stream() {
        let (_events, event_receiver) = mpsc::channel(1);
        let unservable = Arc::new(AtomicBool::new(false));
        let source = UnservableListingSource {
            unservable: unservable.clone(),
            subscription: Arc::new(Mutex::new(Some(FakeSubscription {
                events: event_receiver,
                event_received: Arc::new(Notify::new()),
                dropped: Arc::new(Notify::new()),
                disconnected: None,
            }))),
            fetched: Arc::new(ticker(1, 100)),
        };
        let mut runtime =
            WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source));
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("reliable stream should be available");
        let mut latest = runtime.latest_stream();

        runtime
            .client()
            .call(replace(vec![key()]))
            .await
            .expect("the first replacement should be confirmed");
        tokio::time::timeout(Duration::from_secs(1), latest.changed())
            .await
            .expect("the first generation should publish its snapshot")
            .expect("latest stream should remain open");
        let _ = latest.next_unseen();

        unservable.store(true, Ordering::Release);
        runtime
            .client()
            .call(WatchListRequest::Retry { key: key() })
            .await
            .expect("a listing the source cannot serve is a failed row, not a failed request");

        let error = receive_error(&mut reliable).await;

        assert!(matches!(
            error,
            WatchListStreamError::MarketParticipant(MarketError::ServiceClosed)
        ));
        assert!(latest.next_unseen().is_none());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn retry_of_a_key_that_is_not_followed_should_be_refused() {
        let (source, _subscription_calls, _fetch_calls) = counting_source();
        let runtime = WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source));

        let error = runtime
            .client()
            .call(WatchListRequest::Retry { key: key() })
            .await
            .expect_err("retrying an unfollowed listing must be refused");

        assert!(matches!(
            error,
            RpcCallError::Service {
                source: WatchListServiceError::EntryNotActive,
                ..
            }
        ));
        runtime.shutdown().await;
    }

    /// A replacement the service refuses is refused before anything is asked of a
    /// source: a duplicate entry and an over-capacity list both leave the followed
    /// streams exactly as they were.
    #[tokio::test]
    async fn duplicate_entries_should_be_refused_without_source_calls() {
        let (source, subscription_calls, fetch_calls) = counting_source();
        let runtime = WidgetRuntime::spawn(current_test_handle(), WatchListService::new(source));

        let error = runtime
            .client()
            .call(replace(vec![key(), key()]))
            .await
            .expect_err("a duplicated listing must be refused");

        assert!(matches!(
            error,
            RpcCallError::Service {
                source: WatchListServiceError::DuplicateEntry { .. },
                ..
            }
        ));
        assert_eq!(
            (
                subscription_calls.load(Ordering::Acquire),
                fetch_calls.load(Ordering::Acquire)
            ),
            (0, 0),
            "a refused replacement must not reach a source"
        );
        runtime.shutdown().await;
    }
}

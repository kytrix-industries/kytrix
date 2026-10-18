//! The candlestick protocol, its background service, and the seam the service
//! reads market data through.
//!
//! One service instance owns at most one live generation, keyed by the exact
//! listing and interval it was started for. Everything that crosses the boundary
//! is expressed in those two dimensions, so a message that belongs to an
//! interval the user has already left — or to a listing the pane has moved off —
//! can be recognized and dropped by both sides.
//!
//! The service keeps the whole series it publishes. A page fetched by panning
//! and a live tick both end up in the same [`CandlestickSeries`], which is why
//! the chart never sees the seam between history and realtime: by the time a
//! snapshot is published, the two are already merged and deduplicated.

use std::{collections::BTreeMap, future::Future, sync::Arc};

use futures::StreamExt;
use gpui_kit::{App, SharedString};
use i18n::t;
use market_participants_connector::market::{
    domain::DisconnectReason,
    error::MarketError,
    provider::MarketCapability,
    streams::CandlestickStream,
    timeframe::{Adjustment, TimeFrame},
};
use market_participants_model::{
    decimal::UnixMillis,
    identifier::VenueId,
    listing::{
        route::{VenueListingKey, VenueRoute},
        rules::ListingPrecision,
    },
    market::book::ListingCandlestick,
};
use thiserror::Error;
use tokio::sync::mpsc;
use widget::{
    error::LocalizedError,
    protocol::WidgetProtocol,
    runtime::{ServiceContext, WidgetService},
    stream::StreamPublisher,
};

use super::model::{CandlestickSeries, CandlestickSeriesSnapshot};

/// Commands the UI may queue for a running stream.
const STREAM_COMMAND_CAPACITY: usize = 8;

#[derive(Debug)]
pub(super) enum CandlestickRequest {
    /// Starts the stream for this scope. Idempotent while one is already live.
    Load {
        listing: VenueListingKey,
        timeframe: TimeFrame,
        adjustment: Adjustment,
    },
    /// Replaces the current generation, for a scope the user is retrying.
    Retry {
        listing: VenueListingKey,
        timeframe: TimeFrame,
        adjustment: Adjustment,
    },
    /// Fetches the page before the oldest candle the stream currently holds.
    FetchHistory { before_timestamp: UnixMillis },
    /// Fetches the page after the newest candle the stream currently holds.
    FetchNewer { after_timestamp: UnixMillis },
}

#[derive(Debug)]
pub(super) enum CandlestickResponse {
    StreamStarted,
    HistoryQueued,
    NewerQueued,
}

/// The scope one live candle stream belongs to.
///
/// The adjustment is one of the scope's dimensions rather than a property of the
/// candles: two streams that asked for different conventions hold different
/// prices for the same timestamp, so a snapshot of one may not be shown as the
/// other.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct CandlestickStreamKey {
    listing: VenueListingKey,
    timeframe: TimeFrame,
    adjustment: Adjustment,
}

impl CandlestickStreamKey {
    pub(super) fn new(
        listing: VenueListingKey,
        timeframe: TimeFrame,
        adjustment: Adjustment,
    ) -> Self {
        Self {
            listing,
            timeframe,
            adjustment,
        }
    }

    /// The venue whose candle close a snapshot from this scope is counted down to.
    ///
    /// A candle closes on its venue's clock, so the countdown follows the scope
    /// the candles came from rather than whatever the widget is configured with
    /// when it happens to be drawn.
    pub(super) const fn venue(&self) -> &VenueId {
        &self.listing.route.venue
    }
}

/// What the stream is doing, as far as the chart needs to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum CandlestickStatus {
    /// Subscribing to the live candle stream.
    Connecting,
    /// Subscribed, and waiting for the first history page.
    LoadingHistory,
    /// A snapshot is published and the stream is keeping it current.
    Ready,
    /// Terminal. The stream task stays alive until the generation is replaced.
    Error(MarketError),
}

#[derive(Clone, Debug)]
pub(super) enum CandlestickStreamMessage {
    Status(CandlestickStatus),
    /// The whole series, merged. Replaces whatever the chart was holding.
    Snapshot(CandlestickSeriesSnapshot),
}

pub(super) struct CandlestickProtocol;

impl WidgetProtocol for CandlestickProtocol {
    type Request = CandlestickRequest;
    type Response = CandlestickResponse;
    type StreamKey = CandlestickStreamKey;
    type StreamMessage = CandlestickStreamMessage;
    type Error = CandlestickServiceError;
}

/// Failure of a request against the running stream.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub(super) enum CandlestickServiceError {
    #[error("candlestick stream is not active")]
    NoActiveStream,
    #[error("candlestick stream command queue is closed")]
    StreamClosed,
    #[error("candlestick stream command queue did not accept the request before its deadline")]
    StreamBusy,
}

impl LocalizedError for CandlestickServiceError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::NoActiveStream => t!(cx, "error.candlestick.no_active_stream"),
            Self::StreamClosed => t!(cx, "error.candlestick.stream_closed"),
            Self::StreamBusy => t!(cx, "error.candlestick.stream_busy"),
        }
    }
}

/// One page of candles the service reads through its source.
///
/// The page is listing-scoped, interval-scoped and adjustment-scoped, which is
/// everything a market request carries: the venue's own wire symbol is resolved
/// inside the builder, so nothing here names a symbol or a market.
#[derive(Clone, Debug)]
pub(super) struct CandlestickPageQuery {
    listing: VenueListingKey,
    timeframe: TimeFrame,
    adjustment: Adjustment,
    start_timestamp: Option<UnixMillis>,
    end_timestamp: Option<UnixMillis>,
    limit: u16,
}

impl CandlestickPageQuery {
    /// Names the page one request asks a source for.
    pub(super) const fn new(
        listing: VenueListingKey,
        timeframe: TimeFrame,
        adjustment: Adjustment,
        start_timestamp: Option<UnixMillis>,
        end_timestamp: Option<UnixMillis>,
        limit: u16,
    ) -> Self {
        Self {
            listing,
            timeframe,
            adjustment,
            start_timestamp,
            end_timestamp,
            limit,
        }
    }
}

/// The market data one candlestick service reads through.
pub(super) trait CandlestickSource: Clone + Send + Sync + 'static {
    type Subscription: CandlestickEventSubscription;

    /// The intervals one route publishes.
    ///
    /// A route's own descriptor answers which intervals it serves, and the chart asks
    /// before it subscribes: an interval this build persisted may have stopped being
    /// published, and resetting it here is what keeps the service from ever being asked for
    /// an interval the route does not serve. The query is a lookup rather than a request, so
    /// it is the one question the chart asks its source directly.
    fn supported_timeframes(&self, route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError>;

    /// The price-adjustment conventions one route publishes.
    ///
    /// The same lookup as [`Self::supported_timeframes`], for the other dimension of a
    /// candlestick request: a convention this build persisted may have stopped being
    /// published, and the route's own descriptor is what answers whether it still is.
    fn supported_adjustments(&self, route: &VenueRoute) -> Result<Vec<Adjustment>, MarketError>;

    /// Fetches one page of candles for one listing, interval and adjustment.
    fn fetch_candlesticks(
        &self,
        query: CandlestickPageQuery,
    ) -> impl Future<Output = Result<Vec<ListingCandlestick>, MarketError>> + Send;

    /// Opens one live candle stream for one listing, interval and adjustment.
    ///
    /// Opening the stream is also what validates the scope: the request is
    /// listing-scoped and the interval and adjustment are checked against the route's
    /// own descriptor, so a value this route does not publish is reported as the
    /// market's own rejection rather than as an empty chart.
    fn subscribe_candlesticks(
        &self,
        listing: VenueListingKey,
        timeframe: TimeFrame,
        adjustment: Adjustment,
    ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send;
}

/// One live candle stream.
pub(super) trait CandlestickEventSubscription: Send + 'static {
    /// The listing's precision, whose price increment every price label is rounded
    /// to.
    ///
    /// It comes from the listing's own rules, which the request resolved on its
    /// way to the stream: the tick belongs to the listing, not to the venue's
    /// wire.
    fn precision(&self) -> ListingPrecision;

    fn recv(
        &mut self,
    ) -> impl Future<Output = Option<Result<Arc<ListingCandlestick>, MarketError>>> + Send;
}

impl CandlestickSource for MarketCapability {
    type Subscription = ClientCandlestickSubscription;

    fn supported_timeframes(&self, route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError> {
        MarketCapability::supported_timeframes(self, route)
    }

    fn supported_adjustments(&self, route: &VenueRoute) -> Result<Vec<Adjustment>, MarketError> {
        MarketCapability::supported_adjustments(self, route)
    }

    fn fetch_candlesticks(
        &self,
        query: CandlestickPageQuery,
    ) -> impl Future<Output = Result<Vec<ListingCandlestick>, MarketError>> + Send {
        let market = self.clone();
        async move {
            let listing = query.listing;
            let mut builder = market
                .market(listing.route.clone())
                .candlesticks(listing, query.timeframe)
                .adjustment(query.adjustment)
                .limit(query.limit);
            if let Some(start_timestamp) = query.start_timestamp {
                builder = builder.from(start_timestamp);
            }
            if let Some(end_timestamp) = query.end_timestamp {
                builder = builder.to(end_timestamp);
            }
            builder.fetch().await
        }
    }

    fn subscribe_candlesticks(
        &self,
        listing: VenueListingKey,
        timeframe: TimeFrame,
        adjustment: Adjustment,
    ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send {
        let market = self.clone();
        async move {
            // The listing's own rules carry the increments the chart's labels are
            // rounded to, and resolving the listing is what rejects one this route no
            // longer publishes before a stream is opened for it.
            let resolved = market.resolve_listing(&listing)?;
            let route = resolved.key.route.clone();
            let inner = market
                .market(route)
                .candlesticks(listing, timeframe)
                .adjustment(adjustment)
                .subscribe()
                .await?;
            Ok(ClientCandlestickSubscription {
                inner,
                precision: ListingPrecision::of(&resolved.rules),
            })
        }
    }
}

pub(super) struct ClientCandlestickSubscription {
    inner: CandlestickStream,
    precision: ListingPrecision,
}

impl CandlestickEventSubscription for ClientCandlestickSubscription {
    fn precision(&self) -> ListingPrecision {
        self.precision
    }

    async fn recv(&mut self) -> Option<Result<Arc<ListingCandlestick>, MarketError>> {
        self.inner.next().await.map(|result| result.map(Arc::new))
    }
}

pub(super) struct CandlestickService<S = MarketCapability> {
    source: S,
    history_limit: u16,
    maximum_candles: usize,
}

impl<S> CandlestickService<S> {
    /// Creates a service holding at most `maximum_candles` candles.
    ///
    /// A zero capacity would make the series unable to hold even the candle it
    /// is building, so it is clamped here rather than trusted.
    pub(super) const fn new(source: S, history_limit: u16, maximum_candles: usize) -> Self {
        Self {
            source,
            history_limit,
            maximum_candles: if maximum_candles == 0 {
                1
            } else {
                maximum_candles
            },
        }
    }
}

impl<S> WidgetService<CandlestickProtocol> for CandlestickService<S>
where
    S: CandlestickSource,
{
    async fn run(self, mut context: ServiceContext<CandlestickProtocol>) {
        let mut active_key: Option<CandlestickStreamKey> = None;
        let mut active_commands: Option<mpsc::Sender<StreamCommand>> = None;

        while let Some(request) = context.next_rpc().await {
            let (mut reply, request) = request.into_parts();
            match request {
                request @ (CandlestickRequest::Load { .. } | CandlestickRequest::Retry { .. }) => {
                    let (listing, timeframe, adjustment, retry) = match request {
                        CandlestickRequest::Load {
                            listing,
                            timeframe,
                            adjustment,
                        } => (listing, timeframe, adjustment, false),
                        CandlestickRequest::Retry {
                            listing,
                            timeframe,
                            adjustment,
                        } => (listing, timeframe, adjustment, true),
                        CandlestickRequest::FetchHistory { .. }
                        | CandlestickRequest::FetchNewer { .. } => continue,
                    };
                    let key = CandlestickStreamKey::new(listing.clone(), timeframe, adjustment);
                    // A repeated load for the scope that is already running is
                    // answered without touching the live stream: the caller
                    // has not asked for anything new.
                    if !retry && active_key.as_ref() == Some(&key) {
                        let _ = reply.respond(Ok(CandlestickResponse::StreamStarted));
                        continue;
                    }
                    // Streams are keyed, so a new scope would otherwise leave
                    // the previous one running and publishing into the same
                    // widget. The previous command sender is dropped when the
                    // new one replaces it below, which also lets the old
                    // generation notice that nobody can page it any more.
                    if active_key.as_ref() != Some(&key)
                        && let Some(previous) = active_key.take()
                    {
                        context.cancel_stream(&previous);
                    }

                    let (commands, stream_commands) = mpsc::channel(STREAM_COMMAND_CAPACITY);
                    let source = self.source.clone();
                    let spec = StreamSpec {
                        listing,
                        timeframe,
                        adjustment,
                        history_limit: self.history_limit,
                        maximum_candles: self.maximum_candles,
                    };
                    if retry {
                        context.replace_stream(key.clone(), move |publisher| async move {
                            run_stream(source, spec, stream_commands, publisher).await;
                        });
                    } else {
                        context.start_stream(key.clone(), move |publisher| async move {
                            run_stream(source, spec, stream_commands, publisher).await;
                        });
                    }
                    active_key = Some(key);
                    active_commands = Some(commands);
                    if reply
                        .respond(Ok(CandlestickResponse::StreamStarted))
                        .is_err()
                    {
                        if let Some(previous) = active_key.take() {
                            context.cancel_stream(&previous);
                        }
                        active_commands = None;
                    }
                }
                CandlestickRequest::FetchHistory { before_timestamp } => {
                    let Some(commands) = active_commands.as_ref() else {
                        let _ = reply.respond(Err(CandlestickServiceError::NoActiveStream));
                        continue;
                    };
                    let deadline = reply.deadline();
                    let result = tokio::select! {
                        biased;
                        () = tokio::time::sleep_until(deadline) => {
                            Err(CandlestickServiceError::StreamBusy)
                        }
                        () = reply.cancelled() => continue,
                        result = commands.send(StreamCommand::FetchHistory { before_timestamp }) => {
                            result
                                .map(|()| CandlestickResponse::HistoryQueued)
                                .map_err(|_| CandlestickServiceError::StreamClosed)
                        }
                    };
                    let command_failed = result.is_err();
                    let response_dropped = reply.respond(result).is_err();
                    if command_failed || response_dropped {
                        if let Some(previous) = active_key.take() {
                            context.cancel_stream(&previous);
                        }
                        active_commands = None;
                    }
                }
                CandlestickRequest::FetchNewer { after_timestamp } => {
                    let Some(commands) = active_commands.as_ref() else {
                        let _ = reply.respond(Err(CandlestickServiceError::NoActiveStream));
                        continue;
                    };
                    let deadline = reply.deadline();
                    let result = tokio::select! {
                        biased;
                        () = tokio::time::sleep_until(deadline) => {
                            Err(CandlestickServiceError::StreamBusy)
                        }
                        () = reply.cancelled() => continue,
                        result = commands.send(StreamCommand::FetchNewer { after_timestamp }) => {
                            result
                                .map(|()| CandlestickResponse::NewerQueued)
                                .map_err(|_| CandlestickServiceError::StreamClosed)
                        }
                    };
                    let command_failed = result.is_err();
                    let response_dropped = reply.respond(result).is_err();
                    if command_failed || response_dropped {
                        if let Some(previous) = active_key.take() {
                            context.cancel_stream(&previous);
                        }
                        active_commands = None;
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
struct StreamSpec {
    listing: VenueListingKey,
    timeframe: TimeFrame,
    adjustment: Adjustment,
    history_limit: u16,
    maximum_candles: usize,
}

enum StreamCommand {
    FetchHistory { before_timestamp: UnixMillis },
    FetchNewer { after_timestamp: UnixMillis },
}

/// Runs one stream generation until it fails or is cancelled.
async fn run_stream<S>(
    source: S,
    spec: StreamSpec,
    commands: mpsc::Receiver<StreamCommand>,
    publisher: StreamPublisher<CandlestickProtocol>,
) where
    S: CandlestickSource,
{
    if !publish_status(&publisher, CandlestickStatus::Connecting).await {
        return;
    }
    if let Err(error) = run_stream_once(&source, &spec, commands, &publisher).await {
        log::warn!("candlestick stream failed: {error}");
        // A terminal failure must not retry on its own: the view decides when to
        // ask for a replacement generation, so the task stays alive until then.
        if publish_status(&publisher, CandlestickStatus::Error(error)).await {
            publisher.cancelled().await;
        }
    }
}

async fn run_stream_once<S>(
    source: &S,
    spec: &StreamSpec,
    mut commands: mpsc::Receiver<StreamCommand>,
    publisher: &StreamPublisher<CandlestickProtocol>,
) -> Result<(), MarketError>
where
    S: CandlestickSource,
{
    let listing = spec.listing.clone();
    let timeframe = spec.timeframe;
    let adjustment = spec.adjustment;
    // The subscription is the request that validates the scope: it resolves the
    // listing inside the published catalog and rejects an interval or an adjustment
    // this route's own descriptor does not publish.
    let subscription = source.subscribe_candlesticks(listing.clone(), timeframe, adjustment);
    tokio::pin!(subscription);
    let mut subscription = tokio::select! {
        biased;
        () = publisher.cancelled() => return Ok(()),
        result = &mut subscription => result?,
    };
    let precision = subscription.precision();

    if !publish_status(publisher, CandlestickStatus::LoadingHistory).await {
        return Ok(());
    }

    // The live stream is open before history is requested, so candles that move
    // while the page is in flight are buffered rather than missed.
    let (history, buffered) = fetch_while_buffering(
        source,
        CandlestickPageQuery::new(
            listing.clone(),
            timeframe,
            adjustment,
            None,
            None,
            spec.history_limit,
        ),
        &mut subscription,
        spec.maximum_candles,
        publisher,
    )
    .await?;
    let history_is_empty = history.is_empty();
    let mut series = CandlestickSeries::new(spec.maximum_candles, precision);
    series.merge_initial(history);
    series.merge_initial(buffered.into_values().map(|candle| candle.as_ref().clone()));
    if history_is_empty {
        // Nothing before the first candle exists, so panning left is already at
        // the end of history and must not keep asking for more.
        series.mark_older_exhausted();
    }
    if !publish_snapshot(publisher, series.snapshot()) {
        return Ok(());
    }
    if !publish_status(publisher, CandlestickStatus::Ready).await {
        return Ok(());
    }

    loop {
        tokio::select! {
                biased;
                () = publisher.cancelled() => return Ok(()),
                command = commands.recv() => {
                    let Some(command) = command else {
                        return Ok(());
                    };
                    let (query, load_older) = match command {
                        StreamCommand::FetchHistory { before_timestamp } => {
                            // A duplicate or stale request is dropped here rather
                            // than passed on: the view may have panned further
                            // while this one was queued.
                            if series.older_exhausted()
                                || series.earliest_timestamp().is_some_and(|earliest| before_timestamp > earliest)
                            {
                                continue;
                            }
                            (
                                CandlestickPageQuery::new(
                                    listing.clone(),
                                    timeframe,
                                    adjustment,
                                    None,
                                    Some(before_timestamp.saturating_sub(1)),
                                    spec.history_limit,
                                ),
                                true,
                            )
                        }
                        StreamCommand::FetchNewer { after_timestamp } => {
                            if series.at_live_edge()
                                || series.latest_timestamp().is_some_and(|latest| after_timestamp < latest)
                            {
                                continue;
                            }
                            (
                                CandlestickPageQuery::new(
                                    listing.clone(),
                                    timeframe,
                                    adjustment,
                                    Some(after_timestamp.saturating_add(1)),
                                    None,
                                    spec.history_limit,
                                ),
                                false,
                            )
                        }
                    };
                    let (page, buffered) = fetch_while_buffering(
                        source,
                        query,
                        &mut subscription,
                        spec.maximum_candles,
                        publisher,
                    )
                    .await?;
                    let page_len = page.len();
                    if load_older {
                        if page.is_empty() {
                            series.mark_older_exhausted();
                        } else {
                            series.merge_older(page);
                        }
                    } else {
                        series.merge_newer(page);
                        // A short page means the venue had nothing more to give, so
                        // the window has reached the newest candle again.
                        if page_len < usize::from(spec.history_limit) {
                            series.mark_at_live_edge();
                        }
                    }
                    if series.at_live_edge() {
                        series.merge_newer(
                            buffered
                                .into_values()
                                .map(|candle| candle.as_ref().clone()),
                        );
                    }
        if !publish_snapshot(publisher, series.snapshot()) {
                        return Ok(());
                    }
                    if !publish_status(publisher, CandlestickStatus::Ready).await {
                        return Ok(());
                    }
                }
                event = subscription.recv() => {
                    match event {
                        Some(Ok(candle)) => {
                            if series.upsert_live(candle.as_ref())
                                && !publish_snapshot(publisher, series.snapshot())
                            {
                                return Ok(());
                            }
                        }
                        Some(Err(error)) => return Err(error),
                        None => {
                            return Err(MarketError::Disconnected(DisconnectReason::Normal));
                        }
                    }
                }
            }
    }
}

/// Publishes a status, reporting whether the stream is still alive.
async fn publish_status(
    publisher: &StreamPublisher<CandlestickProtocol>,
    status: CandlestickStatus,
) -> bool {
    publisher
        .publish_reliable(CandlestickStreamMessage::Status(status))
        .await
        .is_ok()
}

/// Publishes a snapshot, reporting whether the stream is still alive.
fn publish_snapshot(
    publisher: &StreamPublisher<CandlestickProtocol>,
    snapshot: CandlestickSeriesSnapshot,
) -> bool {
    publisher
        .publish_latest(CandlestickStreamMessage::Snapshot(snapshot))
        .is_ok()
}

/// Fetches one page while keeping the live stream drained.
///
/// History requests take seconds on a slow venue. Any candle published while one
/// is in flight is newer than the page being fetched, so it is buffered instead
/// of dropped, and the caller merges the buffer once the page has landed.
async fn fetch_while_buffering<S>(
    source: &S,
    query: CandlestickPageQuery,
    subscription: &mut S::Subscription,
    maximum_candles: usize,
    publisher: &StreamPublisher<CandlestickProtocol>,
) -> Result<
    (
        Vec<ListingCandlestick>,
        BTreeMap<UnixMillis, Arc<ListingCandlestick>>,
    ),
    MarketError,
>
where
    S: CandlestickSource,
{
    let fetch = source.fetch_candlesticks(query);
    tokio::pin!(fetch);
    let mut buffered = BTreeMap::new();

    loop {
        tokio::select! {
            biased;
            () = publisher.cancelled() => return Err(MarketError::ServiceClosed),
            result = &mut fetch => {
                return result.map(|history| (bounded_history(history, maximum_candles), buffered));
            }
            event = subscription.recv() => {
                match event {
                    Some(Ok(candle)) => buffer_candle(&mut buffered, candle, maximum_candles),
                    Some(Err(error)) => return Err(error),
                    None => {
                        return Err(MarketError::Disconnected(DisconnectReason::Normal));
                    }
                }
            }
        }
    }
}

/// Keeps the newest `maximum_candles` rows of a fetched page.
fn bounded_history(
    history: Vec<ListingCandlestick>,
    maximum_candles: usize,
) -> Vec<ListingCandlestick> {
    let limit = maximum_candles.max(1);
    let mut bounded = BTreeMap::new();
    for candle in history {
        if let Some(current) = bounded.get_mut(&candle.timestamp) {
            *current = candle;
            continue;
        }
        if bounded.len() == limit {
            if bounded
                .first_key_value()
                .is_some_and(|(&earliest, _)| candle.timestamp < earliest)
            {
                continue;
            }
            bounded.pop_first();
        }
        bounded.insert(candle.timestamp, candle);
    }
    bounded.into_values().collect()
}

fn buffer_candle(
    buffered: &mut BTreeMap<UnixMillis, Arc<ListingCandlestick>>,
    candle: Arc<ListingCandlestick>,
    maximum_candles: usize,
) {
    buffered.insert(candle.timestamp, candle);
    while buffered.len() > maximum_candles.max(1) {
        buffered.pop_first();
    }
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
        str::FromStr,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use market_participants_connector::catalog::{error::SourceError, test_support};
    use market_participants_model::{
        decimal::{Price, Quantity, Tick},
        listing::route::{VenueListingKey, VenueRoute},
        market::{
            book::ListingCandlestick,
            price::MarketPrice,
            units::{MarketQuantity, QuantityUnit},
        },
    };
    use smol_str::SmolStr;
    use tokio::sync::{Mutex, Notify, mpsc, oneshot};
    use widget::{rpc::RpcCallError, runtime::WidgetRuntime};

    use super::*;
    use crate::time::TIMEFRAMES;

    fn route() -> VenueRoute {
        test_support::spot_route()
    }

    fn listing() -> VenueListingKey {
        test_support::listing_key(&route(), "BTCUSDT")
    }

    fn test_price_tick() -> Tick {
        Tick::from_str("0.01").expect("price tick should parse")
    }

    /// The precision the fake subscriptions report for their listing.
    fn test_precision() -> ListingPrecision {
        ListingPrecision {
            price_tick: test_price_tick(),
            quantity_tick: Tick::from_str("0.001").expect("quantity tick should parse"),
        }
    }

    fn candle(timestamp: i64, close: u64) -> ListingCandlestick {
        let price = Price::from(close);
        ListingCandlestick {
            listing: listing(),
            open: MarketPrice {
                value: price,
                quote_asset: test_support::currency("USD"),
                basis: market_participants_model::listing::venue::PriceBasis::PerAssetUnit,
            },
            high: MarketPrice {
                value: price,
                quote_asset: test_support::currency("USD"),
                basis: market_participants_model::listing::venue::PriceBasis::PerAssetUnit,
            },
            low: MarketPrice {
                value: price,
                quote_asset: test_support::currency("USD"),
                basis: market_participants_model::listing::venue::PriceBasis::PerAssetUnit,
            },
            close: MarketPrice {
                value: price,
                quote_asset: test_support::currency("USD"),
                basis: market_participants_model::listing::venue::PriceBasis::PerAssetUnit,
            },
            traded_quantity: Some(MarketQuantity {
                value: Quantity::from(1_u64),
                unit: QuantityUnit::Contracts,
            }),
            traded_notional: None,
            timestamp,
        }
    }

    fn close_of(candle: &ListingCandlestick) -> Price {
        candle.close.value
    }

    fn snapshot_of(message: &CandlestickStreamMessage) -> Option<&CandlestickSeriesSnapshot> {
        match message {
            CandlestickStreamMessage::Snapshot(snapshot) => Some(snapshot),
            CandlestickStreamMessage::Status(_) => None,
        }
    }

    /// A source whose subscription and history page are supplied by the test.
    #[derive(Clone)]
    struct FakeSource {
        subscription: Arc<Mutex<Option<FakeSubscription>>>,
        history: Arc<Mutex<Option<oneshot::Receiver<Vec<ListingCandlestick>>>>>,
        fetch_started: Arc<Notify>,
    }

    struct FakeSubscription {
        events: mpsc::Receiver<Result<Arc<ListingCandlestick>, MarketError>>,
        event_received: Arc<Notify>,
        precision: ListingPrecision,
    }

    impl FakeSource {
        fn new(
            subscription: FakeSubscription,
            history: oneshot::Receiver<Vec<ListingCandlestick>>,
            fetch_started: Arc<Notify>,
        ) -> Self {
            Self {
                subscription: Arc::new(Mutex::new(Some(subscription))),
                history: Arc::new(Mutex::new(Some(history))),
                fetch_started,
            }
        }
    }

    impl CandlestickSource for FakeSource {
        type Subscription = FakeSubscription;

        fn supported_timeframes(&self, _route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError> {
            Ok(TIMEFRAMES.to_vec())
        }

        fn supported_adjustments(
            &self,
            _route: &VenueRoute,
        ) -> Result<Vec<Adjustment>, MarketError> {
            Ok(vec![Adjustment::None])
        }

        async fn fetch_candlesticks(
            &self,
            _query: CandlestickPageQuery,
        ) -> Result<Vec<ListingCandlestick>, MarketError> {
            let history = self
                .history
                .lock()
                .await
                .take()
                .ok_or(MarketError::ServiceClosed)?;
            self.fetch_started.notify_one();
            history.await.map_err(|_| MarketError::ServiceClosed)
        }

        async fn subscribe_candlesticks(
            &self,
            _listing: VenueListingKey,
            _timeframe: TimeFrame,
            _adjustment: Adjustment,
        ) -> Result<Self::Subscription, MarketError> {
            self.subscription
                .lock()
                .await
                .take()
                .ok_or(MarketError::ServiceClosed)
        }
    }

    impl CandlestickEventSubscription for FakeSubscription {
        fn precision(&self) -> ListingPrecision {
            self.precision
        }

        async fn recv(&mut self) -> Option<Result<Arc<ListingCandlestick>, MarketError>> {
            let event = self.events.recv().await;
            if event.is_some() {
                self.event_received.notify_one();
            }
            event
        }
    }

    /// One source, one subscription, and the handles a test drives them with.
    struct Harness {
        runtime: WidgetRuntime<CandlestickProtocol>,
        events: mpsc::Sender<Result<Arc<ListingCandlestick>, MarketError>>,
        history: oneshot::Sender<Vec<ListingCandlestick>>,
        fetch_started: Arc<Notify>,
        event_received: Arc<Notify>,
    }

    fn harness(history_limit: u16, maximum_candles: usize) -> Harness {
        let (events, event_receiver) = mpsc::channel(4);
        let (history, history_receiver) = oneshot::channel();
        let fetch_started = Arc::new(Notify::new());
        let event_received = Arc::new(Notify::new());
        let source = FakeSource::new(
            FakeSubscription {
                events: event_receiver,
                event_received: event_received.clone(),
                precision: test_precision(),
            },
            history_receiver,
            fetch_started.clone(),
        );
        let runtime = WidgetRuntime::spawn(
            current_test_handle(),
            CandlestickService::new(source, history_limit, maximum_candles),
        );

        Harness {
            runtime,
            events,
            history,
            fetch_started,
            event_received,
        }
    }

    fn load(timeframe: TimeFrame) -> CandlestickRequest {
        CandlestickRequest::Load {
            listing: listing(),
            timeframe,
            adjustment: Adjustment::None,
        }
    }

    #[test]
    fn realtime_buffer_should_coalesce_updates_for_one_timestamp() {
        let mut buffered = BTreeMap::new();
        buffer_candle(&mut buffered, Arc::new(candle(1, 10)), 10);

        buffer_candle(&mut buffered, Arc::new(candle(1, 20)), 10);

        assert_eq!(
            buffered
                .first_key_value()
                .map(|(_, candle)| (buffered.len(), close_of(candle))),
            Some((1, Price::from(20_u64)))
        );
    }

    #[test]
    fn realtime_buffer_should_keep_newest_timestamps_within_the_series_limit() {
        let mut buffered = BTreeMap::new();
        buffer_candle(&mut buffered, Arc::new(candle(1, 10)), 2);
        buffer_candle(&mut buffered, Arc::new(candle(2, 20)), 2);

        buffer_candle(&mut buffered, Arc::new(candle(3, 30)), 2);

        assert_eq!(buffered.into_keys().collect::<Vec<_>>(), vec![2, 3]);
    }

    #[test]
    fn fetched_history_should_keep_only_the_newest_rows_at_the_boundary() {
        let history = [5, 1, 4, 2, 3]
            .into_iter()
            .map(|timestamp| candle(timestamp, timestamp as u64))
            .collect();

        let bounded = bounded_history(history, 2);

        assert_eq!(
            bounded
                .into_iter()
                .map(|candle| candle.timestamp)
                .collect::<Vec<_>>(),
            vec![4, 5]
        );
    }

    #[test]
    fn fetched_history_should_deduplicate_before_applying_the_boundary() {
        let history = [(1, 1), (2, 2), (2, 20), (3, 3), (3, 30), (4, 4)]
            .into_iter()
            .map(|(timestamp, close)| candle(timestamp, close))
            .collect();

        let bounded = bounded_history(history, 3);

        assert_eq!(
            bounded
                .into_iter()
                .map(|candle| (candle.timestamp, close_of(&candle)))
                .collect::<Vec<_>>(),
            vec![
                (2, Price::from(20_u64)),
                (3, Price::from(30_u64)),
                (4, Price::from(4_u64)),
            ]
        );
    }

    #[tokio::test]
    async fn initialization_should_buffer_realtime_before_history_completes() {
        let harness = harness(100, 1_000);
        let mut latest = harness.runtime.latest_stream();

        harness
            .runtime
            .client()
            .call(load(TimeFrame::Minute1))
            .await
            .expect("load request should start a stream");
        harness.fetch_started.notified().await;
        // The live candle arrives while the history page is still in flight.
        harness
            .events
            .send(Ok(Arc::new(candle(1, 25))))
            .await
            .expect("fake stream should be open");
        harness.event_received.notified().await;
        harness
            .history
            .send(vec![candle(1, 20)])
            .expect("fake history request should be waiting");

        latest
            .changed()
            .await
            .expect("merged snapshot should be published");
        let close = latest
            .next_unseen()
            .and_then(|envelope| snapshot_of(envelope.message().as_ref()).cloned())
            .and_then(|snapshot| snapshot.latest().map(close_of));

        assert_eq!(close, Some(Price::from(25_u64)));
        harness.runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_published_snapshot_should_carry_the_subscribed_price_tick() {
        let harness = harness(100, 1_000);
        let mut latest = harness.runtime.latest_stream();

        harness
            .runtime
            .client()
            .call(load(TimeFrame::Minute1))
            .await
            .expect("load request should start a stream");
        harness.fetch_started.notified().await;
        harness
            .history
            .send(vec![candle(1, 10)])
            .expect("fake history request should be waiting");
        latest
            .changed()
            .await
            .expect("initial snapshot should be published");

        let price_tick = latest
            .next_unseen()
            .and_then(|envelope| snapshot_of(envelope.message().as_ref()).cloned())
            .map(|snapshot| snapshot.price_tick());

        assert_eq!(price_tick, Some(test_price_tick()));
        harness.runtime.shutdown().await;
    }

    #[tokio::test]
    async fn reaching_capacity_should_not_mark_history_exhausted() {
        let harness = harness(100, 2);
        let mut latest = harness.runtime.latest_stream();

        harness
            .runtime
            .client()
            .call(load(TimeFrame::Minute1))
            .await
            .expect("load request should start a stream");
        harness.fetch_started.notified().await;
        harness
            .history
            .send(vec![candle(1, 10)])
            .expect("initial history request should be waiting");
        latest
            .changed()
            .await
            .expect("initial snapshot should be published");
        let initial = latest
            .next_unseen()
            .and_then(|envelope| snapshot_of(envelope.message().as_ref()).cloned())
            .expect("initial snapshot should exist");
        assert!(!initial.older_exhausted());

        harness
            .events
            .send(Ok(Arc::new(candle(2, 20))))
            .await
            .expect("fake stream should be open");
        harness.event_received.notified().await;
        latest
            .changed()
            .await
            .expect("realtime snapshot should be published");
        let realtime = latest
            .next_unseen()
            .and_then(|envelope| snapshot_of(envelope.message().as_ref()).cloned())
            .expect("realtime snapshot should exist");

        assert_eq!(realtime.candles().len(), 2);
        assert!(!realtime.older_exhausted());
        harness.runtime.shutdown().await;
    }

    /// A source whose pages are served by the query it is asked for, and which records
    /// every page query it receives.
    ///
    /// The pages are shaped so both pagination directions have something to fetch: the
    /// initial page fills the limit without exhausting history (so panning left is still
    /// possible) and is old enough to be off the live edge (so panning right is too). What
    /// each page *is* follows from which bound the query carries — a page fetched by
    /// panning left ends where the request asks it to, one fetched by panning right
    /// starts there — so the fake never has to be told which direction it is serving.
    ///
    /// A request is answered before the command that asked for it is finished with, so a
    /// test that read the recorded queries right after asking for a page could read them
    /// before that page was fetched. A test therefore waits on the snapshot the page is
    /// merged into — the stream publishes one per answered command — rather than on the
    /// request it sent.
    #[derive(Clone)]
    struct RecordingSource {
        subscription: Arc<Mutex<Option<FakeSubscription>>>,
        queries: Arc<Mutex<Vec<CandlestickPageQuery>>>,
    }

    impl RecordingSource {
        fn new(subscription: FakeSubscription) -> Self {
            Self {
                subscription: Arc::new(Mutex::new(Some(subscription))),
                queries: Arc::new(Mutex::new(Vec::with_capacity(4))),
            }
        }

        /// The adjustment convention of every page query this source received, in order.
        async fn page_adjustments(&self) -> Vec<Adjustment> {
            self.queries
                .lock()
                .await
                .iter()
                .map(|query| query.adjustment)
                .collect()
        }
    }

    impl CandlestickSource for RecordingSource {
        type Subscription = FakeSubscription;

        fn supported_timeframes(&self, _route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError> {
            Ok(TIMEFRAMES.to_vec())
        }

        fn supported_adjustments(
            &self,
            _route: &VenueRoute,
        ) -> Result<Vec<Adjustment>, MarketError> {
            Ok(vec![Adjustment::None, Adjustment::Forward])
        }

        async fn fetch_candlesticks(
            &self,
            query: CandlestickPageQuery,
        ) -> Result<Vec<ListingCandlestick>, MarketError> {
            let page = match (query.start_timestamp, query.end_timestamp) {
                // The first page of a stream. Three candles is the whole window the test
                // gives the chart, so a live candle takes it to its capacity.
                (None, None) => vec![candle(10, 10), candle(20, 20), candle(30, 30)],
                // Panning right: the venue answers from where the request starts.
                (Some(start), _) => vec![
                    candle(start, 40),
                    candle(start + 1, 50),
                    candle(start + 2, 60),
                ],
                // Panning left: the page ends where the request asks it to.
                (None, Some(end)) => vec![candle(end - 2, 1), candle(end - 1, 2), candle(end, 3)],
            };
            self.queries.lock().await.push(query);
            Ok(page)
        }

        async fn subscribe_candlesticks(
            &self,
            _listing: VenueListingKey,
            _timeframe: TimeFrame,
            _adjustment: Adjustment,
        ) -> Result<Self::Subscription, MarketError> {
            self.subscription
                .lock()
                .await
                .take()
                .ok_or(MarketError::ServiceClosed)
        }
    }

    /// Waits for one more page to reach the source than `seen`, and returns the new count.
    ///
    /// A page is fetched by the stream task rather than by the request that asked for it —
    /// the request is answered as soon as the command is queued — so a test that read the
    /// recorded queries right after sending one would read them before that page was
    /// fetched. Waiting for the count to move is the wait that actually holds, and it is
    /// bounded so a page that never comes fails the test instead of hanging it.
    async fn pages_recorded(source: &RecordingSource, seen: usize) -> usize {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let recorded = source.queries.lock().await.len();
                if recorded > seen {
                    return recorded;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!("the page the request asked for should reach the source; {seen} were recorded")
        })
    }

    /// Both pagination directions carry the convention the stream was opened with.
    ///
    /// The adjustment is a dimension of the scope, not a property of the first page: a
    /// request for the history before the window, or for the window after it, that went
    /// out as `None` would merge raw prices into a series the chart is pricing the other
    /// way — exactly the defect this phase fixed, where the platform's own paging calls
    /// hard-coded the neutral convention. The page queries are read off the source,
    /// because the service hands them to it and nowhere else.
    ///
    /// Each direction is exercised in the state its own guard admits. A chart at the live
    /// edge never asks for the page after the window, so a pushed candle takes the window
    /// off it first; and a window with room to grow never asks for the page before it, so
    /// the older page is fetched from a window that is full.
    #[tokio::test]
    async fn pagination_should_carry_the_streams_adjustment() {
        let (events, event_receiver) = mpsc::channel(2);
        let source = RecordingSource::new(FakeSubscription {
            events: event_receiver,
            event_received: Arc::new(Notify::new()),
            precision: test_precision(),
        });
        let runtime = WidgetRuntime::spawn(
            current_test_handle(),
            // The window holds exactly one page, which is what lets a pushed candle or a
            // page of history move the edge the pagination guards are read from.
            CandlestickService::new(source.clone(), 3, 3),
        );
        let client = runtime.client();
        let mut latest = runtime.latest_stream();

        client
            .call(CandlestickRequest::Load {
                listing: listing(),
                timeframe: TimeFrame::Minute1,
                adjustment: Adjustment::Forward,
            })
            .await
            .expect("the stream should start");
        latest
            .changed()
            .await
            .expect("the first page should be published");
        let mut recorded = pages_recorded(&source, 0).await;
        assert_eq!(recorded, 1, "the first page is the stream's own request");

        // A candle the venue pushed takes the window to its capacity, and panning left is
        // what takes it off the edge it was following: the page after the window is one the
        // chart asks for only once it has left it.
        events
            .send(Ok(Arc::new(candle(40, 70))))
            .await
            .expect("the live stream should be open");
        latest
            .changed()
            .await
            .expect("the live candle should be published");

        client
            .call(CandlestickRequest::FetchHistory {
                before_timestamp: 5,
            })
            .await
            .expect("a page before the window should be fetched");
        recorded = pages_recorded(&source, recorded).await;
        assert_eq!(
            recorded, 2,
            "the page before the window should have reached the source"
        );

        client
            .call(CandlestickRequest::FetchNewer {
                after_timestamp: 40,
            })
            .await
            .expect("a page after the window should be fetched");
        recorded = pages_recorded(&source, recorded).await;
        assert_eq!(
            recorded, 3,
            "the page after the window should have reached the source"
        );

        assert_eq!(
            source.page_adjustments().await,
            vec![Adjustment::Forward; 3],
            "the first page and both paging directions carry the stream's own convention"
        );
        drop(events);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn an_empty_history_page_should_mark_history_exhausted() {
        let harness = harness(100, 1_000);
        let mut latest = harness.runtime.latest_stream();

        harness
            .runtime
            .client()
            .call(load(TimeFrame::Minute1))
            .await
            .expect("load request should start a stream");
        harness.fetch_started.notified().await;
        harness
            .history
            .send(Vec::new())
            .expect("initial history request should be waiting");
        latest
            .changed()
            .await
            .expect("empty snapshot should be published");

        let snapshot = latest
            .next_unseen()
            .and_then(|envelope| snapshot_of(envelope.message().as_ref()).cloned())
            .expect("empty snapshot should exist");

        assert!(snapshot.candles().is_empty());
        assert!(snapshot.older_exhausted());
        harness.runtime.shutdown().await;
    }

    #[derive(Clone)]
    struct BackpressuredSource {
        subscription: Arc<Mutex<Option<FakeSubscription>>>,
        fetch_count: Arc<AtomicUsize>,
        blocked_fetch_started: Arc<Notify>,
        release_blocked_fetch: Arc<Notify>,
    }

    impl CandlestickSource for BackpressuredSource {
        type Subscription = FakeSubscription;

        fn supported_timeframes(&self, _route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError> {
            Ok(TIMEFRAMES.to_vec())
        }

        fn supported_adjustments(
            &self,
            _route: &VenueRoute,
        ) -> Result<Vec<Adjustment>, MarketError> {
            Ok(vec![Adjustment::None])
        }

        async fn fetch_candlesticks(
            &self,
            _query: CandlestickPageQuery,
        ) -> Result<Vec<ListingCandlestick>, MarketError> {
            let fetch_index = self.fetch_count.fetch_add(1, Ordering::AcqRel);
            match fetch_index {
                0 => Ok(vec![candle(100, 100)]),
                1 => {
                    self.blocked_fetch_started.notify_one();
                    self.release_blocked_fetch.notified().await;
                    Ok(vec![candle(50, 50)])
                }
                _ => Ok(vec![candle(50, 50)]),
            }
        }

        async fn subscribe_candlesticks(
            &self,
            _listing: VenueListingKey,
            _timeframe: TimeFrame,
            _adjustment: Adjustment,
        ) -> Result<Self::Subscription, MarketError> {
            self.subscription
                .lock()
                .await
                .take()
                .ok_or(MarketError::ServiceClosed)
        }
    }

    #[tokio::test]
    async fn timed_out_history_request_should_cancel_the_stream() {
        let (events, event_receiver) = mpsc::channel(1);
        let fetch_count = Arc::new(AtomicUsize::new(0));
        let blocked_fetch_started = Arc::new(Notify::new());
        let release_blocked_fetch = Arc::new(Notify::new());
        let source = BackpressuredSource {
            subscription: Arc::new(Mutex::new(Some(FakeSubscription {
                events: event_receiver,
                event_received: Arc::new(Notify::new()),
                precision: test_precision(),
            }))),
            fetch_count: fetch_count.clone(),
            blocked_fetch_started: blocked_fetch_started.clone(),
            release_blocked_fetch: release_blocked_fetch.clone(),
        };
        let runtime = WidgetRuntime::spawn(
            current_test_handle(),
            CandlestickService::new(source, 100, 1_000),
        );
        let mut latest = runtime.latest_stream();
        let client = runtime.client();

        client
            .call(load(TimeFrame::Minute1))
            .await
            .expect("load request should start a stream");
        latest
            .changed()
            .await
            .expect("initial snapshot should be published");
        let _ = latest.next_unseen();

        client
            .call(CandlestickRequest::FetchHistory {
                before_timestamp: 0,
            })
            .await
            .expect("first history request should start");
        blocked_fetch_started.notified().await;
        // Fill the command queue so the next request cannot be accepted.
        for _ in 0..STREAM_COMMAND_CAPACITY {
            client
                .call(CandlestickRequest::FetchHistory {
                    before_timestamp: 0,
                })
                .await
                .expect("history command queue should have capacity");
        }

        let error = client
            .call_with_timeout(
                CandlestickRequest::FetchHistory {
                    before_timestamp: 0,
                },
                Duration::from_millis(25),
            )
            .await
            .expect_err("full stream queue should outlive the RPC deadline");
        assert!(matches!(
            error,
            RpcCallError::DeadlineExceeded { .. }
                | RpcCallError::Service {
                    source: CandlestickServiceError::StreamBusy,
                    ..
                }
        ));

        tokio::time::timeout(Duration::from_secs(1), events.closed())
            .await
            .expect("timed out command should cancel the stream generation");

        assert_eq!(fetch_count.load(Ordering::Acquire), 2);
        assert!(latest.next_unseen().is_none());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn stream_failure_should_reject_pagination_and_retire_latest_snapshot() {
        let harness = harness(100, 1_000);
        let mut runtime = harness.runtime;
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("reliable stream should be available");
        let mut latest = runtime.latest_stream();
        let client = runtime.client();

        client
            .call(load(TimeFrame::Minute1))
            .await
            .expect("load request should start a stream");
        harness.fetch_started.notified().await;
        harness
            .history
            .send(vec![candle(100, 100)])
            .expect("initial history request should be waiting");
        latest
            .changed()
            .await
            .expect("initial snapshot should be published");
        assert!(latest.next_unseen().is_some());

        harness
            .events
            .send(Err(MarketError::Disconnected(DisconnectReason::Normal)))
            .await
            .expect("fake stream should be open");
        loop {
            let envelope = reliable.recv().await.expect("failure should publish");
            if matches!(
                envelope.message().as_ref(),
                CandlestickStreamMessage::Status(CandlestickStatus::Error(_))
            ) {
                break;
            }
        }

        let error = client
            .call(CandlestickRequest::FetchHistory {
                before_timestamp: 100,
            })
            .await
            .expect_err("failed stream should reject pagination");
        assert!(matches!(
            error,
            RpcCallError::Service {
                source: CandlestickServiceError::StreamClosed,
                ..
            }
        ));
        assert!(latest.next_unseen().is_none());
        runtime.shutdown().await;
    }

    struct DropSignal(Option<oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    /// A source whose first subscription never resolves, so a replacement can be
    /// observed cancelling it.
    #[derive(Clone)]
    struct PendingSubscriptionSource {
        attempts: Arc<AtomicUsize>,
        queries: Arc<Mutex<Vec<(VenueListingKey, TimeFrame)>>>,
        first_started: Arc<Notify>,
        first_drop_signal: Arc<Mutex<Option<oneshot::Sender<()>>>>,
        second_subscription: Arc<Mutex<Option<FakeSubscription>>>,
    }

    impl PendingSubscriptionSource {
        fn new(
            first_drop_signal: oneshot::Sender<()>,
            second_subscription: FakeSubscription,
        ) -> Self {
            Self {
                attempts: Arc::new(AtomicUsize::new(0)),
                queries: Arc::new(Mutex::new(Vec::with_capacity(2))),
                first_started: Arc::new(Notify::new()),
                first_drop_signal: Arc::new(Mutex::new(Some(first_drop_signal))),
                second_subscription: Arc::new(Mutex::new(Some(second_subscription))),
            }
        }
    }

    impl CandlestickSource for PendingSubscriptionSource {
        type Subscription = FakeSubscription;

        fn supported_timeframes(&self, _route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError> {
            Ok(TIMEFRAMES.to_vec())
        }

        fn supported_adjustments(
            &self,
            _route: &VenueRoute,
        ) -> Result<Vec<Adjustment>, MarketError> {
            Ok(vec![Adjustment::None])
        }

        async fn fetch_candlesticks(
            &self,
            _query: CandlestickPageQuery,
        ) -> Result<Vec<ListingCandlestick>, MarketError> {
            Ok(Vec::new())
        }

        async fn subscribe_candlesticks(
            &self,
            listing: VenueListingKey,
            timeframe: TimeFrame,
            _adjustment: Adjustment,
        ) -> Result<Self::Subscription, MarketError> {
            self.queries.lock().await.push((listing, timeframe));
            if self.attempts.fetch_add(1, Ordering::AcqRel) == 0 {
                let _drop_signal = DropSignal(self.first_drop_signal.lock().await.take());
                self.first_started.notify_one();
                return pending().await;
            }
            self.second_subscription
                .lock()
                .await
                .take()
                .ok_or(MarketError::ServiceClosed)
        }
    }

    fn idle_subscription(
        events: mpsc::Receiver<Result<Arc<ListingCandlestick>, MarketError>>,
    ) -> FakeSubscription {
        FakeSubscription {
            events,
            event_received: Arc::new(Notify::new()),
            precision: test_precision(),
        }
    }

    #[tokio::test]
    async fn loading_a_new_interval_should_cancel_a_pending_subscription_request() {
        let (_events, event_receiver) = mpsc::channel(1);
        let (first_dropped, dropped) = oneshot::channel();
        let source =
            PendingSubscriptionSource::new(first_dropped, idle_subscription(event_receiver));
        let first_started = source.first_started.clone();
        let queries = source.queries.clone();
        let runtime = WidgetRuntime::spawn(
            current_test_handle(),
            CandlestickService::new(source, 100, 1_000),
        );
        let client = runtime.client();

        client
            .call(load(TimeFrame::Minute1))
            .await
            .expect("first stream should start");
        first_started.notified().await;
        client
            .call(load(TimeFrame::Minute5))
            .await
            .expect("replacement stream should start");

        tokio::time::timeout(Duration::from_secs(1), dropped)
            .await
            .expect("pending subscription should be cancelled promptly")
            .expect("drop signal should remain connected");
        tokio::time::timeout(Duration::from_secs(1), async {
            while queries.lock().await.len() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacement subscription should start");
        assert_eq!(
            queries
                .lock()
                .await
                .iter()
                .map(|(_, timeframe)| *timeframe)
                .collect::<Vec<_>>(),
            vec![TimeFrame::Minute1, TimeFrame::Minute5]
        );
        runtime.shutdown().await;
    }

    /// Another listing is another scope: loading it replaces the generation of the
    /// listing before it rather than leaving two streams publishing into one chart.
    #[tokio::test]
    async fn loading_another_listing_should_subscribe_to_that_listing() {
        let (_events, event_receiver) = mpsc::channel(1);
        let (first_dropped, dropped) = oneshot::channel();
        let source =
            PendingSubscriptionSource::new(first_dropped, idle_subscription(event_receiver));
        let first_started = source.first_started.clone();
        let queries = source.queries.clone();
        let other_listing = test_support::listing_key(&route(), "ETHUSDT");
        let runtime = WidgetRuntime::spawn(
            current_test_handle(),
            CandlestickService::new(source, 100, 1_000),
        );
        let client = runtime.client();

        client
            .call(load(TimeFrame::Minute1))
            .await
            .expect("first stream should start");
        first_started.notified().await;
        client
            .call(CandlestickRequest::Load {
                listing: other_listing.clone(),
                timeframe: TimeFrame::Minute1,
                adjustment: Adjustment::None,
            })
            .await
            .expect("replacement stream should start");

        tokio::time::timeout(Duration::from_secs(1), dropped)
            .await
            .expect("the previous listing's subscription should be cancelled promptly")
            .expect("drop signal should remain connected");
        tokio::time::timeout(Duration::from_secs(1), async {
            while queries.lock().await.len() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the replacement subscription should start");
        assert_eq!(
            queries
                .lock()
                .await
                .iter()
                .map(|(listing, _)| listing.clone())
                .collect::<Vec<_>>(),
            vec![listing(), other_listing]
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn same_key_load_is_idempotent_and_retry_replaces_the_generation() {
        let (_events, event_receiver) = mpsc::channel(1);
        let (first_dropped, dropped) = oneshot::channel();
        let source =
            PendingSubscriptionSource::new(first_dropped, idle_subscription(event_receiver));
        let first_started = source.first_started.clone();
        let attempts = source.attempts.clone();
        let runtime = WidgetRuntime::spawn(
            current_test_handle(),
            CandlestickService::new(source, 100, 1_000),
        );
        let client = runtime.client();

        client
            .call(load(TimeFrame::Minute1))
            .await
            .expect("first stream should start");
        first_started.notified().await;
        client
            .call(load(TimeFrame::Minute1))
            .await
            .expect("same-key load should be idempotent");
        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::Acquire), 1);

        client
            .call(CandlestickRequest::Retry {
                listing: listing(),
                timeframe: TimeFrame::Minute1,
                adjustment: Adjustment::None,
            })
            .await
            .expect("retry should replace the failed generation");
        tokio::time::timeout(Duration::from_secs(1), dropped)
            .await
            .expect("retry should cancel the previous generation")
            .expect("drop signal should remain connected");
        tokio::time::timeout(Duration::from_secs(1), async {
            while attempts.load(Ordering::Acquire) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacement generation should subscribe once");

        runtime.shutdown().await;
    }

    struct DropTrackedSubscription {
        events: mpsc::Receiver<Result<Arc<ListingCandlestick>, MarketError>>,
        _drop_signal: DropSignal,
    }

    impl CandlestickEventSubscription for DropTrackedSubscription {
        fn precision(&self) -> ListingPrecision {
            test_precision()
        }

        async fn recv(&mut self) -> Option<Result<Arc<ListingCandlestick>, MarketError>> {
            self.events.recv().await
        }
    }

    /// A source handing out one subscription per stream attempt, so the test can
    /// see the previous generation's subscription being dropped.
    #[derive(Clone)]
    struct DropTrackedSource {
        subscriptions: Arc<Mutex<VecDeque<DropTrackedSubscription>>>,
        first_history: Arc<Mutex<Option<oneshot::Receiver<Vec<ListingCandlestick>>>>>,
        first_fetch_started: Arc<Notify>,
    }

    impl CandlestickSource for DropTrackedSource {
        type Subscription = DropTrackedSubscription;

        fn supported_timeframes(&self, _route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError> {
            Ok(TIMEFRAMES.to_vec())
        }

        fn supported_adjustments(
            &self,
            _route: &VenueRoute,
        ) -> Result<Vec<Adjustment>, MarketError> {
            Ok(vec![Adjustment::None])
        }

        async fn fetch_candlesticks(
            &self,
            _query: CandlestickPageQuery,
        ) -> Result<Vec<ListingCandlestick>, MarketError> {
            let first_history = self.first_history.lock().await.take();
            let Some(first_history) = first_history else {
                return Ok(Vec::new());
            };
            self.first_fetch_started.notify_one();
            first_history.await.map_err(|_| MarketError::ServiceClosed)
        }

        async fn subscribe_candlesticks(
            &self,
            _listing: VenueListingKey,
            _timeframe: TimeFrame,
            _adjustment: Adjustment,
        ) -> Result<Self::Subscription, MarketError> {
            self.subscriptions
                .lock()
                .await
                .pop_front()
                .ok_or(MarketError::ServiceClosed)
        }
    }

    #[tokio::test]
    async fn loading_a_new_interval_should_drop_the_previous_subscription() {
        let (first_events, first_receiver) = mpsc::channel(1);
        let (second_events, second_receiver) = mpsc::channel(1);
        let (history, first_history) = oneshot::channel();
        let (first_dropped, dropped) = oneshot::channel();
        let first_fetch_started = Arc::new(Notify::new());
        let source = DropTrackedSource {
            subscriptions: Arc::new(Mutex::new(VecDeque::from([
                DropTrackedSubscription {
                    events: first_receiver,
                    _drop_signal: DropSignal(Some(first_dropped)),
                },
                DropTrackedSubscription {
                    events: second_receiver,
                    _drop_signal: DropSignal(None),
                },
            ]))),
            first_history: Arc::new(Mutex::new(Some(first_history))),
            first_fetch_started: first_fetch_started.clone(),
        };
        let runtime = WidgetRuntime::spawn(
            current_test_handle(),
            CandlestickService::new(source, 100, 1_000),
        );
        let client = runtime.client();

        client
            .call(load(TimeFrame::Minute1))
            .await
            .expect("first stream should start");
        first_fetch_started.notified().await;
        client
            .call(load(TimeFrame::Minute5))
            .await
            .expect("replacement stream should start");

        tokio::time::timeout(Duration::from_secs(1), dropped)
            .await
            .expect("previous subscription should be dropped promptly")
            .expect("drop signal should remain connected");

        drop((first_events, second_events, history));
        runtime.shutdown().await;
    }

    /// A source whose subscription reports what a catalog that could not be
    /// fetched reports.
    #[derive(Clone)]
    struct UnresolvableSource {
        failure: MarketError,
    }

    impl CandlestickSource for UnresolvableSource {
        type Subscription = FakeSubscription;

        fn supported_timeframes(&self, _route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError> {
            Ok(TIMEFRAMES.to_vec())
        }

        fn supported_adjustments(
            &self,
            _route: &VenueRoute,
        ) -> Result<Vec<Adjustment>, MarketError> {
            Ok(vec![Adjustment::None])
        }

        async fn fetch_candlesticks(
            &self,
            _query: CandlestickPageQuery,
        ) -> Result<Vec<ListingCandlestick>, MarketError> {
            std::future::pending().await
        }

        async fn subscribe_candlesticks(
            &self,
            _listing: VenueListingKey,
            _timeframe: TimeFrame,
            _adjustment: Adjustment,
        ) -> Result<Self::Subscription, MarketError> {
            Err(self.failure.clone())
        }
    }

    /// The scope the chart asks for is resolved against the route's catalog before a
    /// stream is opened, and a catalog that could not be fetched fails there. That
    /// failure is what the chart is told about: it must never arrive as an empty
    /// series or as a rejection of the listing itself.
    #[tokio::test]
    async fn a_catalog_failure_on_the_subscription_seam_should_be_reported_as_itself() {
        let failure = MarketError::CatalogSource(SourceError::Network {
            reason: SmolStr::new_static("the test catalog is unreachable"),
        });
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            CandlestickService::new(
                UnresolvableSource {
                    failure: failure.clone(),
                },
                100,
                1_000,
            ),
        );
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("reliable stream should be available");
        runtime
            .client()
            .call(load(TimeFrame::Minute1))
            .await
            .expect("load request should start a stream");

        let reported = loop {
            let envelope = tokio::time::timeout(Duration::from_secs(1), reliable.recv())
                .await
                .expect("the failure should be published")
                .expect("the reliable stream should remain open");
            if let CandlestickStreamMessage::Status(CandlestickStatus::Error(error)) =
                envelope.message().as_ref()
            {
                break error.clone();
            }
        };

        assert_eq!(reported, failure);
        assert!(
            matches!(reported, MarketError::CatalogSource(_)),
            "a catalog that could not be fetched is not a rejection of the listing"
        );
        runtime.shutdown().await;
    }
}

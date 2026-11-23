use std::{collections::BTreeMap, future::Future, sync::Arc};

use futures::StreamExt;
use i18n::{t_smol, translate::Translate};
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
use smol_str::SmolStr;
use thiserror::Error;
use tokio::sync::mpsc;
use widget::{
    error::TranslatedError,
    protocol::WidgetProtocol,
    runtime::{ServiceContext, WidgetService},
    stream::StreamPublisher,
};

use super::model::{
    CandlestickSeries, CandlestickSeriesSnapshot, NewestCandle, OlderMerge, bounded_history,
};

const STREAM_COMMAND_CAPACITY: usize = 8;

#[derive(Debug)]
pub(super) enum CandlestickRequest {
    Load {
        listing: VenueListingKey,
        timeframe: TimeFrame,
        adjustment: Adjustment,
    },
    Retry {
        listing: VenueListingKey,
        timeframe: TimeFrame,
        adjustment: Adjustment,
    },
    FetchHistory {
        before_timestamp: UnixMillis,
        newest: NewestCandle,
    },
    FetchNewer {
        after_timestamp: UnixMillis,
    },
}

#[derive(Debug)]
pub(super) enum CandlestickResponse {
    StreamStarted,
    HistoryQueued,
    NewerQueued,
}

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

    pub(super) const fn venue(&self) -> &VenueId {
        &self.listing.route.venue
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum CandlestickStatus {
    Connecting,
    LoadingHistory,
    Ready,
    Error(MarketError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PageDirection {
    History,
    Newer,
}

#[derive(Clone, Debug)]
pub(super) enum CandlestickStreamMessage {
    Status(CandlestickStatus),
    Snapshot(CandlestickSeriesSnapshot),
    PageSettled { direction: PageDirection },
}

pub(super) struct CandlestickProtocol;

impl WidgetProtocol for CandlestickProtocol {
    type Request = CandlestickRequest;
    type Response = CandlestickResponse;
    type StreamKey = CandlestickStreamKey;
    type StreamMessage = CandlestickStreamMessage;
    type Error = CandlestickServiceError;
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub(super) enum CandlestickServiceError {
    #[error("candlestick stream is not active")]
    NoActiveStream,
    #[error("candlestick stream command queue is closed")]
    StreamClosed,
    #[error("candlestick stream command queue did not accept the request before its deadline")]
    StreamBusy,
}

impl TranslatedError for CandlestickServiceError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::NoActiveStream => t_smol!(translate, "error.candlestick.no_active_stream"),
            Self::StreamClosed => t_smol!(translate, "error.candlestick.stream_closed"),
            Self::StreamBusy => t_smol!(translate, "error.candlestick.stream_busy"),
        }
    }
}

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

pub(super) trait CandlestickSource: Clone + Send + Sync + 'static {
    type Subscription: CandlestickEventSubscription;

    fn supported_timeframes(&self, route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError>;

    fn supported_adjustments(&self, route: &VenueRoute) -> Result<Vec<Adjustment>, MarketError>;

    fn fetch_candlesticks(
        &self,
        query: CandlestickPageQuery,
    ) -> impl Future<Output = Result<Vec<ListingCandlestick>, MarketError>> + Send;

    fn subscribe_candlesticks(
        &self,
        listing: VenueListingKey,
        timeframe: TimeFrame,
        adjustment: Adjustment,
    ) -> impl Future<Output = Result<Self::Subscription, MarketError>> + Send;
}

pub(super) trait CandlestickEventSubscription: Send + 'static {
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
                    if !retry && active_key.as_ref() == Some(&key) {
                        let _ = reply.respond(Ok(CandlestickResponse::StreamStarted));
                        continue;
                    }
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
                CandlestickRequest::FetchHistory {
                    before_timestamp,
                    newest,
                } => {
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
                        result = commands.send(StreamCommand::FetchHistory {
                            before_timestamp,
                            newest,
                        }) => {
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
    FetchHistory {
        before_timestamp: UnixMillis,
        newest: NewestCandle,
    },
    FetchNewer {
        after_timestamp: UnixMillis,
    },
}

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
                let (query, direction, newest) = match command {
                    StreamCommand::FetchHistory {
                        before_timestamp,
                        newest,
                    } => {
                        if series.older_exhausted()
                            || series
                                .earliest_timestamp()
                                .is_some_and(|earliest| before_timestamp > earliest)
                        {
                            if !publish_settled(publisher, PageDirection::History).await {
                                return Ok(());
                            }
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
                            PageDirection::History,
                            Some(newest),
                        )
                    }
                    StreamCommand::FetchNewer { after_timestamp } => {
                        if series.at_live_edge()
                            || series
                                .latest_timestamp()
                                .is_some_and(|latest| after_timestamp < latest)
                        {
                            if !publish_settled(publisher, PageDirection::Newer).await {
                                return Ok(());
                            }
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
                            PageDirection::Newer,
                            None,
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
                match newest {
                    Some(newest) => {
                        match series.merge_older(page, newest) {
                            OlderMerge::MadeNoProgress => series.mark_older_exhausted(),
                            OlderMerge::Advanced | OlderMerge::TrimmedByCapacity => {}
                        }
                    }
                    None => {
                        series.merge_newer(page);
                        if page_len < usize::from(spec.history_limit) {
                            series.mark_at_live_edge();
                        }
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
                if !publish_settled(publisher, direction).await {
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

async fn publish_status(
    publisher: &StreamPublisher<CandlestickProtocol>,
    status: CandlestickStatus,
) -> bool {
    publisher
        .publish_reliable(CandlestickStreamMessage::Status(status))
        .await
        .is_ok()
}

fn publish_snapshot(
    publisher: &StreamPublisher<CandlestickProtocol>,
    snapshot: CandlestickSeriesSnapshot,
) -> bool {
    publisher
        .publish_latest(CandlestickStreamMessage::Snapshot(snapshot))
        .is_ok()
}

async fn publish_settled(
    publisher: &StreamPublisher<CandlestickProtocol>,
    direction: PageDirection,
) -> bool {
    publisher
        .publish_reliable(CandlestickStreamMessage::PageSettled { direction })
        .await
        .is_ok()
}

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
            CandlestickStreamMessage::Status(_) | CandlestickStreamMessage::PageSettled { .. } => {
                None
            }
        }
    }

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
                (None, None) => vec![candle(10, 10), candle(20, 20), candle(30, 30)],
                (Some(start), _) => vec![
                    candle(start, 40),
                    candle(start + 1, 50),
                    candle(start + 2, 60),
                ],
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
                newest: NewestCandle::Evicted,
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

    #[tokio::test]
    async fn a_dropped_pagination_request_should_still_settle_its_direction() {
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
            .send(Vec::new())
            .expect("initial history request should be waiting");
        latest
            .changed()
            .await
            .expect("the empty first page should be published");
        while !matches!(
            reliable
                .recv()
                .await
                .expect("the load should be reported on the reliable channel")
                .message()
                .as_ref(),
            CandlestickStreamMessage::Status(CandlestickStatus::Ready)
        ) {}
        let empty = latest
            .next_unseen()
            .and_then(|envelope| snapshot_of(envelope.message().as_ref()).cloned())
            .expect("the empty page is published before the status that reports it");
        assert!(
            empty.older_exhausted() && empty.at_live_edge(),
            "the fixture is what both historical drops need: nothing older, nothing newer"
        );

        client
            .call(CandlestickRequest::FetchHistory {
                before_timestamp: 0,
                newest: NewestCandle::Evicted,
            })
            .await
            .expect("the request is accepted; the stream is what drops the page");
        let history_settled = tokio::time::timeout(Duration::from_secs(1), reliable.recv())
            .await
            .expect("a dropped history request has to settle, or the view's flag stays set")
            .expect("the reliable stream should remain open");
        assert!(
            matches!(
                history_settled.message().as_ref(),
                CandlestickStreamMessage::PageSettled {
                    direction: PageDirection::History
                }
            ),
            "the history direction is settled even though no page was fetched"
        );

        client
            .call(CandlestickRequest::FetchNewer { after_timestamp: 0 })
            .await
            .expect("the request is accepted; the stream is what drops the page");
        let newer_settled = tokio::time::timeout(Duration::from_secs(1), reliable.recv())
            .await
            .expect("a dropped newer request has to settle, or the view's flag stays set")
            .expect("the reliable stream should remain open");
        assert!(
            matches!(
                newer_settled.message().as_ref(),
                CandlestickStreamMessage::PageSettled {
                    direction: PageDirection::Newer
                }
            ),
            "the newer direction is settled even though no page was fetched"
        );
        assert!(
            latest.next_unseen().is_none(),
            "and a dropped request publishes no snapshot, the one above is the load's own"
        );
        runtime.shutdown().await;
    }

    #[derive(Clone)]
    struct ScriptedSource {
        subscription: Arc<Mutex<Option<FakeSubscription>>>,
        initial: Arc<Mutex<Option<Vec<ListingCandlestick>>>>,
        pages: Arc<Mutex<VecDeque<Vec<ListingCandlestick>>>>,
    }

    impl ScriptedSource {
        fn new(
            subscription: FakeSubscription,
            initial: Vec<ListingCandlestick>,
            pages: Vec<Vec<ListingCandlestick>>,
        ) -> Self {
            Self {
                subscription: Arc::new(Mutex::new(Some(subscription))),
                initial: Arc::new(Mutex::new(Some(initial))),
                pages: Arc::new(Mutex::new(VecDeque::from(pages))),
            }
        }
    }

    impl CandlestickSource for ScriptedSource {
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
            if let Some(initial) = self.initial.lock().await.take() {
                return Ok(initial);
            }
            let page = self
                .pages
                .lock()
                .await
                .pop_front()
                .ok_or(MarketError::ServiceClosed)?;
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

    async fn history_pages_over(
        capacity: usize,
        initial: Vec<ListingCandlestick>,
        pages: Vec<Vec<ListingCandlestick>>,
        newest: NewestCandle,
    ) -> Vec<CandlestickSeriesSnapshot> {
        let (events, event_receiver) = mpsc::channel(1);
        let page_count = pages.len();
        let source = ScriptedSource::new(idle_subscription(event_receiver), initial, pages);
        let runtime = WidgetRuntime::spawn(
            current_test_handle(),
            CandlestickService::new(source, 2, capacity),
        );
        let client = runtime.client();
        let mut latest = runtime.latest_stream();

        client
            .call(load(TimeFrame::Minute1))
            .await
            .expect("the load should start a stream");
        let mut produced = Vec::with_capacity(page_count);
        let mut earliest = latest
            .changed()
            .await
            .map(|()| latest.next_unseen())
            .ok()
            .flatten()
            .and_then(|envelope| snapshot_of(envelope.message().as_ref()).cloned())
            .expect("the first page should be published")
            .earliest_timestamp();

        for index in 0..page_count {
            client
                .call(CandlestickRequest::FetchHistory {
                    before_timestamp: earliest
                        .map(|earliest| earliest.saturating_sub(1))
                        .expect("the first page left a candle to page from"),
                    newest,
                })
                .await
                .expect("the page request should be queued");
            let snapshot = latest
                .changed()
                .await
                .map(|()| latest.next_unseen())
                .ok()
                .flatten()
                .and_then(|envelope| snapshot_of(envelope.message().as_ref()).cloned())
                .unwrap_or_else(|| panic!("page {} should be published", index + 1));
            earliest = snapshot.earliest_timestamp();
            produced.push(snapshot);
        }

        drop(events);
        runtime.shutdown().await;
        produced
    }

    #[tokio::test]
    async fn a_history_page_that_makes_no_progress_should_mark_history_exhausted() {
        let initial = vec![candle(10, 10), candle(20, 20), candle(30, 30)];
        let repeat = vec![candle(10, 10), candle(20, 20)];
        let produced = history_pages_over(10, initial, vec![repeat], NewestCandle::Kept).await;

        assert!(
            produced[0].older_exhausted(),
            "a fetched page that added nothing older is the venue's answer, not the buffer's"
        );
        assert_eq!(produced[0].earliest_timestamp(), Some(10), "nothing moved");
    }

    #[tokio::test]
    async fn a_history_page_trimmed_by_capacity_should_not_mark_history_exhausted() {
        let initial = vec![candle(10, 10), candle(20, 20), candle(30, 30)];
        let older = vec![candle(1, 1), candle(2, 2)];
        let produced = history_pages_over(3, initial, vec![older], NewestCandle::Kept).await;

        assert!(
            !produced[0].older_exhausted(),
            "a full buffer refused the page, which says nothing about how much history is left"
        );
        assert_eq!(
            produced[0].earliest_timestamp(),
            Some(10),
            "and nothing was evicted to make room for it"
        );
        assert!(
            produced[0].at_live_edge(),
            "keeping the newest candle is what keeps the chart at the live edge"
        );
    }

    #[tokio::test]
    async fn a_history_page_that_advances_should_not_mark_history_exhausted() {
        let initial = vec![candle(10, 10), candle(20, 20), candle(30, 30)];
        let older = vec![candle(1, 1), candle(2, 2)];
        let produced = history_pages_over(10, initial, vec![older], NewestCandle::Kept).await;

        assert!(!produced[0].older_exhausted());
        assert_eq!(
            produced[0].earliest_timestamp(),
            Some(1),
            "the window moved"
        );
        assert!(produced[0].at_live_edge());
    }

    #[tokio::test]
    async fn a_fetched_page_should_settle_its_direction_after_its_snapshot() {
        let (events, event_receiver) = mpsc::channel(1);
        let source = ScriptedSource::new(
            idle_subscription(event_receiver),
            vec![candle(10, 10), candle(20, 20), candle(30, 30)],
            vec![vec![candle(1, 1), candle(2, 2)]],
        );
        let mut runtime = WidgetRuntime::spawn(
            current_test_handle(),
            CandlestickService::new(source, 2, 10),
        );
        let mut reliable = runtime
            .take_reliable_stream()
            .expect("reliable stream should be available");
        let mut latest = runtime.latest_stream();
        let client = runtime.client();

        client
            .call(load(TimeFrame::Minute1))
            .await
            .expect("the load should start a stream");
        while !matches!(
            reliable
                .recv()
                .await
                .expect("the load should be reported")
                .message()
                .as_ref(),
            CandlestickStreamMessage::Status(CandlestickStatus::Ready)
        ) {}
        while latest.next_unseen().is_some() {}

        client
            .call(CandlestickRequest::FetchHistory {
                before_timestamp: 9,
                newest: NewestCandle::Kept,
            })
            .await
            .expect("the page request should be queued");

        latest
            .changed()
            .await
            .expect("the fetched page should be published");
        let page = latest
            .next_unseen()
            .and_then(|envelope| snapshot_of(envelope.message().as_ref()).cloned())
            .expect("the fetched page carries a snapshot");
        assert_eq!(
            page.earliest_timestamp(),
            Some(1),
            "the page landed before its settle is read"
        );

        let settled = tokio::time::timeout(Duration::from_secs(1), reliable.recv())
            .await
            .expect("a fetched page has to settle its direction too")
            .expect("the reliable stream should remain open");
        assert!(
            matches!(
                settled.message().as_ref(),
                CandlestickStreamMessage::PageSettled {
                    direction: PageDirection::History
                }
            ),
            "the view learns the request is over from this message and nothing else"
        );
        drop(events);
        runtime.shutdown().await;
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
                newest: NewestCandle::Evicted,
            })
            .await
            .expect("first history request should start");
        blocked_fetch_started.notified().await;
        for _ in 0..STREAM_COMMAND_CAPACITY {
            client
                .call(CandlestickRequest::FetchHistory {
                    before_timestamp: 0,
                    newest: NewestCandle::Evicted,
                })
                .await
                .expect("history command queue should have capacity");
        }

        let error = client
            .call_with_timeout(
                CandlestickRequest::FetchHistory {
                    before_timestamp: 0,
                    newest: NewestCandle::Evicted,
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
                newest: NewestCandle::Evicted,
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

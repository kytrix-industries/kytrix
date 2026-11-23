use std::{collections::VecDeque, convert::Infallible, future::Future, sync::Arc, time::Duration};

use ahash::AHashSet;
use futures::future::try_join_all;
use i18n::{t_smol, translate::Translate};
use market_participants_connector::{
    account::{
        domain::{
            HyperliquidAssetContext, HyperliquidHistoricalOrder, HyperliquidLedgerEvent,
            HyperliquidOpenOrder, HyperliquidPerpetualState, HyperliquidSpotPair,
            HyperliquidSpotState, HyperliquidUserFill, HyperliquidWalletUpdate,
        },
        error::{AccountCatalogError, AccountError},
        hyperliquid::HyperliquidSubscription,
        provider::AccountCapability,
        shared::PositionDirection,
    },
    catalog::{batch::CatalogEntry, display::catalog::DisplaySnapshot, store::Catalog},
    connector::MarketParticipantsConnector,
    venue::loaded::LoadedVenues,
};
use market_participants_model::{
    decimal::{Quantity, SignedQuantity},
    identifier::{VenueId, WalletAddress},
    listing::route::{VenueListingKey, VenueRoute},
    market::{
        account::AssetAmount,
        price::MarketPrice,
        units::{MarketQuantity, Side},
    },
    routing::CapabilityId,
};
use smol_str::SmolStr;
use thiserror::Error;
use tokio::time::Instant;
use widget::{
    error::TranslatedError,
    protocol::WidgetProtocol,
    runtime::{ServiceContext, WidgetService},
    stream::{StreamPublishError, StreamPublisher},
    venue::{VenueDisplay as _, route_display_string},
};

use super::{
    model::{
        AccountSummary, AssetLabels, DetailData, DetailSnapshot, DetailTab, DisplayPrecision,
        SpotSnapshot, TimelineSnapshot, WalletDataError, WalletOverviewSnapshot, WalletRouteKinds,
        now_millis, sort_and_limit_fills, sort_and_limit_historical_orders, sort_and_limit_ledger,
        sort_and_limit_open_orders,
    },
    timeline::WINDOW_MILLIS,
    watch::MAX_WALLET_ADDRESSES,
};

const TIMELINE_INTERVAL: Duration = Duration::from_secs(10);
const LEDGER_WINDOW: i64 = 30 * 24 * 60 * 60 * 1_000;
const MAX_REBASE_INCREMENT_EVENTS: usize = 4_096;
const MAX_APPLIED_INCREMENT_IDS: usize = 4_096;
const MAX_TIMELINE_FILLS: usize = 2_000;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub(super) enum WalletStreamError {
    #[error(transparent)]
    Account(#[from] AccountError),
    #[error(transparent)]
    Model(#[from] market_participants_model::error::ModelError),
    #[error(transparent)]
    Data(#[from] WalletDataError),
    #[error("{operation} exceeded its {capacity}-event buffer")]
    SubscriptionBufferFull {
        operation: &'static str,
        capacity: usize,
    },
    #[error("reading the Hyperliquid wallet data of {route} failed: {source}")]
    DexRead {
        route: VenueRoute,
        source: AccountError,
    },
    #[error("Hyperliquid wallet subscription closed")]
    SubscriptionClosed,
    #[error("Hyperliquid wallet stream closed")]
    StreamClosed,
}

impl TranslatedError for WalletStreamError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        let market = VenueId::Hyperliquid.display_string(translate);
        match self {
            Self::Account(error) => error.translated_message(translate),
            Self::Model(error) => error.translated_message(translate),
            Self::Data(error) => error.translated_message(translate),
            Self::SubscriptionBufferFull {
                operation,
                capacity,
            } => t_smol!(
                translate,
                "widget.hyperliquid_wallet.error_subscription_buffer_full",
                market = market,
                operation = operation,
                capacity = capacity
            ),
            Self::DexRead { route, source } => t_smol!(
                translate,
                "widget.hyperliquid_wallet.error_dex_read",
                market = market,
                dex = route_display_string(route, translate),
                cause = source.translated_message(translate)
            ),
            Self::SubscriptionClosed => t_smol!(
                translate,
                "widget.hyperliquid_wallet.error_subscription_closed",
                market = market
            ),
            Self::StreamClosed => t_smol!(
                translate,
                "widget.hyperliquid_wallet.error_stream_closed",
                market = market
            ),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum WalletRequest {
    Load {
        addresses: Vec<WalletAddress>,
    },
    Reload {
        addresses: Vec<WalletAddress>,
    },
    Retry {
        addresses: Vec<WalletAddress>,
    },
    RetryTimeline {
        address: WalletAddress,
    },
    LoadDetail {
        address: WalletAddress,
        tab: DetailTab,
    },
    RetryDetail {
        address: WalletAddress,
        tab: DetailTab,
    },
}

#[derive(Clone, Debug)]
pub(super) enum WalletResponse {
    StreamsStarted,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) enum WalletStreamKey {
    Overview {
        address: WalletAddress,
    },
    Timeline {
        address: WalletAddress,
    },
    Detail {
        address: WalletAddress,
        tab: DetailTab,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WalletStatus {
    Connecting,
    Ready,
    Failed(WalletStreamError),
}

#[derive(Clone, Debug)]
pub(super) enum WalletStreamMessage {
    Status(WalletStatus),
    Overview(Arc<WalletOverviewSnapshot>),
    Timeline(Arc<TimelineSnapshot>),
    Detail(Arc<DetailSnapshot>),
}

pub(super) struct WalletProtocol;

impl WidgetProtocol for WalletProtocol {
    type Request = WalletRequest;
    type Response = WalletResponse;
    type StreamKey = WalletStreamKey;
    type StreamMessage = WalletStreamMessage;
    type Error = Infallible;
}

pub(super) trait WalletSource: Clone + Send + Sync + 'static {
    type Subscription: WalletSubscription;

    fn wallet_routes(&self) -> Vec<VenueRoute>;
    fn fetch_spot_state(
        &self,
        route: &VenueRoute,
        address: &WalletAddress,
    ) -> impl Future<Output = Result<HyperliquidSpotState, AccountError>> + Send;
    fn fetch_perpetual_state(
        &self,
        route: &VenueRoute,
        address: &WalletAddress,
    ) -> impl Future<Output = Result<HyperliquidPerpetualState, AccountError>> + Send;
    fn fetch_spot_pairs(
        &self,
        route: &VenueRoute,
    ) -> impl Future<Output = Result<Vec<HyperliquidSpotPair>, AccountError>> + Send;
    fn fetch_fills(
        &self,
        address: &WalletAddress,
        aggregate_by_time: bool,
        routes: &[VenueRoute],
    ) -> impl Future<Output = Result<Vec<HyperliquidUserFill>, AccountError>> + Send;
    fn fetch_fills_by_time(
        &self,
        address: &WalletAddress,
        start_timestamp: i64,
        end_timestamp: i64,
        routes: &[VenueRoute],
    ) -> impl Future<Output = Result<Vec<HyperliquidUserFill>, AccountError>> + Send;
    fn fetch_ledger(
        &self,
        address: &WalletAddress,
        start_time: i64,
        routes: &[VenueRoute],
    ) -> impl Future<Output = Result<Vec<HyperliquidLedgerEvent>, AccountError>> + Send;
    fn fetch_open_orders(
        &self,
        route: &VenueRoute,
        address: &WalletAddress,
    ) -> impl Future<Output = Result<Vec<HyperliquidOpenOrder>, AccountError>> + Send;
    fn fetch_historical_orders(
        &self,
        address: &WalletAddress,
        routes: &[VenueRoute],
    ) -> impl Future<Output = Result<Vec<HyperliquidHistoricalOrder>, AccountError>> + Send;
    fn fetch_catalog(
        &self,
        route: &VenueRoute,
    ) -> impl Future<Output = Result<Arc<[CatalogEntry]>, AccountError>> + Send;
    fn display_generation(&self) -> DisplaySnapshot;
    fn subscribe_fills(
        &self,
        address: &WalletAddress,
        routes: &[VenueRoute],
    ) -> impl Future<Output = Result<Self::Subscription, AccountError>> + Send;
    fn subscribe_ledger(
        &self,
        address: &WalletAddress,
        routes: &[VenueRoute],
    ) -> impl Future<Output = Result<Self::Subscription, AccountError>> + Send;
    fn subscribe_fast_contexts(
        &self,
        routes: &[VenueRoute],
    ) -> impl Future<Output = Result<Self::Subscription, AccountError>> + Send
    where
        Self::Subscription: Sized;
}

pub(super) trait WalletSubscription: Send + 'static {
    fn recv(
        &mut self,
    ) -> impl Future<Output = Option<Result<HyperliquidWalletUpdate, AccountError>>> + Send;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WalletScope {
    pub route: VenueRoute,
    pub monitors_wallets: bool,
    pub provides_catalog: bool,
}

pub(crate) fn loaded_scopes(venues: &LoadedVenues) -> Vec<WalletScope> {
    venues
        .entries()
        .iter()
        .map(|(route, entry)| WalletScope {
            route: route.clone(),
            monitors_wallets: entry
                .capabilities
                .iter()
                .any(|support| support.capability == CapabilityId::Wallet),
            provides_catalog: entry.catalog_source.is_some(),
        })
        .collect()
}

pub(crate) fn wallet_routes_of(scopes: &[WalletScope]) -> Vec<VenueRoute> {
    scopes
        .iter()
        .filter(|scope| {
            scope.route.venue == VenueId::Hyperliquid
                && scope.monitors_wallets
                && scope.provides_catalog
        })
        .map(|scope| scope.route.clone())
        .collect()
}

#[derive(Clone)]
pub(super) struct ClientWalletSource {
    connector: MarketParticipantsConnector,
    account: AccountCapability,
    catalog: Arc<Catalog>,
}

impl ClientWalletSource {
    pub(super) fn new(connector: &MarketParticipantsConnector) -> Self {
        Self {
            connector: connector.clone(),
            account: connector.account(),
            catalog: connector.catalog_arc(),
        }
    }
}

impl WalletSource for ClientWalletSource {
    type Subscription = ClientWalletSubscription;

    fn wallet_routes(&self) -> Vec<VenueRoute> {
        wallet_routes_of(&loaded_scopes(self.connector.venues()))
    }

    async fn fetch_spot_state(
        &self,
        route: &VenueRoute,
        address: &WalletAddress,
    ) -> Result<HyperliquidSpotState, AccountError> {
        self.account
            .hyperliquid_spot_state(route.clone(), address.clone())
            .await
    }

    async fn fetch_perpetual_state(
        &self,
        route: &VenueRoute,
        address: &WalletAddress,
    ) -> Result<HyperliquidPerpetualState, AccountError> {
        self.account
            .hyperliquid_perpetual_state(route.clone(), address.clone())
            .await
    }

    async fn fetch_spot_pairs(
        &self,
        route: &VenueRoute,
    ) -> Result<Vec<HyperliquidSpotPair>, AccountError> {
        self.account.hyperliquid_spot_pairs(route.clone()).await
    }

    async fn fetch_fills(
        &self,
        address: &WalletAddress,
        aggregate_by_time: bool,
        routes: &[VenueRoute],
    ) -> Result<Vec<HyperliquidUserFill>, AccountError> {
        self.account
            .hyperliquid_user_fills(routes, address.clone(), aggregate_by_time)
            .await
    }

    async fn fetch_fills_by_time(
        &self,
        address: &WalletAddress,
        start_timestamp: i64,
        end_timestamp: i64,
        routes: &[VenueRoute],
    ) -> Result<Vec<HyperliquidUserFill>, AccountError> {
        self.account
            .hyperliquid_user_fills_by_time(routes, address.clone(), start_timestamp, end_timestamp)
            .await
    }

    async fn fetch_ledger(
        &self,
        address: &WalletAddress,
        start_time: i64,
        routes: &[VenueRoute],
    ) -> Result<Vec<HyperliquidLedgerEvent>, AccountError> {
        self.account
            .hyperliquid_user_ledger(routes, address.clone(), start_time)
            .await
    }

    async fn fetch_open_orders(
        &self,
        route: &VenueRoute,
        address: &WalletAddress,
    ) -> Result<Vec<HyperliquidOpenOrder>, AccountError> {
        self.account
            .hyperliquid_open_orders(route.clone(), address.clone())
            .await
    }

    async fn fetch_historical_orders(
        &self,
        address: &WalletAddress,
        routes: &[VenueRoute],
    ) -> Result<Vec<HyperliquidHistoricalOrder>, AccountError> {
        self.account
            .hyperliquid_historical_orders(routes, address.clone())
            .await
    }

    async fn fetch_catalog(&self, route: &VenueRoute) -> Result<Arc<[CatalogEntry]>, AccountError> {
        self.catalog
            .listings(route)
            .map_err(|error| AccountError::Catalog(AccountCatalogError::Directory(Box::new(error))))
    }

    fn display_generation(&self) -> DisplaySnapshot {
        self.catalog.cached_display()
    }

    async fn subscribe_fills(
        &self,
        address: &WalletAddress,
        routes: &[VenueRoute],
    ) -> Result<Self::Subscription, AccountError> {
        self.account
            .subscribe_hyperliquid_user_fills(routes, address.clone())
            .await
            .map(ClientWalletSubscription::new)
    }

    async fn subscribe_ledger(
        &self,
        address: &WalletAddress,
        routes: &[VenueRoute],
    ) -> Result<Self::Subscription, AccountError> {
        self.account
            .subscribe_hyperliquid_user_ledger(routes, address.clone())
            .await
            .map(ClientWalletSubscription::new)
    }

    async fn subscribe_fast_contexts(
        &self,
        routes: &[VenueRoute],
    ) -> Result<Self::Subscription, AccountError> {
        self.account
            .subscribe_hyperliquid_fast_contexts(routes)
            .await
            .map(ClientWalletSubscription::new)
    }
}

pub(super) struct ClientWalletSubscription {
    inner: HyperliquidSubscription,
}

impl ClientWalletSubscription {
    fn new(subscription: HyperliquidSubscription) -> Self {
        Self {
            inner: subscription,
        }
    }
}

impl WalletSubscription for ClientWalletSubscription {
    async fn recv(&mut self) -> Option<Result<HyperliquidWalletUpdate, AccountError>> {
        self.inner.recv().await
    }
}

struct WalletSubscriptions<S: WalletSubscription> {
    fills: S,
    ledger: S,
    fast_contexts: S,
}

impl<S: WalletSubscription> WalletSubscriptions<S> {
    async fn recv(&mut self) -> Result<HyperliquidWalletUpdate, WalletStreamError> {
        let update = tokio::select! {
            update = self.fills.recv() => update,
            update = self.ledger.recv() => update,
            update = self.fast_contexts.recv() => update,
        };
        update
            .ok_or(WalletStreamError::SubscriptionClosed)?
            .map_err(WalletStreamError::from)
    }
}

pub(super) struct WalletService<S = ClientWalletSource> {
    source: S,
}

impl<S> WalletService<S> {
    pub(super) fn new(source: S) -> Self {
        Self { source }
    }
}

impl<S> WidgetService<WalletProtocol> for WalletService<S>
where
    S: WalletSource,
{
    async fn run(self, mut context: ServiceContext<WalletProtocol>) {
        let mut followed: Option<Vec<WalletAddress>> = None;
        while let Some(request) = context.next_rpc().await {
            let (reply, request) = request.into_parts();
            if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                continue;
            }
            let mut started = Vec::with_capacity(MAX_WALLET_ADDRESSES * 2);
            match request {
                WalletRequest::Load { addresses } => {
                    follow_addresses(&mut context, &mut followed, addresses.clone());
                    for address in &addresses {
                        start_overview(
                            &mut context,
                            self.source.clone(),
                            address.clone(),
                            false,
                            &mut started,
                        );
                        start_timeline(
                            &mut context,
                            self.source.clone(),
                            address.clone(),
                            false,
                            &mut started,
                        );
                    }
                }
                WalletRequest::Reload { addresses } => {
                    follow_addresses(&mut context, &mut followed, addresses.clone());
                    for address in &addresses {
                        start_overview(
                            &mut context,
                            self.source.clone(),
                            address.clone(),
                            true,
                            &mut started,
                        );
                        start_timeline(
                            &mut context,
                            self.source.clone(),
                            address.clone(),
                            false,
                            &mut started,
                        );
                    }
                }
                WalletRequest::Retry { addresses } => {
                    for address in addresses {
                        if !is_followed(&followed, &address) {
                            continue;
                        }
                        start_overview(
                            &mut context,
                            self.source.clone(),
                            address,
                            true,
                            &mut started,
                        );
                    }
                }
                WalletRequest::RetryTimeline { address } => {
                    if is_followed(&followed, &address) {
                        start_timeline(
                            &mut context,
                            self.source.clone(),
                            address,
                            true,
                            &mut started,
                        );
                    }
                }
                WalletRequest::LoadDetail { address, tab } => {
                    if is_followed(&followed, &address) {
                        let key = detail_key(&address, tab);
                        if context.stream_generation(&key).is_none() {
                            start_detail(
                                &mut context,
                                self.source.clone(),
                                key.clone(),
                                address,
                                tab,
                                false,
                            );
                            started.push(key);
                        }
                    }
                }
                WalletRequest::RetryDetail { address, tab } => {
                    if is_followed(&followed, &address) {
                        let key = detail_key(&address, tab);
                        start_detail(
                            &mut context,
                            self.source.clone(),
                            key.clone(),
                            address,
                            tab,
                            true,
                        );
                        started.push(key);
                    }
                }
            }
            if reply.deadline() <= Instant::now()
                || reply.is_cancelled()
                || reply.respond(Ok(WalletResponse::StreamsStarted)).is_err()
            {
                for key in started {
                    context.cancel_stream(&key);
                }
            }
        }
    }
}

fn is_followed(followed: &Option<Vec<WalletAddress>>, address: &WalletAddress) -> bool {
    followed
        .as_deref()
        .is_some_and(|current| current.contains(address))
}

fn follow_addresses(
    context: &mut ServiceContext<WalletProtocol>,
    followed: &mut Option<Vec<WalletAddress>>,
    addresses: Vec<WalletAddress>,
) {
    if followed.as_deref() == Some(addresses.as_slice()) {
        return;
    }
    let Some(previous) = followed.replace(addresses) else {
        return;
    };
    for address in previous {
        if !is_followed(followed, &address) {
            cancel_wallet(context, &address);
        }
    }
}

fn overview_key(address: &WalletAddress) -> WalletStreamKey {
    WalletStreamKey::Overview {
        address: address.clone(),
    }
}

fn timeline_key(address: &WalletAddress) -> WalletStreamKey {
    WalletStreamKey::Timeline {
        address: address.clone(),
    }
}

fn detail_key(address: &WalletAddress, tab: DetailTab) -> WalletStreamKey {
    WalletStreamKey::Detail {
        address: address.clone(),
        tab,
    }
}

fn cancel_wallet(context: &mut ServiceContext<WalletProtocol>, address: &WalletAddress) {
    context.cancel_stream(&overview_key(address));
    context.cancel_stream(&timeline_key(address));
    for tab in DetailTab::ALL {
        context.cancel_stream(&detail_key(address, tab));
    }
}

fn start_overview<S: WalletSource>(
    context: &mut ServiceContext<WalletProtocol>,
    source: S,
    address: WalletAddress,
    replace: bool,
    started: &mut Vec<WalletStreamKey>,
) {
    let key = overview_key(&address);
    if !replace && context.stream_generation(&key).is_some() {
        return;
    }
    let starter = move |publisher| async move {
        run_overview(source, address, publisher).await;
    };
    if replace {
        context.replace_stream(key.clone(), starter);
    } else {
        context.start_stream(key.clone(), starter);
    }
    started.push(key);
}

fn start_timeline<S: WalletSource>(
    context: &mut ServiceContext<WalletProtocol>,
    source: S,
    address: WalletAddress,
    replace: bool,
    started: &mut Vec<WalletStreamKey>,
) {
    let key = timeline_key(&address);
    if !replace && context.stream_generation(&key).is_some() {
        return;
    }
    let starter = move |publisher| async move {
        run_timeline(source, address, publisher).await;
    };
    if replace {
        context.replace_stream(key.clone(), starter);
    } else {
        context.start_stream(key.clone(), starter);
    }
    started.push(key);
}

fn start_detail<S: WalletSource>(
    context: &mut ServiceContext<WalletProtocol>,
    source: S,
    key: WalletStreamKey,
    address: WalletAddress,
    tab: DetailTab,
    replace: bool,
) {
    let starter = move |publisher| async move {
        run_detail(source, address, tab, publisher).await;
    };
    if replace {
        context.replace_stream(key, starter);
    } else {
        context.start_stream(key, starter);
    }
}

async fn publish_status(
    publisher: &StreamPublisher<WalletProtocol>,
    status: WalletStatus,
) -> Result<(), StreamPublishError> {
    publisher
        .publish_reliable(WalletStreamMessage::Status(status))
        .await
}

async fn fail(publisher: &StreamPublisher<WalletProtocol>, error: WalletStreamError) {
    let _ = publish_status(publisher, WalletStatus::Failed(error)).await;
    publisher.cancelled().await;
}

async fn run_overview<S: WalletSource>(
    source: S,
    address: WalletAddress,
    publisher: StreamPublisher<WalletProtocol>,
) {
    let published = publish_status(&publisher, WalletStatus::Connecting).await;
    if published.is_err() {
        return;
    }
    if let Err(error) = run_overview_once(&source, address, &publisher).await {
        fail(&publisher, error).await;
    }
}

async fn run_overview_once<S: WalletSource>(
    source: &S,
    address: WalletAddress,
    publisher: &StreamPublisher<WalletProtocol>,
) -> Result<(), WalletStreamError> {
    let routes = source.wallet_routes();
    let labels = source.display_generation();
    let start_time = ledger_start()?;
    let subscriptions = async {
        tokio::try_join!(
            async {
                source
                    .subscribe_fills(&address, &routes)
                    .await
                    .map_err(WalletStreamError::from)
            },
            async {
                source
                    .subscribe_ledger(&address, &routes)
                    .await
                    .map_err(WalletStreamError::from)
            },
            async {
                source
                    .subscribe_fast_contexts(&routes)
                    .await
                    .map_err(WalletStreamError::from)
            },
        )
    };
    let baseline = read_baseline(source, &routes, &address, start_time);
    let ((fills, ledger, fast_contexts), baseline) = tokio::select! {
        biased;
        () = publisher.cancelled() => return Ok(()),
        result = async { tokio::try_join!(subscriptions, baseline) } => result?,
    };
    let WalletBaseline {
        kinds,
        catalogs,
        spot,
        perpetual,
        pairs,
        fills: fills_history,
        ledger: ledger_history,
    } = baseline;
    let mut state = OverviewState {
        spot,
        perpetual,
        pairs,
        contexts: Vec::new(),
        applied_increment_ids: AppliedIncrementIds::default(),
        fills_snapshot_received: false,
    };
    state.record_baseline_increments(&fills_history, &ledger_history);
    let precision = DisplayPrecision::from_catalogs(AssetLabels::new(labels), catalogs)?;
    let mut subscriptions = WalletSubscriptions {
        fills,
        ledger,
        fast_contexts,
    };
    publish_overview(publisher, &state, &precision)?;
    if publish_status(publisher, WalletStatus::Ready)
        .await
        .is_err()
    {
        return Ok(());
    }

    loop {
        tokio::select! {
            biased;
            () = publisher.cancelled() => return Ok(()),
            update = subscriptions.recv() => {
                let update = update?;
                let reconnect_snapshot = matches!(
                    &update,
                    HyperliquidWalletUpdate::UserFills {
                        is_snapshot: true,
                        ..
                    }
                );
                let should_rebase = reconnect_snapshot && state.fills_snapshot_received;
                state.fills_snapshot_received |= reconnect_snapshot;
                apply_update(update, &mut state)?;
                if should_rebase {
                    let mut rebase_again = true;
                    while rebase_again {
                        rebase_again = rebase_overview(
                            source,
                            &address,
                            &routes,
                            &kinds,
                            &mut state,
                            &mut subscriptions,
                            publisher,
                        )
                        .await?;
                    }
                }
                publish_overview(publisher, &state, &precision)?;
            }
        }
    }
}

struct WalletBaseline {
    kinds: WalletRouteKinds,
    catalogs: Vec<(VenueRoute, Arc<[CatalogEntry]>)>,
    spot: HyperliquidSpotState,
    perpetual: HyperliquidPerpetualState,
    pairs: Vec<HyperliquidSpotPair>,
    fills: Vec<HyperliquidUserFill>,
    ledger: Vec<HyperliquidLedgerEvent>,
}

async fn read_baseline<S: WalletSource>(
    source: &S,
    routes: &[VenueRoute],
    address: &WalletAddress,
    start_time: i64,
) -> Result<WalletBaseline, WalletStreamError> {
    let catalogs = read_catalogs(source, routes).await?;
    let kinds = WalletRouteKinds::of(&catalogs);
    let (spot, perpetual, pairs, fills, ledger) = tokio::try_join!(
        read_spot_state(source, spot_route(&kinds, routes), address),
        read_perpetual_states(source, kinds.perpetual(), address),
        read_spot_pairs(source, kinds.spot()),
        async {
            source
                .fetch_fills(address, false, routes)
                .await
                .map_err(WalletStreamError::from)
        },
        async {
            source
                .fetch_ledger(address, start_time, routes)
                .await
                .map_err(WalletStreamError::from)
        },
    )?;
    Ok(WalletBaseline {
        kinds,
        catalogs,
        spot,
        perpetual,
        pairs,
        fills,
        ledger,
    })
}

fn spot_route<'a>(kinds: &'a WalletRouteKinds, routes: &'a [VenueRoute]) -> Option<&'a VenueRoute> {
    kinds.spot().first().or_else(|| routes.first())
}

async fn read_catalogs<S: WalletSource>(
    source: &S,
    routes: &[VenueRoute],
) -> Result<Vec<(VenueRoute, Arc<[CatalogEntry]>)>, WalletStreamError> {
    let catalogs =
        try_join_all(routes.iter().cloned().map(|route| async move {
            let catalog = source.fetch_catalog(&route).await.map_err(|source| {
                WalletStreamError::DexRead {
                    route: route.clone(),
                    source,
                }
            })?;
            Ok::<_, WalletStreamError>((route, catalog))
        }))
        .await?;
    Ok(catalogs)
}

async fn read_spot_state<S: WalletSource>(
    source: &S,
    route: Option<&VenueRoute>,
    address: &WalletAddress,
) -> Result<HyperliquidSpotState, WalletStreamError> {
    let Some(route) = route else {
        return Err(WalletStreamError::Account(AccountError::Model(
            market_participants_model::error::ModelError::EmptyCollection {
                field: "wallet routes",
            },
        )));
    };
    source
        .fetch_spot_state(route, address)
        .await
        .map_err(WalletStreamError::from)
}

async fn read_perpetual_states<S: WalletSource>(
    source: &S,
    routes: &[VenueRoute],
    address: &WalletAddress,
) -> Result<HyperliquidPerpetualState, WalletStreamError> {
    let states = try_join_all(routes.iter().cloned().map(|route| async move {
        source
            .fetch_perpetual_state(&route, address)
            .await
            .map_err(|source| WalletStreamError::DexRead { route, source })
    }))
    .await?;
    let positions = states
        .into_iter()
        .flat_map(|state| state.positions)
        .collect();
    Ok(HyperliquidPerpetualState { positions })
}

async fn read_open_orders<S: WalletSource>(
    source: &S,
    routes: &[VenueRoute],
    address: &WalletAddress,
) -> Result<Vec<HyperliquidOpenOrder>, WalletStreamError> {
    let orders = try_join_all(routes.iter().cloned().map(|route| async move {
        source
            .fetch_open_orders(&route, address)
            .await
            .map_err(|source| WalletStreamError::DexRead { route, source })
    }))
    .await?;
    Ok(orders.into_iter().flatten().collect())
}

async fn read_spot_pairs<S: WalletSource>(
    source: &S,
    routes: &[VenueRoute],
) -> Result<Vec<HyperliquidSpotPair>, WalletStreamError> {
    let pairs = try_join_all(routes.iter().cloned().map(|route| async move {
        source
            .fetch_spot_pairs(&route)
            .await
            .map_err(|source| WalletStreamError::DexRead { route, source })
    }))
    .await?;
    Ok(pairs.into_iter().flatten().collect())
}

struct OverviewState {
    spot: HyperliquidSpotState,
    perpetual: HyperliquidPerpetualState,
    pairs: Vec<HyperliquidSpotPair>,
    contexts: Vec<HyperliquidAssetContext>,
    applied_increment_ids: AppliedIncrementIds,
    fills_snapshot_received: bool,
}

impl OverviewState {
    fn summary(&self, precision: &DisplayPrecision) -> AccountSummary {
        AccountSummary::from_states(
            &self.spot,
            &self.perpetual,
            &self.pairs,
            &self.contexts,
            precision,
        )
    }

    fn record_baseline_increments(
        &mut self,
        fills: &[HyperliquidUserFill],
        ledger: &[HyperliquidLedgerEvent],
    ) {
        self.applied_increment_ids
            .extend(fills.iter().map(AppliedIncrementId::from_fill));
        self.applied_increment_ids
            .extend(ledger.iter().map(AppliedIncrementId::from_ledger));
    }
}

struct AppliedIncrementIds {
    ids: AHashSet<AppliedIncrementId>,
    order: VecDeque<AppliedIncrementId>,
}

impl Default for AppliedIncrementIds {
    fn default() -> Self {
        Self {
            ids: AHashSet::with_capacity(MAX_APPLIED_INCREMENT_IDS),
            order: VecDeque::with_capacity(MAX_APPLIED_INCREMENT_IDS),
        }
    }
}

impl AppliedIncrementIds {
    fn extend<I>(&mut self, identifiers: I)
    where
        I: IntoIterator<Item = AppliedIncrementId>,
    {
        for identifier in identifiers {
            self.insert(identifier);
        }
    }

    fn insert(&mut self, id: AppliedIncrementId) -> bool {
        if self.ids.contains(&id) {
            return false;
        }
        if self.order.len() == MAX_APPLIED_INCREMENT_IDS
            && let Some(evicted) = self.order.pop_front()
        {
            self.ids.remove(&evicted);
        }
        self.ids.insert(id.clone());
        self.order.push_back(id);
        true
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.order.len()
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum AppliedIncrementId {
    Fill {
        address: WalletAddress,
        listing: VenueListingKey,
        timestamp: i64,
        trade_id: u64,
    },
    Ledger {
        address: WalletAddress,
        hash: SmolStr,
        kind: SmolStr,
        timestamp: i64,
    },
}

impl AppliedIncrementId {
    fn from_fill(fill: &HyperliquidUserFill) -> Self {
        Self::Fill {
            address: fill.address.clone(),
            listing: fill.listing.clone(),
            timestamp: fill.timestamp,
            trade_id: fill.tid,
        }
    }

    fn from_ledger(event: &HyperliquidLedgerEvent) -> Self {
        Self::Ledger {
            address: event.address.clone(),
            hash: event.hash.clone(),
            kind: event.kind.clone(),
            timestamp: event.timestamp,
        }
    }
}

async fn rebase_overview<S: WalletSource>(
    source: &S,
    address: &WalletAddress,
    routes: &[VenueRoute],
    kinds: &WalletRouteKinds,
    state: &mut OverviewState,
    subscriptions: &mut WalletSubscriptions<S::Subscription>,
    publisher: &StreamPublisher<WalletProtocol>,
) -> Result<bool, WalletStreamError> {
    state.contexts.clear();
    let baseline = async {
        tokio::try_join!(
            read_spot_state(source, spot_route(kinds, routes), address),
            read_perpetual_states(source, kinds.perpetual(), address),
        )
    };
    tokio::pin!(baseline);
    let mut queued = RebaseUpdateBuffer::default();
    loop {
        tokio::select! {
            biased;
            () = publisher.cancelled() => return Ok(false),
            result = &mut baseline => {
                let (spot, perpetual) = result?;
                state.spot = spot;
                state.perpetual = perpetual;
                return queued.replay(state);
            }
            update = subscriptions.recv() => queued.push(update?)?,
        }
    }
}

#[derive(Default)]
struct RebaseUpdateBuffer {
    account_updates: VecDeque<HyperliquidWalletUpdate>,
    account_event_count: usize,
    latest_fast_contexts: Option<Vec<HyperliquidAssetContext>>,
    rebase_requested: bool,
}

impl RebaseUpdateBuffer {
    fn push(&mut self, update: HyperliquidWalletUpdate) -> Result<(), WalletStreamError> {
        if matches!(
            &update,
            HyperliquidWalletUpdate::UserFills {
                is_snapshot: true,
                ..
            }
        ) {
            self.rebase_requested = true;
        }
        let event_count = match &update {
            HyperliquidWalletUpdate::UserFills { fills, .. } => fills.len(),
            HyperliquidWalletUpdate::UserLedger { events, .. } => events.len(),
            HyperliquidWalletUpdate::FastAssetContexts { contexts } => {
                self.latest_fast_contexts = Some(contexts.clone());
                return Ok(());
            }
        };
        if event_count == 0 {
            return Ok(());
        }
        let next_count = self.account_event_count.checked_add(event_count).ok_or(
            WalletStreamError::SubscriptionBufferFull {
                operation: "rebase Hyperliquid wallet updates",
                capacity: MAX_REBASE_INCREMENT_EVENTS,
            },
        )?;
        if next_count > MAX_REBASE_INCREMENT_EVENTS {
            return Err(WalletStreamError::SubscriptionBufferFull {
                operation: "rebase Hyperliquid wallet updates",
                capacity: MAX_REBASE_INCREMENT_EVENTS,
            });
        }
        self.account_event_count = next_count;
        self.account_updates.push_back(update);
        Ok(())
    }

    fn replay(self, state: &mut OverviewState) -> Result<bool, WalletStreamError> {
        let Self {
            account_updates,
            latest_fast_contexts,
            rebase_requested,
            ..
        } = self;
        for update in account_updates {
            apply_update(update, state)?;
        }
        if let Some(contexts) = latest_fast_contexts {
            state.contexts = contexts;
        }
        Ok(rebase_requested)
    }
}

fn apply_update(
    update: HyperliquidWalletUpdate,
    state: &mut OverviewState,
) -> Result<(), WalletStreamError> {
    match update {
        HyperliquidWalletUpdate::UserFills { is_snapshot, fills } => {
            for fill in fills {
                let first_observation = state
                    .applied_increment_ids
                    .insert(AppliedIncrementId::from_fill(&fill));
                if !is_snapshot && first_observation {
                    apply_perpetual_fill(&mut state.perpetual, &fill)?;
                }
            }
            Ok(())
        }
        HyperliquidWalletUpdate::UserLedger {
            is_snapshot,
            events,
        } => {
            for event in events {
                let first_observation = state
                    .applied_increment_ids
                    .insert(AppliedIncrementId::from_ledger(&event));
                if !is_snapshot && first_observation {
                    apply_spot_ledger(&mut state.spot, &event)?;
                }
            }
            Ok(())
        }
        HyperliquidWalletUpdate::FastAssetContexts { contexts } => {
            state.contexts = contexts;
            Ok(())
        }
    }
}

fn apply_perpetual_fill(
    state: &mut HyperliquidPerpetualState,
    fill: &HyperliquidUserFill,
) -> Result<(), WalletStreamError> {
    if fill.liquidation {
        return Ok(());
    }
    let quantity = i128::try_from(fill.quantity.value.inner()).map_err(|_| {
        WalletStreamError::Data(WalletDataError::Codec {
            operation: "apply Hyperliquid perpetual fill",
        })
    })?;
    let delta = match fill.side {
        Side::Buy => SignedQuantity::from_inner(quantity),
        Side::Sell => SignedQuantity::from_inner(quantity.checked_neg().ok_or(
            WalletStreamError::Data(WalletDataError::Codec {
                operation: "apply Hyperliquid perpetual fill",
            }),
        )?),
    };
    let start = fill.start_position;
    let end = start
        .checked_add(delta)
        .ok_or(WalletStreamError::Data(WalletDataError::Codec {
            operation: "apply Hyperliquid perpetual fill",
        }))?;
    let index = state
        .positions
        .iter()
        .position(|position| position.listing == fill.listing);
    if end.is_zero() {
        if let Some(index) = index {
            state.positions.remove(index);
        }
        return Ok(());
    }
    let direction = if end.inner().is_negative() {
        PositionDirection::Short
    } else {
        PositionDirection::Long
    };
    let mut position = index
        .and_then(|index| state.positions.get(index).cloned())
        .unwrap_or_else(|| {
            market_participants_connector::account::domain::HyperliquidPerpetualPosition {
                listing: fill.listing.clone(),
                size: MarketQuantity::traded(
                    Quantity::from_inner(start.inner().unsigned_abs()),
                    fill.quantity.unit.clone(),
                )
                .unwrap_or_else(|_| fill.quantity.clone()),
                direction,
                entry_price: None,
                mark_price: None,
                liquidation_price: None,
                position_value: None,
                unrealized_pnl: AssetAmount {
                    asset: fill.closed_pnl.asset.clone(),
                    value: SignedQuantity::ZERO,
                },
                margin: None,
                funding_since_open: AssetAmount {
                    asset: fill.fee.asset.clone(),
                    value: SignedQuantity::ZERO,
                },
            }
        });
    position.size = MarketQuantity::traded(
        Quantity::from_inner(end.inner().unsigned_abs()),
        fill.quantity.unit.clone(),
    )
    .unwrap_or_else(|_| fill.quantity.clone());
    position.direction = direction;
    let same_direction = !start.is_zero() && start.inner().signum() == end.inner().signum();
    if !same_direction {
        position.entry_price = Some(fill.price.clone());
    } else if start.inner().unsigned_abs() < end.inner().unsigned_abs() {
        let added = Quantity::from_inner(fill.quantity.value.inner());
        let existing = Quantity::from_inner(start.inner().unsigned_abs());
        if let Some(old_entry) = position.entry_price {
            let weighted = existing
                .checked_mul_price(old_entry.value)
                .and_then(|value| {
                    added
                        .checked_mul_price(fill.price.value)
                        .and_then(|next| value.checked_add(next))
                })
                .and_then(|value| {
                    value.checked_div_quantity(Quantity::from_inner(end.inner().unsigned_abs()))
                })
                .ok_or(WalletStreamError::Data(WalletDataError::Codec {
                    operation: "calculate Hyperliquid perpetual entry price",
                }))?;
            position.entry_price = Some(MarketPrice {
                value: weighted,
                quote_asset: fill.price.quote_asset.clone(),
                basis: fill.price.basis.clone(),
            });
        } else {
            position.entry_price = Some(fill.price.clone());
        }
    }
    if let Some(index) = index {
        state.positions[index] = position;
    } else {
        state.positions.push(position);
    }
    state
        .positions
        .sort_unstable_by(|left, right| left.listing.cmp(&right.listing));
    Ok(())
}

fn apply_spot_ledger(
    state: &mut HyperliquidSpotState,
    event: &HyperliquidLedgerEvent,
) -> Result<(), WalletStreamError> {
    if !matches!(
        event.kind.as_str(),
        "deposit" | "withdraw" | "internalTransfer" | "subAccountTransfer" | "accountClassTransfer"
    ) {
        return Ok(());
    }
    let (Some(asset), Some(amount), Some(direction)) = (
        event.asset.clone(),
        event.amount.clone(),
        event.transfer_direction,
    ) else {
        return Ok(());
    };
    if amount.asset != asset {
        return Err(WalletStreamError::Data(WalletDataError::Codec {
            operation: "apply Hyperliquid spot ledger",
        }));
    }
    let delta = match direction {
        market_participants_connector::account::domain::HyperliquidTransferDirection::Incoming => {
            amount.value
        }
        market_participants_connector::account::domain::HyperliquidTransferDirection::Outgoing => {
            amount
                .value
                .checked_neg()
                .ok_or(WalletStreamError::Data(WalletDataError::Codec {
                    operation: "apply Hyperliquid spot ledger",
                }))?
        }
    };
    if let Some(balance) = state
        .balances
        .iter_mut()
        .find(|balance| balance.asset == asset)
    {
        balance.total = balance
            .total
            .checked_add(delta)
            .ok_or(WalletStreamError::Data(WalletDataError::Codec {
                operation: "apply Hyperliquid spot ledger",
            }))?;
    } else {
        state.balances.push(
            market_participants_connector::account::domain::HyperliquidSpotBalance {
                asset,
                total: delta,
                hold: SignedQuantity::ZERO,
            },
        );
    }
    state
        .balances
        .sort_unstable_by(|left, right| left.asset.cmp(&right.asset));
    Ok(())
}

fn publish_overview(
    publisher: &StreamPublisher<WalletProtocol>,
    state: &OverviewState,
    precision: &DisplayPrecision,
) -> Result<(), WalletStreamError> {
    publisher
        .publish_latest(WalletStreamMessage::Overview(Arc::new(
            WalletOverviewSnapshot {
                summary: state.summary(precision),
                precision: precision.clone(),
            },
        )))
        .map_err(|_| WalletStreamError::StreamClosed)
}

async fn run_timeline<S: WalletSource>(
    source: S,
    address: WalletAddress,
    publisher: StreamPublisher<WalletProtocol>,
) {
    if publish_status(&publisher, WalletStatus::Connecting)
        .await
        .is_err()
    {
        return;
    }
    let mut next_tick = Instant::now();
    let mut published = false;
    loop {
        tokio::select! {
            biased;
            () = publisher.cancelled() => return,
            () = tokio::time::sleep_until(next_tick) => {}
        }
        let request_started = Instant::now();
        let end = match now_millis() {
            Some(value) => value,
            None => {
                fail(&publisher, time_error()).await;
                return;
            }
        };
        let Some(start) = end.checked_sub(WINDOW_MILLIS) else {
            fail(
                &publisher,
                WalletStreamError::Data(WalletDataError::Codec {
                    operation: "calculate Hyperliquid timeline window",
                }),
            )
            .await;
            return;
        };
        let routes = source.wallet_routes();
        let result = tokio::select! {
            biased;
            () = publisher.cancelled() => return,
            result = source.fetch_fills_by_time(&address, start, end, &routes) => result,
        };
        match result {
            Ok(fills) => {
                if fills.len() > MAX_TIMELINE_FILLS {
                    fail(
                        &publisher,
                        WalletStreamError::Data(WalletDataError::Codec {
                            operation: "validate Hyperliquid timeline fill count",
                        }),
                    )
                    .await;
                    return;
                }
                let snapshot = TimelineSnapshot {
                    window_start: start,
                    window_end: end,
                    fills: fills.into(),
                };
                if publisher
                    .publish_latest(WalletStreamMessage::Timeline(Arc::new(snapshot)))
                    .is_err()
                {
                    return;
                }
                if !published {
                    published = true;
                    if publish_status(&publisher, WalletStatus::Ready)
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            Err(error) => {
                fail(&publisher, error.into()).await;
                return;
            }
        }
        next_tick = request_started + TIMELINE_INTERVAL;
        let now = Instant::now();
        while next_tick <= now {
            next_tick += TIMELINE_INTERVAL;
        }
    }
}

async fn run_detail<S: WalletSource>(
    source: S,
    address: WalletAddress,
    tab: DetailTab,
    publisher: StreamPublisher<WalletProtocol>,
) {
    if publish_status(&publisher, WalletStatus::Connecting)
        .await
        .is_err()
    {
        return;
    }
    if matches!(tab, DetailTab::Spot | DetailTab::Perpetual) {
        let routes = source.wallet_routes();
        let result = tokio::select! {
            biased;
            () = publisher.cancelled() => return,
            result = async {
                tokio::try_join!(
                    async {
                        source
                            .subscribe_fast_contexts(&routes)
                            .await
                            .map_err(WalletStreamError::from)
                    },
                    fetch_detail(&source, &address, &routes, tab)
                )
            } => result,
        };
        let (mut fast_contexts, mut data) = match result {
            Ok(value) => value,
            Err(error) => {
                fail(&publisher, error).await;
                return;
            }
        };
        if publisher
            .publish_latest(WalletStreamMessage::Detail(Arc::new(data.clone())))
            .is_err()
        {
            return;
        }
        if publish_status(&publisher, WalletStatus::Ready)
            .await
            .is_err()
        {
            return;
        }
        loop {
            tokio::select! {
                biased;
                () = publisher.cancelled() => return,
                update = fast_contexts.recv() => match update {
                    Some(Ok(HyperliquidWalletUpdate::FastAssetContexts { contexts })) => {
                        if data.apply_contexts(&contexts).is_err()
                            || publisher
                                .publish_latest(WalletStreamMessage::Detail(Arc::new(data.clone())))
                                .is_err()
                        {
                            return;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        fail(&publisher, error.into()).await;
                        return;
                    }
                    None => {
                        fail(&publisher, WalletStreamError::SubscriptionClosed).await;
                        return;
                    }
                }
            }
        }
    }

    let routes = source.wallet_routes();
    let result = tokio::select! {
        biased;
        () = publisher.cancelled() => return,
        result = fetch_detail(&source, &address, &routes, tab) => result,
    };
    match result {
        Ok(data) => {
            if publisher
                .publish_latest(WalletStreamMessage::Detail(Arc::new(data)))
                .is_err()
            {
                return;
            }
            if publish_status(&publisher, WalletStatus::Ready)
                .await
                .is_ok()
            {
                publisher.cancelled().await;
            }
        }
        Err(error) => fail(&publisher, error).await,
    }
}

async fn fetch_detail<S: WalletSource>(
    source: &S,
    address: &WalletAddress,
    routes: &[VenueRoute],
    tab: DetailTab,
) -> Result<DetailSnapshot, WalletStreamError> {
    let labels = source.display_generation();
    match tab {
        DetailTab::Spot => {
            let catalogs = read_catalogs(source, routes).await?;
            let kinds = WalletRouteKinds::of(&catalogs);
            let (state, pairs) = tokio::try_join!(
                read_spot_state(source, spot_route(&kinds, routes), address),
                read_spot_pairs(source, kinds.spot()),
            )?;
            let precision = DisplayPrecision::from_catalogs(AssetLabels::new(labels), catalogs)?;
            Ok(DetailSnapshot::new(
                DetailData::Spot(SpotSnapshot::new(state, pairs, Vec::new(), &precision)?),
                precision,
            )?)
        }
        DetailTab::Perpetual => {
            let (precision, kinds) = precision_of(source, routes, labels).await?;
            let state = read_perpetual_states(source, kinds.perpetual(), address).await?;
            Ok(DetailSnapshot::new(
                DetailData::Perpetual(state),
                precision,
            )?)
        }
        DetailTab::OpenOrders => {
            let (precision, kinds) = precision_of(source, routes, labels).await?;
            let orders = read_open_orders(source, kinds.perpetual(), address).await?;
            Ok(DetailSnapshot::new(
                DetailData::OpenOrders(sort_and_limit_open_orders(orders)),
                precision,
            )?)
        }
        DetailTab::Trades => {
            let (precision, _) = precision_of(source, routes, labels).await?;
            let fills = source.fetch_fills(address, true, routes).await?;
            Ok(DetailSnapshot::new(
                DetailData::Trades(sort_and_limit_fills(fills)),
                precision,
            )?)
        }
        DetailTab::HistoricalOrders => {
            let (precision, _) = precision_of(source, routes, labels).await?;
            let orders = source.fetch_historical_orders(address, routes).await?;
            Ok(DetailSnapshot::new(
                DetailData::HistoricalOrders(sort_and_limit_historical_orders(orders)),
                precision,
            )?)
        }
        DetailTab::Ledger => {
            let events = source
                .fetch_ledger(address, ledger_start()?, routes)
                .await?;
            Ok(DetailSnapshot::new(
                DetailData::Ledger(sort_and_limit_ledger(events)),
                DisplayPrecision::money_only(AssetLabels::new(labels))?,
            )?)
        }
    }
}

async fn precision_of<S: WalletSource>(
    source: &S,
    routes: &[VenueRoute],
    labels: DisplaySnapshot,
) -> Result<(DisplayPrecision, WalletRouteKinds), WalletStreamError> {
    let catalogs = read_catalogs(source, routes).await?;
    let kinds = WalletRouteKinds::of(&catalogs);
    let precision = DisplayPrecision::from_catalogs(AssetLabels::new(labels), catalogs)?;
    Ok((precision, kinds))
}

fn ledger_start() -> Result<i64, WalletStreamError> {
    let now = now_millis().ok_or_else(time_error)?;
    now.checked_sub(LEDGER_WINDOW).ok_or_else(time_error)
}

fn time_error() -> WalletStreamError {
    WalletStreamError::Data(WalletDataError::Codec {
        operation: "read Hyperliquid wallet time",
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use market_participants_connector::account::domain::HyperliquidWalletUpdate;
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId, WalletAddress},
        listing::route::VenueListingKey,
        market::units::Side,
    };

    use super::*;
    use crate::test_support::{
        SubscriptionKind, TestSource, address, context, deposit, detail_key, fill, latest_reading,
        open_order, overview_key, pair_listing, perp_entry, perpetual_route, perpetual_state,
        position, runtime, spot_entry, spot_pair, spot_state, test_routes, timeline_key, until,
        wait_for_status, wallet_balance,
    };

    fn wallets(addresses: &[WalletAddress]) -> Vec<WalletAddress> {
        addresses.to_vec()
    }

    fn venue() -> TestSource {
        TestSource::new(&test_routes())
    }

    fn listing() -> VenueListingKey {
        crate::test_support::listing_key(&perpetual_route(), "ETH")
    }

    fn overview_of(message: &WalletStreamMessage) -> &Arc<WalletOverviewSnapshot> {
        match message {
            WalletStreamMessage::Overview(snapshot) => snapshot,
            other => panic!("an overview reading should be published, not {other:?}"),
        }
    }

    fn detail_of(message: &WalletStreamMessage) -> &Arc<DetailSnapshot> {
        match message {
            WalletStreamMessage::Detail(snapshot) => snapshot,
            other => panic!("a detail reading should be published, not {other:?}"),
        }
    }

    #[test]
    fn wallet_routes_should_be_the_wallet_routes_of_this_venue_only() {
        let wallet = |route: VenueRoute, provides_catalog: bool| WalletScope {
            route,
            monitors_wallets: true,
            provides_catalog,
        };
        let this_venue = wallet(perpetual_route(), true);
        let another_venue = wallet(
            VenueRoute::new(
                VenueId::Binance,
                VenueScopeId::new("futures").expect("a canonical scope"),
            ),
            true,
        );
        let no_catalog = wallet(crate::test_support::spot_route(), false);
        let control = WalletScope {
            route: crate::test_support::route("discovery"),
            monitors_wallets: false,
            provides_catalog: false,
        };

        assert!(
            this_venue.route.venue == VenueId::Hyperliquid
                && this_venue.monitors_wallets
                && this_venue.provides_catalog,
            "this venue's fixture must be one the filter would otherwise accept: {this_venue:?}"
        );
        assert!(
            another_venue.route.venue != VenueId::Hyperliquid
                && another_venue.monitors_wallets
                && another_venue.provides_catalog,
            "the foreign fixture must be one the filter would otherwise accept: {another_venue:?}"
        );

        let routes = wallet_routes_of(&[this_venue, another_venue, no_catalog, control]);

        assert_eq!(
            routes,
            vec![perpetual_route()],
            "only this venue's wallet route, published with a catalog, is read"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_catalog_that_cannot_be_read_should_fail_the_address() {
        let wallet = address("1");
        let source = venue();
        source.fail_catalog_of(&perpetual_route());
        let mut runtime = runtime(source.clone());
        let mut reliable = runtime.take_reliable_stream().expect("a stream");

        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: wallets(std::slice::from_ref(&wallet)),
            })
            .await
            .expect("the load should be answered");

        wait_for_status(
            &mut reliable,
            &overview_key(&wallet),
            &WalletStatus::Failed(WalletStreamError::DexRead {
                route: perpetual_route(),
                source: AccountError::Protocol {
                    operation: "read the fake wallet catalog",
                },
            }),
        )
        .await;
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_load_should_start_the_overview_and_the_timeline_of_every_wallet() {
        let first = address("1");
        let second = address("2");
        let source = venue();
        let mut runtime = runtime(source.clone());
        let mut reliable = runtime.take_reliable_stream().expect("a stream");
        let followed = wallets(&[first.clone(), second.clone()]);

        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: followed.clone(),
            })
            .await
            .expect("the load should be answered");
        until(|| source.counters().streams() == 6).await;

        let counters = source.counters();
        assert_eq!(counters.fills_subscriptions, 2);
        assert_eq!(counters.ledger_subscriptions, 2);
        assert_eq!(counters.fast_context_subscriptions, 2);
        assert_eq!(counters.spot_states, 2);
        let read = source.read_addresses();
        assert!(
            !read.is_empty() && read.iter().all(|read| read == &first || read == &second),
            "every reading is made for an address the pane named: {read:?}"
        );
        wait_for_status(&mut reliable, &overview_key(&first), &WalletStatus::Ready).await;
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_base_asset_quoted_against_two_assets_should_still_be_read() {
        let spot = crate::test_support::spot_route();
        let source = TestSource::new(&[
            (
                perpetual_route(),
                vec![perp_entry(&perpetual_route(), "ETH", "0.1", "0.001")],
            ),
            (
                spot.clone(),
                vec![
                    spot_entry(&spot, "HYPE", "USDC", "0.001", "0.01"),
                    spot_entry(&spot, "HYPE", "USDH", "0.01", "0.01"),
                ],
            ),
        ]);
        let wallet = address("1");
        let mut runtime = runtime(source.clone());
        let mut reliable = runtime.take_reliable_stream().expect("a stream");

        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: wallets(std::slice::from_ref(&wallet)),
            })
            .await
            .expect("the load should be answered");
        wait_for_status(&mut reliable, &overview_key(&wallet), &WalletStatus::Ready).await;

        let spot = detail_key(&wallet, DetailTab::Spot);
        runtime
            .client()
            .call(WalletRequest::LoadDetail {
                address: wallet.clone(),
                tab: DetailTab::Spot,
            })
            .await
            .expect("the spot detail should be answered");
        wait_for_status(&mut reliable, &spot, &WalletStatus::Ready).await;
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_public_scope_should_read_one_stream_set_per_address() {
        let source = venue();
        let runtime = runtime(source.clone());
        let wallets = vec![address("1"), address("2")];
        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: wallets.clone(),
            })
            .await
            .expect("the load should be answered");
        until(|| source.counters().spot_states == 2).await;

        assert_eq!(
            source.counters().fills_subscriptions,
            2,
            "every public address has its own fill subscription"
        );
        assert_eq!(
            source.counters().fast_context_subscriptions,
            2,
            "each wallet owns its own reading, so it owns its own route-level stream"
        );
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_partition_that_fails_should_fail_the_address_and_name_itself() {
        let wallet = address("1");
        let source = venue();
        source.fail_state_of(&perpetual_route());
        let mut runtime = runtime(source.clone());
        let mut reliable = runtime.take_reliable_stream().expect("a stream");
        let followed = wallets(std::slice::from_ref(&wallet));

        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: followed.clone(),
            })
            .await
            .expect("the load should be answered");

        wait_for_status(
            &mut reliable,
            &overview_key(&wallet),
            &WalletStatus::Failed(WalletStreamError::DexRead {
                route: perpetual_route(),
                source: AccountError::Protocol {
                    operation: "read the fake wallet state",
                },
            }),
        )
        .await;
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_repeated_load_should_leave_the_running_streams_alone() {
        let wallet = address("1");
        let source = venue();
        let runtime = runtime(source.clone());
        let followed = wallets(std::slice::from_ref(&wallet));
        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: followed.clone(),
            })
            .await
            .expect("the load should be answered");
        until(|| source.counters().streams() == 3).await;

        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: followed.clone(),
            })
            .await
            .expect("the second load should be answered");
        tokio::task::yield_now().await;

        assert_eq!(source.counters().streams(), 3);
        assert_eq!(source.subscription_drops(), 0);
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn another_address_list_should_replace_the_streams_of_the_old_one() {
        let second = address("2");
        let source = venue();
        let runtime = runtime(source.clone());
        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: wallets(&[address("1")]),
            })
            .await
            .expect("the load should be answered");
        until(|| source.counters().streams() == 3).await;

        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: wallets(std::slice::from_ref(&second)),
            })
            .await
            .expect("the second load should be answered");
        until(|| source.counters().streams() == 6 && source.subscription_drops() == 3).await;

        assert_eq!(source.subscription_drops(), 3);
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_retry_should_restart_only_the_wallets_that_failed() {
        let first = address("1");
        let second = address("2");
        let source = venue();
        let runtime = runtime(source.clone());
        let followed = wallets(&[first.clone(), second.clone()]);
        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: followed.clone(),
            })
            .await
            .expect("the load should be answered");
        until(|| source.counters().streams() == 6).await;

        runtime
            .client()
            .call(WalletRequest::Retry {
                addresses: vec![first],
            })
            .await
            .expect("the retry should be answered");
        until(|| source.counters().subscriptions() == 6).await;

        assert_eq!(source.counters().fills_subscriptions, 3);
        assert_eq!(source.counters().ledger_subscriptions, 3);
        assert_eq!(source.subscription_drops(), 3);
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_reload_should_follow_the_wallets_it_names_and_read_their_totals_again() {
        let first = address("1");
        let second = address("2");
        let third = address("3");
        let source = venue();
        let runtime = runtime(source.clone());
        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: wallets(&[first.clone(), second.clone()]),
            })
            .await
            .expect("the load should be answered");
        until(|| source.counters().streams() == 6 && source.counters().fills_by_time == 2).await;

        runtime
            .client()
            .call(WalletRequest::Reload {
                addresses: wallets(&[first, third]),
            })
            .await
            .expect("the reload should be answered");
        until(|| source.counters().fills_subscriptions == 4).await;
        assert_eq!(
            source.counters().ledger_subscriptions,
            4,
            "both readings of every wallet the reload names are subscribed again"
        );
        assert!(
            source.subscription_drops() >= 3,
            "the wallet the reload left out is no longer followed"
        );
        until(|| source.counters().fills_by_time == 3).await;
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_timeline_should_be_read_at_once_and_then_on_its_cadence() {
        let wallet = address("1");
        let source = venue();
        let runtime = runtime(source.clone());
        let followed = wallets(std::slice::from_ref(&wallet));
        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: followed.clone(),
            })
            .await
            .expect("the load should be answered");
        until(|| source.counters().fills_by_time == 1).await;

        tokio::time::advance(Duration::from_secs(9)).await;
        tokio::task::yield_now().await;
        assert_eq!(source.counters().fills_by_time, 1);

        tokio::time::advance(Duration::from_secs(1)).await;
        until(|| source.counters().fills_by_time == 2).await;
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeline_that_failed_should_stop_until_it_is_asked_for_again() {
        let wallet = address("1");
        let source = venue();
        source.push_timeline(Err(AccountError::Protocol {
            operation: "timeline",
        }));
        let mut runtime = runtime(source.clone());
        let mut reliable = runtime.take_reliable_stream().expect("a stream");
        let followed = wallets(std::slice::from_ref(&wallet));
        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: followed.clone(),
            })
            .await
            .expect("the load should be answered");
        let key = timeline_key(&wallet);
        wait_for_status(
            &mut reliable,
            &key,
            &WalletStatus::Failed(WalletStreamError::Account(AccountError::Protocol {
                operation: "timeline",
            })),
        )
        .await;

        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
        assert_eq!(source.counters().fills_by_time, 1);

        source.push_timeline(Ok(Vec::new()));
        runtime
            .client()
            .call(WalletRequest::RetryTimeline {
                address: wallet.clone(),
            })
            .await
            .expect("the retry should be answered");
        until(|| source.counters().fills_by_time == 2).await;
        wait_for_status(&mut reliable, &key, &WalletStatus::Ready).await;
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeline_answer_above_the_api_limit_should_fail_the_stream() {
        let wallet = address("1");
        let source = venue();
        let listing = listing();
        source.push_timeline(Ok((0..=MAX_TIMELINE_FILLS)
            .map(|index| {
                fill(
                    &wallet,
                    &listing,
                    Side::Buy,
                    "1",
                    "100",
                    i64::try_from(index).expect("an index should fit"),
                    index as u64,
                )
            })
            .collect::<Vec<_>>()));
        let mut runtime = runtime(source.clone());
        let mut reliable = runtime.take_reliable_stream().expect("a stream");
        let followed = wallets(std::slice::from_ref(&wallet));
        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: followed.clone(),
            })
            .await
            .expect("the load should be answered");

        wait_for_status(
            &mut reliable,
            &timeline_key(&wallet),
            &WalletStatus::Failed(WalletStreamError::Data(WalletDataError::Codec {
                operation: "validate Hyperliquid timeline fill count",
            })),
        )
        .await;
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_overview_should_publish_the_baseline_and_follow_the_stream() {
        let wallet = address("1");
        let listing = listing();
        let source = venue();
        source
            .spot(spot_state(vec![wallet_balance("USDC", "10")]))
            .pairs(vec![spot_pair("HYPE", "USDC")])
            .perpetual(perpetual_state(vec![position(&listing, "2", Some("100"))]))
            .fills(vec![fill(&wallet, &listing, Side::Buy, "1", "100", 1, 1)]);
        let runtime = runtime(source.clone());
        let mut latest = runtime.latest_stream();
        let followed = wallets(std::slice::from_ref(&wallet));
        let key = overview_key(&wallet);
        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: followed.clone(),
            })
            .await
            .expect("the load should be answered");

        let reading = latest_reading(&mut latest, &key).await;
        let snapshot = overview_of(reading.as_ref());
        assert_eq!(
            snapshot
                .summary
                .spot_value
                .expect("a priced balance should value")
                .to_string(),
            "10"
        );

        source
            .publish(
                SubscriptionKind::FastContexts,
                HyperliquidWalletUpdate::FastAssetContexts {
                    contexts: vec![context(&listing, Some("110"), None)],
                },
            )
            .await;
        let reading = latest_reading(&mut latest, &key).await;
        let snapshot = overview_of(reading.as_ref());
        assert_eq!(
            snapshot
                .summary
                .perpetual_value
                .expect("a priced position should value")
                .to_string(),
            "220"
        );
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_replayed_fill_snapshot_should_read_the_totals_again() {
        let wallet = address("1");
        let source = venue();
        let runtime = runtime(source.clone());
        let followed = wallets(std::slice::from_ref(&wallet));
        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: followed.clone(),
            })
            .await
            .expect("the load should be answered");
        until(|| source.counters().spot_states == 1).await;

        source
            .publish(
                SubscriptionKind::Fills,
                HyperliquidWalletUpdate::UserFills {
                    is_snapshot: true,
                    fills: Vec::new(),
                },
            )
            .await;
        tokio::task::yield_now().await;
        assert_eq!(source.counters().spot_states, 1);

        source
            .publish(
                SubscriptionKind::Fills,
                HyperliquidWalletUpdate::UserFills {
                    is_snapshot: true,
                    fills: Vec::new(),
                },
            )
            .await;
        until(|| source.counters().spot_states == 2).await;
        assert_eq!(source.counters().perpetual_states, 2);
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_event_the_baseline_already_held_should_not_be_applied_twice() {
        let wallet = address("1");
        let listing = listing();
        let address = wallet.clone();
        let source = venue();
        source
            .fills(vec![fill(&address, &listing, Side::Buy, "1", "100", 7, 7)])
            .perpetual(perpetual_state(vec![position(&listing, "2", Some("100"))]));
        let runtime = runtime(source.clone());
        let mut latest = runtime.latest_stream();
        let followed = wallets(std::slice::from_ref(&wallet));
        let key = overview_key(&wallet);
        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: followed.clone(),
            })
            .await
            .expect("the load should be answered");
        let _ = latest_reading(&mut latest, &key).await;
        source
            .publish(
                SubscriptionKind::FastContexts,
                HyperliquidWalletUpdate::FastAssetContexts {
                    contexts: vec![context(&listing, Some("110"), None)],
                },
            )
            .await;
        let reading = latest_reading(&mut latest, &key).await;
        assert_eq!(
            overview_of(reading.as_ref())
                .summary
                .unrealized_pnl
                .expect("a priced position should have a profit")
                .to_string(),
            "20"
        );

        source
            .publish(
                SubscriptionKind::Fills,
                HyperliquidWalletUpdate::UserFills {
                    is_snapshot: false,
                    fills: vec![fill(&address, &listing, Side::Buy, "1", "100", 7, 7)],
                },
            )
            .await;
        let reading = latest_reading(&mut latest, &key).await;

        assert_eq!(
            overview_of(reading.as_ref())
                .summary
                .unrealized_pnl
                .expect("a priced position should have a profit")
                .to_string(),
            "20"
        );
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_snapshot_event_should_not_move_the_totals() {
        let wallet = address("1");
        let listing = listing();
        let address = wallet.clone();
        let source = venue();
        source.perpetual(perpetual_state(vec![position(&listing, "2", Some("100"))]));
        let runtime = runtime(source.clone());
        let mut latest = runtime.latest_stream();
        let followed = wallets(std::slice::from_ref(&wallet));
        let key = overview_key(&wallet);
        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: followed.clone(),
            })
            .await
            .expect("the load should be answered");
        let _ = latest_reading(&mut latest, &key).await;
        source
            .publish(
                SubscriptionKind::FastContexts,
                HyperliquidWalletUpdate::FastAssetContexts {
                    contexts: vec![context(&listing, Some("100"), None)],
                },
            )
            .await;
        let reading = latest_reading(&mut latest, &key).await;
        assert_eq!(
            overview_of(reading.as_ref())
                .summary
                .perpetual_value
                .expect("a priced position should value")
                .to_string(),
            "200"
        );

        source
            .publish(
                SubscriptionKind::Fills,
                HyperliquidWalletUpdate::UserFills {
                    is_snapshot: true,
                    fills: vec![fill(&address, &listing, Side::Buy, "5", "100", 9, 9)],
                },
            )
            .await;
        let reading = latest_reading(&mut latest, &key).await;

        assert_eq!(
            overview_of(reading.as_ref())
                .summary
                .perpetual_value
                .expect("a priced position should value")
                .to_string(),
            "200"
        );
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_deposit_should_move_the_spot_balance() {
        let wallet = address("1");
        let address = wallet.clone();
        let source = venue();
        source
            .spot(spot_state(vec![wallet_balance("USDC", "10")]))
            .pairs(vec![spot_pair("HYPE", "USDC")]);
        let runtime = runtime(source.clone());
        let mut latest = runtime.latest_stream();
        let followed = wallets(std::slice::from_ref(&wallet));
        let key = overview_key(&wallet);
        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: followed.clone(),
            })
            .await
            .expect("the load should be answered");
        let _ = latest_reading(&mut latest, &key).await;

        source
            .publish(
                SubscriptionKind::Ledger,
                HyperliquidWalletUpdate::UserLedger {
                    is_snapshot: false,
                    events: vec![deposit(&address, "5", 11)],
                },
            )
            .await;

        let reading = latest_reading(&mut latest, &key).await;
        let snapshot = overview_of(reading.as_ref());
        assert_eq!(
            snapshot
                .summary
                .spot_value
                .expect("a priced balance should value")
                .to_string(),
            "15"
        );
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_detail_tab_should_be_read_once_and_again_only_when_asked() {
        let wallet = address("1");
        let listing = listing();
        let source = venue();
        source.open_orders(vec![open_order(&listing, "1", 1)]);
        let mut runtime = runtime(source.clone());
        let mut reliable = runtime.take_reliable_stream().expect("a stream");
        let followed = wallets(std::slice::from_ref(&wallet));
        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: followed.clone(),
            })
            .await
            .expect("the load should be answered");
        let key = detail_key(&wallet, DetailTab::OpenOrders);
        runtime
            .client()
            .call(WalletRequest::LoadDetail {
                address: wallet.clone(),
                tab: DetailTab::OpenOrders,
            })
            .await
            .expect("the detail should be answered");
        wait_for_status(&mut reliable, &key, &WalletStatus::Ready).await;
        assert_eq!(source.counters().open_orders, 1);

        runtime
            .client()
            .call(WalletRequest::LoadDetail {
                address: wallet.clone(),
                tab: DetailTab::OpenOrders,
            })
            .await
            .expect("the second detail should be answered");
        tokio::task::yield_now().await;
        assert_eq!(source.counters().open_orders, 1);

        runtime
            .client()
            .call(WalletRequest::RetryDetail {
                address: wallet.clone(),
                tab: DetailTab::OpenOrders,
            })
            .await
            .expect("the retry should be answered");
        until(|| source.counters().open_orders == 2).await;
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_detail_tab_with_a_live_value_should_follow_the_fast_contexts() {
        let wallet = address("1");
        let source = venue();
        source
            .spot(spot_state(vec![wallet_balance("USDC", "10")]))
            .pairs(vec![spot_pair("HYPE", "USDC")]);
        let runtime = runtime(source.clone());
        let mut latest = runtime.latest_stream();
        let followed = wallets(std::slice::from_ref(&wallet));
        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: followed.clone(),
            })
            .await
            .expect("the load should be answered");
        let key = detail_key(&wallet, DetailTab::Spot);
        runtime
            .client()
            .call(WalletRequest::LoadDetail {
                address: wallet.clone(),
                tab: DetailTab::Spot,
            })
            .await
            .expect("the detail should be answered");
        let reading = latest_reading(&mut latest, &key).await;
        let snapshot = detail_of(reading.as_ref());
        assert_eq!(spot_rows_of(snapshot), 1);

        source
            .publish(
                SubscriptionKind::FastContexts,
                HyperliquidWalletUpdate::FastAssetContexts {
                    contexts: vec![context(&pair_listing("HYPE", "USDC"), Some("2"), None)],
                },
            )
            .await;
        let reading = latest_reading(&mut latest, &key).await;
        let snapshot = detail_of(reading.as_ref());
        assert_eq!(
            spot_rows_of(snapshot),
            1,
            "revaluing a balance is not a row of its own"
        );
        runtime.shutdown().await;
    }

    fn spot_rows_of(snapshot: &DetailSnapshot) -> usize {
        let DetailData::Spot(spot) = &snapshot.data else {
            panic!("a spot tab reading should be the spot tab");
        };
        spot.rows.len()
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_should_report_a_subscription_that_closed() {
        let wallet = address("1");
        let source = venue();
        let mut runtime = runtime(source.clone());
        let mut reliable = runtime.take_reliable_stream().expect("a stream");
        let followed = wallets(std::slice::from_ref(&wallet));
        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: followed.clone(),
            })
            .await
            .expect("the load should be answered");
        let key = overview_key(&wallet);
        wait_for_status(&mut reliable, &key, &WalletStatus::Ready).await;

        source
            .publish(
                SubscriptionKind::FastContexts,
                HyperliquidWalletUpdate::FastAssetContexts {
                    contexts: Vec::new(),
                },
            )
            .await;
        source.fail_subscription(SubscriptionKind::Ledger).await;

        wait_for_status(
            &mut reliable,
            &key,
            &WalletStatus::Failed(WalletStreamError::Account(AccountError::Protocol {
                operation: "test",
            })),
        )
        .await;
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_caller_that_gave_up_should_leave_no_stream_behind() {
        let wallet = address("1");
        let source = venue();
        let runtime = runtime(source.clone());
        let followed = wallets(std::slice::from_ref(&wallet));
        let client = runtime.client();
        let call = client.call(WalletRequest::Load {
            addresses: followed.clone(),
        });
        drop(call);

        until(|| source.subscription_drops() == source.counters().streams()).await;
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_ledger_of_a_tab_should_be_read_with_a_window() {
        let wallet = address("1");
        let source = venue();
        let runtime = runtime(source.clone());
        let followed = wallets(std::slice::from_ref(&wallet));
        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: followed.clone(),
            })
            .await
            .expect("the load should be answered");
        runtime
            .client()
            .call(WalletRequest::LoadDetail {
                address: wallet.clone(),
                tab: DetailTab::Ledger,
            })
            .await
            .expect("the detail should be answered");
        until(|| source.counters().ledger >= 2).await;
        runtime.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_detail_of_a_wallet_the_scope_no_longer_holds_should_not_be_read() {
        let wallet = address("1");
        let source = venue();
        let runtime = runtime(source.clone());
        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: wallets(&[address("2")]),
            })
            .await
            .expect("the load should be answered");
        until(|| source.counters().spot_states == 1).await;

        runtime
            .client()
            .call(WalletRequest::LoadDetail {
                address: wallet.clone(),
                tab: DetailTab::OpenOrders,
            })
            .await
            .expect("the request should be answered");
        tokio::task::yield_now().await;

        assert_eq!(
            source.counters().open_orders,
            0,
            "a wallet outside the scope is not read"
        );
        runtime.shutdown().await;
    }

    #[test]
    fn an_address_list_should_hold_exactly_the_addresses_it_names() {
        let first = address("1");
        let second = address("2");

        assert_eq!(
            wallets(std::slice::from_ref(&first)),
            wallets(std::slice::from_ref(&first))
        );
        assert_ne!(
            wallets(std::slice::from_ref(&first)),
            wallets(std::slice::from_ref(&second))
        );
        assert!(wallets(std::slice::from_ref(&first)).contains(&first));
        assert!(!wallets(std::slice::from_ref(&first)).contains(&second));
    }

    #[test]
    fn applied_increment_ids_should_evict_the_oldest_at_capacity() {
        let address = address("1");
        let listing = listing();
        let mut ids = AppliedIncrementIds::default();
        for index in 0..MAX_APPLIED_INCREMENT_IDS {
            let mut fill = fill(&address, &listing, Side::Buy, "1", "100", 1, 0);
            fill.tid = index as u64;
            assert!(ids.insert(AppliedIncrementId::from_fill(&fill)));
        }
        assert_eq!(ids.len(), MAX_APPLIED_INCREMENT_IDS);

        let mut newest = fill(&address, &listing, Side::Buy, "1", "100", 1, 0);
        newest.tid = (MAX_APPLIED_INCREMENT_IDS - 1) as u64;
        assert!(!ids.insert(AppliedIncrementId::from_fill(&newest)));

        let mut overflow = fill(&address, &listing, Side::Buy, "1", "100", 1, 0);
        overflow.tid = MAX_APPLIED_INCREMENT_IDS as u64;
        assert!(ids.insert(AppliedIncrementId::from_fill(&overflow)));
        assert_eq!(ids.len(), MAX_APPLIED_INCREMENT_IDS);

        let mut oldest = fill(&address, &listing, Side::Buy, "1", "100", 1, 0);
        oldest.tid = 0;
        assert!(ids.insert(AppliedIncrementId::from_fill(&oldest)));
        assert!(!ids.insert(AppliedIncrementId::from_fill(&oldest)));
    }

    #[test]
    fn a_rebase_buffer_should_keep_only_the_newest_contexts() {
        let listing = listing();
        let mut buffer = RebaseUpdateBuffer::default();
        for index in 0..8 {
            buffer
                .push(HyperliquidWalletUpdate::FastAssetContexts {
                    contexts: vec![context(&listing, Some(index.to_string().as_str()), None)],
                })
                .expect("contexts should replace one another");
        }

        assert!(buffer.account_updates.is_empty());
        assert_eq!(buffer.account_event_count, 0);
        assert_eq!(
            buffer
                .latest_fast_contexts
                .as_ref()
                .expect("the newest contexts should be kept")
                .len(),
            1
        );
    }

    #[test]
    fn a_rebase_buffer_should_refuse_to_grow_without_a_bound() {
        let address = address("1");
        let listing = listing();
        let mut buffer = RebaseUpdateBuffer::default();
        for index in 0..MAX_REBASE_INCREMENT_EVENTS {
            let mut fill = fill(&address, &listing, Side::Buy, "1", "100", 1, 0);
            fill.tid = index as u64;
            buffer
                .push(HyperliquidWalletUpdate::UserFills {
                    is_snapshot: false,
                    fills: vec![fill],
                })
                .expect("updates within the bound should be held");
        }

        let error = buffer
            .push(HyperliquidWalletUpdate::UserFills {
                is_snapshot: false,
                fills: vec![fill(&address, &listing, Side::Buy, "1", "100", 1, 0)],
            })
            .expect_err("a slow rebase should not grow without a bound");

        assert_eq!(
            error,
            WalletStreamError::SubscriptionBufferFull {
                operation: "rebase Hyperliquid wallet updates",
                capacity: MAX_REBASE_INCREMENT_EVENTS,
            }
        );
    }

    #[test]
    fn a_rebase_buffer_should_ask_for_another_rebase_for_a_replayed_snapshot() {
        let mut buffer = RebaseUpdateBuffer::default();

        buffer
            .push(HyperliquidWalletUpdate::UserFills {
                is_snapshot: true,
                fills: Vec::new(),
            })
            .expect("an empty replayed snapshot should be accepted");

        assert!(buffer.rebase_requested);
        assert!(buffer.account_updates.is_empty());
    }

    #[test]
    fn a_perpetual_fill_should_average_an_addition_and_keep_the_entry_of_a_reduction() {
        let address = address("1");
        let listing = listing();
        let mut state = perpetual_state(vec![position(&listing, "2", Some("100"))]);

        let mut addition = fill(&address, &listing, Side::Buy, "1", "130", 1, 1);
        addition.start_position = "2".parse().expect("a position should be valid");
        apply_perpetual_fill(&mut state, &addition).expect("an addition should apply");
        assert_eq!(
            (
                state.positions[0].size.value.to_string(),
                state.positions[0]
                    .entry_price
                    .as_ref()
                    .expect("an addition keeps an entry")
                    .value
                    .to_string(),
            ),
            ("3".to_owned(), "110".to_owned())
        );

        let mut reduction = fill(&address, &listing, Side::Sell, "1", "90", 1, 2);
        reduction.start_position = "3".parse().expect("a position should be valid");
        apply_perpetual_fill(&mut state, &reduction).expect("a reduction should apply");
        assert_eq!(
            (
                state.positions[0].size.value.to_string(),
                state.positions[0]
                    .entry_price
                    .as_ref()
                    .expect("a reduction keeps the entry")
                    .value
                    .to_string(),
            ),
            ("2".to_owned(), "110".to_owned())
        );
    }

    #[test]
    fn a_perpetual_fill_should_close_a_position_and_reset_a_flipped_one() {
        let address = address("1");
        let listing = listing();
        let mut closed = perpetual_state(vec![position(&listing, "2", Some("100"))]);
        let mut closing = fill(&address, &listing, Side::Sell, "2", "90", 1, 1);
        closing.start_position = "2".parse().expect("a position should be valid");
        apply_perpetual_fill(&mut closed, &closing).expect("a close should apply");
        assert!(closed.positions.is_empty());

        let mut flipped = perpetual_state(vec![position(&listing, "2", Some("100"))]);
        let mut flipping = fill(&address, &listing, Side::Sell, "3", "90", 1, 2);
        flipping.start_position = "2".parse().expect("a position should be valid");
        apply_perpetual_fill(&mut flipped, &flipping).expect("a flip should apply");
        assert_eq!(
            (
                flipped.positions[0].size.value.to_string(),
                flipped.positions[0].direction,
                flipped.positions[0]
                    .entry_price
                    .as_ref()
                    .expect("a flip opens at the fill's price")
                    .value
                    .to_string(),
            ),
            ("1".to_owned(), PositionDirection::Short, "90".to_owned()),
            "selling past a long leaves a short of the other side's size"
        );
    }

    #[test]
    fn a_ledger_event_that_is_not_a_transfer_should_not_move_a_balance() {
        let address = address("1");
        let mut state = spot_state(vec![wallet_balance("USDC", "10")]);
        let mut event = deposit(&address, "5", 1);
        event.kind = SmolStr::new_static("rewardsClaim");

        apply_spot_ledger(&mut state, &event).expect("a claim should be a no-op");

        assert_eq!(state.balances[0].total.to_string(), "10");
    }

    #[test]
    fn a_transfer_between_wallets_should_move_the_spot_balance() {
        let sender = address("1");
        let other = address("2");
        let mut state = spot_state(vec![wallet_balance("USDC", "10")]);

        for (kind, direction, expected) in [
            (
                "internalTransfer",
                market_participants_connector::account::domain::HyperliquidTransferDirection::Outgoing,
                "7",
            ),
            (
                "subAccountTransfer",
                market_participants_connector::account::domain::HyperliquidTransferDirection::Incoming,
                "10",
            ),
            (
                "accountClassTransfer",
                market_participants_connector::account::domain::HyperliquidTransferDirection::Incoming,
                "13",
            ),
            (
                "spotTransfer",
                market_participants_connector::account::domain::HyperliquidTransferDirection::Outgoing,
                "13",
            ),
        ] {
            let mut event = deposit(&sender, "3", 1);
            event.kind = SmolStr::new_static(kind);
            event.transfer_direction = Some(direction);
            event.related_wallet = Some(other.clone());

            apply_spot_ledger(&mut state, &event).expect("the transfer should apply");

            assert_eq!(
                state.balances[0].total.to_string(),
                expected,
                "{kind} should be accounted for like the move it is"
            );
        }
    }

    #[test]
    fn a_transfer_whose_direction_is_unknown_should_not_move_a_balance() {
        let address = address("1");
        let mut state = spot_state(vec![wallet_balance("USDC", "10")]);
        let mut event = deposit(&address, "3", 1);
        event.kind = SmolStr::new_static("subAccountTransfer");
        event.transfer_direction = None;

        apply_spot_ledger(&mut state, &event).expect("an undirected transfer should be a no-op");

        assert_eq!(state.balances[0].total.to_string(), "10");
    }

    #[test]
    fn a_liquidated_fill_should_not_move_a_position() {
        let address = address("1");
        let listing = listing();
        let mut state = perpetual_state(vec![position(&listing, "2", Some("100"))]);
        let mut liquidation = fill(&address, &listing, Side::Sell, "2", "90", 1, 1);
        liquidation.liquidation = true;
        liquidation.start_position = "2".parse().expect("a position should be valid");

        apply_perpetual_fill(&mut state, &liquidation).expect("a liquidation should be a no-op");

        assert_eq!(state.positions[0].size.value.to_string(), "2");
    }

    #[test]
    fn a_ledger_event_whose_amount_names_another_asset_should_be_refused() {
        let address = address("1");
        let mut state = spot_state(vec![wallet_balance("USDC", "10")]);
        let mut event = deposit(&address, "5", 1);
        event.amount = Some(crate::test_support::amount_of("HYPE", "5"));

        let error = apply_spot_ledger(&mut state, &event)
            .expect_err("a mismatched amount should be refused");

        assert!(matches!(
            error,
            WalletStreamError::Data(WalletDataError::Codec { .. })
        ));
        assert_eq!(state.balances[0].total.to_string(), "10");
    }

    #[tokio::test(start_paused = true)]
    async fn a_status_should_not_be_repeated_for_every_reading() {
        let wallet = address("1");
        let listing = listing();
        let source = venue();
        let mut runtime = runtime(source.clone());
        let mut reliable = runtime.take_reliable_stream().expect("a stream");
        let mut latest = runtime.latest_stream();
        let followed = wallets(std::slice::from_ref(&wallet));
        let key = overview_key(&wallet);
        runtime
            .client()
            .call(WalletRequest::Load {
                addresses: followed.clone(),
            })
            .await
            .expect("the load should be answered");
        wait_for_status(&mut reliable, &key, &WalletStatus::Ready).await;

        for _ in 0..3 {
            source
                .publish(
                    SubscriptionKind::FastContexts,
                    HyperliquidWalletUpdate::FastAssetContexts {
                        contexts: vec![context(&listing, Some("100"), None)],
                    },
                )
                .await;
            let reading = latest_reading(&mut latest, &key).await;
            assert!(matches!(reading.as_ref(), WalletStreamMessage::Overview(_)));
            assert!(!matches!(
                reading.as_ref(),
                WalletStreamMessage::Status(WalletStatus::Ready)
            ));
        }
        runtime.shutdown().await;
    }
}

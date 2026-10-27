//! What a Hyperliquid wallet asks the venue for, and how it keeps the answer current.
//!
//! # What one stream is about
//!
//! A stream is about one wallet address and nothing else. The pane follows addresses, and which of
//! the venue's partitions an address holds data in is the venue's answer rather than a choice the
//! user made, so no route appears in a request, in a stream key or in the configuration — and an
//! address is drawn once however many partitions it trades on.
//!
//! # How one reading covers every partition
//!
//! The venue answers about a wallet in two shapes. Some requests name a partition — the account
//! state and the open orders carry a DEX parameter, so each perpetual partition is read once — and
//! some name only the user (fills, ledger, historical orders and the fast asset contexts), so they
//! are read once and decoded against the union of every partition's catalog. An address therefore
//! reads its partitions concurrently and merges them by listing, which is unique to the partition
//! that published it. [`WalletRouteKinds`] decides which partition is read which way, from the
//! catalogs rather than from the routes' names.
//!
//! # Three streams, because a wallet is read three ways
//!
//! A wallet's overview is a live total, its timeline is a window of fills that is re-read every
//! ten seconds, and a detail tab is a reading taken when it is opened. They fail independently
//! — a rate-limited timeline request says nothing about the totals beside it — so they are
//! three streams under three keys rather than one, and the pane decides which of them it is
//! showing.
//!
//! # Why the overview rebases
//!
//! The venue's account state arrives two ways: as a total the venue computed, and as the
//! events that moved it. Applying an event to a total is only correct while the total is the
//! one that event follows, and a reconnecting subscription replays a snapshot of the events it
//! has seen — which the HTTP totals may already include. So a replayed snapshot is what makes
//! the totals stale, and it is answered by reading them again while the events that arrive
//! during that read are held in order and replayed onto the fresh totals.
//!
//! # Why an event is applied once
//!
//! The replayed snapshot overlaps both the HTTP history and the live stream, so every event is
//! reduced to an identifier and applied only the first time it is seen. The identifiers are
//! kept in a bounded set: what matters is the overlaps around a rebase, not the history.
//!
//! # Where the money comes from
//!
//! The venue's own mark and mid prices arrive on a separate, route-level subscription, and a
//! total is only as true as the prices it is valued at. So the overview and the two valuation
//! tabs subscribe to it and revalue on every update, which is why a wallet's total moves
//! without a fill.

use std::{collections::VecDeque, convert::Infallible, future::Future, sync::Arc, time::Duration};

use ahash::AHashSet;
use futures::future::try_join_all;
use gpui_kit::{App, SharedString};
use i18n::t;
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
    error::LocalizedError,
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

/// How often a wallet's timeline is read again.
const TIMELINE_INTERVAL: Duration = Duration::from_secs(10);
/// How much ledger history one wallet is read with.
const LEDGER_WINDOW: i64 = 30 * 24 * 60 * 60 * 1_000;
/// How many events one rebase may hold while the fresh totals are being read.
const MAX_REBASE_INCREMENT_EVENTS: usize = 4_096;
/// How many applied events are remembered, which is what makes a replay apply once.
const MAX_APPLIED_INCREMENT_IDS: usize = 4_096;
/// The venue caps one `userFillsByTime` answer at this many fills, so a larger one is refused
/// rather than drawn as if it were the window it claims to be.
const MAX_TIMELINE_FILLS: usize = 2_000;

/// What one wallet reading could not be made from.
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
    /// One partition of one reading could not be made.
    ///
    /// An address's reading covers every partition the venue publishes, and a reading with one
    /// partition missing is not the number it claims to be. So the failure is attributed to the
    /// partition rather than flattened into a general one: the user can see which partition
    /// broke, and the retry control restarts that address's whole reading.
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

impl LocalizedError for WalletStreamError {
    fn localized_message(&self, cx: &App) -> SharedString {
        // The venue this widget reads is named by the catalogue rather than spelled
        // into each message, so a sentence about it follows a rename of it.
        let market = VenueId::Hyperliquid.display_string(cx);
        match self {
            Self::Account(error) => error.localized_message(cx),
            Self::Model(error) => error.localized_message(cx),
            Self::Data(error) => error.localized_message(cx),
            Self::SubscriptionBufferFull {
                operation,
                capacity,
            } => t!(
                cx,
                "widget.hyperliquid_wallet.error_subscription_buffer_full",
                market = market,
                operation = operation,
                capacity = capacity
            ),
            Self::DexRead { route, source } => t!(
                cx,
                "widget.hyperliquid_wallet.error_dex_read",
                market = market,
                dex = route_display_string(route, cx),
                cause = source.localized_message(cx)
            ),
            Self::SubscriptionClosed => t!(
                cx,
                "widget.hyperliquid_wallet.error_subscription_closed",
                market = market
            ),
            Self::StreamClosed => t!(
                cx,
                "widget.hyperliquid_wallet.error_stream_closed",
                market = market
            ),
        }
    }
}

/// What the pane asks its service to do.
#[derive(Clone, Debug)]
pub(super) enum WalletRequest {
    /// Follows these addresses, starting whatever is not already running.
    Load { addresses: Vec<WalletAddress> },
    /// Follows these addresses, reading every one of their totals again.
    ///
    /// The market this pane reads was loaded again: a wallet's totals are displayed with the
    /// precision its stream resolved from the catalogs when it was opened, so every one of those
    /// streams has to resolve again. The addresses are the whole set to follow, the way a load
    /// states it, because the request this stands in for may have been replaced before the
    /// service saw it.
    Reload { addresses: Vec<WalletAddress> },
    /// Starts the overview of the addresses that failed again.
    Retry { addresses: Vec<WalletAddress> },
    /// Reads one wallet's timeline again.
    RetryTimeline { address: WalletAddress },
    /// Reads one tab of one wallet.
    LoadDetail {
        address: WalletAddress,
        tab: DetailTab,
    },
    /// Reads one tab of one wallet again.
    RetryDetail {
        address: WalletAddress,
        tab: DetailTab,
    },
}

/// What the service answers a pane that asked for something.
#[derive(Clone, Debug)]
pub(super) enum WalletResponse {
    StreamsStarted,
}

/// Which reading a stream is.
///
/// A key names one address, which is the whole identity of a reading: a discovery that makes a
/// partition appear does not turn a reading about an address into another reading, it only changes
/// what that reading covers.
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

/// What one stream reports about itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WalletStatus {
    Connecting,
    Ready,
    Failed(WalletStreamError),
}

/// What one stream publishes.
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

/// Everything one pane reads a wallet through.
///
/// The seam is the venue itself rather than the questions a pane asks: what a detail tab is
/// made of is this service's business, and a fake only has to answer what the venue would.
pub(super) trait WalletSource: Clone + Send + Sync + 'static {
    type Subscription: WalletSubscription;

    /// The routes one reading of an address covers.
    ///
    /// They come from the registry rather than from the pane: which partitions this venue
    /// publishes is its own answer, and every route that monitors wallets and publishes a catalog
    /// is one of them. The set is read at the moment of the read, so a registry that moved between
    /// two readings is picked up by the next one.
    fn wallet_routes(&self) -> Vec<VenueRoute>;
    /// One wallet's spot balances.
    ///
    /// A balance is held in an asset rather than on a partition, so any of the venue's routes can
    /// be asked for it; the route given is the one whose catalog this reading is ensured against.
    fn fetch_spot_state(
        &self,
        route: &VenueRoute,
        address: &WalletAddress,
    ) -> impl Future<Output = Result<HyperliquidSpotState, AccountError>> + Send;
    /// One wallet's perpetual positions on one partition.
    fn fetch_perpetual_state(
        &self,
        route: &VenueRoute,
        address: &WalletAddress,
    ) -> impl Future<Output = Result<HyperliquidPerpetualState, AccountError>> + Send;
    /// The spot pairs one route publishes, which is route metadata rather than a
    /// wallet's own state.
    fn fetch_spot_pairs(
        &self,
        route: &VenueRoute,
    ) -> impl Future<Output = Result<Vec<HyperliquidSpotPair>, AccountError>> + Send;
    /// One wallet's fills, decoded across every route given.
    fn fetch_fills(
        &self,
        address: &WalletAddress,
        aggregate_by_time: bool,
        routes: &[VenueRoute],
    ) -> impl Future<Output = Result<Vec<HyperliquidUserFill>, AccountError>> + Send;
    /// One wallet's fills inside a bounded window, decoded across every route given.
    fn fetch_fills_by_time(
        &self,
        address: &WalletAddress,
        start_timestamp: i64,
        end_timestamp: i64,
        routes: &[VenueRoute],
    ) -> impl Future<Output = Result<Vec<HyperliquidUserFill>, AccountError>> + Send;
    /// One wallet's ledger from `start_time` on, decoded across every route given.
    fn fetch_ledger(
        &self,
        address: &WalletAddress,
        start_time: i64,
        routes: &[VenueRoute],
    ) -> impl Future<Output = Result<Vec<HyperliquidLedgerEvent>, AccountError>> + Send;
    /// One wallet's open orders on one partition.
    fn fetch_open_orders(
        &self,
        route: &VenueRoute,
        address: &WalletAddress,
    ) -> impl Future<Output = Result<Vec<HyperliquidOpenOrder>, AccountError>> + Send;
    /// One wallet's historical orders, decoded across every route given.
    fn fetch_historical_orders(
        &self,
        address: &WalletAddress,
        routes: &[VenueRoute],
    ) -> impl Future<Output = Result<Vec<HyperliquidHistoricalOrder>, AccountError>> + Send;
    /// One route's published catalog, which is what a reading is displayed with.
    ///
    /// A route publishes one catalog rather than one per market: the spot partition's carries its
    /// pairs, and every other partition's carries its perpetual listings.
    fn fetch_catalog(
        &self,
        route: &VenueRoute,
    ) -> impl Future<Output = Result<Arc<[CatalogEntry]>, AccountError>> + Send;
    /// The display generation this source reads its catalogs from.
    fn display_generation(&self) -> DisplaySnapshot;
    /// One wallet's fills stream, decoded across every route given.
    fn subscribe_fills(
        &self,
        address: &WalletAddress,
        routes: &[VenueRoute],
    ) -> impl Future<Output = Result<Self::Subscription, AccountError>> + Send;
    /// One wallet's ledger stream, decoded across every route given.
    fn subscribe_ledger(
        &self,
        address: &WalletAddress,
        routes: &[VenueRoute],
    ) -> impl Future<Output = Result<Self::Subscription, AccountError>> + Send;
    /// The venue's fast asset-context stream, decoded across every route given.
    ///
    /// It names no wallet: the venue broadcasts every partition's contexts on one subscription,
    /// and the set is what the answer is filtered to.
    fn subscribe_fast_contexts(
        &self,
        routes: &[VenueRoute],
    ) -> impl Future<Output = Result<Self::Subscription, AccountError>> + Send
    where
        Self::Subscription: Sized;
}

/// One wallet's live updates, as the venue reports them.
pub(super) trait WalletSubscription: Send + 'static {
    fn recv(
        &mut self,
    ) -> impl Future<Output = Option<Result<HyperliquidWalletUpdate, AccountError>>> + Send;
}

/// One loaded scope, as a wallet reading sees it.
///
/// The three facts are one reading of one commit: the route, whether it monitors
/// wallets and whether it publishes a catalog a reading can be displayed with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WalletScope {
    /// The route of this scope.
    pub route: VenueRoute,
    /// Whether this scope serves the wallet monitor.
    pub monitors_wallets: bool,
    /// Whether this scope publishes a catalog.
    pub provides_catalog: bool,
}

/// Reads every scope the loaded venues answer, as a wallet reading sees them.
///
/// The routes and the entries they are read from come from one reading of one commit —
/// [`LoadedVenues::entries`] takes the venue table's lock once — so a route a reading
/// covers and the capabilities it is filtered by are the same commit's.
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

/// The routes one wallet reading covers, among the loaded scopes.
///
/// This widget is Hyperliquid's: its readings are decoded against that venue's catalogs, and it
/// knows nothing about anybody else's accounts. So a route of another venue that also monitors a
/// wallet monitor is not one of them — reading it would fail every address with a partition error
/// the pane can do nothing about. A route counts when it belongs to this venue, monitors wallets,
/// and publishes a catalog a reading can be displayed with.
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

/// The venue, through the market participants connector.
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

/// One wallet's three live subscriptions, as one source of updates.
struct WalletSubscriptions<S: WalletSubscription> {
    fills: S,
    ledger: S,
    fast_contexts: S,
}

impl<S: WalletSubscription> WalletSubscriptions<S> {
    /// The next update from any of them, whichever arrives first.
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

/// What one pane reads its wallets through.
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
            // An address of a load starts two streams, its totals and its timeline; a detail
            // stream is started by the one request that opens its tab.
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
                        // The totals are what a reload is about: that reading is displayed with
                        // the precision its stream resolved when it was opened. A timeline reads
                        // the current catalogs on every tick of its own, so it is only started
                        // for an address that is not being read at all.
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
                    // A recovery restarts what failed and nothing else: the addresses that are
                    // still reading keep their streams, and the set being followed is whatever
                    // the last load settled on.
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

/// Whether the addresses one pane follows still hold this one.
fn is_followed(followed: &Option<Vec<WalletAddress>>, address: &WalletAddress) -> bool {
    followed
        .as_deref()
        .is_some_and(|current| current.contains(address))
}

/// Follows a new address set, stopping the streams of the addresses that left it.
///
/// A list that did not move leaves every stream running: a repeated load is not a reason to
/// re-read a wallet that is already being read.
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

/// The key of one wallet's totals.
fn overview_key(address: &WalletAddress) -> WalletStreamKey {
    WalletStreamKey::Overview {
        address: address.clone(),
    }
}

/// The key of one wallet's timeline.
fn timeline_key(address: &WalletAddress) -> WalletStreamKey {
    WalletStreamKey::Timeline {
        address: address.clone(),
    }
}

/// The key of one tab of one wallet.
fn detail_key(address: &WalletAddress, tab: DetailTab) -> WalletStreamKey {
    WalletStreamKey::Detail {
        address: address.clone(),
        tab,
    }
}

/// Stops following one wallet a pane no longer holds.
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

/// Publishes a status a pane reports.
async fn publish_status(
    publisher: &StreamPublisher<WalletProtocol>,
    status: WalletStatus,
) -> Result<(), StreamPublishError> {
    publisher
        .publish_reliable(WalletStreamMessage::Status(status))
        .await
}

/// Reports why a stream stopped, and waits for the pane to replace it.
///
/// A failure is terminal: nothing is retried here, and the stream parks until the pane asks for
/// it again.
async fn fail(publisher: &StreamPublisher<WalletProtocol>, error: WalletStreamError) {
    let _ = publish_status(publisher, WalletStatus::Failed(error)).await;
    publisher.cancelled().await;
}

/// One wallet's totals, kept current from the venue's live account updates.
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
    // One route set and one grouping for the whole stream: a reading is internally consistent,
    // and a registry that moves while a stream is running is picked up by the next reading
    // rather than by half of this one.
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
    // Both halves are awaited together: a subscription that opens before the totals were read
    // would deliver events the totals do not yet include, and the totals read before the
    // subscription opened would be missing whatever happened in between.
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

/// One address's totals, as every partition of the venue answered for it.
struct WalletBaseline {
    /// Which partitions are read which way, decided from the catalogs below.
    kinds: WalletRouteKinds,
    /// Every route's published catalog, which is what the reading is decoded and displayed with.
    catalogs: Vec<(VenueRoute, Arc<[CatalogEntry]>)>,
    spot: HyperliquidSpotState,
    perpetual: HyperliquidPerpetualState,
    pairs: Vec<HyperliquidSpotPair>,
    fills: Vec<HyperliquidUserFill>,
    ledger: Vec<HyperliquidLedgerEvent>,
}

/// Reads one address's totals, at one moment.
///
/// The catalogs come first because they are what decides which partition is read which way; the
/// reads that follow them run concurrently. A partition that fails fails the address: a reading
/// missing one partition is not the total it claims to be.
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

/// The route a spot balance is read through, when the venue published one.
///
/// A balance is held in an asset rather than on a partition, so any route answers for it; a route
/// that published spot listings is preferred because this read ensures its catalog first.
fn spot_route<'a>(kinds: &'a WalletRouteKinds, routes: &'a [VenueRoute]) -> Option<&'a VenueRoute> {
    kinds.spot().first().or_else(|| routes.first())
}

/// Every route's published catalog, one read per route.
///
/// This is the union a reading is displayed with: a listing's precision belongs to the partition
/// that published it, so all of them are needed at once (门禁 G1).
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

/// One wallet's spot balances.
///
/// A reading with no partition to ask is a failure rather than an empty reading: the registry
/// publishes the routes a wallet is read from, so an empty set means nothing could be read at all.
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

/// One wallet's positions across every perpetual partition, at one moment.
///
/// A listing belongs to exactly one partition, so the merge is a concatenation rather than a
/// choice between two answers about the same market.
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

/// One wallet's open orders across every perpetual partition.
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

/// The spot pairs every spot partition publishes.
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

/// One wallet's totals, and what has already been applied to them.
struct OverviewState {
    spot: HyperliquidSpotState,
    perpetual: HyperliquidPerpetualState,
    pairs: Vec<HyperliquidSpotPair>,
    contexts: Vec<HyperliquidAssetContext>,
    applied_increment_ids: AppliedIncrementIds,
    /// Whether a replayed fills snapshot has already been seen once.
    ///
    /// The first one is what a fresh subscription always sends, so only a second one means the
    /// subscription was replaced under totals that had moved on.
    fills_snapshot_received: bool,
}

impl OverviewState {
    /// What this wallet is worth right now.
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

/// What has already been applied to a wallet's totals.
///
/// It is bounded in both directions: an identifier is remembered once, and the oldest is
/// forgotten at capacity, which is what keeps a long-lived pane from growing with its history.
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

    /// Remembers an identifier, and says whether this is the first time it was seen.
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

/// What makes one event the same event as another.
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

/// Reads the totals again, and replays what arrived while it was reading.
///
/// The contexts are cleared first: a total valued at a price from before the rebase would be
/// stale in a way nothing later corrects.
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

/// The events that arrived while the totals were being read again.
#[derive(Default)]
struct RebaseUpdateBuffer {
    account_updates: VecDeque<HyperliquidWalletUpdate>,
    account_event_count: usize,
    /// The contexts are a cache rather than a log, so only the newest one is worth replaying.
    latest_fast_contexts: Option<Vec<HyperliquidAssetContext>>,
    /// Whether another replay arrived while this rebase was running.
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

/// Applies one live update to a wallet's totals.
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

/// Moves one perpetual position by a fill.
///
/// A fill says where the position started and which way it went, so the size after it is the
/// start plus the fill and the entry price is only what the fill changed: an addition averages
/// the two entries, a reduction leaves the entry alone, and a flip is a new position at the
/// fill's price.
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
    // The size is a magnitude, so where the fill left the position is what its direction says:
    // a fill that crosses zero leaves a position on the other side, not the same one.
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

/// Moves one spot balance by a transfer.
///
/// Five ledger deltas move the USDC a wallet holds on the spot side: a deposit and a withdrawal
/// cross the venue's boundary, an internal transfer or a sub-account transfer moves it between
/// wallets, and an account class transfer moves it between the spot and perpetual balances of
/// one wallet. Every other kind the venue reports — a token transfer, a vault, a staking reward
/// — is a record of something this does not account for here, and the next reading of the spot
/// state is what settles it.
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

/// One wallet's fills over the window a timeline shows, re-read on a fixed cadence.
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
        // The window is re-read against the routes the registry publishes right now, so a
        // partition that appeared since the last tick is covered by the next one.
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
        // The cadence is measured from when the request went out, so a slow answer does not
        // push the next one later and later.
        next_tick = request_started + TIMELINE_INTERVAL;
        let now = Instant::now();
        while next_tick <= now {
            next_tick += TIMELINE_INTERVAL;
        }
    }
}

/// One tab of one wallet, revalued while it is open if it holds a live value.
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

/// Reads one tab of one wallet, as the venue answers it.
///
/// Every tab is displayed against the union of every route's catalog: a reading is one shape of one
/// account, and the account holds its markets on whichever partition listed them.
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
            // A fill can be a spot fill, so the union carries the spot partitions' listings as
            // well: the reading is refused rather than drawn without the precision it needs.
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
            // The ledger's own read is what ensures every route's catalog here: its amounts are
            // money, and the rows it carries are resolved against that union by the connector.
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

/// The precision every route's catalog describes, with the grouping derived from them.
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

/// The start of the ledger history one wallet is read with.
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

    use gpui_kit::TestAppContext;
    use market_participants_connector::account::domain::HyperliquidWalletUpdate;
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId, WalletAddress},
        listing::route::VenueListingKey,
        market::units::Side,
    };

    use super::*;
    use crate::test_support::{
        SubscriptionKind, TestSource, address, context, deposit, fill, latest_reading, open_order,
        overview_key, pair_listing, perp_entry, perpetual_route, perpetual_state, position,
        runtime, spot_entry, spot_pair, spot_state, test_routes, timeline_key, until,
        wait_for_status, wallet_balance,
    };

    /// The address list one test drives the service with.
    fn wallets(addresses: &[WalletAddress]) -> Vec<WalletAddress> {
        addresses.to_vec()
    }

    /// A venue whose routes publish the test listings.
    fn venue() -> TestSource {
        TestSource::new(&test_routes())
    }

    /// The perpetual listing every test reading is about.
    fn listing() -> VenueListingKey {
        crate::test_support::listing_key(&perpetual_route(), "ETH")
    }

    /// The overview reading one message carries.
    fn overview_of(message: &WalletStreamMessage) -> &Arc<WalletOverviewSnapshot> {
        match message {
            WalletStreamMessage::Overview(snapshot) => snapshot,
            other => panic!("an overview reading should be published, not {other:?}"),
        }
    }

    /// The detail reading one message carries.
    fn detail_of(message: &WalletStreamMessage) -> &Arc<DetailSnapshot> {
        match message {
            WalletStreamMessage::Detail(snapshot) => snapshot,
            other => panic!("a detail reading should be published, not {other:?}"),
        }
    }

    /// The routes a reading covers are this venue's wallet routes and nothing else.
    ///
    /// The scopes are built here rather than taken from the loaded venues so that the rule has
    /// something to refuse: another venue that monitors wallets and publishes a catalog
    /// passes every other test the filter makes, and it is still not a route this widget may read.
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
        // A wallet route of this venue that publishes no catalog: a reading displayed without one
        // would draw numbers it cannot stand behind, so it is not read either.
        let no_catalog = wallet(crate::test_support::spot_route(), false);
        // A scope of this venue that does not monitor wallets.
        let control = WalletScope {
            route: crate::test_support::route("discovery"),
            monitors_wallets: false,
            provides_catalog: false,
        };

        // The fixtures must differ in the venue and nothing else, or the case would be decided by
        // another clause of the filter instead of the one it is about.
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

    /// The routes a reading covers come from the loaded venues, and those are the venue's own
    /// answer: every Hyperliquid scope that monitors wallets and publishes a catalog is one of
    /// them, and a scope that carries neither — the discovery scope the venue used to publish —
    /// is not.
    #[gpui_kit::test]
    fn client_wallet_routes_should_name_every_hyperliquid_wallet_route(cx: &mut TestAppContext) {
        crate::test_support::install_metadata(cx);
        cx.update(|cx| {
            crate::test_support::install_connector(cx, &crate::test_support::test_routes());
        });
        let source =
            cx.update(|cx| ClientWalletSource::new(cx.global::<MarketParticipantsConnector>()));

        let routes = source.wallet_routes();

        assert!(!routes.is_empty(), "the venue publishes wallet routes");
        assert!(
            routes
                .iter()
                .all(|route| route.venue.as_str() == crate::test_support::VENUE),
            "the capability is Hyperliquid's: {routes:?}"
        );
        assert!(
            routes
                .iter()
                .any(|route| route.scope.as_str() == crate::test_support::SPOT_SCOPE)
        );
        assert!(
            routes
                .iter()
                .any(|route| route.scope.as_str() == crate::test_support::PERPETUAL_SCOPE)
        );
        assert!(
            !routes
                .iter()
                .any(|route| route.scope.as_str() == "discovery"),
            "a scope's route reads no wallet: {routes:?}"
        );
    }

    /// A catalog that cannot be read fails the address it was read for, and names the partition:
    /// a reading displayed without the catalog of one of its partitions would draw a number it
    /// cannot stand behind.
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

    /// One base asset quoted against two assets is one asset with two markets, and both of them
    /// are indexed: the reading is drawn, not failed.
    ///
    /// This is the shape the live venue publishes — HYPE trades against USDC, USDE, USDH and
    /// USDT0, and a dozen other tokens quote against two assets — so a catalog with one pair per
    /// base is the fixture that hid it.
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

    /// A public watch never reads the route's configured account, so the wallets it follows are
    /// exactly the addresses it names.
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

    /// A partition that cannot answer fails the address it was read for, and names itself.
    ///
    /// An address is read from every partition the venue publishes, so a reading with one of them
    /// missing is not the account it claims to be: it fails rather than being drawn from whatever
    /// the other partitions answered.
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

        // The three streams of the wallet that left are gone, and three new ones are open.
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
        // The wallet that failed is read once more, and the wallet beside it is not read at all:
        // the venue was asked for three more streams, which is one wallet's worth.
        until(|| source.counters().subscriptions() == 6).await;

        assert_eq!(source.counters().fills_subscriptions, 3);
        assert_eq!(source.counters().ledger_subscriptions, 3);
        // The two streams the wallet already had were replaced, and the other wallet's were not
        // touched at all.
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
        // Every wallet the reload names has its totals read again — whether or not this service
        // was already following it — and the one it leaves out is not read at all.
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
        // A wallet that is already being read keeps its timeline: that reading fetches the
        // current catalogs on every tick, so the reload only starts the one the new wallet needs.
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

        // The venue's own mark price arrives on the route-level fast-context subscription.
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

        // The first snapshot is what a fresh subscription always sends.
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

        // A second one means the subscription was replaced under totals that had moved on.
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

        // The venue replays the same fill as a live update, which the totals already include.
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

        // A replayed snapshot describes the totals the venue already reported, so it moves
        // nothing: only the position it names would change, and it is the same position.
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

    /// How many balance rows one spot reading of a detail tab holds.
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

    /// A wallet the scope does not hold is not read: the pane is the authority on what it
    /// follows, and a request for anything else is not this service's work.
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

    /// An address list is read as the list it is: an address the pane removed is one the service
    /// no longer follows, and one it still holds is read whatever the order says.
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

        // The oldest is what makes room for the one past capacity…
        let mut overflow = fill(&address, &listing, Side::Buy, "1", "100", 1, 0);
        overflow.tid = MAX_APPLIED_INCREMENT_IDS as u64;
        assert!(ids.insert(AppliedIncrementId::from_fill(&overflow)));
        assert_eq!(ids.len(), MAX_APPLIED_INCREMENT_IDS);

        // …which is why it is seen as a first observation again, and only once.
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
        // A master account reading what its sub-accounts did is not told which way it went.
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

    /// A ledger event that names another asset than the one it moves is not a balance movement
    /// this can apply.
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
            // A reading that moved a total is published, and no second status follows it: the
            // ready state was reported once, when the stream became live.
            assert!(!matches!(
                reading.as_ref(),
                WalletStreamMessage::Status(WalletStatus::Ready)
            ));
        }
        runtime.shutdown().await;
    }
}

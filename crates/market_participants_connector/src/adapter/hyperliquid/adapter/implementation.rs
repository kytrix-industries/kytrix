//! The Hyperliquid adapter: transport, catalog, account decoding and streams.
//!
//! One adapter serves every Hyperliquid scope. Which endpoint, credential and wire
//! identifier a call uses is decided by the route the request was validated
//! against — never by the venue name — so the core spot scope, the core perpetual
//! scope and a builder-deployed perpetual DEX share this code without ever being
//! folded into one another. Origins come from the execution context, and every
//! listing identity comes from the typed identifiers the gateway resolved.

pub(super) use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(super) use app_config::network::NetworkRouteGeneration;
pub(super) use bytes::Bytes;
pub(super) use futures::{StreamExt, future::BoxFuture, stream};
pub(super) use market_participants_model::{
    decimal::{Price, ProfitOrLoss, Quantity, Ratio, SignedQuantity, Tick},
    identifier::{IdentifierValue, NativeSymbol, VenueId, VenueScopeId, WalletAddress},
    instrument::{Instrument, InstrumentTerms},
    listing::{
        identifiers::{HyperliquidListingIdentifiers, VenueListingIdentifiers},
        listing_impl::ListingStatus,
        route::{VenueListingKey, VenueRoute},
        rules::{
            CommonTradingRules, FundingInterval, FundingSchedule, LeverageTier, LotPolicy,
            MarginMode, MarginProfile, PerpetualMarginTerms, PerpetualRisk, PerpetualTradingRules,
            PriceRule, ProductTradingRules, QuantityRule, ShortSalePolicy, SpotTradingRules,
            TradingRules, VenueTradingPolicy,
        },
        venue::HyperliquidAssetId,
    },
    market::{
        book::MarketSequence,
        selection::OrderQuantity,
        units::{MarketTradeId, PositionSide, QuantityUnit, Side},
    },
    routing::CapabilityId,
};
pub(super) use smol_str::{SmolStr, format_smolstr};
pub(super) use tokio_util::sync::CancellationToken;
pub(super) use url::Url;

pub(super) use super::super::{
    HyperliquidCancel, HyperliquidCancelAction, HyperliquidCandle, HyperliquidCodec,
    HyperliquidInfoRequest, HyperliquidLimitOrderType, HyperliquidMarginMode, HyperliquidMetaAsset,
    HyperliquidMetaResponse, HyperliquidOrder, HyperliquidOrderAction, HyperliquidOrderType,
    HyperliquidSubscribe, HyperliquidSubscription,
    budget::{Spent, Weight, retry_after as info_retry_after},
    catalog,
    signing::{Eip712Signer, NonceGenerator},
    types::{HyperliquidActiveContextValues, HyperliquidPerpDex, HyperliquidSpotMetaResponse},
};
use super::contexts::FastContextBoard;
pub(super) use crate::{
    account::shared::{
        AccountTradeId, HyperliquidFillDirection, MarginMode as AccountMarginMode, OrderId,
        OrderKind, OrderStatus, PositionDirection, PositionId, TimeInForce, TriggerPriceKind,
    },
    adapter::{
        ExecutionContext, MarketParticipantAdapter, NativeCandlestick, NativeCatalogEntry,
        NativeOrderBook, NativeTicker, NativeTrade, PreparedRequest, ProtocolEvent,
        ProtocolRequest, ProtocolResponse, SubscriptionHandle, VenueExecutionContext,
    },
    connection::{
        ConnectionKey, ConnectionState, ReconnectPolicy, SubscriptionTopic,
        physical::ConnectionProtocol,
    },
    error::{MarketParticipantError, OrderErrorKind},
    market_participants::model::{
        AuthenticatedQuery, CancelOrderCommand, HyperliquidTransferDirection,
        HyperliquidWalletQuery, NativeAccountTrade, NativeBalance,
        NativeHyperliquidHistoricalOrder, NativeHyperliquidLedgerEvent,
        NativeHyperliquidLiquidatedPosition, NativeHyperliquidOpenOrder,
        NativeHyperliquidPerpetualPosition, NativeHyperliquidPerpetualState,
        NativeHyperliquidSpotBalance, NativeHyperliquidSpotPair, NativeHyperliquidSpotState,
        NativeHyperliquidUserFill, NativeHyperliquidWalletUpdate, NativeOrder,
        NativeOrderBookLevel, NativePosition, PlaceOrderCommand, TimeFrame,
    },
    protocol::WalletTarget,
    routing::{CORE_PERP_SCOPE, CORE_SPOT_SCOPE, HYPERLIQUID_WALLET_SCOPE},
    runtime::MarketParticipantNetworkSnapshot,
    venue::metadata::{
        PerpRouteMeta, RouteContexts, ScopeMetadata, SpotRouteMeta, VenueMetadata, VenueReading,
    },
};

/// How many times an `info` request the venue refused for its own allowance is retried.
///
/// The venue states its allowance per minute, so a request refused over it is worth
/// asking for again shortly: one address's reading is dozens of requests, and the
/// alternative to waiting is an address that stays unreadable until its reader asks
/// again. The bound is what keeps a venue that is refusing for a reason this does not
/// know from turning one read into an unbounded conversation.
const INFO_RETRY_ATTEMPTS: u32 = 3;

/// The first wait before a retry of a rate-limited `info` request.
const INFO_RETRY_BASE: Duration = Duration::from_millis(250);

/// The deepest order book Hyperliquid hands over in one answer.
pub(super) const ORDER_BOOK_DEPTH_LIMIT: u16 = 20;

/// The most builder-deployed perpetual DEXs one discovery verifies.
///
/// One discovery is two requests plus one `meta` request per deployed DEX, and the venue
/// alone decides how many DEXs it names. Every one of those requests goes over HTTP with
/// the public client's own timeouts, so without a bound a single answer decides how long
/// the discovery conversation — and the wait of every reader behind it — can run, and how
/// many requests every monitored address then sends. This is the same class of bound as
/// [`ORDER_BOOK_DEPTH_LIMIT`] and the decompression limit of `contexts`: a remote input
/// does not decide how much work this process does. Sixteen leaves room for the deployed
/// DEXs a live venue publishes while turning an unbounded fan-out into a bounded one — a
/// bound on the work, not a promise about how long the work takes. A venue over it is
/// refused whole rather than truncated, which is why the load's own tests read it.
pub(crate) const MAX_DEPLOYED_PERP_DEXES: usize = 16;

/// The request timeout of one Hyperliquid call.
pub(super) const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a calendar month may run, which bounds a candle window.
const MONTH_MILLIS: i64 = 31 * 24 * 60 * 60 * 1_000;

/// The funding interval every Hyperliquid asset funds on.
const FUNDING_INTERVAL_MILLIS: i64 = 3_600_000;

/// The candle intervals Hyperliquid publishes, in the model's own spelling.
pub(super) const TIMEFRAMES: &[TimeFrame] = &[
    TimeFrame::Minute1,
    TimeFrame::Minute3,
    TimeFrame::Minute5,
    TimeFrame::Minute15,
    TimeFrame::Minute30,
    TimeFrame::Hour1,
    TimeFrame::Hour2,
    TimeFrame::Hour4,
    TimeFrame::Hour8,
    TimeFrame::Hour12,
    TimeFrame::Day1,
    TimeFrame::Day3,
    TimeFrame::Week1,
    TimeFrame::Month1,
];

/// The listing key of every coin one route publishes.
pub(super) type RouteListings = Arc<BTreeMap<SmolStr, VenueListingKey>>;

/// The wire identity of one prepared listing.
///
/// Everything a request encodes is read from here: the resolved listing's typed
/// identifiers and its native symbol, never a string parsed back out of a coin
/// name.
pub(super) enum WireListing<'a> {
    Spot {
        coin: &'a NativeSymbol,
        asset_id: HyperliquidAssetId,
    },
    Perpetual {
        coin: &'a NativeSymbol,
        asset_id: HyperliquidAssetId,
    },
    Outcome {
        coin: &'a NativeSymbol,
        asset_id: HyperliquidAssetId,
    },
}

impl<'a> WireListing<'a> {
    /// Reads the wire identity of one prepared listing request.
    pub(super) fn of(
        prepared: &'a PreparedRequest,
        context: &ExecutionContext,
    ) -> Result<Self, MarketParticipantError> {
        let listing = prepared.listing().ok_or_else(|| {
            protocol_error(
                "encode a Hyperliquid listing request",
                SmolStr::new_static("the request carries no resolved listing"),
            )
        })?;
        let VenueListingIdentifiers::Hyperliquid(identifiers) = &listing.identifiers else {
            return Err(unsupported(context));
        };
        let coin = listing.native_symbol.as_ref().ok_or_else(|| {
            protocol_error(
                "encode a Hyperliquid listing request",
                SmolStr::new_static("the listing publishes no native coin"),
            )
        })?;
        match identifiers {
            HyperliquidListingIdentifiers::Spot { asset_id, .. } => Ok(Self::Spot {
                coin,
                asset_id: *asset_id,
            }),
            HyperliquidListingIdentifiers::Perpetual { asset_id, .. } => Ok(Self::Perpetual {
                coin,
                asset_id: *asset_id,
            }),
            HyperliquidListingIdentifiers::Outcome { asset_id, .. } => Ok(Self::Outcome {
                coin,
                asset_id: *asset_id,
            }),
        }
    }

    /// Returns the venue's own coin of this listing.
    pub(super) fn coin(&self) -> SmolStr {
        match self {
            Self::Spot { coin, .. } | Self::Perpetual { coin, .. } | Self::Outcome { coin, .. } => {
                SmolStr::new(coin.as_str())
            }
        }
    }

    /// Returns the asset index an order or a cancel addresses.
    pub(super) fn asset_id(&self) -> u32 {
        match self {
            Self::Spot { asset_id, .. }
            | Self::Perpetual { asset_id, .. }
            | Self::Outcome { asset_id, .. } => asset_id.0,
        }
    }
}

#[derive(Clone)]
pub struct HyperliquidAdapter {
    /// The static attribution this adapter is registered under.
    venue: VenueId,
    pub codec: HyperliquidCodec,
    /// The venue's live asset contexts, as one board.
    ///
    /// `fastAssetCtxs` publishes one board for the whole venue and then only what moved,
    /// and the subscription is shared by topic, so every reader of it — every monitored
    /// address, every open detail tab — is handed the whole of what the venue has
    /// published rather than the last message's share of it. See [`FastContextBoard`].
    fast_contexts: Arc<FastContextBoard>,
    pub signer: Option<Eip712Signer>,
}

impl HyperliquidAdapter {
    /// Builds an adapter without credentials.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            venue: VenueId::Hyperliquid,
            codec: HyperliquidCodec,
            fast_contexts: Arc::new(FastContextBoard::default()),
            signer: None,
        })
    }

    /// Builds an adapter from the configured HyperCore credentials.
    ///
    /// The credentials are the venue's own configured set rather than a name this crate
    /// hardcodes: no execution context names a credential set, so an adapter cannot ask
    /// for one of its own. HyperCore authenticates its whole surface with that one set,
    /// so the set is read through the venue's primary scope — the scope every other one
    /// it serves, deployed scopes included, signs with.
    pub fn from_config(
        config: &MarketParticipantNetworkSnapshot,
    ) -> Result<Arc<Self>, MarketParticipantError> {
        let mut adapter = Self::new().as_ref().clone();
        match hyperliquid_credentials(config, &built_in_scope(CORE_PERP_SCOPE)) {
            CredentialLookup::Configured(signer) => adapter.signer = Some(signer),
            CredentialLookup::Unconfigured => {}
            // The row is HyperCore's own shape, so the one set that reaches here is one
            // the operator has not finished filling in.
            CredentialLookup::Invalid => {
                return Err(MarketParticipantError::IncompleteCredentials {
                    venue: VenueId::Hyperliquid,
                    // HyperCore stores one set for the whole venue, so there is no market to
                    // name: its page draws one credential group, and naming the scope the set
                    // was read through would send the operator to a row that does not exist.
                    scope: None,
                });
            }
        }
        Ok(Arc::new(adapter))
    }

    /// Returns the static venue attribution of this adapter.
    pub(super) fn venue_id(&self) -> &VenueId {
        &self.venue
    }

    /// Returns the signer this execution signs with.
    ///
    /// The scope is the execution's own: HyperCore signs every scope it serves with the
    /// one set it stores, so a scope it deploys answers the same signer as the scope this
    /// build routes it through.
    pub(super) fn signer_for_context(
        &self,
        context: &ExecutionContext,
    ) -> Result<Option<Eip712Signer>, MarketParticipantError> {
        let Some(config) = context.current_runtime() else {
            return Ok(self.signer.clone());
        };
        match hyperliquid_credentials(&config, &context.route().scope) {
            CredentialLookup::Configured(signer) => Ok(Some(signer)),
            CredentialLookup::Unconfigured => Ok(None),
            CredentialLookup::Invalid => Err(MarketParticipantError::AuthenticationFailed {
                route: context.route(),
                account: context.auth.as_ref().map(|auth| auth.account_id().into()),
                source: crate::error::AuthError::IncompleteCredentials {
                    venue: VenueId::Hyperliquid,
                    // One set for the whole venue, so no market is the one to finish — see
                    // `from_config`.
                    scope: None,
                },
            }),
        }
    }

    /// Opens or reuses the socket of one request's route and capability.
    pub(super) async fn connection(
        &self,
        context: &ExecutionContext,
        request: &ProtocolRequest,
    ) -> Result<Arc<crate::connection::Connection>, MarketParticipantError> {
        let key =
            crate::adapter::bind_execution_identity(connection_key(context, request)?, context)?;
        let pool = Arc::clone(&context.connection_pool);
        pool.connect_or_create(
            key,
            ConnectionState::stateless(),
            connection_protocol(),
            pool.cancellation(),
            ReconnectPolicy::default(),
        )
        .await
    }

    /// Sends one `post` request over this route's socket and decodes the answer.
    pub(super) async fn request<T, F>(
        &self,
        context: &ExecutionContext,
        request: &ProtocolRequest,
        encode: F,
    ) -> Result<T, MarketParticipantError>
    where
        T: serde::de::DeserializeOwned,
        F: FnOnce(u64) -> Result<Bytes, MarketParticipantError>,
    {
        let connection = self.connection(context, request).await?;
        let request_id = connection.allocate_request_id();
        let bytes = connection
            .send_and_wait_with_id(request_id, encode(request_id)?, REQUEST_TIMEOUT)
            .await?;
        decode_response(&bytes)
    }

    /// Posts one `info` request to this venue's HTTP endpoint.
    pub(super) async fn post_info<T>(
        &self,
        context: &VenueExecutionContext,
        request: &HyperliquidInfoRequest,
    ) -> Result<T, MarketParticipantError>
    where
        T: serde::de::DeserializeOwned,
    {
        post_hyperliquid_info(
            &context.http_client,
            context.http_origin(),
            context.venue(),
            request,
        )
        .await
    }

    /// Reads what the venue publishes, in one reading of its own tables.
    ///
    /// The venue answers `perpDexs` with every DEX it knows, its first entry being
    /// the core DEX the core perpetual scope already is. Each remaining DEX is
    /// verified against its own `metaAndAssetCtxs` answer and the venue's spot token
    /// table: a DEX whose metadata is not a complete, collateral-backed
    /// `{dex}:{coin}` universe is **reported** rather than left out, because the
    /// venue publishes it and a scope set short of it would be answering something
    /// other than what the venue said. The bound is reported the same way:
    /// a venue that names more deployments than one reading verifies is not read
    /// further, and nothing is truncated.
    ///
    /// Everything the reading answers — the deployment table, the spot scope's
    /// metadata and every perpetual scope's metadata — comes from this one
    /// conversation. A DEX's position in the deployment table is what the venue's
    /// own asset ID for its perpetuals is built from, so a table read at another
    /// moment would move the identity of every listing this reading publishes.
    pub(super) async fn read_venue(
        &self,
        context: &VenueExecutionContext,
    ) -> Result<VenueReading, MarketParticipantError> {
        let venue = *context.venue();
        let dexes: Vec<Option<HyperliquidPerpDex>> = self
            .post_info(context, &HyperliquidInfoRequest::PerpDexs)
            .await?;
        // The bound is checked before any scope is read: the venue decides how many
        // DEXs it names, and one answer must not decide how much work this process
        // does. A venue over the bound is refused whole rather than truncated.
        let deployed = catalog::deployed_perp_dexes(&dexes);
        if deployed.len() > MAX_DEPLOYED_PERP_DEXES {
            return Ok(VenueReading::LimitExceeded {
                named: deployed.len(),
                limit: MAX_DEPLOYED_PERP_DEXES,
            });
        }
        // The venue's spot answer carries both the pairs of the spot scope and the
        // token table every scope resolves its collateral against: the first member
        // of `spotMetaAndAssetCtxs` *is* the token table, so the reading takes both
        // from that one answer instead of asking for the table again.
        let (spot, contexts): (
            HyperliquidSpotMetaResponse,
            Vec<HyperliquidActiveContextValues>,
        ) = self
            .post_info(context, &HyperliquidInfoRequest::SpotMetaAndAssetCtxs)
            .await?;
        let names = spot.universe.iter().map(|pair| pair.name.clone());
        let core = self.perpetual_scope_metadata(context, None).await?;
        // The core scope is not a deployment: its own collateral failing to resolve
        // is the venue's metadata not decoding, which is a discovery failure rather
        // than a scope left out of an otherwise complete set.
        let core_collateral = collateral_name(&spot, &core.0)?;
        let mut per_scope: BTreeMap<VenueRoute, ScopeMetadata> = BTreeMap::new();
        per_scope.insert(
            VenueRoute::new(venue, built_in_scope(CORE_PERP_SCOPE)),
            ScopeMetadata::Perp(PerpRouteMeta {
                perp_dex_index: 0,
                meta: core.0,
                contexts: core.1,
                collateral: core_collateral,
            }),
        );
        let mut verified = Vec::with_capacity(deployed.len());
        for name in deployed {
            let index = dex_index(&dexes, &name)?;
            // A name this build reserves no scope shape for is a deployment it
            // cannot serve, which is the same answer as metadata that does not
            // verify: the venue published it and this reading has nothing to
            // register for it.
            let Ok(scope) = catalog::perp_dex_scope(&name) else {
                return Ok(VenueReading::Unverifiable { dex: name });
            };
            let (meta, contexts) = self.perpetual_scope_metadata(context, Some(&name)).await?;
            // A deployment whose collateral is not a spot token is a deployment this
            // build cannot serve: its catalog could never be decoded, and leaving it
            // out would publish a scope set short of what the venue said.
            let Ok(collateral) = collateral_name(&spot, &meta) else {
                return Ok(VenueReading::Unverifiable { dex: name });
            };
            if !is_verified_perp_dex(&name, index, &meta, &spot) {
                return Ok(VenueReading::Unverifiable { dex: name });
            }
            per_scope.insert(
                VenueRoute::new(venue, scope),
                ScopeMetadata::Perp(PerpRouteMeta {
                    perp_dex_index: index,
                    meta,
                    contexts,
                    collateral,
                }),
            );
            verified.push(name);
        }
        per_scope.insert(
            VenueRoute::new(venue, built_in_scope(CORE_SPOT_SCOPE)),
            ScopeMetadata::Spot(SpotRouteMeta {
                contexts: context_map(names, contexts),
                meta: spot,
            }),
        );
        Ok(VenueReading::Verified {
            deployments: verified,
            metadata: Arc::new(VenueMetadata::new(per_scope, dexes)),
        })
    }
    /// Reads one perpetual scope's own metadata answer.
    ///
    /// The scope's assets and their contexts arrive together, so a listing identity
    /// built from the universe and a catalog decoded from it are the same moment's
    /// answer. Whether the answer can be *served* is deliberately not decided here:
    /// a failure at this point is the venue not answering, while an answer whose
    /// collateral is not a spot token is answered by the caller, because the core
    /// scope and a deployment report it differently.
    async fn perpetual_scope_metadata(
        &self,
        context: &VenueExecutionContext,
        dex: Option<&SmolStr>,
    ) -> Result<(HyperliquidMetaResponse, RouteContexts), MarketParticipantError> {
        let (meta, contexts): (HyperliquidMetaResponse, Vec<HyperliquidActiveContextValues>) = self
            .post_info(
                context,
                &HyperliquidInfoRequest::MetaAndAssetCtxs { dex: dex.cloned() },
            )
            .await?;
        let names: Vec<SmolStr> = meta
            .universe
            .iter()
            .map(|asset| asset.name.clone())
            .collect();
        Ok((meta, context_map(names.into_iter(), contexts)))
    }

    /// Returns the listing key of every coin one route publishes.
    ///
    /// The table is read from the metadata of the commit that admitted the request:
    /// nothing here asks the venue, so a route whose scope the loaded venue does not
    /// cover is refused rather than fetched.
    pub(super) fn route_listings(
        &self,
        context: &ExecutionContext,
        route: &VenueRoute,
    ) -> Result<RouteListings, MarketParticipantError> {
        match scope_metadata(context, route)? {
            ScopeMetadata::Spot(meta) => spot_listings(route, &meta.meta),
            ScopeMetadata::Perp(meta) => perpetual_listings(route, meta),
        }
    }

    /// Returns the listing key of every coin a set of routes publishes, as one table.
    ///
    /// A venue-wide wallet read — fills, a fills window, the ledger, historical orders,
    /// the fast asset contexts — names the coins of every scope the venue serves, while
    /// the request that reads it is addressed to one route. The answer is therefore
    /// decoded against the union of every monitored route's catalog, and a row is
    /// resolved by the coin it names.
    ///
    /// Two routes naming one coin is refused rather than resolved by whichever route was
    /// merged last. The venue's coin forms are *meant* to be disjoint — a spot pair reads
    /// `BASE/QUOTE`, a core perpetual the bare coin, a builder-deployed one
    /// `{dex}:{coin}` — but the coin is the venue's own `pair.name` / `asset.name` and no
    /// shape is checked when a route's table is derived, so a coin two routes claim is a
    /// possibility the answer decides, not one this code may assume away: keeping one of
    /// the two would decode the other route's rows with the wrong precision and units.
    /// The check is of untrusted input, in the same class as `is_verified_perp_dex`.
    ///
    /// Every route is read before the table is returned: a route whose catalog cannot be
    /// read fails the whole read rather than silently dropping exactly the rows the
    /// caller monitors through it.
    pub(super) fn route_listings_across(
        &self,
        context: &ExecutionContext,
        routes: &[VenueRoute],
    ) -> Result<RouteListings, MarketParticipantError> {
        let mut listings: BTreeMap<SmolStr, VenueListingKey> = BTreeMap::new();
        for route in routes {
            let route_listings = self.route_listings(context, route)?;
            for (coin, listing) in route_listings.iter() {
                if let Some(claimed) = listings.get(coin)
                    && claimed != listing
                {
                    return Err(protocol_error(
                        "merge the catalogs of the monitored wallet routes",
                        format_smolstr!(
                            "the coin {coin} is published by two routes: {claimed:?} and \
                             {listing:?}"
                        ),
                    ));
                }
                listings.insert(coin.clone(), listing.clone());
            }
        }
        Ok(Arc::new(listings))
    }

    /// Builds the catalog of one route.
    ///
    /// The catalog is derived from the metadata of the commit that admitted the
    /// request: the venue's own answer is read at load time, and nothing here asks
    /// it again.
    pub(super) fn fetch_markets(
        &self,
        context: &ExecutionContext,
        route: &VenueRoute,
    ) -> Result<Vec<NativeCatalogEntry>, MarketParticipantError> {
        match scope_metadata(context, route)? {
            ScopeMetadata::Spot(meta) => spot_catalog_entries(route, &meta.meta),
            ScopeMetadata::Perp(meta) => perpetual_catalog_entries(route, meta),
        }
    }

    /// The candlesticks of one listing.
    pub(super) async fn candles(
        &self,
        context: &ExecutionContext,
        request: &ProtocolRequest,
        coin: SmolStr,
        route: &VenueRoute,
        timeframe: TimeFrame,
        start_timestamp: Option<i64>,
        end_timestamp: Option<i64>,
        limit: u16,
    ) -> Result<Vec<NativeCandlestick>, MarketParticipantError> {
        let interval = self.codec.encode_timeframe(route, timeframe)?;
        let (start, end) = candle_request_window(
            timeframe,
            limit,
            start_timestamp,
            end_timestamp,
            current_timestamp_millis()?,
        );
        let rows: Vec<HyperliquidCandle> = self
            .request(context, request, |id| {
                self.codec.encode(
                    &self.codec.info_post(
                        self.codec
                            .candle_snapshot_request(coin.clone(), interval, start, end),
                        id,
                    ),
                )
            })
            .await?;
        rows.into_iter()
            .filter(|row| row.t >= start && row.t <= end)
            .take(usize::from(limit))
            .map(|row| {
                Ok(NativeCandlestick {
                    timestamp: row.t,
                    open: parse_price(&row.o, "decode a Hyperliquid candle")?,
                    high: parse_price(&row.h, "decode a Hyperliquid candle")?,
                    low: parse_price(&row.l, "decode a Hyperliquid candle")?,
                    close: parse_price(&row.c, "decode a Hyperliquid candle")?,
                    volume: parse_quantity(&row.v, "decode a Hyperliquid candle")?,
                })
            })
            .collect()
    }

    /// One listing's ticker.
    pub(super) fn ticker(
        &self,
        context: &ExecutionContext,
        route: &VenueRoute,
        coin: &str,
    ) -> Result<NativeTicker, MarketParticipantError> {
        let meta = scope_metadata(context, route)?;
        let values = meta.contexts().get(coin).ok_or_else(|| {
            protocol_error(
                "decode a Hyperliquid ticker",
                SmolStr::new_static("the route publishes no context for the requested coin"),
            )
        })?;
        decode_ticker(values, None)
    }

    /// Every listing's ticker on one route.
    pub(super) fn all_tickers(
        &self,
        context: &ExecutionContext,
        route: &VenueRoute,
    ) -> Result<Vec<(VenueListingKey, NativeTicker)>, MarketParticipantError> {
        let meta = scope_metadata(context, route)?;
        let listings = match meta {
            ScopeMetadata::Spot(meta) => spot_listings(route, &meta.meta)?,
            ScopeMetadata::Perp(meta) => perpetual_listings(route, meta)?,
        };
        let mut tickers = Vec::with_capacity(listings.len());
        for (coin, key) in listings.iter() {
            let Some(values) = meta.contexts().get(coin) else {
                continue;
            };
            tickers.push((key.clone(), decode_ticker(values, None)?));
        }
        Ok(tickers)
    }

    /// The recent trades of one listing.
    pub(super) async fn trades(
        &self,
        context: &ExecutionContext,
        request: &ProtocolRequest,
        coin: SmolStr,
        limit: u16,
    ) -> Result<Vec<NativeTrade>, MarketParticipantError> {
        let rows: Vec<HyperliquidTrade> = self
            .request(context, request, |id| {
                self.codec.encode(
                    &self
                        .codec
                        .info_post(self.codec.recent_trades_request(coin), id),
                )
            })
            .await?;
        let mut trades = rows
            .into_iter()
            .take(usize::from(limit))
            .map(|row| {
                Ok(NativeTrade {
                    id: MarketTradeId::Numeric(row.tid),
                    timestamp: row.time,
                    price: parse_price(&row.px, "decode a Hyperliquid trade")?,
                    quantity: parse_quantity(&row.sz, "decode a Hyperliquid trade")?,
                    side: decode_side(&row.side, "decode a Hyperliquid trade side")?,
                })
            })
            .collect::<Result<Vec<_>, MarketParticipantError>>()?;
        trades.sort_unstable_by(|left, right| {
            (left.timestamp, &left.id).cmp(&(right.timestamp, &right.id))
        });
        Ok(trades)
    }

    /// One listing's order book snapshot.
    ///
    /// Hyperliquid publishes `l2Book` as a snapshot numbered by its own timestamp
    /// and nothing else: there is no numeric sequence, so the sequence is that
    /// native watermark and never an invented counter.
    pub(super) async fn order_book(
        &self,
        context: &ExecutionContext,
        request: &ProtocolRequest,
        coin: SmolStr,
        depth: u16,
    ) -> Result<NativeOrderBook, MarketParticipantError> {
        if depth > ORDER_BOOK_DEPTH_LIMIT {
            return Err(MarketParticipantError::InvalidLimit {
                operation: "order book".into(),
                requested: depth,
                maximum: ORDER_BOOK_DEPTH_LIMIT,
            });
        }
        let book: HyperliquidBookResponse = self
            .request(context, request, |id| {
                self.codec.encode(
                    &self
                        .codec
                        .info_post(HyperliquidInfoRequest::L2Book { coin: coin.clone() }, id),
                )
            })
            .await?;
        let [bids, asks] = book.levels;
        Ok(NativeOrderBook {
            sequence: native_sequence(book.time)?,
            timestamp: Some(book.time),
            bids: decode_book_levels(bids, depth)?,
            asks: decode_book_levels(asks, depth)?,
        })
    }

    /// The spot state of one monitored or authenticated wallet.
    pub(super) async fn wallet_spot_state(
        &self,
        context: &ExecutionContext,
        request: &ProtocolRequest,
        user: SmolStr,
    ) -> Result<NativeHyperliquidSpotState, MarketParticipantError> {
        let state: HyperliquidSpotStateWire = self
            .request(context, request, |id| {
                self.codec.encode(&self.codec.info_post(
                    HyperliquidInfoRequest::SpotClearinghouseState { user: user.clone() },
                    id,
                ))
            })
            .await?;
        let balances = state
            .balances
            .into_iter()
            .map(|value| {
                Ok(NativeHyperliquidSpotBalance {
                    coin: catalog::token_asset(&value.coin)?.asset_id(),
                    total: parse_signed_quantity(
                        &value.total,
                        "decode a Hyperliquid spot balance",
                    )?,
                    hold: parse_signed_quantity(&value.hold, "decode a Hyperliquid spot hold")?,
                })
            })
            .collect::<Result<Vec<_>, MarketParticipantError>>()?;
        Ok(NativeHyperliquidSpotState { balances })
    }

    /// The perpetual state of one monitored or authenticated wallet.
    pub(super) async fn wallet_perpetual_state(
        &self,
        context: &ExecutionContext,
        request: &ProtocolRequest,
        user: SmolStr,
        route: &VenueRoute,
        dex: Option<SmolStr>,
    ) -> Result<NativeHyperliquidPerpetualState, MarketParticipantError> {
        let state: HyperliquidPerpetualStateWire = self
            .request(context, request, |id| {
                self.codec.encode(&self.codec.info_post(
                    HyperliquidInfoRequest::ClearinghouseState {
                        user: user.clone(),
                        dex: dex.clone(),
                    },
                    id,
                ))
            })
            .await?;
        let listings = self.route_listings(context, route)?;
        let mut positions = Vec::with_capacity(state.asset_positions.len());
        for item in state.asset_positions {
            let value = item.position;
            let size = parse_signed_quantity(&value.szi, "decode a Hyperliquid position size")?;
            if size.is_zero() {
                continue;
            }
            let Some(listing) = listings.get(&value.coin) else {
                log::debug!(
                    "skipped a Hyperliquid position of {}: this route publishes no such listing",
                    value.coin
                );
                continue;
            };
            positions.push(NativeHyperliquidPerpetualPosition {
                listing: listing.clone(),
                size,
                entry_price: optional_price(&value.entry_px, "decode a Hyperliquid entry price")?,
                mark_price: optional_price(&value.mark_px, "decode a Hyperliquid mark price")?,
                liquidation_price: optional_price(
                    &value.liquidation_px,
                    "decode a Hyperliquid liquidation price",
                )?,
                position_value: optional_quantity(
                    &value.position_value,
                    "decode a Hyperliquid position value",
                )?,
                unrealized_pnl: parse_profit(
                    &value.unrealized_pnl,
                    "decode a Hyperliquid position pnl",
                )?,
                margin: optional_quantity(&value.margin_used, "decode a Hyperliquid margin")?,
                funding_since_open: ProfitOrLoss::ZERO,
            });
        }
        Ok(NativeHyperliquidPerpetualState { positions })
    }

    /// The orders of one monitored wallet.
    ///
    /// The venue's two order reads are not the same read, and which catalog decodes one
    /// follows the wire: `frontendOpenOrders` names a `dex`, so it is read per route and
    /// decoded against that route alone, while `historicalOrders` names the user alone —
    /// one order history per address — and is therefore decoded against every monitored
    /// route's catalog.
    pub(super) async fn wallet_orders(
        &self,
        context: &ExecutionContext,
        request: &ProtocolRequest,
        query: &HyperliquidWalletQuery,
        historical: bool,
    ) -> Result<ProtocolResponse, MarketParticipantError> {
        let user = wallet_user(query.wallet_target(), context)?;
        let route = query.route().clone();
        if historical {
            let listings = self.route_listings_across(context, query.decoding_routes())?;
            let rows: Vec<HyperliquidHistoricalOrderWire> = self
                .request(context, request, |id| {
                    self.codec.encode(&self.codec.info_post(
                        HyperliquidInfoRequest::HistoricalOrders { user: user.clone() },
                        id,
                    ))
                })
                .await?;
            let mut orders = Vec::with_capacity(rows.len());
            for row in rows {
                let Some(listing) = listings.get(&row.order.coin) else {
                    continue;
                };
                orders.push(decode_historical_order(listing, row)?);
            }
            return Ok(ProtocolResponse::HyperliquidHistoricalOrders(orders));
        }
        let listings = self.route_listings(context, &route)?;
        let rows: Vec<HyperliquidOrderWire> = self
            .request(context, request, |id| {
                self.codec.encode(&self.codec.info_post(
                    HyperliquidInfoRequest::FrontendOpenOrders {
                        user: user.clone(),
                        dex: wallet_dex(query.wallet_target(), &route)?,
                    },
                    id,
                ))
            })
            .await?;
        let mut orders = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(listing) = listings.get(&row.coin) else {
                continue;
            };
            orders.push(decode_open_order(listing, row)?);
        }
        Ok(ProtocolResponse::HyperliquidOpenOrders(orders))
    }

    /// The fills of one monitored wallet.
    ///
    /// `userFills` and `userFillsByTime` name the user alone — the venue publishes one
    /// fills history per address, not one per scope — so the answer is decoded against
    /// every monitored route's catalog rather than against the addressed route's.
    pub(super) async fn wallet_fills(
        &self,
        context: &ExecutionContext,
        request: &ProtocolRequest,
        target: &WalletTarget,
        decoding_routes: &[VenueRoute],
        aggregate_by_time: bool,
        window: Option<(i64, i64)>,
    ) -> Result<Vec<NativeHyperliquidUserFill>, MarketParticipantError> {
        let user = wallet_user(target, context)?;
        let address = wallet_address(target, context)?;
        let listings = self.route_listings_across(context, decoding_routes)?;
        let rows: Vec<HyperliquidFillWire> = match window {
            Some((start_time, end_time)) => {
                self.request(context, request, |id| {
                    self.codec.encode(&self.codec.info_post(
                        HyperliquidInfoRequest::UserFillsByTime {
                            user: user.clone(),
                            start_time,
                            end_time,
                            aggregate_by_time,
                        },
                        id,
                    ))
                })
                .await?
            }
            None => {
                self.request(context, request, |id| {
                    self.codec.encode(&self.codec.info_post(
                        HyperliquidInfoRequest::UserFills {
                            user: user.clone(),
                            aggregate_by_time: Some(aggregate_by_time),
                        },
                        id,
                    ))
                })
                .await?
            }
        };
        let mut fills = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(listing) = listings.get(&row.coin) else {
                continue;
            };
            fills.push(decode_fill(&address, listing, row)?);
        }
        Ok(fills)
    }

    /// The ledger of one monitored wallet.
    ///
    /// `userNonFundingLedgerUpdates` names the user alone, so this read is decoded
    /// against every monitored route's catalog — the same set the fills read uses,
    /// rather than the route the request happened to be addressed to.
    pub(super) async fn wallet_ledger(
        &self,
        context: &ExecutionContext,
        request: &ProtocolRequest,
        target: &WalletTarget,
        decoding_routes: &[VenueRoute],
        start_time: i64,
    ) -> Result<Vec<NativeHyperliquidLedgerEvent>, MarketParticipantError> {
        if start_time < 0 {
            return Err(MarketParticipantError::Configuration(
                "a ledger start time is never negative".into(),
            ));
        }
        let user = wallet_user(target, context)?;
        let address = wallet_address(target, context)?;
        let listings = self.route_listings_across(context, decoding_routes)?;
        let rows: Vec<HyperliquidLedgerWire> = self
            .request(context, request, |id| {
                self.codec.encode(&self.codec.info_post(
                    HyperliquidInfoRequest::UserNonFundingLedgerUpdates {
                        user: user.clone(),
                        start_time,
                    },
                    id,
                ))
            })
            .await?;
        decode_ledger_rows(&address, rows, &listings)
    }

    /// The balances of one authenticated account.
    pub(super) async fn balances(
        &self,
        context: &ExecutionContext,
        request: &ProtocolRequest,
        query: &AuthenticatedQuery,
    ) -> Result<Vec<NativeBalance>, MarketParticipantError> {
        let user: SmolStr = SmolStr::new(query.target().account().as_str());
        let state: HyperliquidSpotStateWire = self
            .request(context, request, |id| {
                self.codec.encode(&self.codec.info_post(
                    HyperliquidInfoRequest::SpotClearinghouseState { user: user.clone() },
                    id,
                ))
            })
            .await?;
        state
            .balances
            .into_iter()
            .map(|value| {
                let total = parse_signed_quantity(&value.total, "decode a Hyperliquid balance")?;
                let hold = parse_signed_quantity(&value.hold, "decode a Hyperliquid balance hold")?;
                let available = total
                    .checked_sub(hold)
                    .ok_or(market_participants_model::error::ModelError::DecimalOverflow)
                    .map_err(model_error)?;
                Ok(NativeBalance {
                    asset: catalog::token_asset(&value.coin)?.asset_id(),
                    total: total.magnitude(),
                    available: available.magnitude(),
                })
            })
            .collect()
    }

    /// The positions of one authenticated account.
    pub(super) async fn positions(
        &self,
        context: &ExecutionContext,
        request: &ProtocolRequest,
        query: &AuthenticatedQuery,
    ) -> Result<Vec<NativePosition>, MarketParticipantError> {
        let route = query.route().clone();
        let user: SmolStr = SmolStr::new(query.target().account().as_str());
        let state: HyperliquidPerpetualStateWire = self
            .request(context, request, |id| {
                self.codec.encode(&self.codec.info_post(
                    HyperliquidInfoRequest::ClearinghouseState {
                        user: user.clone(),
                        dex: perp_dex_wire_name(&route.scope)?,
                    },
                    id,
                ))
            })
            .await?;
        let listings = self.route_listings(context, &route)?;
        let mut positions = Vec::with_capacity(state.asset_positions.len());
        for item in state.asset_positions {
            let value = item.position;
            let signed = parse_signed_quantity(&value.szi, "decode a Hyperliquid position size")?;
            if signed.is_zero() {
                continue;
            }
            let Some(listing) = listings.get(&value.coin) else {
                continue;
            };
            let entry_price =
                optional_price(&value.entry_px, "decode a Hyperliquid position entry price")?
                    .ok_or_else(|| {
                        protocol_error(
                            "decode a Hyperliquid position entry price",
                            SmolStr::new_static("the position publishes no entry price"),
                        )
                    })?;
            let mark_price =
                optional_price(&value.mark_px, "decode a Hyperliquid position mark price")?
                    .unwrap_or(entry_price);
            positions.push(NativePosition {
                listing: listing.clone(),
                id: Some(
                    PositionId::new(format_smolstr!("{}:{}", user, value.coin)).map_err(
                        |error| protocol_error("create a Hyperliquid position ID", error),
                    )?,
                ),
                position_side: PositionSide::Net,
                direction: if signed.inner().is_negative() {
                    PositionDirection::Short
                } else {
                    PositionDirection::Long
                },
                margin_mode: Some(AccountMarginMode::Cross),
                leverage: None,
                quantity: signed.magnitude(),
                entry_price,
                break_even_price: None,
                mark_price,
                unrealized_profit_or_loss: parse_profit(
                    &value.unrealized_pnl,
                    "decode a Hyperliquid position pnl",
                )?,
                liquidation_price: optional_price(
                    &value.liquidation_px,
                    "decode a Hyperliquid liquidation price",
                )?,
                notional: optional_quantity(
                    &value.position_value,
                    "decode a Hyperliquid position notional",
                )?
                .unwrap_or(Quantity::ZERO),
                initial_margin: ProfitOrLoss::ZERO,
                maintenance_margin: None,
                update_timestamp: state.time,
            });
        }
        Ok(positions)
    }

    /// The account trades of one authenticated account.
    pub(super) async fn account_trades(
        &self,
        context: &ExecutionContext,
        request: &ProtocolRequest,
        query: &AuthenticatedQuery,
    ) -> Result<Vec<NativeAccountTrade>, MarketParticipantError> {
        let route = query.route().clone();
        let listings = self.route_listings(context, &route)?;
        let user: SmolStr = SmolStr::new(query.target().account().as_str());
        let rows: Vec<HyperliquidFillWire> = self
            .request(context, request, |id| {
                self.codec.encode(&self.codec.info_post(
                    HyperliquidInfoRequest::UserFills {
                        user: user.clone(),
                        aggregate_by_time: Some(false),
                    },
                    id,
                ))
            })
            .await?;
        let mut trades = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(listing) = listings.get(&row.coin) else {
                continue;
            };
            trades.push(NativeAccountTrade {
                listing: listing.clone(),
                id: AccountTradeId::new(format_smolstr!("{}", row.tid))
                    .map_err(|error| protocol_error("create a Hyperliquid trade ID", error))?,
                order_id: OrderId::normal(format_smolstr!("{}", row.tid))
                    .map_err(|error| protocol_error("create a Hyperliquid order ID", error))?,
                side: decode_side(&row.side, "decode a Hyperliquid trade side")?,
                price: parse_price(&row.px, "decode a Hyperliquid trade price")?,
                quantity: parse_quantity(&row.sz, "decode a Hyperliquid trade quantity")?,
                fee_asset: catalog::token_asset(&row.fee_token)?.asset_id(),
                fee: parse_profit(&row.fee, "decode a Hyperliquid trade fee")?,
                liquidity: None,
                timestamp: row.time,
            });
        }
        Ok(trades)
    }

    /// The orders of one authenticated account.
    pub(super) async fn orders(
        &self,
        context: &ExecutionContext,
        request: &ProtocolRequest,
        query: &AuthenticatedQuery,
        historical: bool,
    ) -> Result<Vec<NativeOrder>, MarketParticipantError> {
        let route = query.route().clone();
        let listings = self.route_listings(context, &route)?;
        let user: SmolStr = SmolStr::new(query.target().account().as_str());
        if historical {
            let rows: Vec<HyperliquidHistoricalOrderWire> = self
                .request(context, request, |id| {
                    self.codec.encode(&self.codec.info_post(
                        HyperliquidInfoRequest::HistoricalOrders { user: user.clone() },
                        id,
                    ))
                })
                .await?;
            let mut orders = Vec::with_capacity(rows.len());
            for row in rows {
                let Some(listing) = listings.get(&row.order.coin) else {
                    continue;
                };
                orders.push(decode_order(listing, row.order, row.status, false)?);
            }
            return Ok(orders);
        }
        let rows: Vec<HyperliquidOrderWire> = self
            .request(context, request, |id| {
                self.codec.encode(&self.codec.info_post(
                    HyperliquidInfoRequest::FrontendOpenOrders {
                        user: user.clone(),
                        dex: perp_dex_wire_name(&route.scope)?,
                    },
                    id,
                ))
            })
            .await?;
        let mut orders = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(listing) = listings.get(&row.coin) else {
                continue;
            };
            orders.push(decode_order(
                listing,
                row,
                SmolStr::new_static("open"),
                true,
            )?);
        }
        Ok(orders)
    }

    /// The spot pairs one spot route publishes.
    pub(super) fn spot_pairs(
        &self,
        context: &ExecutionContext,
        route: &VenueRoute,
    ) -> Result<Vec<NativeHyperliquidSpotPair>, MarketParticipantError> {
        let entries = self.fetch_markets(context, route)?;
        let mut pairs = Vec::with_capacity(entries.len());
        for entry in entries {
            let InstrumentTerms::Spot(terms) = entry.instrument.terms() else {
                continue;
            };
            let VenueListingIdentifiers::Hyperliquid(HyperliquidListingIdentifiers::Spot {
                asset_id,
                ..
            }) = &entry.identifiers
            else {
                continue;
            };
            pairs.push(NativeHyperliquidSpotPair {
                coin: IdentifierValue::new(entry.native_symbol.as_str())
                    .map_err(|error| protocol_error("decode a Hyperliquid spot coin", error))?,
                base_token: terms.base().clone(),
                quote_token: terms.quote().clone(),
                asset_id: *asset_id,
            });
        }
        Ok(pairs)
    }

    /// Places one order and answers its venue order ID.
    pub(super) async fn place_order(
        &self,
        context: &ExecutionContext,
        prepared: &PreparedRequest,
        request: &ProtocolRequest,
        command: PlaceOrderCommand,
    ) -> Result<OrderId, MarketParticipantError> {
        command.validate().map_err(model_error)?;
        let listing = WireListing::of(prepared, context)?;
        let signer = self.signer_for_context(context)?.ok_or_else(|| {
            MarketParticipantError::AuthenticationFailed {
                route: context.route(),
                account: Some(SmolStr::new(command.target.account().as_str())),
                source: crate::error::AuthError::InvalidCredentials,
            }
        })?;
        let quantity = match &command.quantity {
            OrderQuantity::AssetUnits { value, .. }
            | OrderQuantity::Contracts(value)
            | OrderQuantity::OutcomeTokens { value, .. } => *value,
            OrderQuantity::Notional { .. } => {
                return Err(MarketParticipantError::Configuration(
                    "a Hyperliquid order is placed in the listing's own units".into(),
                ));
            }
        };
        let coin = listing.coin();
        let (limit_price, order_type) = match &command.kind {
            OrderKind::Limit {
                price,
                time_in_force,
            } => (
                format_smolstr!("{price}"),
                HyperliquidOrderType::Limit(HyperliquidLimitOrderType {
                    time_in_force: encode_time_in_force(*time_in_force)?,
                }),
            ),
            OrderKind::Market => {
                let book = self.order_book(context, request, coin, 1).await?;
                let reference = match command.side {
                    Side::Buy => book.asks.first().map(|level| level.price),
                    Side::Sell => book.bids.first().map(|level| level.price),
                }
                .ok_or_else(|| {
                    protocol_error(
                        "prepare a Hyperliquid market order",
                        SmolStr::new_static("the order book published no top level"),
                    )
                })?;
                let slippage = command.market_max_slippage.ok_or_else(|| {
                    MarketParticipantError::Configuration(
                        "a Hyperliquid market order needs a maximum slippage".into(),
                    )
                })?;
                (
                    format_smolstr!(
                        "{}",
                        aggressive_market_price(reference, command.side, slippage)?
                    ),
                    HyperliquidOrderType::Limit(HyperliquidLimitOrderType {
                        time_in_force: SmolStr::new_static("Ioc"),
                    }),
                )
            }
            _ => {
                return Err(MarketParticipantError::Configuration(
                    "the Hyperliquid adapter places market and limit orders".into(),
                ));
            }
        };
        let action = HyperliquidOrderAction {
            r#type: SmolStr::new_static("order"),
            orders: vec![HyperliquidOrder {
                asset: listing.asset_id(),
                is_buy: command.side == Side::Buy,
                sz: format_smolstr!("{quantity}"),
                limit_px: limit_price,
                order_type,
                reduce_only: command.reduce_only,
            }],
            grouping: SmolStr::new_static("na"),
        };
        let signed = signer.attach(action, NonceGenerator::new().next()?)?;
        let response: HyperliquidOrderResponse = self
            .request(context, request, |id| {
                self.codec.encode(&self.codec.action_post(signed, id))
            })
            .await?;
        let status = response
            .response
            .data
            .statuses
            .into_iter()
            .next()
            .ok_or_else(|| MarketParticipantError::OrderError {
                kind: OrderErrorKind::Other {
                    code: "missing-status".into(),
                    message: "the Hyperliquid order response published no status".into(),
                },
                raw_data: None,
            })?;
        let oid = match status {
            HyperliquidOrderStatus::Resting { resting }
            | HyperliquidOrderStatus::Filled { filled: resting } => resting.oid,
            HyperliquidOrderStatus::Error { error } => {
                return Err(MarketParticipantError::Rejected {
                    code: SmolStr::new_static("hyperliquid"),
                    message: error,
                    retryable: false,
                    raw_response: None,
                });
            }
            HyperliquidOrderStatus::WaitingForFill { .. }
            | HyperliquidOrderStatus::WaitingForTrigger { .. } => {
                return Err(MarketParticipantError::OrderError {
                    kind: OrderErrorKind::Other {
                        code: "pending".into(),
                        message: "the Hyperliquid order is pending and has no order ID".into(),
                    },
                    raw_data: None,
                });
            }
        };
        OrderId::normal(format_smolstr!("{oid}"))
            .map_err(|error| protocol_error("create a Hyperliquid order ID", error))
    }

    /// Cancels one order.
    pub(super) async fn cancel_order(
        &self,
        context: &ExecutionContext,
        prepared: &PreparedRequest,
        request: &ProtocolRequest,
        command: CancelOrderCommand,
    ) -> Result<OrderId, MarketParticipantError> {
        let asset = WireListing::of(prepared, context)?.asset_id();
        let signer = self.signer_for_context(context)?.ok_or_else(|| {
            MarketParticipantError::AuthenticationFailed {
                route: context.route(),
                account: Some(SmolStr::new(command.target.account().as_str())),
                source: crate::error::AuthError::InvalidCredentials,
            }
        })?;
        let oid = command.order_id.as_str().parse::<u64>().map_err(|_| {
            MarketParticipantError::Configuration("a Hyperliquid order ID is numeric".into())
        })?;
        let action = HyperliquidCancelAction {
            r#type: SmolStr::new_static("cancel"),
            cancels: vec![HyperliquidCancel { asset, oid }],
        };
        let signed = signer.attach(action, NonceGenerator::new().next()?)?;
        let _: serde::de::IgnoredAny = self
            .request(context, request, |id| {
                self.codec.encode(&self.codec.action_post(signed, id))
            })
            .await?;
        Ok(command.order_id)
    }

    /// Opens the subscription of one prepared request.
    pub(super) async fn subscribe_request(
        &self,
        context: ExecutionContext,
        prepared: PreparedRequest,
    ) -> Result<SubscriptionHandle, MarketParticipantError> {
        let request = prepared.request().clone();
        let (subscription, kind) = match &request {
            ProtocolRequest::SubscribeCandlesticks(query) => {
                let listing = listing_key(&prepared)?;
                let coin = WireListing::of(&prepared, &context)?.coin();
                let interval: SmolStr = self
                    .codec
                    .encode_timeframe(query.route(), query.timeframe)?
                    .into();
                (
                    self.codec
                        .candle_subscription(coin.clone(), interval.clone()),
                    SubscriptionKind::Candlestick {
                        listing,
                        coin,
                        interval,
                    },
                )
            }
            ProtocolRequest::SubscribeTicker(_query) => {
                let listing = listing_key(&prepared)?;
                let coin = WireListing::of(&prepared, &context)?.coin();
                (
                    self.codec.ticker_subscription(coin.clone()),
                    SubscriptionKind::Ticker { listing, coin },
                )
            }
            ProtocolRequest::SubscribeTrades(_query) => {
                let listing = listing_key(&prepared)?;
                let coin = WireListing::of(&prepared, &context)?.coin();
                (
                    self.codec.trades_subscription(coin.clone()),
                    SubscriptionKind::Trade { listing, coin },
                )
            }
            ProtocolRequest::SubscribeOrderBook(query) => {
                if query.depth > ORDER_BOOK_DEPTH_LIMIT {
                    return Err(MarketParticipantError::InvalidLimit {
                        operation: "order book".into(),
                        requested: query.depth,
                        maximum: ORDER_BOOK_DEPTH_LIMIT,
                    });
                }
                let listing = listing_key(&prepared)?;
                let coin = WireListing::of(&prepared, &context)?.coin();
                (
                    self.codec.order_book_subscription(coin.clone()),
                    SubscriptionKind::OrderBook {
                        listing,
                        coin,
                        depth: query.depth,
                    },
                )
            }
            ProtocolRequest::SubscribeOrders(query) => {
                let user: SmolStr = query.target().account().as_str().into();
                (
                    self.codec.user_fills_subscription(user),
                    SubscriptionKind::Order,
                )
            }
            ProtocolRequest::SubscribePositions(query) => {
                let user: SmolStr = query.target().account().as_str().into();
                (
                    self.codec.user_ledger_subscription(user),
                    SubscriptionKind::Position,
                )
            }
            ProtocolRequest::SubscribeHyperliquidUserFills(query) => {
                let user = wallet_user(query.wallet_target(), &context)?;
                let address = wallet_address(query.wallet_target(), &context)?;
                (
                    self.codec.user_fills_subscription(user),
                    SubscriptionKind::WalletFills {
                        address,
                        routes: query.decoding_routes().to_vec(),
                    },
                )
            }
            ProtocolRequest::SubscribeHyperliquidUserLedger(query) => {
                let user = wallet_user(query.wallet_target(), &context)?;
                let address = wallet_address(query.wallet_target(), &context)?;
                (
                    self.codec.user_ledger_subscription(user),
                    SubscriptionKind::WalletLedger {
                        address,
                        routes: query.decoding_routes().to_vec(),
                    },
                )
            }
            ProtocolRequest::SubscribeHyperliquidFastContexts(query) => (
                HyperliquidSubscribe {
                    method: SmolStr::new_static("subscribe"),
                    subscription: HyperliquidSubscription::FastAssetCtxs,
                },
                SubscriptionKind::FastContexts {
                    routes: query.decoding_routes().to_vec(),
                    board: Arc::clone(&self.fast_contexts),
                },
            ),
            _ => return Err(unsupported(&context)),
        };
        let connection = self.connection(&context, &request).await?;
        // The topic is the subscription body, which names the coin or the wallet it is
        // about and never the decoding route set: two public addresses on one route hold
        // two topics on one socket, and two readers of one address hold one topic however
        // many routes each of them decodes against — each of them decodes the shared
        // stream with the catalogs it asked for.
        let topic = SubscriptionTopic::from(self.codec.encode(&subscription.subscription)?);
        let mut unsubscribe = subscription.clone();
        unsubscribe.method = SmolStr::new_static("unsubscribe");
        let connection_subscription = connection.subscribe(
            topic,
            self.codec.encode(&subscription)?,
            self.codec.encode(&unsubscribe)?,
        )?;
        let cancellation = CancellationToken::new();
        let state = (
            Box::pin(connection.event_stream()),
            cancellation.clone(),
            kind,
            self.clone(),
            context,
        );
        let stream = stream::unfold(
            state,
            |(mut source, cancellation, kind, adapter, context)| async move {
                loop {
                    let next = source.next();
                    let payload = tokio::select! {
                        () = cancellation.cancelled() => return None,
                        payload = next => payload,
                    };
                    let payload = match payload {
                        Some(Ok(payload)) => payload,
                        // A connection that stopped publishing ends its streams with why.
                        Some(Err(error)) => {
                            return Some((
                                Err(error),
                                (source, cancellation, kind, adapter, context),
                            ));
                        }
                        None => return None,
                    };
                    let listings = match kind.decoding_routes() {
                        Some(routes) => match adapter.route_listings_across(&context, routes) {
                            Ok(listings) => Some(listings),
                            Err(error) => {
                                return Some((
                                    Err(error),
                                    (source, cancellation, kind, adapter, context),
                                ));
                            }
                        },
                        None => None,
                    };
                    match decode_event(&payload, &kind, listings.as_ref()) {
                        Ok(Some(event)) => {
                            return Some((
                                Ok(event),
                                (source, cancellation, kind, adapter, context),
                            ));
                        }
                        Ok(None) => continue,
                        Err(error) => {
                            return Some((
                                Err(error),
                                (source, cancellation, kind, adapter, context),
                            ));
                        }
                    }
                }
            },
        );
        Ok(SubscriptionHandle::with_connection_subscription(
            Box::pin(stream),
            cancellation,
            connection_subscription,
        ))
    }
}

/// Posts one `info` request to one Hyperliquid HTTP origin.
///
/// The catalog and the discovery answers go over HTTP: they are the requests whose
/// answers carry a whole universe rather than one subject, and they are the ones a
/// caller outside an execution context can make. A wallet's own rows go over the
/// socket instead, so the weight this spends is the metadata a reading is decoded
/// with rather than the reading itself.
///
/// The request addresses the venue rather than a route: the reading it belongs to
/// covers every scope the venue may publish, and it is dialled on the venue's own
/// primary scope's origin. A failure is therefore reported against the venue.
///
/// Every request is made against the process's
/// `budget::info_budget`, so a burst is spread
/// over the minutes the venue's allowance needs rather than sent as one. A refusal
/// is retried while it keeps saying the allowance is what was missing: `429` is the
/// venue's own answer that nothing was served, which a second attempt can be
/// answered for once the wait has passed. Any other refusal is the caller's to see.
pub(crate) async fn post_hyperliquid_info<T>(
    http: &reqwest::Client,
    origin: &Url,
    venue: &VenueId,
    request: &HyperliquidInfoRequest,
) -> Result<T, MarketParticipantError>
where
    T: serde::de::DeserializeOwned,
{
    let endpoint = origin
        .join("info")
        .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{error}")))?;
    let body = HyperliquidCodec.encode(request)?;
    let weight = Weight::of(request.weight());
    let mut attempt = 0_u32;
    loop {
        // The reservation returns its weight unless the venue served this request.
        let mut spent = Spent::reserve(weight).await;
        let response = http
            .post(endpoint.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.clone())
            .send()
            .await
            .map_err(|error| MarketParticipantError::VenueRequestFailed {
                venue: *venue,
                endpoint: format_smolstr!("{endpoint}"),
                source: network::error::NetworkError::Io {
                    message: format_smolstr!("{error}"),
                },
            })?;
        let status = response.status();
        let retry_after = info_retry_after(response.headers());
        let body =
            response
                .bytes()
                .await
                .map_err(|error| MarketParticipantError::DecodeFailed {
                    operation: "read a Hyperliquid info response".into(),
                    raw_data: Bytes::from(error.to_string()),
                })?;
        if status.is_success() {
            spent.served();
            return serde_json::from_slice(&body).map_err(|_| {
                MarketParticipantError::DecodeFailed {
                    operation: "decode a Hyperliquid info response".into(),
                    raw_data: body,
                }
            });
        }
        let error = crate::adapter::error::http_response_error_with_wait(
            crate::adapter::error::FailureTarget::Venue(venue),
            &format_smolstr!("{endpoint}"),
            status,
            retry_after.map(|wait| u64::try_from(wait.as_millis()).unwrap_or(u64::MAX)),
            body,
        );
        // The venue served nothing, so the weight this request reserved is still this
        // process's to spend — and the guard has already returned it for this attempt.
        if !error.is_rate_limit() || attempt == INFO_RETRY_ATTEMPTS {
            return Err(error);
        }
        let wait = retry_after.unwrap_or_else(|| info_retry_backoff(attempt));
        attempt += 1;
        tokio::time::sleep(wait).await;
    }
}

/// The backoff before one retry of a rate-limited `info` request.
///
/// The wait is drawn from a band rather than fixed, so two requests refused by the
/// same exhausted allowance do not come back at the same instant: the band doubles
/// with every attempt, and [`INFO_RETRY_ATTEMPTS`] attempts put the longest wait
/// before the last one at twice [`INFO_RETRY_BASE`] to the power of the attempts.
fn info_retry_backoff(attempt: u32) -> Duration {
    let base = INFO_RETRY_BASE.saturating_mul(1_u32 << attempt.min(INFO_RETRY_ATTEMPTS));
    let jitter = rand::random_range(0..=base.as_millis() as u64);
    base + Duration::from_millis(jitter)
}

/// Returns the listing key of one prepared request.
pub(super) fn listing_key(
    prepared: &PreparedRequest,
) -> Result<VenueListingKey, MarketParticipantError> {
    prepared
        .listing()
        .map(|listing| listing.key.clone())
        .ok_or_else(|| {
            protocol_error(
                "encode a Hyperliquid listing request",
                SmolStr::new_static("the request carries no resolved listing"),
            )
        })
}

/// The error of a capability this adapter does not serve on this route.
pub(super) fn unsupported(context: &ExecutionContext) -> MarketParticipantError {
    MarketParticipantError::UnsupportedCapability {
        route: context.route(),
        capability: context.capability(),
    }
}

/// The key one request's connection is opened or reused by.
///
/// Sharing one socket between the wallet scopes of this venue is entirely a property of
/// this key, so the key is built in one place and exercised as the adapter builds it
/// rather than restated by a test fixture.
///
/// The endpoint is the configuration's stream address as it stands: this venue publishes the
/// stream under a path of its own, so the address the operator chose *is* the stream endpoint
/// and nothing is appended to it.
///
/// The protocol is built beside the key because the key is built from it: the upgrade
/// headers a connection declares are one value, and the key that decides whether a socket
/// may be reused reads them off the protocol that dials.
fn connection_key(
    context: &ExecutionContext,
    request: &ProtocolRequest,
) -> Result<ConnectionKey, MarketParticipantError> {
    let endpoint = context.websocket_origin().clone();
    let route = shared_socket_route(&context.route(), context.capability());
    Ok(ConnectionKey::for_route(
        endpoint,
        context.proxy.clone(),
        connection_auth(context, request)?,
        route.clone(),
        shared_socket_generation(context, &route)?,
        &connection_protocol(),
    ))
}

/// The protocol every connection of this venue dials with.
///
/// The key of a connection is built from this, so the protocol of a key built for a test
/// is the protocol production dials: a fixture that restated it could declare one set of
/// upgrade headers and dial another.
fn connection_protocol() -> ConnectionProtocol {
    ConnectionProtocol::new(HyperliquidCodec::extract_response_id)
        .with_keepalive(super::super::heartbeat::KEEPALIVE)
        .with_liveness_timeout(super::super::heartbeat::LIVENESS_TIMEOUT)
}

/// The route one connection is keyed by.
///
/// Every wallet-monitor connection of this venue is keyed by the canonical wallet
/// scope instead of the scope that opened it, which is what makes one monitored
/// address read through one socket rather than one per deployed DEX. Only the key
/// is folded: the origin and proxy the request was resolved with stay its own
/// route's, so two scopes that resolve different transport still dial separate
/// sockets — a socket dialled through one proxy cannot serve a request that must use
/// another — and every other capability keeps its own scope in the key because its
/// socket serves different wire parameters over the same origin.
fn shared_socket_route(route: &VenueRoute, capability: CapabilityId) -> VenueRoute {
    if capability != CapabilityId::Wallet {
        return route.clone();
    }
    VenueRoute::new(
        route.venue,
        VenueScopeId::new(HYPERLIQUID_WALLET_SCOPE)
            .expect("the canonical wallet scope is a valid scope identifier"),
    )
}

/// The route generation one connection is fenced by.
///
/// A folded wallet connection is fenced by the canonical scope's route, because that
/// is the target its key names and the one the pool re-reads when it decides whether
/// a connection is still current. The opening scope's generation cannot be used: a
/// target a proxy of its own was ever configured for carries a generation number of
/// its own, so two scopes that resolve the very same route would still differ — which
/// would both keep their wallet sockets apart and make the pool close a healthy socket
/// as soon as any snapshot is published.
///
/// The generation is read from the runtime snapshot the adapter was given, and a
/// connection without one is refused rather than keyed with the opening scope's
/// generation: such a key would name the canonical target while carrying a number the
/// pool never compares it against, so the first snapshot would close it.
fn shared_socket_generation(
    context: &ExecutionContext,
    route: &VenueRoute,
) -> Result<NetworkRouteGeneration, MarketParticipantError> {
    if context.capability() != CapabilityId::Wallet {
        return Ok(context.network_route_generation());
    }
    let runtime = context.current_runtime().ok_or_else(|| {
        MarketParticipantError::Internal(
            "the execution context of a wallet connection carries no runtime snapshot, so the \
             route generation of this venue's canonical wallet scope is unknown"
                .into(),
        )
    })?;
    Ok(runtime.network_route_generation(&route.venue))
}

/// The authentication identity one connection is keyed by.
///
/// A public wallet read and a route-level public stream never contribute a trading
/// credential: their target names no account, so the socket they use is the public
/// one even if the execution context were to carry an identity.
fn connection_auth(
    context: &ExecutionContext,
    request: &ProtocolRequest,
) -> Result<Option<SmolStr>, MarketParticipantError> {
    if matches!(
        request.wallet_target(),
        Some(WalletTarget::Public { .. } | WalletTarget::Route { .. })
    ) {
        return Ok(None);
    }
    if context.capability() == CapabilityId::AuthenticatedTrading {
        let Some(auth) = context.auth.as_ref() else {
            return Err(MarketParticipantError::AuthenticationFailed {
                route: context.route(),
                account: None,
                source: crate::error::AuthError::AccountIdentityMissing,
            });
        };
        return Ok(Some(auth.account_id().into()));
    }
    Ok(context.auth.as_ref().map(|auth| auth.account_id().into()))
}

/// What the settings say about this venue's credentials.
///
/// A refused set is not carried further than this: the venue is this adapter's own, and
/// the error a user is shown names the venue rather than the shape the settings store.
enum CredentialLookup {
    Configured(Eip712Signer),
    Unconfigured,
    Invalid,
}

/// Resolves the HyperCore credentials one scope signs with.
///
/// The venue is the credential identity: its own settings row holds the structure its
/// authenticated requests sign with, so nothing here searches for a set of the right
/// shape, and the scope decides only which of the venue's scopes is asked — every one
/// of them answers the one set the venue stores.
fn hyperliquid_credentials(
    config: &MarketParticipantNetworkSnapshot,
    scope: &VenueScopeId,
) -> CredentialLookup {
    let venue = VenueId::Hyperliquid;
    match config.resolve_hyperliquid_credentials(&venue, scope) {
        crate::runtime::CredentialState::Configured(credentials) => {
            CredentialLookup::Configured(Eip712Signer::new(credentials.api_agent_private_key))
        }
        crate::runtime::CredentialState::Unconfigured => CredentialLookup::Unconfigured,
        crate::runtime::CredentialState::Invalid(_) => CredentialLookup::Invalid,
    }
}

/// Returns the venue metadata of one route's scope.
///
/// The metadata is the reading the commit that admitted this request published: it
/// is handed to the execution context, never fetched here, so a request path that
/// reads metadata cannot reach the venue. A scope the loaded venue's reading
/// did not cover is refused — the admission that let this execution through is the
/// only thing that could have covered it.
fn scope_metadata<'a>(
    context: &'a ExecutionContext,
    route: &VenueRoute,
) -> Result<&'a ScopeMetadata, MarketParticipantError> {
    context
        .venue_metadata()
        .and_then(|metadata| metadata.scope(route))
        .ok_or_else(|| unsupported(context))
}

/// Validates a built-in Hyperliquid scope name.
fn built_in_scope(name: &'static str) -> VenueScopeId {
    VenueScopeId::new(name).expect("a built-in Hyperliquid scope name is canonical")
}

/// Pairs one route's universe names with the contexts that arrived beside them.
fn context_map(
    names: impl Iterator<Item = SmolStr>,
    contexts: Vec<HyperliquidActiveContextValues>,
) -> RouteContexts {
    names.zip(contexts).collect()
}

/// Returns the wire DEX name a scope addresses, rejecting a scope that is not one.
pub(super) fn perp_dex_wire_name(
    scope: &VenueScopeId,
) -> Result<Option<SmolStr>, MarketParticipantError> {
    if scope.as_str() == CORE_PERP_SCOPE {
        return Ok(None);
    }
    match catalog::perp_dex_name(scope) {
        Some(name) => Ok(Some(SmolStr::new(name))),
        None => Err(MarketParticipantError::UnsupportedCapability {
            route: VenueRoute::new(VenueId::Hyperliquid, scope.clone()),
            capability: CapabilityId::MarketCatalog,
        }),
    }
}

/// Looks up the position of one DEX in the venue's own `perpDexs` answer.
///
/// The name must be one the venue itself published as a deployment: a name the
/// table does not carry is not a DEX this adapter may read. Whether the name has a
/// scope shape this build reserves is [`catalog::perp_dex_scope`]'s answer, which
/// the reading asks before it registers anything.
fn dex_index(
    dexes: &[Option<HyperliquidPerpDex>],
    name: &str,
) -> Result<u32, MarketParticipantError> {
    if !catalog::deployed_perp_dexes(dexes)
        .iter()
        .any(|deployed| deployed == name)
    {
        return Err(protocol_error(
            "resolve a Hyperliquid perpetual DEX",
            format_smolstr!("the venue publishes no DEX named {name}"),
        ));
    }
    dexes
        .iter()
        .position(|dex| dex.as_ref().is_some_and(|dex| dex.name == name))
        .and_then(|index| u32::try_from(index).ok())
        .ok_or_else(|| {
            protocol_error(
                "resolve a Hyperliquid perpetual DEX",
                format_smolstr!("the DEX {name} has no published index"),
            )
        })
}

/// Resolves the token name one DEX settles in.
///
/// The DEX metadata publishes the collateral as a spot token index, and the token
/// list is what maps that index to a name; a DEX whose collateral cannot be
/// resolved publishes no listing this adapter may describe.
fn collateral_name(
    spot: &HyperliquidSpotMetaResponse,
    meta: &HyperliquidMetaResponse,
) -> Result<SmolStr, MarketParticipantError> {
    let index = meta.collateral_token.ok_or_else(|| {
        protocol_error(
            "decode a Hyperliquid settlement asset",
            SmolStr::new_static("the perpetual metadata publishes no collateral token"),
        )
    })?;
    spot.tokens
        .iter()
        .find(|token| token.index == index)
        .map(|token| token.name.clone())
        .ok_or_else(|| {
            protocol_error(
                "decode a Hyperliquid settlement asset",
                format_smolstr!("the collateral token index {index} is not a spot token"),
            )
        })
}

/// Returns whether one DEX metadata answer is complete enough to register.
///
/// A builder-deployed DEX becomes a scope only when its metadata decodes, publishes
/// at least one asset, names a collateral token that resolves to a spot token and
/// names every one of its coins in the `{dex}:{coin}` form the venue reserves for
/// builder-deployed assets. Anything this adapter cannot verify is refused instead
/// of being guessed at.
pub(crate) fn is_verified_perp_dex(
    name: &str,
    perp_dex_index: u32,
    meta: &HyperliquidMetaResponse,
    spot: &HyperliquidSpotMetaResponse,
) -> bool {
    if meta.universe.is_empty() {
        return false;
    }
    let Some(collateral) = meta.collateral_token else {
        return false;
    };
    if !spot.tokens.iter().any(|token| token.index == collateral) {
        return false;
    }
    meta.universe.iter().enumerate().all(|(index, asset)| {
        let Ok(index_in_meta) = u32::try_from(index) else {
            return false;
        };
        catalog::perp_asset_id(perp_dex_index, index_in_meta).is_ok()
            && asset
                .name
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with(':'))
            && asset.max_leverage > 0
    })
}

/// Derives every listing of one spot route.
fn spot_listings(
    route: &VenueRoute,
    meta: &HyperliquidSpotMetaResponse,
) -> Result<RouteListings, MarketParticipantError> {
    let mut listings = BTreeMap::new();
    for pair in &meta.universe {
        let asset_id = catalog::spot_asset_id(pair.index)?;
        let key = catalog::listing_key(route, catalog::spot_continuity(&route.scope, asset_id))?;
        listings.insert(pair.name.clone(), key);
    }
    Ok(Arc::new(listings))
}

/// Derives every listing of one perpetual route.
fn perpetual_listings(
    route: &VenueRoute,
    meta: &PerpRouteMeta,
) -> Result<RouteListings, MarketParticipantError> {
    let mut listings = BTreeMap::new();
    for (index, asset) in meta.meta.universe.iter().enumerate() {
        let index_in_meta = u32::try_from(index).map_err(|_| {
            protocol_error(
                "decode a Hyperliquid perpetual asset ID",
                SmolStr::new_static("the universe index exceeds u32"),
            )
        })?;
        let asset_id = catalog::perp_asset_id(meta.perp_dex_index, index_in_meta)?;
        let key =
            catalog::listing_key(route, catalog::perpetual_continuity(&route.scope, asset_id))?;
        listings.insert(asset.name.clone(), key);
    }
    Ok(Arc::new(listings))
}

/// Builds the catalog entries of one spot route.
fn spot_catalog_entries(
    route: &VenueRoute,
    meta: &HyperliquidSpotMetaResponse,
) -> Result<Vec<NativeCatalogEntry>, MarketParticipantError> {
    let mut entries = Vec::with_capacity(meta.universe.len());
    for pair in &meta.universe {
        let (base, quote, base_token_id, quote_token_id) =
            catalog::spot_assets(&meta.tokens, pair.tokens, "decode a Hyperliquid spot pair")?;
        let base_token = meta
            .tokens
            .iter()
            .find(|token| token.index == pair.tokens[0])
            .expect("the base token was just resolved");
        let asset_id = catalog::spot_asset_id(pair.index)?;
        let (instrument, asset_terms, reference_terms) = catalog::spot_instrument(base, quote)?;
        let native = native_symbol(&pair.name, "decode a Hyperliquid spot coin")?;
        entries.push(NativeCatalogEntry {
            listing: catalog::listing_key(route, catalog::spot_continuity(&route.scope, asset_id))?,
            native_symbol: native.clone(),
            aliases: spot_aliases(base, quote, &native)?,
            identifiers: VenueListingIdentifiers::Hyperliquid(catalog::spot_identifiers(
                asset_id,
                base_token_id,
                quote_token_id,
            )),
            rules: spot_rules(&instrument, base_token.sz_decimals)?,
            status: ListingStatus::Trading,
            available_from: None,
            available_until: None,
            instrument,
            asset_terms,
            reference_terms,
            // Every asset these listings publish states its own display code.
            asset_codes: Vec::new(),
        });
    }
    Ok(entries)
}

/// Builds the catalog entries of one perpetual route.
fn perpetual_catalog_entries(
    route: &VenueRoute,
    meta: &PerpRouteMeta,
) -> Result<Vec<NativeCatalogEntry>, MarketParticipantError> {
    // A builder-deployed DEX names its own assets `{dex}:{coin}`. That prefix is
    // the DEX's namespace rather than part of the asset, so the canonical identity
    // is built from the coin alone and the venue's own name stays the listing's
    // native symbol and its listing identity. This is what makes one coin of two
    // routes the same economic instrument, distinguished by the route it is
    // published under, while the route never enters the economic name.
    let dex = perp_dex_wire_name(&route.scope)?;
    let prefix = dex.map(|name| format_smolstr!("{name}:"));
    let mut entries = Vec::with_capacity(meta.meta.universe.len());
    for (index, asset) in meta.meta.universe.iter().enumerate() {
        let index_in_meta = u32::try_from(index).map_err(|_| {
            protocol_error(
                "decode a Hyperliquid perpetual asset ID",
                SmolStr::new_static("the universe index exceeds u32"),
            )
        })?;
        let asset_id = catalog::perp_asset_id(meta.perp_dex_index, index_in_meta)?;
        let coin = match &prefix {
            Some(prefix) => asset.name.strip_prefix(prefix.as_str()).ok_or_else(|| {
                protocol_error(
                    "decode a Hyperliquid perpetual coin",
                    format_smolstr!("{} is not an asset of the DEX it was read from", asset.name),
                )
            })?,
            None => asset.name.as_str(),
        };
        let (instrument, asset_terms, reference_terms) =
            catalog::perpetual_instrument(coin, &meta.collateral)?;
        entries.push(NativeCatalogEntry {
            listing: catalog::listing_key(
                route,
                catalog::perpetual_continuity(&route.scope, asset_id),
            )?,
            native_symbol: native_symbol(&asset.name, "decode a Hyperliquid perpetual coin")?,
            aliases: Vec::new(),
            identifiers: VenueListingIdentifiers::Hyperliquid(catalog::perpetual_identifiers(
                &route.scope,
                asset_id,
                asset.name.clone(),
            )?),
            rules: perpetual_rules(asset, &instrument, &meta.collateral, &meta.meta)?,
            status: if asset.is_delisted {
                ListingStatus::Delisted
            } else {
                ListingStatus::Trading
            },
            available_from: None,
            available_until: None,
            instrument,
            asset_terms,
            reference_terms,
            // Every asset these listings publish states its own display code.
            asset_codes: Vec::new(),
        });
    }
    Ok(entries)
}

/// The display aliases of one spot listing, which never carry its identity.
fn spot_aliases(
    base: &str,
    quote: &str,
    native: &NativeSymbol,
) -> Result<Vec<NativeSymbol>, MarketParticipantError> {
    let alias = format_smolstr!("{base}/{quote}");
    if alias == native.as_str() {
        return Ok(Vec::new());
    }
    Ok(vec![native_symbol(
        &alias,
        "decode a Hyperliquid spot alias",
    )?])
}

/// One price tick of a Hyperliquid market.
///
/// Hyperliquid bounds a price to six decimals on a perpetual and eight on a spot
/// pair, less the asset's own size precision.
fn price_tick(sz_decimals: u8, spot: bool) -> Result<Tick, MarketParticipantError> {
    let maximum = if spot { 8_u8 } else { 6_u8 };
    let decimals = maximum.checked_sub(sz_decimals).ok_or_else(|| {
        protocol_error(
            "decode a Hyperliquid price precision",
            SmolStr::new_static("the asset trades in more decimals than a price may carry"),
        )
    })?;
    tick(decimals)
}

/// The tick of `decimal_places` decimals.
fn tick(decimal_places: u8) -> Result<Tick, MarketParticipantError> {
    if decimal_places > 18 {
        return Err(protocol_error(
            "decode a Hyperliquid tick",
            SmolStr::new_static("the decimal precision exceeds eighteen places"),
        ));
    }
    Tick::from_inner(10_u128.pow(u32::from(18 - decimal_places)))
        .map_err(|error| protocol_error("decode a Hyperliquid tick", error))
}

/// The quantity step of `decimal_places` decimals.
fn step(decimal_places: u8) -> Result<Quantity, MarketParticipantError> {
    Ok(Quantity::from_inner(tick(decimal_places)?.inner()))
}

/// The trading rules of one spot listing.
fn spot_rules(
    instrument: &Instrument,
    sz_decimals: u8,
) -> Result<TradingRules, MarketParticipantError> {
    let (quote_asset, basis) =
        market_participants_model::listing::navigation::expected_price_terms(instrument)
            .map_err(model_error)?;
    let InstrumentTerms::Spot(terms) = instrument.terms() else {
        return Err(product_mismatch(instrument));
    };
    Ok(TradingRules {
        common: CommonTradingRules {
            price: PriceRule {
                quote_asset,
                basis,
                tick: price_tick(sz_decimals, true)?,
                minimum: None,
                maximum: None,
            },
            quantity: QuantityRule {
                unit: QuantityUnit::AssetUnits {
                    asset: terms.base().clone(),
                },
                step: step(sz_decimals)?,
                minimum: None,
                maximum: None,
            },
            notional: None,
            venue_policy: Arc::new(VenueTradingPolicy::continuous()),
        },
        product: Arc::new(ProductTradingRules::Spot(
            SpotTradingRules::new(
                LotPolicy::FractionalAllowed {
                    lot_size: step(sz_decimals)?,
                    minimum_fraction: None,
                },
                ShortSalePolicy::NotApplicable,
            )
            .map_err(model_error)?,
        )),
    })
}

/// The trading rules of one perpetual listing.
fn perpetual_rules(
    asset: &HyperliquidMetaAsset,
    instrument: &Instrument,
    collateral: &str,
    meta: &HyperliquidMetaResponse,
) -> Result<TradingRules, MarketParticipantError> {
    let (quote_asset, basis) =
        market_participants_model::listing::navigation::expected_price_terms(instrument)
            .map_err(model_error)?;
    let InstrumentTerms::Perpetual(terms) = instrument.terms() else {
        return Err(product_mismatch(instrument));
    };
    let base_asset = terms
        .underlying()
        .reference()
        .asset()
        .cloned()
        .ok_or_else(|| product_mismatch(instrument))?;
    let collateral_asset = catalog::token_asset(collateral)?.asset_id();
    let (initial_margin, maintenance_margin) = catalog::margin_fractions(asset.max_leverage)?;
    Ok(TradingRules {
        common: CommonTradingRules {
            price: PriceRule {
                quote_asset,
                basis,
                tick: price_tick(asset.sz_decimals, false)?,
                minimum: None,
                maximum: None,
            },
            quantity: QuantityRule {
                unit: QuantityUnit::AssetUnits { asset: base_asset },
                step: step(asset.sz_decimals)?,
                minimum: None,
                maximum: None,
            },
            notional: None,
            venue_policy: Arc::new(VenueTradingPolicy::continuous()),
        },
        product: Arc::new(ProductTradingRules::Perpetual(PerpetualTradingRules {
            risk: PerpetualRisk::Published(
                PerpetualMarginTerms::new(
                    MarginProfile::new(
                        hyperliquid_margin_mode(asset),
                        initial_margin,
                        maintenance_margin,
                        collateral_asset.clone(),
                    )
                    .map_err(model_error)?,
                    leverage_tiers(asset, meta)?,
                )
                .map_err(model_error)?,
            ),
            funding: FundingSchedule {
                interval: FundingInterval::Fixed {
                    interval_millis: FUNDING_INTERVAL_MILLIS,
                },
                funding_rate_cap: Some(catalog::funding_rate_cap()?),
                funding_rate_floor: None,
                asset: collateral_asset,
            },
            open_interest_cap: None,
        })),
    })
}

/// The margin accounting mode one asset's published rule maps to.
///
/// Hyperliquid distinguishes "strict isolated" (margin cannot be removed) from
/// "no cross" (isolated with margin removal), and the model has one isolated mode,
/// so both map onto it rather than being restated as a mode the model cannot
/// express.
fn hyperliquid_margin_mode(asset: &HyperliquidMetaAsset) -> MarginMode {
    match asset.margin_mode {
        Some(HyperliquidMarginMode::StrictIsolated | HyperliquidMarginMode::NoCross) => {
            MarginMode::Isolated
        }
        Some(HyperliquidMarginMode::Normal) | None => MarginMode::Cross,
    }
}

/// The leverage tiers one asset's margin table publishes.
///
/// A tier's notional ceiling is the *next* published lower bound, which is the
/// venue's own number; the last tier has no published ceiling, so it reaches the
/// largest notional the model can express rather than an invented one. An asset
/// without a published table keeps its own maximum leverage at every notional.
fn leverage_tiers(
    asset: &HyperliquidMetaAsset,
    meta: &HyperliquidMetaResponse,
) -> Result<Vec<LeverageTier>, MarketParticipantError> {
    let table = asset.margin_table_id.and_then(|id| {
        meta.margin_tables
            .iter()
            .find(|entry| entry.0 == id)
            .map(|entry| &entry.1)
    });
    let Some(table) = table.filter(|table| !table.margin_tiers.is_empty()) else {
        return single_leverage_tier(asset.max_leverage);
    };
    let mut tiers = Vec::with_capacity(table.margin_tiers.len());
    for (index, tier) in table.margin_tiers.iter().enumerate() {
        let max_leverage = if index == 0 {
            asset.max_leverage
        } else {
            tier.max_leverage
        };
        if max_leverage == 0 {
            continue;
        }
        let notional_cap = match table.margin_tiers.get(index + 1) {
            Some(next) => {
                match catalog::parse_notional(&next.lower_bound, "decode a Hyperliquid margin tier")
                {
                    Ok(notional) if !notional.is_zero() => notional,
                    _ => catalog::unbounded_notional(),
                }
            }
            None => catalog::unbounded_notional(),
        };
        let (_, maintenance_margin) = catalog::margin_fractions(max_leverage)?;
        tiers.push(
            LeverageTier::new(notional_cap, max_leverage, maintenance_margin)
                .map_err(model_error)?,
        );
    }
    tiers.sort_by_key(|tier| tier.notional_cap);
    if tiers.is_empty()
        || tiers
            .windows(2)
            .any(|pair| pair[0].notional_cap >= pair[1].notional_cap)
    {
        return single_leverage_tier(asset.max_leverage);
    }
    Ok(tiers)
}

/// The one leverage tier of an asset whose venue publishes no usable table.
fn single_leverage_tier(max_leverage: u32) -> Result<Vec<LeverageTier>, MarketParticipantError> {
    let (_, maintenance_margin) = catalog::margin_fractions(max_leverage)?;
    Ok(vec![
        LeverageTier::new(
            catalog::unbounded_notional(),
            max_leverage,
            maintenance_margin,
        )
        .map_err(model_error)?,
    ])
}

/// Reads one native symbol the venue published.
fn native_symbol(
    value: &str,
    operation: &'static str,
) -> Result<NativeSymbol, MarketParticipantError> {
    NativeSymbol::new(value).map_err(|error| protocol_error(operation, error))
}

/// The error of an instrument that is not of this route's product.
fn product_mismatch(instrument: &Instrument) -> MarketParticipantError {
    protocol_error(
        "build Hyperliquid trading rules",
        format_smolstr!(
            "{} is not the product this route publishes",
            instrument.kind()
        ),
    )
}

/// The time in force one order command encodes with.
pub(super) fn encode_time_in_force(
    time_in_force: TimeInForce,
) -> Result<SmolStr, MarketParticipantError> {
    match time_in_force {
        TimeInForce::GoodTillCancelled => Ok(SmolStr::new_static("Gtc")),
        TimeInForce::ImmediateOrCancel => Ok(SmolStr::new_static("Ioc")),
        TimeInForce::PostOnly => Ok(SmolStr::new_static("Alo")),
        TimeInForce::FillOrKill => Err(MarketParticipantError::Configuration(
            "Hyperliquid does not support fill-or-kill orders".into(),
        )),
        TimeInForce::GoodTillDate(_) => Err(MarketParticipantError::Configuration(
            "Hyperliquid does not support good-till-date orders".into(),
        )),
    }
}

/// The price one market order reaches for on one side.
pub(super) fn aggressive_market_price(
    reference: Price,
    side: Side,
    slippage: Ratio,
) -> Result<Price, MarketParticipantError> {
    let multiplier = match side {
        Side::Buy => Ratio::from(1_u8).checked_add(slippage),
        Side::Sell => Ratio::from(1_u8).checked_sub(slippage),
    }
    .ok_or_else(|| {
        protocol_error(
            "prepare a Hyperliquid market order",
            SmolStr::new_static("the market slippage price overflowed"),
        )
    })?;
    reference.checked_mul_ratio(multiplier).ok_or_else(|| {
        protocol_error(
            "prepare a Hyperliquid market order",
            SmolStr::new_static("the aggressive market price overflowed"),
        )
    })
}

/// The window one candlestick request covers, in milliseconds.
///
/// The venue answers with every candle inside the requested range and applies its
/// own cap, so an open-ended start would ask for the whole history and the caller's
/// `limit` would then cut an arbitrary slice out of it. The window is `limit`
/// intervals wide, which is one interval wider than the page the caller asked for,
/// because the venue answers on its own grid and includes the candle the window's
/// start falls inside.
pub(super) fn candle_request_window(
    timeframe: TimeFrame,
    limit: u16,
    start_timestamp: Option<i64>,
    end_timestamp: Option<i64>,
    now: i64,
) -> (i64, i64) {
    let interval_millis = match fixed_interval_millis(timeframe) {
        Some(millis) => millis,
        None => MONTH_MILLIS.saturating_mul(calendar_months(timeframe)),
    };
    let span = interval_millis.saturating_mul(i64::from(limit));
    let end = end_timestamp.unwrap_or(now);
    match start_timestamp {
        Some(start) => (start, end.min(start.saturating_add(span))),
        None => (end.saturating_sub(span), end),
    }
}

/// The length of one fixed candle interval in milliseconds.
fn fixed_interval_millis(timeframe: TimeFrame) -> Option<i64> {
    Some(match timeframe {
        TimeFrame::Second1 => 1_000,
        TimeFrame::Minute1 => 60_000,
        TimeFrame::Minute2 => 120_000,
        TimeFrame::Minute3 => 180_000,
        TimeFrame::Minute5 => 300_000,
        TimeFrame::Minute10 => 600_000,
        TimeFrame::Minute15 => 900_000,
        TimeFrame::Minute20 => 1_200_000,
        TimeFrame::Minute30 => 1_800_000,
        TimeFrame::Minute45 => 2_700_000,
        TimeFrame::Hour1 => 3_600_000,
        TimeFrame::Hour2 => 7_200_000,
        TimeFrame::Hour3 => 10_800_000,
        TimeFrame::Hour4 => 14_400_000,
        TimeFrame::Hour6 => 21_600_000,
        TimeFrame::Hour8 => 28_800_000,
        TimeFrame::Hour12 => 43_200_000,
        TimeFrame::Day1 => 86_400_000,
        TimeFrame::Day2 => 172_800_000,
        TimeFrame::Day3 => 259_200_000,
        TimeFrame::Week1 => 604_800_000,
        // A calendar month, quarter or year is not a fixed length, so it is bounded
        // by the longest one instead.
        TimeFrame::Month1 | TimeFrame::Month3 | TimeFrame::Quarter1 | TimeFrame::Year1 => {
            return None;
        }
    })
}

/// How many calendar months one interval spans.
const fn calendar_months(timeframe: TimeFrame) -> i64 {
    match timeframe {
        TimeFrame::Month1 => 1,
        TimeFrame::Month3 | TimeFrame::Quarter1 => 3,
        TimeFrame::Year1 => 12,
        _ => 0,
    }
}

/// The current wall-clock time in Unix milliseconds.
pub(super) fn current_timestamp_millis() -> Result<i64, MarketParticipantError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| {
            MarketParticipantError::Internal("the system clock is before the Unix epoch".into())
        })?
        .as_millis();
    i64::try_from(millis)
        .map_err(|_| MarketParticipantError::Internal("a Hyperliquid timestamp exceeds i64".into()))
}

/// The address one wallet request reads.
///
/// A public target reads the address it names; a route-level target names no wallet at
/// all, and says so rather than being read as some account.
pub(super) fn wallet_user(
    target: &WalletTarget,
    context: &ExecutionContext,
) -> Result<SmolStr, MarketParticipantError> {
    let _ = context;
    match target {
        WalletTarget::Public { address, .. } => Ok(SmolStr::new(address.as_str())),
        WalletTarget::Route { .. } => Err(protocol_error(
            "read a Hyperliquid wallet",
            SmolStr::new_static("a route-level target names no wallet"),
        )),
    }
}

/// The wallet address one wallet request is identified by.
pub(super) fn wallet_address(
    target: &WalletTarget,
    context: &ExecutionContext,
) -> Result<WalletAddress, MarketParticipantError> {
    let user = wallet_user(target, context)?;
    user.parse::<WalletAddress>()
        .map_err(|error| protocol_error("decode a Hyperliquid wallet address", error))
}

/// The wire DEX name one wallet request reads.
///
/// A spot read is not DEX-scoped: the first perpetual DEX is where the venue
/// publishes spot orders, so the field stays absent there.
pub(super) fn wallet_dex(
    _target: &WalletTarget,
    route: &VenueRoute,
) -> Result<Option<SmolStr>, MarketParticipantError> {
    if route.scope.as_str() == CORE_SPOT_SCOPE {
        return Ok(None);
    }
    perp_dex_wire_name(&route.scope)
}

/// The native sequence of one order book watermark.
fn native_sequence(timestamp: i64) -> Result<MarketSequence, MarketParticipantError> {
    IdentifierValue::new(format_smolstr!("{timestamp}"))
        .map(MarketSequence::Native)
        .map_err(|error| protocol_error("decode a Hyperliquid order book sequence", error))
}

/// Decodes one order book side.
fn decode_book_levels(
    levels: Vec<HyperliquidBookLevel>,
    depth: u16,
) -> Result<Vec<NativeOrderBookLevel>, MarketParticipantError> {
    levels
        .into_iter()
        .take(usize::from(depth))
        .map(|level| {
            Ok(NativeOrderBookLevel {
                price: parse_price(&level.px, "decode a Hyperliquid order book price")?,
                quantity: parse_quantity(&level.sz, "decode a Hyperliquid order book quantity")?,
            })
        })
        .collect()
}

/// Turns one model failure into the market-participant error that carries it.
pub(super) fn model_error(
    error: market_participants_model::error::ModelError,
) -> MarketParticipantError {
    MarketParticipantError::MarketModel(Box::new(error))
}

/// Turns one protocol-shape failure into a typed protocol error.
pub(super) fn protocol_error(
    operation: &'static str,
    reason: impl std::fmt::Display,
) -> MarketParticipantError {
    MarketParticipantError::ProtocolError {
        operation: operation.into(),
        reason: format_smolstr!("{reason}"),
        raw_data: None,
    }
}

/// Reads one price the venue published as a decimal string.
pub(super) fn parse_price(
    value: &str,
    operation: &'static str,
) -> Result<Price, MarketParticipantError> {
    value
        .parse()
        .map_err(|error| protocol_error(operation, error))
}

/// Reads one quantity the venue published as a decimal string.
fn parse_quantity(
    value: &str,
    operation: &'static str,
) -> Result<Quantity, MarketParticipantError> {
    value
        .parse()
        .map_err(|error| protocol_error(operation, error))
}

/// Reads one signed quantity the venue published as a decimal string.
fn parse_signed_quantity(
    value: &str,
    operation: &'static str,
) -> Result<SignedQuantity, MarketParticipantError> {
    value
        .parse()
        .map_err(|error| protocol_error(operation, error))
}

/// Reads one profit or loss the venue published as a decimal string.
fn parse_profit(
    value: &str,
    operation: &'static str,
) -> Result<ProfitOrLoss, MarketParticipantError> {
    value
        .parse()
        .map_err(|error| protocol_error(operation, error))
}

/// Reads one optional price the venue published.
fn optional_price(
    value: &Option<SmolStr>,
    operation: &'static str,
) -> Result<Option<Price>, MarketParticipantError> {
    value
        .as_deref()
        .map(|value| parse_price(value, operation))
        .transpose()
}

/// Reads one optional quantity the venue published.
fn optional_quantity(
    value: &Option<SmolStr>,
    operation: &'static str,
) -> Result<Option<Quantity>, MarketParticipantError> {
    value
        .as_deref()
        .map(|value| parse_quantity(value, operation))
        .transpose()
}

/// Decodes one side of a trade, an order or a position.
pub(super) fn decode_side(
    value: &str,
    operation: &'static str,
) -> Result<Side, MarketParticipantError> {
    if value.eq_ignore_ascii_case("B") {
        Ok(Side::Buy)
    } else if value.eq_ignore_ascii_case("A") {
        Ok(Side::Sell)
    } else {
        Err(protocol_error(
            operation,
            format_smolstr!("unsupported side {value:?}"),
        ))
    }
}

/// One instrument's ticker, read from the asset context the venue publishes.
///
/// Hyperliquid reports no open, high or low, so the change a row measures is taken
/// from `prevDayPx` — the price the venue references a day back — and the volumes
/// are the notional it traded and the base amount, which the venue publishes for
/// some markets only. A context without the base amount derives it from the
/// notional at the current price, which is that same day estimated rather than the
/// venue's own tally.
fn decode_ticker(
    context: &HyperliquidActiveContextValues,
    payload: Option<&[u8]>,
) -> Result<NativeTicker, MarketParticipantError> {
    let Some(close) = context.mid_px.as_deref().or(context.mark_px.as_deref()) else {
        return Err(MarketParticipantError::ProtocolError {
            operation: "decode a Hyperliquid ticker price".into(),
            reason: SmolStr::new_static(
                "the asset context published neither a mid nor a mark price",
            ),
            raw_data: payload.map(Bytes::copy_from_slice),
        });
    };
    let close = parse_price(close, "decode a Hyperliquid ticker price")?;
    let open = context
        .prev_day_px
        .as_deref()
        .map(|value| parse_price(value, "decode a Hyperliquid ticker open price"))
        .transpose()?
        .unwrap_or(close);
    let traded_notional = context
        .day_ntl_vlm
        .as_deref()
        .map(|value| parse_quantity(value, "decode a Hyperliquid ticker notional"))
        .transpose()?;
    let traded_quantity = match context.day_base_vlm.as_deref() {
        Some(value) => Some(parse_quantity(
            value,
            "decode a Hyperliquid ticker quantity",
        )?),
        None => traded_notional.and_then(|notional| notional.checked_div_price(close)),
    };
    Ok(NativeTicker {
        monotonic_id: None,
        open,
        high: None,
        low: None,
        close,
        traded_quantity,
        traded_notional,
    })
}

#[derive(Clone, serde::Deserialize)]
pub(super) struct HyperliquidOrderResponse {
    pub response: HyperliquidOrderResponseBody,
}
#[derive(Clone, serde::Deserialize)]
pub(super) struct HyperliquidOrderResponseBody {
    pub data: HyperliquidOrderStatuses,
}
#[derive(Clone, serde::Deserialize)]
pub(super) struct HyperliquidOrderStatuses {
    pub statuses: Vec<HyperliquidOrderStatus>,
}
#[derive(Clone, serde::Deserialize)]
#[serde(untagged)]
pub(super) enum HyperliquidOrderStatus {
    Resting {
        resting: HyperliquidOrderId,
    },
    Filled {
        filled: HyperliquidOrderId,
    },
    WaitingForFill {
        #[serde(rename = "waitingForFill")]
        _waiting_for_fill: serde::de::IgnoredAny,
    },
    WaitingForTrigger {
        #[serde(rename = "waitingForTrigger")]
        _waiting_for_trigger: serde::de::IgnoredAny,
    },
    Error {
        error: SmolStr,
    },
}
#[derive(Clone, serde::Deserialize)]
pub(super) struct HyperliquidOrderId {
    pub oid: u64,
}
#[derive(Clone, serde::Deserialize)]
pub(super) struct HyperliquidBookResponse {
    pub time: i64,
    pub levels: [Vec<HyperliquidBookLevel>; 2],
}
#[derive(Clone, serde::Deserialize)]
pub(super) struct HyperliquidBookLevel {
    pub px: SmolStr,
    pub sz: SmolStr,
}
#[derive(Clone, serde::Deserialize)]
pub(super) struct HyperliquidTrade {
    #[serde(default)]
    pub coin: Option<SmolStr>,
    pub side: SmolStr,
    pub px: SmolStr,
    pub sz: SmolStr,
    pub time: i64,
    pub tid: u64,
}

#[derive(Clone, serde::Deserialize)]
struct HyperliquidOrderBookEvent {
    channel: SmolStr,
    data: HyperliquidOrderBookEventData,
}

#[derive(Clone, serde::Deserialize)]
struct HyperliquidOrderBookEventData {
    coin: SmolStr,
    time: i64,
    levels: [Vec<HyperliquidBookLevel>; 2],
}

#[derive(Clone, serde::Deserialize)]
pub(super) struct HyperliquidSpotStateWire {
    pub balances: Vec<HyperliquidSpotBalanceWire>,
}

#[derive(Clone, serde::Deserialize)]
pub(super) struct HyperliquidSpotBalanceWire {
    pub coin: SmolStr,
    pub total: SmolStr,
    pub hold: SmolStr,
}

#[derive(Clone, serde::Deserialize)]
pub(super) struct HyperliquidPerpetualStateWire {
    #[serde(rename = "assetPositions")]
    pub asset_positions: Vec<HyperliquidAssetPositionWire>,
    #[serde(default)]
    pub time: i64,
}

#[derive(Clone, serde::Deserialize)]
pub(super) struct HyperliquidAssetPositionWire {
    pub position: HyperliquidPositionWire,
}

#[derive(Clone, serde::Deserialize)]
pub(super) struct HyperliquidPositionWire {
    pub coin: SmolStr,
    pub szi: SmolStr,
    #[serde(rename = "entryPx")]
    pub entry_px: Option<SmolStr>,
    #[serde(rename = "markPx")]
    pub mark_px: Option<SmolStr>,
    #[serde(rename = "liquidationPx")]
    pub liquidation_px: Option<SmolStr>,
    #[serde(rename = "positionValue")]
    pub position_value: Option<SmolStr>,
    #[serde(rename = "unrealizedPnl")]
    pub unrealized_pnl: SmolStr,
    #[serde(rename = "marginUsed")]
    pub margin_used: Option<SmolStr>,
}

#[derive(Clone, serde::Deserialize)]
pub(super) struct HyperliquidOrderWire {
    pub coin: SmolStr,
    pub oid: u64,
    #[serde(rename = "limitPx")]
    pub limit_px: SmolStr,
    pub side: SmolStr,
    pub sz: SmolStr,
    #[serde(rename = "origSz")]
    pub orig_sz: Option<SmolStr>,
    pub timestamp: i64,
    #[serde(rename = "reduceOnly")]
    pub reduce_only: bool,
    #[serde(rename = "orderType")]
    pub order_type: SmolStr,
    #[serde(rename = "isTrigger")]
    pub is_trigger: bool,
    #[serde(rename = "triggerPx")]
    pub trigger_px: Option<SmolStr>,
    pub tif: Option<SmolStr>,
}

#[derive(Clone, serde::Deserialize)]
pub(super) struct HyperliquidHistoricalOrderWire {
    pub order: HyperliquidOrderWire,
    pub status: SmolStr,
}

/// The order kind one wire order describes.
fn decode_order_kind(
    is_trigger: bool,
    trigger_price: Option<&str>,
    order_type: &str,
    time_in_force: Option<&str>,
    limit_price: Price,
) -> Result<OrderKind, MarketParticipantError> {
    if !is_trigger {
        if order_type == "Market" {
            return Ok(OrderKind::Market);
        }
        let time_in_force = match time_in_force {
            None | Some("Gtc") => TimeInForce::GoodTillCancelled,
            Some("Ioc") => TimeInForce::ImmediateOrCancel,
            Some("Alo") => TimeInForce::PostOnly,
            Some(_) => {
                return Err(protocol_error(
                    "decode a Hyperliquid time in force",
                    SmolStr::new_static("unknown time in force"),
                ));
            }
        };
        return Ok(OrderKind::Limit {
            price: limit_price,
            time_in_force,
        });
    }
    let trigger_price = trigger_price
        .ok_or_else(|| {
            protocol_error(
                "decode a Hyperliquid trigger price",
                SmolStr::new_static("the trigger order published no trigger price"),
            )
        })?
        .parse::<Price>()
        .map_err(|error| protocol_error("decode a Hyperliquid trigger price", error))?;
    let trigger_price_kind = TriggerPriceKind::Mark;
    let normalized = order_type.to_ascii_lowercase();
    let is_market = normalized.contains("market");
    if normalized.contains("take profit") {
        return Ok(if is_market {
            OrderKind::TakeProfitMarket {
                trigger_price,
                trigger_price_kind,
            }
        } else {
            OrderKind::TakeProfitLimit {
                price: limit_price,
                trigger_price,
                trigger_price_kind,
                time_in_force: TimeInForce::GoodTillCancelled,
            }
        });
    }
    if normalized.contains("stop") {
        return Ok(if is_market {
            OrderKind::StopMarket {
                trigger_price,
                trigger_price_kind,
            }
        } else {
            OrderKind::StopLimit {
                price: limit_price,
                trigger_price,
                trigger_price_kind,
                time_in_force: TimeInForce::GoodTillCancelled,
            }
        });
    }
    Err(protocol_error(
        "decode a Hyperliquid trigger order type",
        SmolStr::new_static("unknown trigger order type"),
    ))
}

/// The price, quantity and filled quantity of one wire order.
fn order_amounts(
    wire: &HyperliquidOrderWire,
) -> Result<(Price, Quantity, Quantity), MarketParticipantError> {
    let price = parse_price(&wire.limit_px, "decode a Hyperliquid order price")?;
    let quantity = parse_quantity(
        wire.orig_sz.as_deref().unwrap_or(wire.sz.as_str()),
        "decode a Hyperliquid order quantity",
    )?;
    let filled = parse_quantity(&wire.sz, "decode a Hyperliquid filled quantity")?;
    Ok((price, quantity, filled))
}

/// The order kind of one wire order, falling back to the limit order it also is.
fn order_kind(wire: &HyperliquidOrderWire, price: Price) -> OrderKind {
    decode_order_kind(
        wire.is_trigger,
        wire.trigger_px.as_deref(),
        wire.order_type.as_str(),
        wire.tif.as_deref(),
        price,
    )
    .unwrap_or(OrderKind::Limit {
        price,
        time_in_force: TimeInForce::GoodTillCancelled,
    })
}

/// One authenticated order of one listing.
fn decode_order(
    listing: &VenueListingKey,
    wire: HyperliquidOrderWire,
    status: SmolStr,
    active: bool,
) -> Result<NativeOrder, MarketParticipantError> {
    let (price, quantity, filled) = order_amounts(&wire)?;
    let order_status = if active {
        OrderStatus::Open
    } else if status.eq_ignore_ascii_case("filled") {
        OrderStatus::Filled
    } else if status.eq_ignore_ascii_case("canceled") || status.eq_ignore_ascii_case("cancelled") {
        OrderStatus::Cancelled
    } else {
        OrderStatus::Rejected
    };
    Ok(NativeOrder {
        listing: listing.clone(),
        id: OrderId::normal(format_smolstr!("{}", wire.oid))
            .map_err(|error| protocol_error("create a Hyperliquid order ID", error))?,
        side: decode_side(&wire.side, "decode a Hyperliquid order side")?,
        position_side: None,
        status: order_status,
        kind: order_kind(&wire, price),
        quantity,
        filled_quantity: filled,
        average_filled_price: None,
        reduce_only: wire.reduce_only,
        margin_mode: Some(AccountMarginMode::Cross),
        leverage: None,
        create_timestamp: wire.timestamp,
        update_timestamp: wire.timestamp,
    })
}

/// One open order of one monitored wallet.
fn decode_open_order(
    listing: &VenueListingKey,
    wire: HyperliquidOrderWire,
) -> Result<NativeHyperliquidOpenOrder, MarketParticipantError> {
    let (price, quantity, _) = order_amounts(&wire)?;
    Ok(NativeHyperliquidOpenOrder {
        listing: listing.clone(),
        side: decode_side(&wire.side, "decode a Hyperliquid order side")?,
        quantity,
        price,
        status: OrderStatus::Open,
        kind: order_kind(&wire, price),
        reduce_only: wire.reduce_only,
        timestamp: wire.timestamp,
        order_id: IdentifierValue::new(format_smolstr!("{}", wire.oid))
            .map_err(|error| protocol_error("create a Hyperliquid order ID", error))?,
    })
}

/// One historical order of one monitored wallet.
fn decode_historical_order(
    listing: &VenueListingKey,
    row: HyperliquidHistoricalOrderWire,
) -> Result<NativeHyperliquidHistoricalOrder, MarketParticipantError> {
    let (price, quantity, _) = order_amounts(&row.order)?;
    Ok(NativeHyperliquidHistoricalOrder {
        listing: listing.clone(),
        side: decode_side(&row.order.side, "decode a Hyperliquid order side")?,
        quantity,
        price,
        kind: order_kind(&row.order, price),
        reduce_only: row.order.reduce_only,
        status: IdentifierValue::new(row.status)
            .map_err(|error| protocol_error("decode a Hyperliquid order status", error))?,
        timestamp: row.order.timestamp,
        order_id: IdentifierValue::new(format_smolstr!("{}", row.order.oid))
            .map_err(|error| protocol_error("create a Hyperliquid order ID", error))?,
    })
}

/// One fill of one monitored wallet.
fn decode_fill(
    address: &WalletAddress,
    listing: &VenueListingKey,
    row: HyperliquidFillWire,
) -> Result<NativeHyperliquidUserFill, MarketParticipantError> {
    Ok(NativeHyperliquidUserFill {
        address: address.clone(),
        listing: listing.clone(),
        side: decode_side(&row.side, "decode a Hyperliquid fill side")?,
        quantity: parse_quantity(&row.sz, "decode a Hyperliquid fill quantity")?,
        price: parse_price(&row.px, "decode a Hyperliquid fill price")?,
        timestamp: row.time,
        start_position: parse_signed_quantity(
            row.start_position.as_deref().unwrap_or("0"),
            "decode a Hyperliquid fill position",
        )?,
        closed_pnl: parse_profit(
            row.closed_pnl.as_deref().unwrap_or("0"),
            "decode a Hyperliquid fill pnl",
        )?,
        fee: parse_profit(&row.fee, "decode a Hyperliquid fill fee")?,
        fee_asset: catalog::token_asset(&row.fee_token)?.asset_id(),
        hash: row
            .hash
            .map(|hash| {
                IdentifierValue::new(hash)
                    .map_err(|error| protocol_error("decode a Hyperliquid fill hash", error))
            })
            .transpose()?,
        tid: row.tid,
        liquidation: row.liquidation.is_some(),
        direction: row
            .dir
            .as_deref()
            .map(HyperliquidFillDirection::from_raw)
            .transpose()
            .map_err(|reason| protocol_error("decode a Hyperliquid fill direction", reason))?,
    })
}

#[derive(Clone, serde::Deserialize)]
pub(super) struct HyperliquidFillWire {
    pub coin: SmolStr,
    pub side: SmolStr,
    pub sz: SmolStr,
    pub px: SmolStr,
    pub time: i64,
    #[serde(rename = "startPosition", default)]
    pub start_position: Option<SmolStr>,
    #[serde(rename = "closedPnl", default)]
    pub closed_pnl: Option<SmolStr>,
    pub fee: SmolStr,
    #[serde(rename = "feeToken")]
    pub fee_token: SmolStr,
    pub tid: u64,
    pub hash: Option<SmolStr>,
    pub liquidation: Option<serde::de::IgnoredAny>,
    pub dir: Option<SmolStr>,
}

#[derive(Clone, serde::Deserialize)]
struct HyperliquidCandleEvent {
    channel: SmolStr,
    data: HyperliquidCandleEventData,
}
#[derive(Clone, serde::Deserialize)]
struct HyperliquidCandleEventData {
    t: i64,
    #[serde(rename = "i")]
    interval: SmolStr,
    o: SmolStr,
    h: SmolStr,
    l: SmolStr,
    c: SmolStr,
    v: SmolStr,
    #[serde(rename = "s")]
    coin: Option<SmolStr>,
}
#[derive(Clone, serde::Deserialize)]
struct HyperliquidTradesEvent {
    channel: SmolStr,
    data: Vec<HyperliquidTrade>,
}

#[derive(Clone, serde::Deserialize)]
struct HyperliquidUserFillsEvent {
    channel: SmolStr,
    data: HyperliquidUserFillsEventData,
}

#[derive(Clone, serde::Deserialize)]
struct HyperliquidUserFillsEventData {
    #[serde(rename = "isSnapshot", default)]
    is_snapshot: bool,
    user: WalletAddress,
    fills: Vec<HyperliquidFillWire>,
}

#[derive(Clone, serde::Deserialize)]
struct HyperliquidUserLedgerEvent {
    channel: SmolStr,
    data: HyperliquidUserLedgerEventData,
}

#[derive(Clone, serde::Deserialize)]
struct HyperliquidUserLedgerEventData {
    #[serde(rename = "isSnapshot", default)]
    is_snapshot: bool,
    user: WalletAddress,
    #[serde(rename = "nonFundingLedgerUpdates", default)]
    events: Vec<HyperliquidLedgerWire>,
}

/// One ledger delta as the venue reports it: either one this application models, or
/// one that is kept for its type name alone.
///
/// The venue adds delta types over time — vaults, staking, sends — and a wallet
/// reading is not the place to refuse an account because one of them appeared. A
/// delta that is not modelled is carried as its own type name and skipped by
/// [`decode_ledger_rows`], while a *modelled* type whose payload does not parse is
/// still a protocol error.
#[derive(Clone, serde::Deserialize)]
#[serde(untagged)]
pub(super) enum HyperliquidLedgerDeltaWire {
    Modelled(HyperliquidModelledLedgerDelta),
    Unmodelled(HyperliquidUnmodelledLedgerDelta),
}

/// A ledger delta this application can describe.
#[derive(Clone, serde::Deserialize)]
#[serde(tag = "type")]
pub(super) enum HyperliquidModelledLedgerDelta {
    #[serde(rename = "deposit")]
    Deposit { usdc: SmolStr },
    #[serde(rename = "withdraw")]
    Withdraw { usdc: SmolStr },
    #[serde(rename = "accountClassTransfer")]
    AccountClassTransfer {
        usdc: SmolStr,
        #[serde(rename = "toPerp")]
        to_perp: bool,
    },
    #[serde(rename = "internalTransfer")]
    InternalTransfer {
        usdc: SmolStr,
        user: SmolStr,
        destination: SmolStr,
    },
    #[serde(rename = "subAccountTransfer")]
    SubAccountTransfer {
        usdc: SmolStr,
        user: SmolStr,
        destination: SmolStr,
    },
    #[serde(rename = "spotTransfer")]
    SpotTransfer {
        token: SmolStr,
        amount: SmolStr,
        #[serde(default, rename = "usdcValue")]
        usdc_value: Option<SmolStr>,
        user: SmolStr,
        destination: SmolStr,
    },
    #[serde(rename = "send")]
    Send {
        token: SmolStr,
        amount: SmolStr,
        #[serde(default, rename = "usdcValue")]
        usdc_value: Option<SmolStr>,
        user: SmolStr,
        destination: SmolStr,
    },
    #[serde(rename = "liquidation")]
    Liquidation {
        #[serde(rename = "liquidatedNtlPos")]
        liquidated_ntl_pos: SmolStr,
        #[serde(default, rename = "liquidatedPositions")]
        liquidated_positions: Vec<HyperliquidLiquidatedPositionWire>,
    },
}

/// One position a liquidation closed, as the venue names it.
#[derive(Clone, serde::Deserialize)]
pub(super) struct HyperliquidLiquidatedPositionWire {
    coin: SmolStr,
    /// The signed size the venue closed, in the coin's own base asset.
    szi: SmolStr,
}

/// A ledger delta this application does not model, kept for its type name.
#[derive(Clone, serde::Deserialize)]
pub(super) struct HyperliquidUnmodelledLedgerDelta {
    #[serde(rename = "type")]
    kind: SmolStr,
}

/// What one modelled ledger delta says about a wallet.
pub(super) struct LedgerDeltaFields {
    kind: &'static str,
    coin: SmolStr,
    amount: SmolStr,
    /// The value of the move in USDC, when the delta reports one of its own: a
    /// token transfer knows what its token was worth, and a USDC move is worth what
    /// it moved.
    value: Option<SmolStr>,
    direction: Option<HyperliquidTransferDirection>,
    related_wallet: Option<SmolStr>,
}

impl LedgerDeltaFields {
    /// A move of USDC, in the direction the delta itself states.
    fn usdc(
        kind: &'static str,
        usdc: SmolStr,
        direction: Option<HyperliquidTransferDirection>,
    ) -> Self {
        Self {
            kind,
            coin: SmolStr::new_static("USDC"),
            amount: usdc,
            value: None,
            direction,
            related_wallet: None,
        }
    }

    /// A move of USDC between two wallets, read from the wallet it belongs to.
    fn between(
        kind: &'static str,
        usdc: SmolStr,
        address: &WalletAddress,
        user: &str,
        destination: &str,
    ) -> Self {
        let (direction, related_wallet) = transfer_sides(address, user, destination);
        Self {
            direction,
            related_wallet,
            ..Self::usdc(kind, usdc, None)
        }
    }

    /// A move of a token between two wallets, with the value the delta reports.
    fn token(
        kind: &'static str,
        token: SmolStr,
        amount: SmolStr,
        usdc_value: Option<SmolStr>,
        address: &WalletAddress,
        user: &str,
        destination: &str,
    ) -> Self {
        let (direction, related_wallet) = transfer_sides(address, user, destination);
        Self {
            kind,
            coin: token,
            amount,
            value: usdc_value,
            direction,
            related_wallet,
        }
    }
}

/// Which way a transfer moved, from the wallet a ledger is read for.
///
/// The venue names both ends of a transfer, so the direction is the end this wallet
/// is: a wallet that is neither end — a master account reading what its
/// sub-accounts did — is not told which way the transfer went, and does not guess.
pub(super) fn transfer_sides(
    address: &WalletAddress,
    user: &str,
    destination: &str,
) -> (Option<HyperliquidTransferDirection>, Option<SmolStr>) {
    let outgoing = address.as_str().eq_ignore_ascii_case(user);
    let incoming = address.as_str().eq_ignore_ascii_case(destination);
    let direction = match (outgoing, incoming) {
        (true, false) => Some(HyperliquidTransferDirection::Outgoing),
        (false, true) => Some(HyperliquidTransferDirection::Incoming),
        (true, true) | (false, false) => None,
    };
    let related_wallet = match (outgoing, incoming) {
        (true, false) => Some(destination),
        (false, true) => Some(user),
        (true, true) | (false, false) => None,
    };
    (direction, related_wallet.map(SmolStr::new))
}

/// The operation one modelled ledger delta is decoded by, so a payload that does
/// not parse is reported against the type it claimed to be.
pub(super) fn modelled_ledger_operation(kind: &str) -> Option<&'static str> {
    match kind {
        "deposit" => Some("decode a Hyperliquid deposit"),
        "withdraw" => Some("decode a Hyperliquid withdrawal"),
        "accountClassTransfer" => Some("decode a Hyperliquid account transfer"),
        "internalTransfer" => Some("decode a Hyperliquid internal transfer"),
        "subAccountTransfer" => Some("decode a Hyperliquid sub-account transfer"),
        "spotTransfer" => Some("decode a Hyperliquid spot transfer"),
        "send" => Some("decode a Hyperliquid send"),
        "liquidation" => Some("decode a Hyperliquid liquidation"),
        _ => None,
    }
}

#[derive(Clone, serde::Deserialize)]
pub(super) struct HyperliquidLedgerWire {
    pub time: i64,
    pub hash: SmolStr,
    pub delta: HyperliquidLedgerDeltaWire,
}

/// Decodes the rows of one ledger answer, skipping the delta types this
/// application does not model.
///
/// `listings` is the route's own `coin`-to-listing map: a liquidation names the
/// positions it closed by coin, and only a coin this route publishes is one of
/// its listings.
pub(super) fn decode_ledger_rows(
    address: &WalletAddress,
    rows: Vec<HyperliquidLedgerWire>,
    listings: &RouteListings,
) -> Result<Vec<NativeHyperliquidLedgerEvent>, MarketParticipantError> {
    let mut events = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(event) = decode_ledger(address, row, listings)? {
            events.push(event);
        }
    }
    Ok(events)
}

/// One ledger row, or nothing when its delta is of a type this does not model.
pub(super) fn decode_ledger(
    address: &WalletAddress,
    row: HyperliquidLedgerWire,
    listings: &RouteListings,
) -> Result<Option<NativeHyperliquidLedgerEvent>, MarketParticipantError> {
    let HyperliquidLedgerWire { time, hash, delta } = row;
    let delta = match delta {
        HyperliquidLedgerDeltaWire::Modelled(delta) => delta,
        HyperliquidLedgerDeltaWire::Unmodelled(delta) => {
            if let Some(operation) = modelled_ledger_operation(delta.kind.as_str()) {
                return Err(protocol_error(
                    operation,
                    SmolStr::new_static("malformed ledger delta"),
                ));
            }
            log::debug!(
                "skipped a Hyperliquid ledger delta of unmodelled type {}",
                delta.kind
            );
            return Ok(None);
        }
    };
    let fields = match delta {
        // A liquidation is not a move of a token: it reports the notional the
        // venue closed and the positions it closed, so it is decoded on its own.
        HyperliquidModelledLedgerDelta::Liquidation {
            liquidated_ntl_pos,
            liquidated_positions,
        } => {
            return decode_liquidation(
                address,
                time,
                hash,
                &liquidated_ntl_pos,
                &liquidated_positions,
                listings,
            )
            .map(Some);
        }
        HyperliquidModelledLedgerDelta::Deposit { usdc } => LedgerDeltaFields::usdc(
            "deposit",
            usdc,
            Some(HyperliquidTransferDirection::Incoming),
        ),
        HyperliquidModelledLedgerDelta::Withdraw { usdc } => LedgerDeltaFields::usdc(
            "withdraw",
            usdc,
            Some(HyperliquidTransferDirection::Outgoing),
        ),
        // Moving to perpetual takes USDC out of the spot balance, and moving back
        // puts it in.
        HyperliquidModelledLedgerDelta::AccountClassTransfer { usdc, to_perp } => {
            LedgerDeltaFields::usdc(
                "accountClassTransfer",
                usdc,
                Some(if to_perp {
                    HyperliquidTransferDirection::Outgoing
                } else {
                    HyperliquidTransferDirection::Incoming
                }),
            )
        }
        HyperliquidModelledLedgerDelta::InternalTransfer {
            usdc,
            user,
            destination,
        } => LedgerDeltaFields::between("internalTransfer", usdc, address, &user, &destination),
        HyperliquidModelledLedgerDelta::SubAccountTransfer {
            usdc,
            user,
            destination,
        } => LedgerDeltaFields::between("subAccountTransfer", usdc, address, &user, &destination),
        HyperliquidModelledLedgerDelta::SpotTransfer {
            token,
            amount,
            usdc_value,
            user,
            destination,
        } => LedgerDeltaFields::token(
            "spotTransfer",
            token,
            amount,
            usdc_value,
            address,
            &user,
            &destination,
        ),
        HyperliquidModelledLedgerDelta::Send {
            token,
            amount,
            usdc_value,
            user,
            destination,
        } => LedgerDeltaFields::token(
            "send",
            token,
            amount,
            usdc_value,
            address,
            &user,
            &destination,
        ),
    };
    let operation = modelled_ledger_operation(fields.kind).unwrap_or("decode a Hyperliquid ledger");
    let amount = parse_signed_quantity(&fields.amount, operation)?;
    let value = match fields.value {
        Some(value) => parse_signed_quantity(&value, operation)?,
        None => amount,
    };
    let coin = catalog::token_asset(&fields.coin)?.asset_id();
    let related_wallet = fields
        .related_wallet
        .map(|wallet| {
            wallet
                .parse::<WalletAddress>()
                .map_err(|error| protocol_error(operation, error))
        })
        .transpose()?;
    Ok(Some(NativeHyperliquidLedgerEvent {
        address: address.clone(),
        timestamp: time,
        hash: IdentifierValue::new(hash).map_err(|error| protocol_error(operation, error))?,
        coin: Some(coin),
        kind: IdentifierValue::new(fields.kind)
            .map_err(|error| protocol_error(operation, error))?,
        amount: Some(amount),
        value: Some(value),
        transfer_direction: fields.direction,
        related_wallet,
        liquidated_notional: None,
        liquidated_positions: Vec::<NativeHyperliquidLiquidatedPosition>::new(),
    }))
}

/// One liquidation ledger row.
///
/// The venue denominates the notional it closed in USDC — the asset the entry is
/// reported in — and names every position it closed by coin with the signed size
/// it closed, so each position is resolved through the route's own catalog and a
/// coin this route does not publish is left out instead of being invented. A
/// liquidation moves nothing on its own: the notional is reported as the venue's
/// own number and no amount is fabricated for it.
fn decode_liquidation(
    address: &WalletAddress,
    time: i64,
    hash: SmolStr,
    liquidated_notional: &SmolStr,
    liquidated_positions: &[HyperliquidLiquidatedPositionWire],
    listings: &RouteListings,
) -> Result<NativeHyperliquidLedgerEvent, MarketParticipantError> {
    let operation = "decode a Hyperliquid liquidation";
    let notional = parse_signed_quantity(liquidated_notional, operation)?;
    let mut positions = Vec::with_capacity(liquidated_positions.len());
    for position in liquidated_positions {
        let Some(listing) = listings.get(&position.coin) else {
            continue;
        };
        positions.push(NativeHyperliquidLiquidatedPosition {
            listing: listing.clone(),
            size: parse_signed_quantity(&position.szi, operation)?,
        });
    }
    Ok(NativeHyperliquidLedgerEvent {
        address: address.clone(),
        timestamp: time,
        hash: IdentifierValue::new(hash).map_err(|error| protocol_error(operation, error))?,
        coin: Some(catalog::token_asset("USDC")?.asset_id()),
        kind: IdentifierValue::new("liquidation")
            .map_err(|error| protocol_error(operation, error))?,
        amount: None,
        value: None,
        transfer_direction: None,
        related_wallet: None,
        liquidated_notional: Some(notional),
        liquidated_positions: positions,
    })
}

#[derive(Clone, serde::Deserialize)]
struct HyperliquidActiveContextEvent {
    channel: SmolStr,
    data: HyperliquidActiveContextWire,
}

#[derive(Clone, serde::Deserialize)]
struct HyperliquidActiveContextWire {
    coin: SmolStr,
    ctx: HyperliquidActiveContextValues,
}

/// One subscription and the events it carries.
#[derive(Clone)]
pub(super) enum SubscriptionKind {
    Candlestick {
        listing: VenueListingKey,
        coin: SmolStr,
        interval: SmolStr,
    },
    Ticker {
        listing: VenueListingKey,
        coin: SmolStr,
    },
    Trade {
        listing: VenueListingKey,
        coin: SmolStr,
    },
    OrderBook {
        listing: VenueListingKey,
        coin: SmolStr,
        depth: u16,
    },
    Order,
    Position,
    WalletFills {
        address: WalletAddress,
        /// The routes whose published listings decode this stream's fills.
        routes: Vec<VenueRoute>,
    },
    WalletLedger {
        address: WalletAddress,
        /// The routes whose published listings decode this stream's ledger rows.
        routes: Vec<VenueRoute>,
    },
    FastContexts {
        /// The routes whose published listings decode this stream's contexts.
        routes: Vec<VenueRoute>,
        /// The venue's board of live contexts, which this stream merges into and reads.
        ///
        /// It belongs to the adapter rather than to the stream because the subscription is
        /// shared by topic: the venue sends the whole board once, when the first reader
        /// opens it, and a reader that joins later has to be answered from what the venue
        /// published before it arrived. See [`FastContextBoard`].
        board: Arc<FastContextBoard>,
    },
}

impl SubscriptionKind {
    /// Returns the routes this subscription's payloads are decoded against.
    ///
    /// Only the wallet-monitor streams carry venue-wide payloads: a coin they publish
    /// belongs to whichever monitored route listed it, so the reader resolves every
    /// monitored route's catalog and each row is resolved by the coin it names. A
    /// listing stream resolved its coin once, when it was subscribed, and needs no table.
    fn decoding_routes(&self) -> Option<&[VenueRoute]> {
        match self {
            Self::WalletFills { routes, .. }
            | Self::WalletLedger { routes, .. }
            | Self::FastContexts { routes, .. } => Some(routes),
            Self::Candlestick { .. }
            | Self::Ticker { .. }
            | Self::Trade { .. }
            | Self::OrderBook { .. }
            | Self::Order
            | Self::Position => None,
        }
    }
}

/// Decodes one payload of one subscription into a protocol event.
fn decode_event(
    payload: &[u8],
    kind: &SubscriptionKind,
    listings: Option<&RouteListings>,
) -> Result<Option<ProtocolEvent>, MarketParticipantError> {
    match kind {
        SubscriptionKind::Candlestick {
            listing,
            coin,
            interval,
        } => {
            let event: HyperliquidCandleEvent =
                match serde_json::from_slice::<HyperliquidCandleEvent>(payload) {
                    Ok(event) if event.channel == "candle" => event,
                    _ => return Ok(None),
                };
            if event.data.coin.as_deref() != Some(coin.as_str()) || event.data.interval != *interval
            {
                return Ok(None);
            }
            let candle = event.data;
            let parse = |value: &str| {
                value
                    .parse::<Price>()
                    .map_err(|error| protocol_error("decode a Hyperliquid candle event", error))
            };
            Ok(Some(ProtocolEvent::Candlestick(
                crate::adapter::NativeListing::new(
                    listing.clone(),
                    NativeCandlestick {
                        timestamp: candle.t,
                        open: parse(&candle.o)?,
                        high: parse(&candle.h)?,
                        low: parse(&candle.l)?,
                        close: parse(&candle.c)?,
                        volume: parse_quantity(&candle.v, "decode a Hyperliquid candle event")?,
                    },
                ),
            )))
        }
        SubscriptionKind::Ticker { listing, coin } => {
            let event: HyperliquidActiveContextEvent =
                match serde_json::from_slice::<HyperliquidActiveContextEvent>(payload) {
                    // A context pushed for a spot instrument arrives on a channel of
                    // its own and carries the same numbers as the perpetual one.
                    Ok(event)
                        if matches!(
                            event.channel.as_str(),
                            "activeAssetCtx" | "activeSpotAssetCtx"
                        ) =>
                    {
                        event
                    }
                    _ => return Ok(None),
                };
            if event.data.coin != *coin {
                return Ok(None);
            }
            Ok(Some(ProtocolEvent::Ticker(
                crate::adapter::NativeListing::new(
                    listing.clone(),
                    decode_ticker(&event.data.ctx, Some(payload))?,
                ),
            )))
        }
        SubscriptionKind::Trade { listing, coin } => {
            let event: HyperliquidTradesEvent =
                match serde_json::from_slice::<HyperliquidTradesEvent>(payload) {
                    Ok(event) if event.channel == "trades" => event,
                    _ => return Ok(None),
                };
            let Some(trade) = event.data.first() else {
                return Ok(None);
            };
            if trade.coin.as_deref() != Some(coin.as_str()) {
                return Ok(None);
            }
            Ok(Some(ProtocolEvent::Trade(
                crate::adapter::NativeListing::new(
                    listing.clone(),
                    NativeTrade {
                        id: MarketTradeId::Numeric(trade.tid),
                        timestamp: trade.time,
                        price: parse_price(&trade.px, "decode a Hyperliquid trade event")?,
                        quantity: parse_quantity(&trade.sz, "decode a Hyperliquid trade event")?,
                        side: decode_side(&trade.side, "decode a Hyperliquid trade side")?,
                    },
                ),
            )))
        }
        SubscriptionKind::OrderBook {
            listing,
            coin,
            depth,
        } => {
            let event = match serde_json::from_slice::<HyperliquidOrderBookEvent>(payload) {
                Ok(event) if event.channel == "l2Book" => event,
                _ => return Ok(None),
            };
            if event.data.coin != *coin {
                return Ok(None);
            }
            // The venue numbers this snapshot by its own timestamp and nothing else,
            // so the sequence is that native watermark: no numeric counter is
            // invented, and the update states that it is a full snapshot.
            let sequence = native_sequence(event.data.time)?;
            let [bids, asks] = event.data.levels;
            Ok(Some(ProtocolEvent::OrderBook(
                crate::adapter::NativeListing::new(
                    listing.clone(),
                    crate::adapter::NativeOrderBookUpdate {
                        snapshot: true,
                        first_sequence: sequence.clone(),
                        last_sequence: sequence,
                        previous_sequence: None,
                        timestamp: Some(event.data.time),
                        bids: decode_book_levels(bids, *depth)?,
                        asks: decode_book_levels(asks, *depth)?,
                    },
                ),
            )))
        }
        SubscriptionKind::WalletFills { address, .. } => {
            let event: HyperliquidUserFillsEvent = match serde_json::from_slice::<
                HyperliquidUserFillsEvent,
            >(payload)
            {
                Ok(event) if event.channel == "userFills" && event.data.user == *address => event,
                _ => return Ok(None),
            };
            let Some(listings) = listings else {
                return Err(protocol_error(
                    "decode a Hyperliquid wallet stream",
                    SmolStr::new_static("the monitored routes' listings were not resolved"),
                ));
            };
            let is_snapshot = event.data.is_snapshot;
            let mut fills = Vec::with_capacity(event.data.fills.len());
            for row in event.data.fills {
                if let Some(listing) = listings.get(&row.coin) {
                    fills.push(decode_fill(address, listing, row)?);
                }
            }
            Ok(Some(ProtocolEvent::HyperliquidWalletUpdate(
                NativeHyperliquidWalletUpdate::UserFills { is_snapshot, fills },
            )))
        }
        SubscriptionKind::WalletLedger { address, .. } => {
            let event: HyperliquidUserLedgerEvent =
                match serde_json::from_slice::<HyperliquidUserLedgerEvent>(payload) {
                    Ok(event)
                        if event.channel == "userNonFundingLedgerUpdates"
                            && event.data.user == *address =>
                    {
                        event
                    }
                    _ => return Ok(None),
                };
            let Some(listings) = listings else {
                return Err(protocol_error(
                    "decode a Hyperliquid wallet stream",
                    SmolStr::new_static("the monitored routes' listings were not resolved"),
                ));
            };
            let is_snapshot = event.data.is_snapshot;
            let events = decode_ledger_rows(address, event.data.events, listings)?;
            Ok(Some(ProtocolEvent::HyperliquidWalletUpdate(
                NativeHyperliquidWalletUpdate::UserLedger {
                    is_snapshot,
                    events,
                },
            )))
        }
        SubscriptionKind::FastContexts { board, .. } => {
            let Some(listings) = listings else {
                return Err(protocol_error(
                    "decode a Hyperliquid fast asset context stream",
                    SmolStr::new_static("the monitored routes' listings were not resolved"),
                ));
            };
            let Some(contexts) = board.absorb(payload, listings)? else {
                return Ok(None);
            };
            Ok(Some(ProtocolEvent::HyperliquidWalletUpdate(
                NativeHyperliquidWalletUpdate::FastAssetContexts { contexts },
            )))
        }
        SubscriptionKind::Order | SubscriptionKind::Position => Ok(None),
    }
}

/// Decodes one `post` answer of the socket API.
pub(super) fn decode_response<T: serde::de::DeserializeOwned>(
    payload: &[u8],
) -> Result<T, MarketParticipantError> {
    if let Ok(response) = serde_json::from_slice::<HyperliquidInfoFrame<T>>(payload)
        && response.channel == "post"
        && response.data.response.r#type == "info"
    {
        return Ok(response.data.response.payload.data);
    }
    if let Ok(response) = serde_json::from_slice::<HyperliquidActionFrame<T>>(payload)
        && response.channel == "post"
        && response.data.response.r#type == "action"
    {
        return Ok(response.data.response.payload);
    }
    if let Ok(response) = serde_json::from_slice::<HyperliquidResponseEnvelope<T>>(payload) {
        return Ok(response.data);
    }
    if let Ok(response) = serde_json::from_slice::<HyperliquidDirectResponse<T>>(payload) {
        return Ok(response.response);
    }
    Err(MarketParticipantError::DecodeFailed {
        operation: "decode a Hyperliquid post response".into(),
        raw_data: Bytes::copy_from_slice(payload),
    })
}
#[derive(serde::Deserialize)]
struct HyperliquidInfoFrame<T> {
    channel: SmolStr,
    data: HyperliquidInfoFrameData<T>,
}
#[derive(serde::Deserialize)]
struct HyperliquidInfoFrameData<T> {
    #[serde(rename = "id")]
    _id: u64,
    response: HyperliquidInfoResponse<T>,
}
#[derive(serde::Deserialize)]
struct HyperliquidInfoResponse<T> {
    #[serde(rename = "type")]
    r#type: SmolStr,
    payload: HyperliquidInfoPayload<T>,
}
#[derive(serde::Deserialize)]
struct HyperliquidInfoPayload<T> {
    #[serde(rename = "type")]
    _type: SmolStr,
    data: T,
}
#[derive(serde::Deserialize)]
struct HyperliquidActionFrame<T> {
    channel: SmolStr,
    data: HyperliquidActionFrameData<T>,
}
#[derive(serde::Deserialize)]
struct HyperliquidActionFrameData<T> {
    #[serde(rename = "id")]
    _id: u64,
    response: HyperliquidActionResponse<T>,
}
#[derive(serde::Deserialize)]
struct HyperliquidActionResponse<T> {
    #[serde(rename = "type")]
    r#type: SmolStr,
    payload: T,
}
#[derive(serde::Deserialize)]
struct HyperliquidResponseEnvelope<T> {
    data: T,
}
#[derive(serde::Deserialize)]
struct HyperliquidDirectResponse<T> {
    response: T,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use app_config::{market_participants::origins::OriginPair, network::ResolvedProxy};
    use app_config_provider::notification::ConfigNotification;
    use market_participants_model::{
        identifier::{IdentifierValue, VenueId, VenueScopeId, WalletAddress},
        listing::route::VenueRoute,
        market::book::MarketSequence,
        routing::CapabilityId,
    };
    use network::transport::hub::{TransportHub, TransportPolicy};

    use super::*;
    use crate::{
        adapter::{
            Authentication, PreparedRequest, hyperliquid::adapter::contexts::fast_contexts_payload,
        },
        connection::ConnectionPool,
        market_participants::model::{NativeHyperliquidAssetContext, OrderBookQuery},
        protocol::RequestIdentity,
        test_support::InfoSocket,
    };

    fn route(scope: &str) -> VenueRoute {
        VenueRoute::new(
            VenueId::Hyperliquid,
            VenueScopeId::new(scope).expect("the fixture scope is canonical"),
        )
    }

    /// The listing key of one asset of one perpetual route.
    fn perp_key(route: &VenueRoute, index_in_meta: u32) -> VenueListingKey {
        let asset_id =
            catalog::perp_asset_id(0, index_in_meta).expect("the fixture asset ID should fit");
        catalog::listing_key(route, catalog::perpetual_continuity(&route.scope, asset_id))
            .expect("the fixture listing key should derive")
    }

    /// The identity of one capability of one scope.
    fn identity(scope: &str, capability: CapabilityId) -> RequestIdentity {
        RequestIdentity::new(route(scope), capability)
    }

    /// The venue's own public origins, spelled as the settings spell them.
    fn fixture_origins() -> OriginPair {
        OriginPair {
            http: "https://api.hyperliquid.xyz/"
                .parse()
                .expect("the origin parses"),
            websocket: "wss://api.hyperliquid.xyz/ws"
                .parse()
                .expect("the origin parses"),
        }
    }

    fn fixture_context(
        identity: RequestIdentity,
        auth: Option<Authentication>,
    ) -> ExecutionContext {
        let transport = Arc::new(TransportHub::new(TransportPolicy::default()));
        let pool = Arc::new(ConnectionPool::with_transport(transport));
        let context =
            ExecutionContext::new(identity, fixture_origins(), reqwest::Client::new(), pool);
        match auth {
            Some(auth) => context.with_authentication(auth),
            None => context,
        }
    }

    /// The context of one request whose origins are a local fixture server.
    fn fixture_context_at(identity: RequestIdentity, origin: &str) -> ExecutionContext {
        let transport = Arc::new(TransportHub::new(TransportPolicy::default()));
        let pool = Arc::new(ConnectionPool::with_transport(transport));
        let origin: url::Url = origin.parse().expect("the fixture origin is a URL");
        let origins = OriginPair {
            http: origin.clone(),
            websocket: origin,
        };
        ExecutionContext::new(identity, origins, reqwest::Client::new(), pool)
    }

    /// The venue metadata one fixture server answers.
    ///
    /// The load reads this value once and commits it; a fixture context carries what
    /// a commit would have handed the adapter, so a test drives the request path
    /// against the metadata of a reading rather than against a fetch of its own.
    async fn fixture_reading(origin: &str) -> Arc<VenueMetadata> {
        let context = fixture_venue_context(origin);
        let VenueReading::Verified { metadata, .. } = HyperliquidAdapter::new()
            .read_venue(&context)
            .await
            .expect("the fixture answers one reading")
        else {
            panic!("the fixture's deployments all verify");
        };
        metadata
    }

    /// The context of one venue reading whose origin is a local fixture server.
    ///
    /// The read addresses the venue rather than a route: it carries the venue
    /// identity and the origin its configuration gives it, and no request identity.
    fn fixture_venue_context(origin: &str) -> VenueExecutionContext {
        let origin: url::Url = origin.parse().expect("the fixture origin is a URL");
        VenueExecutionContext::new(
            VenueId::Hyperliquid,
            OriginPair {
                http: origin.clone(),
                websocket: origin,
            },
            reqwest::Client::new(),
        )
    }

    /// The context one wallet read of one scope runs in, as the gateway resolves it.
    fn wallet_context(
        scope: &str,
        snapshot: &MarketParticipantNetworkSnapshot,
        proxy: ResolvedProxy,
        notification: ConfigNotification<MarketParticipantNetworkSnapshot>,
    ) -> ExecutionContext {
        let resolved = snapshot.network_route_generation(&route(scope).venue);
        fixture_context(identity(scope, CapabilityId::Wallet), None)
            .with_proxy(proxy, resolved)
            .with_runtime(notification)
    }

    /// One public wallet read of one scope, as the pane issues it.
    fn wallet_request(scope: &str) -> ProtocolRequest {
        ProtocolRequest::FetchHyperliquidOpenOrders(HyperliquidWalletQuery::public(
            route(scope),
            "0x1111111111111111111111111111111111111111"
                .parse::<WalletAddress>()
                .expect("the fixture address is valid"),
        ))
    }

    /// The connection key one read of one scope dials with.
    ///
    /// Which key a connection is opened by is the context's answer rather than the request's,
    /// so one fixture request drives every capability's key here; what varies is the scope.
    fn key_of(scope: &str, context: &ExecutionContext) -> ConnectionKey {
        connection_key(context, &wallet_request(scope))
            .expect("the fixture context publishes a WebSocket origin")
    }

    /// A connection is dialled at the stream address the settings resolved, exactly as it stands.
    ///
    /// This venue publishes its stream under a path of its own — `wss://api.hyperliquid.xyz/ws`
    /// is the whole address and `wss://api.hyperliquid.xyz/` is not one it answers on — so the
    /// endpoint a socket is keyed by is the configuration's address and nothing is appended to
    /// it. A build that appended a path here would dial a socket that closes on connect.
    #[test]
    fn a_connection_is_dialled_at_the_stream_address_the_settings_resolve() {
        let snapshot = MarketParticipantNetworkSnapshot::default();
        let notification = ConfigNotification::new(Arc::new(snapshot.clone()));
        let context = wallet_context(
            CORE_PERP_SCOPE,
            &snapshot,
            ResolvedProxy::NoProxy,
            notification,
        );

        assert_eq!(
            key_of(CORE_PERP_SCOPE, &context).endpoint.as_str(),
            "wss://api.hyperliquid.xyz/ws",
            "the key is the address the operator's settings resolved"
        );
    }

    /// Folding stops at the hop: a scope dialled through its own proxy keeps its socket.
    ///
    /// The key records what a socket was dialled with, so a scope whose proxy differs from
    /// the canonical one cannot be served by the same socket. Every other dimension is
    /// already folded, which leaves the hop as the only thing that can keep two wallet
    /// scopes of one venue apart.
    #[test]
    fn a_wallet_scope_dialled_through_its_own_proxy_keeps_its_own_connection() {
        let snapshot = MarketParticipantNetworkSnapshot::default();
        let notification = ConfigNotification::new(Arc::new(snapshot.clone()));

        let spot = wallet_context(
            CORE_SPOT_SCOPE,
            &snapshot,
            ResolvedProxy::NoProxy,
            notification.clone(),
        );
        let proxied = wallet_context(
            CORE_PERP_SCOPE,
            &snapshot,
            ResolvedProxy::Socks5 {
                host: SmolStr::new_static("127.0.0.1"),
                port: 1080,
                credentials: None,
                resolve_dns_at_remote: true,
            },
            notification,
        );

        assert_ne!(
            key_of(CORE_SPOT_SCOPE, &spot),
            key_of(CORE_PERP_SCOPE, &proxied)
        );
    }

    /// Every wallet-monitor scope of the venue dials one connection key.
    ///
    /// The venue caps one IP at ten WebSocket connections, and one monitored address is read
    /// across the core spot scope, the core perpetual scope and every deployed perpetual DEX.
    /// The pool decides whether a socket is reused by key equality, so key equality is the
    /// property that has to hold for a monitored address to cost one connection instead of one
    /// per deployed scope.
    #[test]
    fn every_wallet_scope_of_the_venue_shares_one_connection_key() {
        let snapshot = MarketParticipantNetworkSnapshot::default();
        let notification = ConfigNotification::new(Arc::new(snapshot.clone()));
        let context = |scope: &str| {
            wallet_context(
                scope,
                &snapshot,
                ResolvedProxy::NoProxy,
                notification.clone(),
            )
        };

        let spot = context(CORE_SPOT_SCOPE);
        let perp = context(CORE_PERP_SCOPE);
        let deployed = context("perp-dex/xyz");

        assert_eq!(
            key_of(CORE_SPOT_SCOPE, &spot),
            key_of(CORE_PERP_SCOPE, &perp)
        );
        assert_eq!(
            key_of(CORE_PERP_SCOPE, &perp),
            key_of("perp-dex/xyz", &deployed)
        );
    }

    /// A wallet read without the runtime snapshot is refused rather than mis-keyed.
    ///
    /// The folded generation is the canonical scope's, and only the runtime snapshot knows it.
    /// Keying the connection with the opening scope's generation instead would name the
    /// canonical target while carrying a number the pool never compares it against, so the
    /// first snapshot would close a healthy socket. The adapter reports the missing snapshot as
    /// the internal contract violation it is, not as something the user could fix.
    #[test]
    fn a_wallet_connection_without_a_runtime_snapshot_is_refused() {
        let context = fixture_context(identity(CORE_SPOT_SCOPE, CapabilityId::Wallet), None);

        let error = connection_key(&context, &wallet_request(CORE_SPOT_SCOPE))
            .expect_err("a folded key cannot be built without the runtime snapshot");

        assert!(
            matches!(error, MarketParticipantError::Internal(_)),
            "the refusal should say the runtime snapshot is missing, got {error:?}"
        );
    }

    /// Folding the wallet target folds nothing else: market data keeps its scopes.
    ///
    /// Folding is the context's answer rather than the request's — `connection_key` asks the
    /// capability the request was resolved against — so the same fixture request drives both.
    #[test]
    fn market_data_scopes_keep_their_own_connection_keys() {
        let snapshot = MarketParticipantNetworkSnapshot::default();
        let notification = ConfigNotification::new(Arc::new(snapshot.clone()));
        let market_context = |scope: &str| {
            let resolved = snapshot.network_route_generation(&route(scope).venue);
            fixture_context(identity(scope, CapabilityId::Tickers), None)
                .with_proxy(ResolvedProxy::NoProxy, resolved)
                .with_runtime(notification.clone())
        };

        let spot = key_of(CORE_SPOT_SCOPE, &market_context(CORE_SPOT_SCOPE));
        let perp = key_of(CORE_PERP_SCOPE, &market_context(CORE_PERP_SCOPE));

        assert_ne!(spot, perp);
        assert_eq!(perp.route.scope.as_str(), CORE_PERP_SCOPE);
    }

    /// A folded key still says which venue and scope the socket serves.
    #[test]
    fn a_folded_wallet_key_names_the_canonical_wallet_scope() {
        let snapshot = MarketParticipantNetworkSnapshot::default();
        let notification = ConfigNotification::new(Arc::new(snapshot.clone()));
        let context = wallet_context(
            "perp-dex/xyz",
            &snapshot,
            ResolvedProxy::NoProxy,
            notification,
        );
        let key = key_of("perp-dex/xyz", &context);

        assert_eq!(key.route.venue, VenueId::Hyperliquid);
        assert_eq!(key.route.scope.as_str(), HYPERLIQUID_WALLET_SCOPE);
    }

    /// Discovery answers exactly the deployed DEXs whose metadata verified.
    ///
    /// A reading reports a deployment it cannot verify instead of dropping it.
    ///
    /// The venue's first `perpDexs` entry is the core DEX and is never a scope of
    /// its own; a deployed DEX whose coins are not the `{dex}:{coin}` form the
    /// venue reserves, and one whose collateral resolves to no spot token, are
    /// deployments this build cannot serve. The reading names the first of them
    /// rather than registering a scope set short of what the venue published,
    /// and it stops there: nothing after an unverifiable deployment is read.
    ///
    /// One reading is one conversation: the deployment table, the token table and
    /// every scope's own metadata are read once, so the scopes a commit registers
    /// and the metadata it publishes describe the same moment of the venue.
    #[tokio::test]
    async fn a_reading_reports_a_deployment_it_cannot_verify() {
        let (origin, requests, server) = crate::test_support::info_fixture_server(
            vec![
                r#"[null,{"name":"xyz"},{"name":"bad"},{"name":"noc"}]"#.to_owned(),
            ],
            crate::test_support::spot_meta_answer(&["USDC"]),
            crate::test_support::dex_meta(&["BTC"]),
            vec![
                (
                    "xyz".to_owned(),
                    crate::test_support::dex_meta(&["xyz:ABC", "xyz:DEF"]),
                ),
                ("bad".to_owned(), crate::test_support::dex_meta(&["ABC"])),
                (
                    "noc".to_owned(),
                    r#"{"universe":[{"name":"noc:ABC","szDecimals":3,"maxLeverage":10}],"marginTables":[],"collateralToken":7}"#
                        .to_owned(),
                ),
            ],
        )
        .await;
        let context = fixture_venue_context(&origin);

        let reading = HyperliquidAdapter::new()
            .read_venue(&context)
            .await
            .expect("the fixture conversation answers");

        assert!(
            matches!(&reading, VenueReading::Unverifiable { dex } if dex == "bad"),
            "the deployment whose coins are not its own is reported, not dropped: {reading:?}"
        );
        let served = requests.lock().clone();
        let count = |kind: &str| served.iter().filter(|seen| *seen == kind).count();
        assert_eq!(count("perpDexs"), 1, "one deployment table: {served:?}");
        assert_eq!(
            count("spotMeta"),
            0,
            "the token table is not asked for on its own: the spot answer carries it: {served:?}"
        );
        assert_eq!(
            count("spotMetaAndAssetCtxs"),
            1,
            "the spot scope's answer is read once, and it holds the token table: {served:?}"
        );
        assert_eq!(
            count("metaAndAssetCtxs"),
            3,
            "the core scope, the deployment that verifies and the one that does not — \
             the deployments after the refused one are never read: {served:?}"
        );
        server.abort();
    }

    /// A reading refuses a venue that names more deployments than it verifies.
    ///
    /// The venue decides how many DEXs it names, so the bound is this build's own
    /// answer about how much work one answer may cause — and a venue over it is
    /// refused whole rather than truncated. The bound is checked against the
    /// deployment table alone, so not one deployment is read: the fixture serves the
    /// table and nothing else.
    #[tokio::test]
    async fn a_reading_refuses_more_deployments_than_it_verifies() {
        // The venue's deployed names are two to four lowercase letters, so the fixture
        // claims more DEXs than the bound with names of that shape.
        let name_of = |index: usize| {
            format!(
                "{}{}",
                (b'a' + (index / 26) as u8) as char,
                (b'a' + (index % 26) as u8) as char
            )
        };
        let claimed = MAX_DEPLOYED_PERP_DEXES + 4;
        let named: Vec<String> = (0..claimed)
            .map(|index| format!(r#"{{"name":"{}"}}"#, name_of(index)))
            .collect();
        let dex_meta: Vec<(String, String)> = (0..claimed)
            .map(|index| {
                let name = name_of(index);
                let coin = format!("{name}:ABC");
                (name, crate::test_support::dex_meta(&[coin.as_str()]))
            })
            .collect();
        let (origin, requests, server) = crate::test_support::info_fixture_server(
            vec![format!("[null,{}]", named.join(","))],
            crate::test_support::spot_meta_answer(&["USDC"]),
            crate::test_support::dex_meta(&["BTC"]),
            dex_meta,
        )
        .await;
        let context = fixture_venue_context(&origin);

        let reading = HyperliquidAdapter::new()
            .read_venue(&context)
            .await
            .expect("the fixture conversation answers");

        assert!(
            matches!(
                &reading,
                VenueReading::LimitExceeded { named, limit }
                    if *named == claimed && *limit == MAX_DEPLOYED_PERP_DEXES
            ),
            "the venue claimed {claimed} deployments: {reading:?}"
        );
        let served = requests.lock().clone();
        assert_eq!(
            served,
            vec!["perpDexs".to_owned()],
            "the bound is decided from the deployment table alone: {served:?}"
        );
        server.abort();
    }

    /// A route set is decoded as one table of every route's catalogs.
    ///
    /// The answer to a venue-wide read names the coins of every scope the venue serves, so
    /// the table it is decoded with has to be the union — and a route whose catalog cannot
    /// be read has to fail the read rather than leave its coins out of the table.
    #[tokio::test]
    async fn a_route_set_decodes_against_the_union_of_its_catalogs() {
        let (origin, _requests, server) = crate::test_support::info_fixture_server(
            vec![r#"[null,{"name":"xyz"}]"#.to_owned()],
            crate::test_support::spot_meta_answer(&["USDC"]),
            crate::test_support::dex_meta(&["BTC"]),
            vec![(
                "xyz".to_owned(),
                crate::test_support::dex_meta(&["xyz:ABC"]),
            )],
        )
        .await;
        let adapter = HyperliquidAdapter::new();
        let context = fixture_context_at(identity(CORE_PERP_SCOPE, CapabilityId::Wallet), &origin)
            .with_venue_metadata(Some(fixture_reading(&origin).await));

        let union = adapter
            .route_listings_across(&context, &[route(CORE_PERP_SCOPE), route("perp-dex/xyz")])
            .expect("both monitored routes publish a catalog");
        assert!(union.contains_key(&SmolStr::new_static("BTC")));
        assert!(
            union.contains_key(&SmolStr::new_static("xyz:ABC")),
            "the deployed route's coin is part of the union"
        );

        let addressed = adapter
            .route_listings_across(&context, &[route(CORE_PERP_SCOPE)])
            .expect("the core route publishes a catalog");
        assert!(addressed.contains_key(&SmolStr::new_static("BTC")));
        assert!(
            !addressed.contains_key(&SmolStr::new_static("xyz:ABC")),
            "a set without the deployed route has no catalog for its coin"
        );

        // A route the fixture serves no catalog for fails the whole table rather than
        // being dropped from it.
        adapter
            .route_listings_across(
                &context,
                &[route(CORE_PERP_SCOPE), route("perp-dex/absent")],
            )
            .expect_err("a monitored route without a readable catalog fails the read");
        server.abort();
    }

    /// A coin two monitored routes publish is refused, not resolved by the last merge.
    ///
    /// A route's table is derived from the venue's own `pair.name` / `asset.name` and no
    /// shape is checked there, so a venue that names a core perpetual `xyz:ABC` while the
    /// deployed route publishes that same coin makes one coin mean two listings. Keeping
    /// either one would decode the other route's rows with the wrong precision and units,
    /// which is a wrong number rather than a missing one — so the merge is where the
    /// untrusted naming is checked, as `is_verified_perp_dex` checks it for a DEX.
    #[tokio::test]
    async fn a_coin_two_monitored_routes_publish_is_refused() {
        let (origin, _requests, server) = crate::test_support::info_fixture_server(
            vec![r#"[null,{"name":"xyz"}]"#.to_owned()],
            crate::test_support::spot_meta_answer(&["USDC"]),
            // The core route publishes the deployed coin form under its own scope.
            crate::test_support::dex_meta(&["xyz:ABC"]),
            vec![(
                "xyz".to_owned(),
                crate::test_support::dex_meta(&["xyz:ABC"]),
            )],
        )
        .await;
        let context = fixture_context_at(identity(CORE_PERP_SCOPE, CapabilityId::Wallet), &origin)
            .with_venue_metadata(Some(fixture_reading(&origin).await));

        let error = HyperliquidAdapter::new()
            .route_listings_across(&context, &[route(CORE_PERP_SCOPE), route("perp-dex/xyz")])
            .expect_err("one coin two routes publish is not decodable");

        assert!(
            matches!(
                error,
                MarketParticipantError::ProtocolError { ref operation, .. }
                    if operation == "merge the catalogs of the monitored wallet routes"
            ),
            "{error:?}"
        );
        server.abort();
    }

    fn asset(name: &str, max_leverage: u32) -> HyperliquidMetaAsset {
        HyperliquidMetaAsset {
            name: SmolStr::new(name),
            sz_decimals: 3,
            max_leverage,
            is_delisted: false,
            margin_mode: None,
            margin_table_id: None,
        }
    }

    fn spot_meta(tokens: &[&str]) -> HyperliquidSpotMetaResponse {
        HyperliquidSpotMetaResponse {
            tokens: tokens
                .iter()
                .enumerate()
                .map(
                    |(index, name)| crate::adapter::hyperliquid::HyperliquidSpotToken {
                        name: SmolStr::new(*name),
                        sz_decimals: 8,
                        index: u32::try_from(index).expect("the fixture index should fit"),
                    },
                )
                .collect(),
            universe: Vec::new(),
        }
    }

    fn dex(name: &str) -> Option<HyperliquidPerpDex> {
        Some(HyperliquidPerpDex {
            name: SmolStr::new(name),
            full_name: None,
        })
    }

    /// A DEX is only verified when its metadata names a resolvable collateral and
    /// every coin in the form the venue reserves for builder-deployed assets.
    #[test]
    fn a_dex_is_verified_only_when_its_metadata_is_complete() {
        let spot = spot_meta(&["USDC"]);
        let prefixed = HyperliquidMetaResponse {
            universe: vec![asset("xyz:ABC", 10), asset("xyz:DEF", 20)],
            margin_tables: Vec::new(),
            collateral_token: Some(0),
        };
        assert!(is_verified_perp_dex("xyz", 1, &prefixed, &spot));

        let unprefixed = HyperliquidMetaResponse {
            universe: vec![asset("ABC", 10)],
            ..prefixed.clone()
        };
        assert!(
            !is_verified_perp_dex("xyz", 1, &unprefixed, &spot),
            "a coin outside the {{dex}}:{{coin}} form is not a deployed asset"
        );

        let other_dex = HyperliquidMetaResponse {
            universe: vec![asset("abc:ABC", 10)],
            ..prefixed.clone()
        };
        assert!(
            !is_verified_perp_dex("xyz", 1, &other_dex, &spot),
            "a coin of another DEX is not this scope's asset"
        );

        let no_collateral = HyperliquidMetaResponse {
            collateral_token: None,
            ..prefixed.clone()
        };
        assert!(!is_verified_perp_dex("xyz", 1, &no_collateral, &spot));

        let unknown_collateral = HyperliquidMetaResponse {
            collateral_token: Some(9),
            ..prefixed.clone()
        };
        assert!(!is_verified_perp_dex("xyz", 1, &unknown_collateral, &spot));

        let empty = HyperliquidMetaResponse {
            universe: Vec::new(),
            ..prefixed.clone()
        };
        assert!(!is_verified_perp_dex("xyz", 1, &empty, &spot));

        let unleveraged = HyperliquidMetaResponse {
            universe: vec![asset("xyz:ABC", 0)],
            ..prefixed
        };
        assert!(!is_verified_perp_dex("xyz", 1, &unleveraged, &spot));
    }

    /// A DEX index is the venue's own position, and a name it does not publish as a
    /// deployment has no index at all.
    ///
    /// Whether the name has a scope shape this build reserves is a different
    /// question: the position exists for every name the venue published, and the
    /// reading is what refuses a name it cannot register.
    #[test]
    fn a_dex_index_is_the_published_position_or_nothing() {
        let dexes = [None, dex("test"), dex("xyz"), dex("A")];

        assert_eq!(dex_index(&dexes, "test").expect("test is deployed"), 1);
        assert_eq!(dex_index(&dexes, "xyz").expect("xyz is deployed"), 2);
        assert_eq!(
            dex_index(&dexes, "A").expect("A is a name the venue published"),
            3
        );
        assert!(dex_index(&dexes, "nope").is_err());
        assert!(
            catalog::perp_dex_scope("A").is_err(),
            "a name outside the namespace builds no scope"
        );
    }

    /// Hyperliquid publishes a twenty-level book, so a deeper request is refused
    /// before anything is resolved.
    #[tokio::test]
    async fn an_order_book_deeper_than_the_venue_publishes_is_refused() {
        let adapter = HyperliquidAdapter::new();
        let route = route(CORE_PERP_SCOPE);
        let query = OrderBookQuery {
            listing: perp_key(&route, 0),
            depth: ORDER_BOOK_DEPTH_LIMIT + 1,
        };
        let error = adapter
            .execute(
                fixture_context(identity(CORE_PERP_SCOPE, CapabilityId::OrderBook), None),
                PreparedRequest::new(ProtocolRequest::FetchOrderBook(query), None),
            )
            .await
            .expect_err("a depth above the venue's own limit is refused");

        assert!(matches!(
            error,
            MarketParticipantError::InvalidLimit {
                maximum: ORDER_BOOK_DEPTH_LIMIT,
                ..
            }
        ));
    }

    /// One `l2Book` event is a full snapshot numbered by the venue's own watermark
    /// and never by an invented counter.
    #[test]
    fn an_order_book_event_keeps_the_venue_watermark_as_a_native_sequence() {
        let route = route(CORE_PERP_SCOPE);
        let listing = perp_key(&route, 0);
        let event = decode_event(
            br#"{"channel":"l2Book","data":{"coin":"BTC","time":200,"levels":[[{"px":"100","sz":"1"}],[{"px":"101","sz":"2"}]]}}"#,
            &SubscriptionKind::OrderBook {
                listing,
                coin: SmolStr::new_static("BTC"),
                depth: 1,
            },
            None,
        )
        .expect("the order-book event should decode")
        .expect("the order-book event should publish");

        let ProtocolEvent::OrderBook(update) = event else {
            panic!("expected an order-book event");
        };
        assert!(update.data.snapshot);
        assert_eq!(
            update.data.first_sequence,
            MarketSequence::Native(
                IdentifierValue::new("200").expect("the watermark is a valid identity value")
            )
        );
        assert_eq!(update.data.first_sequence, update.data.last_sequence);
        assert_eq!(update.data.previous_sequence, None);
        assert_eq!(update.data.timestamp, Some(200));
        assert!(!matches!(
            update.data.first_sequence,
            MarketSequence::Numeric(_)
        ));
        assert_eq!(update.data.bids.len(), 1);
        assert_eq!(update.data.asks.len(), 1);
    }

    /// Hyperliquid publishes no rolling-statistics identifier, so a ticker never
    /// claims one.
    #[test]
    fn a_ticker_never_claims_a_monotonic_id_the_venue_did_not_publish() {
        let route = route(CORE_PERP_SCOPE);
        let listing = perp_key(&route, 0);
        let event = decode_event(
            br#"{"channel":"activeSpotAssetCtx","data":{"coin":"@107","ctx":{"prevDayPx":"23.993","dayNtlVlm":"228614808.04913011","markPx":"23.277","midPx":"23.281","dayBaseVlm":"9756716.93999999"}}}"#,
            &SubscriptionKind::Ticker {
                listing,
                coin: SmolStr::new_static("@107"),
            },
            None,
        )
        .expect("the spot context should decode")
        .expect("the spot context should publish a ticker");

        let ProtocolEvent::Ticker(ticker) = event else {
            panic!("expected a ticker event");
        };
        assert_eq!(ticker.data.monotonic_id, None);
        assert_eq!(ticker.data.close.to_string(), "23.281");
        assert_eq!(ticker.data.open.to_string(), "23.993");
        assert_eq!(
            ticker
                .data
                .traded_quantity
                .map(|quantity| quantity.to_string()),
            Some("9756716.93999999".to_owned())
        );
    }

    /// A context without a price is a failure, not a silently empty ticker, and a
    /// context of another coin is not this subscription's event.
    #[test]
    fn a_context_is_published_only_for_its_own_coin_and_only_with_a_price() {
        let route = route(CORE_PERP_SCOPE);
        let listing = perp_key(&route, 0);
        let other = decode_event(
            br#"{"channel":"activeAssetCtx","data":{"coin":"BTC","ctx":{"prevDayPx":"1","dayNtlVlm":"2","markPx":"3"}}}"#,
            &SubscriptionKind::Ticker {
                listing: listing.clone(),
                coin: SmolStr::new_static("ETH"),
            },
            None,
        )
        .expect("another coin's context should decode");
        assert!(other.is_none());

        let error = decode_event(
            br#"{"channel":"activeAssetCtx","data":{"coin":"BTC","ctx":{"prevDayPx":"1","dayNtlVlm":"2","markPx":null,"midPx":null}}}"#,
            &SubscriptionKind::Ticker {
                listing,
                coin: SmolStr::new_static("BTC"),
            },
            None,
        )
        .err()
        .expect("a context without a price must not be discarded");

        assert!(matches!(
            error,
            MarketParticipantError::ProtocolError { operation, .. }
                if operation == "decode a Hyperliquid ticker price"
        ));
    }

    /// One wire order decodes with the listing the route published for its coin.
    #[test]
    fn a_wire_order_keeps_its_listing_and_its_own_kind() {
        let route = route(CORE_PERP_SCOPE);
        let listing = perp_key(&route, 5);
        let order = decode_open_order(
            &listing,
            HyperliquidOrderWire {
                coin: SmolStr::new_static("BTC"),
                oid: 7,
                limit_px: SmolStr::new_static("100.5"),
                side: SmolStr::new_static("B"),
                sz: SmolStr::new_static("1"),
                orig_sz: Some(SmolStr::new_static("2")),
                timestamp: 1_700_000_000_000,
                reduce_only: true,
                order_type: SmolStr::new_static("Limit"),
                is_trigger: false,
                trigger_px: None,
                tif: Some(SmolStr::new_static("Alo")),
            },
        )
        .expect("the open order should decode");

        assert_eq!(order.listing, listing);
        assert_eq!(order.side, Side::Buy);
        assert_eq!(order.quantity.to_string(), "2");
        assert_eq!(order.price.to_string(), "100.5");
        assert!(order.reduce_only);
        assert_eq!(
            order.kind,
            OrderKind::Limit {
                price: "100.5".parse().expect("the fixture price parses"),
                time_in_force: TimeInForce::PostOnly,
            }
        );
        assert_eq!(order.order_id.as_str(), "7");
    }

    /// The ledger skips delta types this does not model and reports a malformed one
    /// of a type it does model.
    #[test]
    fn a_ledger_skips_unknown_deltas_and_reports_malformed_known_ones() {
        let address = "0x1111111111111111111111111111111111111111"
            .parse::<WalletAddress>()
            .expect("the fixture address is valid");
        let events = decode_ledger_rows(
            &address,
            vec![
                ledger_row(serde_json::json!({ "type": "deposit", "usdc": "5" })),
                // A vault deposit is a move this application does not describe, and
                // one of them must not cost the wallet every other row of its ledger.
                ledger_row(
                    serde_json::json!({ "type": "vaultDeposit", "vault": "0xabc", "usdc": "1" }),
                ),
                ledger_row(serde_json::json!({ "type": "deposit", "usdc": "2" })),
            ],
            &empty_listings(),
        )
        .expect("the rows this models should decode");

        assert_eq!(events.len(), 2);
        assert_eq!(
            events
                .iter()
                .map(|event| event.amount.expect("an amount should decode").to_string())
                .collect::<Vec<_>>(),
            ["5", "2"]
        );
        assert_eq!(
            events[0].transfer_direction,
            Some(HyperliquidTransferDirection::Incoming)
        );

        let error = decode_ledger_rows(
            &address,
            vec![ledger_row(
                serde_json::json!({ "type": "withdraw", "usdc": "not a number" }),
            )],
            &empty_listings(),
        )
        .expect_err("a malformed withdrawal should be rejected");
        assert!(matches!(
            error,
            MarketParticipantError::ProtocolError { operation, .. }
                if operation == "decode a Hyperliquid withdrawal"
        ));
    }

    /// A transfer is read from the wallet the ledger belongs to, and a wallet that
    /// is neither end is not told which way it went.
    #[test]
    fn a_transfer_states_the_direction_from_the_wallet_it_belongs_to() {
        let wallet = "0x1111111111111111111111111111111111111111"
            .parse::<WalletAddress>()
            .expect("the fixture address is valid");
        let other = "0x2222222222222222222222222222222222222222";

        let outgoing = decode_ledger(
            &wallet,
            ledger_row(serde_json::json!({
                "type": "internalTransfer", "usdc": "3",
                "user": wallet.as_str(), "destination": other,
            })),
            &empty_listings(),
        )
        .expect("the row should decode")
        .expect("an internal transfer is a row this models");
        assert_eq!(
            outgoing.transfer_direction,
            Some(HyperliquidTransferDirection::Outgoing)
        );
        assert_eq!(
            outgoing
                .related_wallet
                .map(|wallet| wallet.as_str().to_owned()),
            Some(other.to_owned())
        );

        let neither = decode_ledger(
            &wallet,
            ledger_row(serde_json::json!({
                "type": "subAccountTransfer", "usdc": "3",
                "user": "0x3333333333333333333333333333333333333333",
                "destination": "0x4444444444444444444444444444444444444444",
            })),
            &empty_listings(),
        )
        .expect("the row should decode")
        .expect("a sub-account transfer is a row this models");
        assert_eq!(neither.transfer_direction, None);
        assert_eq!(neither.related_wallet, None);
    }

    /// A liquidation row reports the notional the venue closed and the positions
    /// it closed, each resolved to a listing of the route and leaving out a coin
    /// the route does not publish.
    #[test]
    fn a_liquidation_reports_its_notional_and_its_positions() {
        let address = "0x1111111111111111111111111111111111111111"
            .parse::<WalletAddress>()
            .expect("the fixture address is valid");
        let route = route(CORE_PERP_SCOPE);
        let listed = perp_key(&route, 0);
        let listings: RouteListings = Arc::new(BTreeMap::from([(
            SmolStr::new_static("BTC"),
            listed.clone(),
        )]));

        let event = decode_ledger(
            &address,
            ledger_row(serde_json::json!({
                "type": "liquidation",
                "liquidatedNtlPos": "1234.5",
                "accountValue": "0",
                "leverageType": "Cross",
                "liquidatedPositions": [
                    { "coin": "BTC", "szi": "-0.5" },
                    { "coin": "xyz:NVDA", "szi": "3" },
                ],
            })),
            &listings,
        )
        .expect("the row should decode")
        .expect("a liquidation is a row this models");

        assert_eq!(
            event
                .liquidated_notional
                .expect("the venue reports the closed notional")
                .to_string(),
            "1234.5"
        );
        assert_eq!(
            event.amount, None,
            "a liquidation moves no token of its own"
        );
        assert_eq!(
            event.kind.as_str(),
            "liquidation",
            "the row keeps the venue's own type name"
        );
        assert_eq!(event.liquidated_positions.len(), 1);
        assert_eq!(event.liquidated_positions[0].listing, listed);
        assert_eq!(event.liquidated_positions[0].size.to_string(), "-0.5");
    }

    /// A malformed liquidation is reported against its own type instead of being
    /// skipped as an unmodelled delta.
    #[test]
    fn a_malformed_liquidation_is_reported() {
        let address = "0x1111111111111111111111111111111111111111"
            .parse::<WalletAddress>()
            .expect("the fixture address is valid");
        let error = decode_ledger(
            &address,
            ledger_row(serde_json::json!({
                "type": "liquidation",
                "liquidatedNtlPos": "not a number",
                "liquidatedPositions": [],
            })),
            &empty_listings(),
        )
        .expect_err("a malformed liquidation should be rejected");

        assert!(matches!(
            error,
            MarketParticipantError::ProtocolError { operation, .. }
                if operation == "decode a Hyperliquid liquidation"
        ));
    }

    /// The listings of a fixture route, for the ledger rows that name no coin.
    fn empty_listings() -> RouteListings {
        Arc::new(BTreeMap::new())
    }

    /// The wallet streams are the ones that decode against catalogs, and each of them
    /// decodes against the whole route set it was opened with.
    #[test]
    fn every_wallet_stream_decodes_against_the_routes_it_was_opened_with() {
        let core = route(CORE_PERP_SCOPE);
        let deployed = route("perp-dex/xyz");
        let routes = vec![core.clone(), deployed];
        let address = "0x1111111111111111111111111111111111111111"
            .parse::<WalletAddress>()
            .expect("the fixture address is valid");

        for kind in [
            SubscriptionKind::WalletFills {
                address: address.clone(),
                routes: routes.clone(),
            },
            SubscriptionKind::WalletLedger {
                address,
                routes: routes.clone(),
            },
            SubscriptionKind::FastContexts {
                routes: routes.clone(),
                board: Arc::new(FastContextBoard::default()),
            },
        ] {
            assert_eq!(kind.decoding_routes(), Some(routes.as_slice()));
        }

        // A listing stream resolved its coin when it was subscribed.
        let listing = SubscriptionKind::Ticker {
            listing: perp_key(&core, 0),
            coin: SmolStr::new_static("BTC"),
        };
        assert_eq!(listing.decoding_routes(), None);
    }

    /// The venue's answer to one `userFills` request: one core coin and one deployed coin.
    fn fill_answer() -> String {
        serde_json::json!([
            {
                "coin": "BTC", "side": "B", "sz": "0.5", "px": "100.5",
                "time": 1_700_000_000_000_i64, "startPosition": "0", "closedPnl": "0",
                "fee": "0.01", "feeToken": "USDC", "tid": 1, "hash": "0xabc",
            },
            {
                "coin": "xyz:ABC", "side": "A", "sz": "3", "px": "42.5",
                "time": 1_700_000_001_000_i64, "startPosition": "0", "closedPnl": "0",
                "fee": "0.02", "feeToken": "USDC", "tid": 2, "hash": "0xdef",
            },
        ])
        .to_string()
    }

    /// The venue's answer to one `userNonFundingLedgerUpdates` request: one liquidation
    /// whose closed positions are one core coin and one deployed coin.
    fn ledger_answer() -> String {
        serde_json::json!([
            {
                "time": 1_700_000_000_000_i64,
                "hash": "0xabc",
                "delta": {
                    "type": "liquidation",
                    "liquidatedNtlPos": "1234.5",
                    "accountValue": "0",
                    "leverageType": "Cross",
                    "liquidatedPositions": [
                        { "coin": "BTC", "szi": "-0.5" },
                        { "coin": "xyz:ABC", "szi": "3" },
                    ],
                },
            },
        ])
        .to_string()
    }

    /// The routes of the positions one ledger's liquidations closed, in arrival order.
    fn liquidation_scopes(events: &[NativeHyperliquidLedgerEvent]) -> Vec<&str> {
        events
            .iter()
            .flat_map(|event| event.liquidated_positions.iter())
            .map(|position| position.listing.route.scope.as_str())
            .collect()
    }

    /// The venue's answer to one `historicalOrders` request: one core coin and one
    /// deployed coin.
    fn historical_answer() -> String {
        let order = |coin: &str, oid: u64| {
            serde_json::json!({
                "order": {
                    "coin": coin, "oid": oid, "limitPx": "42.5", "side": "B", "sz": "3",
                    "timestamp": 1_700_000_000_000_i64, "reduceOnly": false,
                    "orderType": "Limit", "isTrigger": false, "triggerPx": null, "tif": "Gtc",
                },
                "status": "filled",
            })
        };
        serde_json::json!([order("BTC", 1), order("xyz:ABC", 2)]).to_string()
    }

    /// The context of one wallet read whose origins are a local fixture: the catalogs come
    /// over plain HTTP and the read itself over a plain socket, so the transport is built
    /// with the plaintext policy a test owns.
    fn wallet_fixture_context(
        identity: RequestIdentity,
        http_origin: &str,
        socket_url: &str,
    ) -> ExecutionContext {
        let transport = Arc::new(TransportHub::new(TransportPolicy {
            allow_plaintext: true,
            extra_root_certificates: Vec::new(),
        }));
        let pool = Arc::new(ConnectionPool::with_transport(transport));
        let snapshot = Arc::new(MarketParticipantNetworkSnapshot::default());
        let notification = ConfigNotification::new(snapshot);
        ExecutionContext::new(
            identity,
            OriginPair {
                http: http_origin.parse().expect("the fixture origin is a URL"),
                websocket: socket_url.parse().expect("the fixture socket is a URL"),
            },
            reqwest::Client::new(),
            pool,
        )
        .with_runtime(notification)
    }

    /// A wallet fill read is one venue-wide request decoded against every monitored route.
    ///
    /// The venue answers `userFills` with the coins of every scope it serves. Decoded
    /// against the addressed route's catalog alone, the deployed route's rows are dropped —
    /// which is what the second half of this test pins: the same answer, the same socket,
    /// and a one-route decoding set.
    #[tokio::test]
    async fn a_wallet_fill_read_decodes_every_monitored_routes_coins() {
        let (http_origin, _requests, catalogs) = crate::test_support::info_fixture_server(
            vec![r#"[null,{"name":"xyz"}]"#.to_owned()],
            crate::test_support::spot_meta_answer(&["USDC"]),
            crate::test_support::dex_meta(&["BTC"]),
            vec![(
                "xyz".to_owned(),
                crate::test_support::dex_meta(&["xyz:ABC"]),
            )],
        )
        .await;
        let socket = InfoSocket::spawn(fill_answer(), Vec::new()).await;
        let address = "0x1111111111111111111111111111111111111111"
            .parse::<WalletAddress>()
            .expect("the fixture address is valid");
        let core = route(CORE_PERP_SCOPE);
        let deployed = route("perp-dex/xyz");
        let query =
            HyperliquidWalletQuery::public_across(vec![core.clone(), deployed], address.clone())
                .expect("the monitored routes decode the answer");
        let context = wallet_fixture_context(
            identity(CORE_PERP_SCOPE, CapabilityId::Wallet),
            &http_origin,
            socket.url(),
        )
        .with_venue_metadata(Some(fixture_reading(&http_origin).await));
        let request = ProtocolRequest::FetchHyperliquidUserFills {
            query: query.clone(),
            aggregate_by_time: false,
        };

        let fills = HyperliquidAdapter::new()
            .wallet_fills(
                &context,
                &request,
                query.wallet_target(),
                query.decoding_routes(),
                false,
                None,
            )
            .await
            .expect("the fixture answers the fills read");

        assert_eq!(
            fills
                .iter()
                .map(|fill| fill.listing.route.scope.as_str())
                .collect::<Vec<_>>(),
            ["core-perp", "perp-dex/xyz"],
            "a venue-wide answer keeps the rows of every monitored route"
        );

        let addressed = HyperliquidWalletQuery::public(core, address);
        let fills = HyperliquidAdapter::new()
            .wallet_fills(
                &context,
                &request,
                addressed.wallet_target(),
                addressed.decoding_routes(),
                false,
                None,
            )
            .await
            .expect("the fixture answers the fills read");
        assert_eq!(
            fills
                .iter()
                .map(|fill| fill.listing.route.scope.as_str())
                .collect::<Vec<_>>(),
            ["core-perp"],
            "a one-route set has no catalog for the deployed route's coin"
        );

        catalogs.abort();
        socket.shutdown().await;
    }

    /// Two readers of one address hold one wire subscription however many routes each of
    /// them decodes against.
    ///
    /// The topic is the encoded subscription body, which names the address and never the
    /// decoding route set, so the socket carries one `userFills` subscription for that
    /// address and each reader decodes the shared stream with the catalogs it asked for.
    /// Were the set part of the topic, the second reader would fork a second subscription.
    #[tokio::test]
    async fn readers_of_one_address_share_one_wire_subscription() {
        let socket = InfoSocket::spawn("null".to_owned(), Vec::new()).await;
        let address = "0x1111111111111111111111111111111111111111"
            .parse::<WalletAddress>()
            .expect("the fixture address is valid");
        let core = route(CORE_PERP_SCOPE);
        let deployed = route("perp-dex/xyz");
        let adapter = HyperliquidAdapter::new();
        let context = wallet_fixture_context(
            identity(CORE_PERP_SCOPE, CapabilityId::Wallet),
            "https://api.hyperliquid.xyz",
            socket.url(),
        );
        let wide = ProtocolRequest::SubscribeHyperliquidUserFills(
            HyperliquidWalletQuery::public_across(vec![core.clone(), deployed], address.clone())
                .expect("the monitored routes decode the stream"),
        );
        let narrow = ProtocolRequest::SubscribeHyperliquidUserFills(
            HyperliquidWalletQuery::public(core, address),
        );

        // Both subscriptions are held: a released one leaves the topic, and the second
        // reader would then open it again rather than share it.
        let first = adapter
            .subscribe_request(context.clone(), PreparedRequest::new(wide, None))
            .await
            .expect("the fixture socket accepts the first subscription");
        let second = adapter
            .subscribe_request(context, PreparedRequest::new(narrow, None))
            .await
            .expect("the fixture socket accepts the second subscription");

        let frames = socket.settled_frames().await;
        assert_eq!(
            frames.len(),
            1,
            "one address holds one subscription, whatever each reader decodes with: {frames:?}"
        );
        drop((first, second));
        socket.shutdown().await;
    }

    /// The ledger is a venue-wide read, decoded against every monitored route's catalog.
    ///
    /// `userNonFundingLedgerUpdates` names the user alone, and a liquidation names the
    /// positions it closed: a position of a deployed route's coin is resolved by that
    /// route's listing only when the whole monitored set decodes the answer.
    #[tokio::test]
    async fn a_wallet_ledger_read_decodes_every_monitored_routes_coins() {
        let (http_origin, _requests, catalogs) = crate::test_support::info_fixture_server(
            vec![r#"[null,{"name":"xyz"}]"#.to_owned()],
            crate::test_support::spot_meta_answer(&["USDC"]),
            crate::test_support::dex_meta(&["BTC"]),
            vec![(
                "xyz".to_owned(),
                crate::test_support::dex_meta(&["xyz:ABC"]),
            )],
        )
        .await;
        let socket = InfoSocket::spawn(ledger_answer(), Vec::new()).await;
        let address = "0x1111111111111111111111111111111111111111"
            .parse::<WalletAddress>()
            .expect("the fixture address is valid");
        let core = route(CORE_PERP_SCOPE);
        let deployed = route("perp-dex/xyz");
        let query =
            HyperliquidWalletQuery::public_across(vec![core.clone(), deployed], address.clone())
                .expect("the monitored routes decode the answer");
        let context = wallet_fixture_context(
            identity(CORE_PERP_SCOPE, CapabilityId::Wallet),
            &http_origin,
            socket.url(),
        )
        .with_venue_metadata(Some(fixture_reading(&http_origin).await));
        let request = ProtocolRequest::FetchHyperliquidUserLedger {
            query: query.clone(),
            start_time: 0,
        };

        let events = HyperliquidAdapter::new()
            .wallet_ledger(
                &context,
                &request,
                query.wallet_target(),
                query.decoding_routes(),
                0,
            )
            .await
            .expect("the fixture answers the ledger read");

        assert_eq!(
            liquidation_scopes(&events),
            ["core-perp", "perp-dex/xyz"],
            "a venue-wide ledger keeps the closed positions of every monitored route"
        );

        let addressed = HyperliquidWalletQuery::public(core, address);
        let events = HyperliquidAdapter::new()
            .wallet_ledger(
                &context,
                &request,
                addressed.wallet_target(),
                addressed.decoding_routes(),
                0,
            )
            .await
            .expect("the fixture answers the ledger read");
        assert_eq!(
            liquidation_scopes(&events),
            ["core-perp"],
            "a one-route set has no catalog for the deployed route's coin"
        );

        catalogs.abort();
        socket.shutdown().await;
    }

    /// Historical orders are a venue-wide read, decoded against every monitored route.
    ///
    /// `historicalOrders` names the user alone — unlike `frontendOpenOrders`, which names a
    /// `dex` — so one answer carries every scope's coins and the whole monitored set is what
    /// decodes it.
    #[tokio::test]
    async fn a_wallet_historical_order_read_decodes_every_monitored_routes_coins() {
        let (http_origin, _requests, catalogs) = crate::test_support::info_fixture_server(
            vec![r#"[null,{"name":"xyz"}]"#.to_owned()],
            crate::test_support::spot_meta_answer(&["USDC"]),
            crate::test_support::dex_meta(&["BTC"]),
            vec![(
                "xyz".to_owned(),
                crate::test_support::dex_meta(&["xyz:ABC"]),
            )],
        )
        .await;
        let socket = InfoSocket::spawn(historical_answer(), Vec::new()).await;
        let address = "0x1111111111111111111111111111111111111111"
            .parse::<WalletAddress>()
            .expect("the fixture address is valid");
        let core = route(CORE_PERP_SCOPE);
        let deployed = route("perp-dex/xyz");
        let query =
            HyperliquidWalletQuery::public_across(vec![core.clone(), deployed], address.clone())
                .expect("the monitored routes decode the answer");
        let context = wallet_fixture_context(
            identity(CORE_PERP_SCOPE, CapabilityId::Wallet),
            &http_origin,
            socket.url(),
        )
        .with_venue_metadata(Some(fixture_reading(&http_origin).await));
        let request = ProtocolRequest::FetchHyperliquidHistoricalOrders(query.clone());

        let orders = HyperliquidAdapter::new()
            .wallet_orders(&context, &request, &query, true)
            .await
            .expect("the fixture answers the orders read");
        let ProtocolResponse::HyperliquidHistoricalOrders(orders) = orders else {
            panic!("expected historical orders");
        };
        assert_eq!(
            orders
                .iter()
                .map(|order| order.listing.route.scope.as_str())
                .collect::<Vec<_>>(),
            ["core-perp", "perp-dex/xyz"],
            "a venue-wide answer keeps the orders of every monitored route"
        );

        let addressed = HyperliquidWalletQuery::public(core, address);
        let orders = HyperliquidAdapter::new()
            .wallet_orders(
                &context,
                &ProtocolRequest::FetchHyperliquidHistoricalOrders(addressed.clone()),
                &addressed,
                true,
            )
            .await
            .expect("the fixture answers the orders read");
        let ProtocolResponse::HyperliquidHistoricalOrders(orders) = orders else {
            panic!("expected historical orders");
        };
        assert_eq!(
            orders
                .iter()
                .map(|order| order.listing.route.scope.as_str())
                .collect::<Vec<_>>(),
            ["core-perp"],
            "a one-route set has no catalog for the deployed route's coin"
        );

        catalogs.abort();
        socket.shutdown().await;
    }

    /// A wallet context stream is decoded against every monitored route's catalog.
    ///
    /// One `fastAssetCtxs` subscription carries the contexts of every scope the venue
    /// serves — its body names no scope at all — so the reader decodes the shared events
    /// with the catalogs of everything it monitors, and a context of a deployed route's
    /// coin reaches it. This drives the subscription path itself: the fixture pushes the
    /// event down the socket once the subscription is asked for.
    #[tokio::test]
    async fn a_wallet_context_stream_decodes_every_monitored_routes_coins() {
        let (http_origin, _requests, catalogs) = crate::test_support::info_fixture_server(
            vec![r#"[null,{"name":"xyz"}]"#.to_owned()],
            crate::test_support::spot_meta_answer(&["USDC"]),
            crate::test_support::dex_meta(&["BTC"]),
            vec![(
                "xyz".to_owned(),
                crate::test_support::dex_meta(&["xyz:NVDA"]),
            )],
        )
        .await;
        let event = String::from_utf8(fast_contexts_payload(
            r#"{"BTC":{"markPx":"100.5","midPx":"100.25"},"xyz:NVDA":{"markPx":"465.13","midPx":null}}"#,
        ))
        .expect("the fixture event is JSON");
        let socket = InfoSocket::spawn("null".to_owned(), vec![event]).await;
        let query = HyperliquidWalletQuery::route_level_across(vec![
            route(CORE_PERP_SCOPE),
            route("perp-dex/xyz"),
        ])
        .expect("the monitored routes decode the stream");
        let context = wallet_fixture_context(
            identity(CORE_PERP_SCOPE, CapabilityId::Wallet),
            &http_origin,
            socket.url(),
        )
        .with_venue_metadata(Some(fixture_reading(&http_origin).await));
        let request = ProtocolRequest::SubscribeHyperliquidFastContexts(query);

        let handle = HyperliquidAdapter::new()
            .subscribe_request(context, PreparedRequest::new(request, None))
            .await
            .expect("the fixture socket accepts the subscription");
        let mut stream = handle.into_stream();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .expect("the fixture pushes one event")
            .expect("the stream stays open")
            .expect("the fixture's event decodes");
        let contexts = fast_contexts(event);

        assert_eq!(
            contexts
                .iter()
                .map(|context| context.listing.route.scope.as_str())
                .collect::<Vec<_>>(),
            ["core-perp", "perp-dex/xyz"],
            "a venue-wide stream publishes the contexts of every monitored route"
        );

        drop(stream);
        catalogs.abort();
        socket.shutdown().await;
    }

    /// Every reader of the venue's context stream is answered from one board.
    ///
    /// `fastAssetCtxs` sends its whole board once, when a subscription opens, and the
    /// physical subscription is shared by topic: a reader that subscribes after that message
    /// was delivered never sees it. Its own first answer must therefore carry what the venue
    /// published before it arrived, which only a board the readers share can do. The fixture
    /// drives this by pushing one event per subscription frame: the reader that opens the
    /// topic sees the whole board, and the reader that opens it again sees a delta naming a
    /// single coin.
    #[tokio::test]
    async fn a_reader_that_joins_the_context_stream_late_reads_the_whole_board() {
        let (http_origin, _requests, catalogs) = crate::test_support::info_fixture_server(
            vec![r#"[null,{"name":"xyz"}]"#.to_owned()],
            crate::test_support::spot_meta_answer(&["USDC"]),
            crate::test_support::dex_meta(&["BTC"]),
            vec![(
                "xyz".to_owned(),
                crate::test_support::dex_meta(&["xyz:NVDA"]),
            )],
        )
        .await;
        let opening = String::from_utf8(fast_contexts_payload(
            r#"{"BTC":{"markPx":"100.5","midPx":"100.25"}}"#,
        ))
        .expect("the fixture event is JSON");
        let delta = String::from_utf8(fast_contexts_payload(r#"{"xyz:NVDA":{"markPx":"465.13"}}"#))
            .expect("the fixture event is JSON");
        let socket = InfoSocket::spawn("null".to_owned(), vec![opening, delta]).await;
        let query = HyperliquidWalletQuery::route_level_across(vec![
            route(CORE_PERP_SCOPE),
            route("perp-dex/xyz"),
        ])
        .expect("the monitored routes decode the stream");
        let context = wallet_fixture_context(
            identity(CORE_PERP_SCOPE, CapabilityId::Wallet),
            &http_origin,
            socket.url(),
        )
        .with_venue_metadata(Some(fixture_reading(&http_origin).await));
        let adapter = HyperliquidAdapter::new();

        let first = adapter
            .subscribe_request(
                context.clone(),
                PreparedRequest::new(
                    ProtocolRequest::SubscribeHyperliquidFastContexts(query.clone()),
                    None,
                ),
            )
            .await
            .expect("the fixture socket accepts the subscription");
        let mut stream = first.into_stream();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .expect("the fixture pushes the opening message")
            .expect("the stream stays open")
            .expect("the opening message decodes");
        let contexts = fast_contexts(event);
        assert_eq!(
            (
                contexts
                    .iter()
                    .map(|context| context.listing.route.scope.as_str())
                    .collect::<Vec<_>>(),
                contexts[0]
                    .mark_price
                    .as_ref()
                    .map(|price| price.to_string()),
            ),
            (vec!["core-perp"], Some("100.5".to_owned())),
            "the reader that opened the topic reads the whole board"
        );

        // The topic is released with the stream, so the next reader opens it again — and the
        // venue answers that opening with the message that came after the board.
        drop(stream);
        let second = adapter
            .subscribe_request(
                context,
                PreparedRequest::new(
                    ProtocolRequest::SubscribeHyperliquidFastContexts(query),
                    None,
                ),
            )
            .await
            .expect("the fixture socket accepts the second subscription");
        let mut stream = second.into_stream();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .expect("the fixture pushes the delta")
            .expect("the stream stays open")
            .expect("the delta decodes");
        let contexts = fast_contexts(event);
        assert_eq!(
            contexts
                .iter()
                .map(|context| context.listing.route.scope.as_str())
                .collect::<Vec<_>>(),
            ["core-perp", "perp-dex/xyz"],
            "a reader that never saw the opening message is still answered from the board"
        );
        assert_eq!(
            contexts[0]
                .mark_price
                .as_ref()
                .map(|price| price.to_string()),
            Some("100.5".to_owned()),
            "and the price it reads is the one the other reader's message carried"
        );

        drop(stream);
        catalogs.abort();
        socket.shutdown().await;
    }

    /// The contexts one context-stream event carries.
    fn fast_contexts(event: ProtocolEvent) -> Vec<NativeHyperliquidAssetContext> {
        let ProtocolEvent::HyperliquidWalletUpdate(
            NativeHyperliquidWalletUpdate::FastAssetContexts { contexts },
        ) = event
        else {
            panic!("expected fast asset contexts");
        };
        contexts
    }

    /// A public wallet read is the address it names, and a route-level target names
    /// no wallet at all.
    #[test]
    fn a_wallet_target_reads_exactly_what_it_names() {
        let context = fixture_context(identity(CORE_PERP_SCOPE, CapabilityId::Wallet), None);
        let address = "0x1111111111111111111111111111111111111111"
            .parse::<WalletAddress>()
            .expect("the fixture address is valid");
        let public = WalletTarget::Public {
            route: route(CORE_PERP_SCOPE),
            address,
        };
        assert_eq!(
            wallet_user(&public, &context)
                .expect("a public target reads its address")
                .as_str(),
            "0x1111111111111111111111111111111111111111"
        );

        let route_level = WalletTarget::Route {
            route: route(CORE_PERP_SCOPE),
        };
        assert!(wallet_user(&route_level, &context).is_err());
    }

    /// A public wallet read never contributes a trading credential, while a
    /// trading connection without one is refused.
    #[test]
    fn a_public_wallet_read_never_carries_a_connection_credential() {
        let auth = Authentication::ApiKey {
            account_id: SmolStr::new_static("account"),
            api_key: SmolStr::new_static("api-key"),
            secret: SmolStr::new_static("secret"),
        };
        let context = fixture_context(identity(CORE_PERP_SCOPE, CapabilityId::Wallet), Some(auth));
        let public = ProtocolRequest::FetchHyperliquidOpenOrders(HyperliquidWalletQuery::public(
            route(CORE_PERP_SCOPE),
            "0x1111111111111111111111111111111111111111"
                .parse::<WalletAddress>()
                .expect("the fixture address is valid"),
        ));

        assert_eq!(
            connection_auth(&context, &public).expect("a public read is allowed"),
            None
        );

        let trading =
            ProtocolRequest::CancelOrder(crate::market_participants::model::CancelOrderCommand {
                target: crate::protocol::AuthenticatedTarget::new(
                    market_participants_model::identifier::MarketParticipantAccountId::new(
                        "account",
                    )
                    .expect("the fixture account is valid"),
                    route(CORE_PERP_SCOPE),
                ),
                listing: perp_key(&route(CORE_PERP_SCOPE), 0),
                order_id: OrderId::normal("1").expect("the fixture order ID is valid"),
            });
        let unauthenticated = fixture_context(
            identity(CORE_PERP_SCOPE, CapabilityId::AuthenticatedTrading),
            None,
        );
        let error = connection_auth(&unauthenticated, &trading)
            .expect_err("a trading connection needs an account");
        assert!(matches!(
            error,
            MarketParticipantError::AuthenticationFailed { .. }
        ));
        assert_eq!(
            connection_auth(&context, &trading).expect("the configured account is used"),
            Some(SmolStr::new_static("account"))
        );
    }

    /// A candle window is `limit` intervals wide, and a calendar month is bounded
    /// by the longest one rather than by a missing duration.
    #[test]
    fn a_candle_window_is_bounded_by_the_interval_and_the_limit() {
        let hour = 3_600_000;
        assert_eq!(
            candle_request_window(TimeFrame::Hour2, 300, None, None, 1_000 * hour),
            (1_000 * hour - 300 * 2 * hour, 1_000 * hour)
        );
        assert_eq!(
            candle_request_window(TimeFrame::Hour2, 300, None, Some(500 * hour), 1_000 * hour),
            (500 * hour - 300 * 2 * hour, 500 * hour)
        );
        assert_eq!(
            candle_request_window(TimeFrame::Hour2, 300, Some(500 * hour), None, 10_000 * hour),
            (500 * hour, 500 * hour + 300 * 2 * hour)
        );
        assert_eq!(
            candle_request_window(TimeFrame::Hour2, 300, Some(500 * hour), None, 600 * hour),
            (500 * hour, 600 * hour)
        );
        let (start, end) =
            candle_request_window(TimeFrame::Month1, 3, None, None, 1_000_000 * hour);
        assert_eq!(end - start, 3 * 31 * 24 * hour);
    }

    /// A price moves against the side the order is on, and the venue's own two
    /// unsupported time-in-force instructions are refused rather than mapped.
    #[test]
    fn a_market_price_moves_against_the_requested_side() {
        let reference = Price::from(100_u8);
        let slippage = "0.05".parse::<Ratio>().expect("the fixture ratio parses");

        assert_eq!(
            aggressive_market_price(reference, Side::Buy, slippage)
                .expect("the buy price should fit")
                .to_string(),
            "105"
        );
        assert_eq!(
            aggressive_market_price(reference, Side::Sell, slippage)
                .expect("the sell price should fit")
                .to_string(),
            "95"
        );
        assert_eq!(
            encode_time_in_force(TimeInForce::PostOnly).expect("Alo is supported"),
            "Alo"
        );
        assert!(encode_time_in_force(TimeInForce::FillOrKill).is_err());
        assert!(encode_time_in_force(TimeInForce::GoodTillDate(1)).is_err());
    }

    /// One ledger row, as the venue would write it.
    fn ledger_row(delta: serde_json::Value) -> HyperliquidLedgerWire {
        serde_json::from_value(serde_json::json!({ "time": 1, "hash": "hash", "delta": delta }))
            .expect("the fixture ledger row should deserialize")
    }

    /// The proxy of a fixture context is the direct route, so a request that
    /// reaches the network is a test fixture mistake rather than a silent dial.
    #[test]
    fn a_fixture_context_is_direct_and_unauthenticated_by_default() {
        let context = fixture_context(identity(CORE_PERP_SCOPE, CapabilityId::Tickers), None);

        assert_eq!(context.proxy, ResolvedProxy::NoProxy);
        assert_eq!(
            context.network_route_generation(),
            NetworkRouteGeneration::default()
        );
        assert!(context.auth.is_none());
    }

    /// Each reading asks the venue again.
    ///
    /// The adapter keeps no metadata of its own: what a request path reads is the
    /// value the commit handed it, so a load after a deployment reads the venue's
    /// own answer as it stands rather than a table an earlier request filled.
    #[tokio::test]
    async fn two_readings_read_the_venues_tables_twice() {
        let (origin, requests, server) = crate::test_support::info_fixture_server(
            vec![r#"[null,{"name":"xyz"}]"#.to_owned()],
            crate::test_support::spot_meta_answer(&["USD", "USDC"]),
            crate::test_support::dex_meta(&["BTC"]),
            vec![(
                "xyz".to_owned(),
                crate::test_support::dex_meta(&["xyz:ABC"]),
            )],
        )
        .await;
        let context = fixture_venue_context(&origin);
        let adapter = HyperliquidAdapter::new();

        for round in 0..2 {
            let reading = adapter
                .read_venue(&context)
                .await
                .expect("the fixture answers a reading");
            assert!(
                matches!(
                    reading,
                    VenueReading::Verified { ref deployments, .. }
                        if deployments == &[SmolStr::new_static("xyz")]
                ),
                "the fixture's one deployed DEX is read on round {round}"
            );
        }
        let served = requests.lock().clone();
        server.abort();

        let count = |kind: &str| served.iter().filter(|seen| *seen == kind).count();
        assert_eq!(
            count("perpDexs"),
            2,
            "each reading reads the DEX table again: {served:?}"
        );
        assert_eq!(
            count("spotMetaAndAssetCtxs"),
            2,
            "and the spot scope's answer carries the token table with it: {served:?}"
        );
    }

    /// A refusal that says the venue's allowance is what was missing is retried, and the
    /// answer of the attempt that was served is the one the caller reads.
    #[tokio::test]
    async fn a_rate_limited_info_request_is_answered_by_its_retry() {
        let (origin, requests, server) = crate::test_support::refusing_fixture_server(vec![
            crate::test_support::FixtureAnswer::refused(429, "", "null"),
            crate::test_support::FixtureAnswer::ok(r#"[null,{"name":"xyz"}]"#),
        ])
        .await;
        let context = fixture_venue_context(&origin);

        let dexes: Vec<Option<HyperliquidPerpDex>> = post_hyperliquid_info(
            &context.http_client,
            context.http_origin(),
            context.venue(),
            &HyperliquidInfoRequest::PerpDexs,
        )
        .await
        .expect("the retry is answered");

        assert_eq!(
            requests.lock().clone(),
            vec!["perpDexs".to_owned(); 2],
            "the refusal is asked again"
        );
        assert_eq!(dexes.len(), 2, "the served answer is what the caller reads");
        server.abort();
    }

    /// A venue that keeps refusing for its allowance is asked a bounded number of times
    /// and then reported: one read is not an unbounded conversation.
    #[tokio::test]
    async fn a_refusal_that_keeps_naming_the_allowance_is_bounded() {
        let (origin, requests, server) = crate::test_support::refusing_fixture_server(vec![
            crate::test_support::FixtureAnswer::refused(429, "", "null"),
        ])
        .await;
        let context = fixture_venue_context(&origin);

        let error = post_hyperliquid_info::<Vec<Option<HyperliquidPerpDex>>>(
            &context.http_client,
            context.http_origin(),
            context.venue(),
            &HyperliquidInfoRequest::PerpDexs,
        )
        .await
        .expect_err("a venue that keeps refusing is reported");

        assert_eq!(
            requests.lock().len(),
            usize::try_from(INFO_RETRY_ATTEMPTS).expect("the attempt bound fits") + 1,
            "the first attempt plus its retries"
        );
        assert!(
            matches!(error, MarketParticipantError::RateLimited { .. }),
            "{error:?}"
        );
        server.abort();
    }

    /// A refusal that is not about the allowance is the caller's answer at once: it is not
    /// evidence that a second attempt would be answered differently.
    #[tokio::test]
    async fn a_refusal_that_is_not_about_the_allowance_is_not_retried() {
        let (origin, requests, server) = crate::test_support::refusing_fixture_server(vec![
            crate::test_support::FixtureAnswer::refused(422, "", "no such dex"),
        ])
        .await;
        let context = fixture_venue_context(&origin);

        let error = post_hyperliquid_info::<Vec<Option<HyperliquidPerpDex>>>(
            &context.http_client,
            context.http_origin(),
            context.venue(),
            &HyperliquidInfoRequest::PerpDexs,
        )
        .await
        .expect_err("the refusal is reported");

        assert_eq!(requests.lock().len(), 1, "a 422 is asked once");
        assert!(
            matches!(
                error,
                MarketParticipantError::Rejected { ref code, ref message, retryable: false, .. }
                    if code == "422" && message == "no such dex"
            ),
            "{error:?}"
        );
        server.abort();
    }

    /// a stream decodes with the reading it was opened under.
    ///
    /// One `userFills` subscription is decoded against the venue metadata the
    /// *admission* handed its execution context, and that context is what the stream
    /// carries for its whole life. Re-reading the venue therefore cannot reach a
    /// stream that is already open: it moves what the next subscription is admitted
    /// with, and the established one keeps resolving the coins it was opened on.
    ///
    /// The fixture is re-read between the two subscriptions — the deployed scope's
    /// `meta` names `xyz:ABC` on the first reading and `xyz:DEF` on the second — and
    /// each stream is pushed the *same* frame, carrying both coins. Each stream
    /// decodes exactly the row of its own reading and drops the other, which is the
    /// whole contract: one row out of two on both sides, resolved to different
    /// listings.
    #[tokio::test]
    async fn a_wallet_fill_stream_decodes_with_the_reading_it_was_opened_under() {
        let ((http_origin, _requests, catalogs), reading) =
            crate::test_support::info_fixture_server_with_readings(
                vec![r#"[null,{"name":"xyz"}]"#.to_owned()],
                crate::test_support::spot_meta_answer(&["USDC"]),
                crate::test_support::dex_meta(&["BTC"]),
                vec![(
                    "xyz".to_owned(),
                    vec![
                        crate::test_support::dex_meta(&["xyz:ABC"]),
                        crate::test_support::dex_meta(&["xyz:DEF"]),
                    ],
                )],
            )
            .await;
        let address = "0x1111111111111111111111111111111111111111"
            .parse::<WalletAddress>()
            .expect("the fixture address is valid");
        let query = HyperliquidWalletQuery::public_across(
            vec![route(CORE_PERP_SCOPE), route("perp-dex/xyz")],
            address.clone(),
        )
        .expect("the monitored routes decode the stream");
        let adapter = HyperliquidAdapter::new();
        // Both frames carry both coins: which one a stream decodes is the reading it
        // was opened under and nothing else.
        let frame = wallet_fills_payload(&address, &[("xyz:ABC", 1), ("xyz:DEF", 2)]);

        // The venue's first reading names `xyz:ABC`, and the stream is opened on it
        // while no frame has arrived yet: what it decodes is decided later, after the
        // venue has been re-read.
        *reading.lock() = 0;
        let opening_socket = InfoSocket::spawn("null".to_owned(), Vec::new()).await;
        let opening = wallet_fixture_context(
            identity(CORE_PERP_SCOPE, CapabilityId::Wallet),
            &http_origin,
            opening_socket.url(),
        )
        .with_venue_metadata(Some(fixture_reading(&http_origin).await));
        let established = adapter
            .subscribe_request(
                opening,
                PreparedRequest::new(
                    ProtocolRequest::SubscribeHyperliquidUserFills(query.clone()),
                    None,
                ),
            )
            .await
            .expect("the fixture socket accepts the first subscription");
        let mut established = established.into_stream();

        // The venue answers its next reading now: the same scope publishes `xyz:DEF`.
        // The next subscription is admitted with that reading, and the established one
        // is already open.
        *reading.lock() = 1;
        let fresh_socket = InfoSocket::spawn("null".to_owned(), Vec::new()).await;
        let reread = wallet_fixture_context(
            identity(CORE_PERP_SCOPE, CapabilityId::Wallet),
            &http_origin,
            fresh_socket.url(),
        )
        .with_venue_metadata(Some(fixture_reading(&http_origin).await));
        let fresh = adapter
            .subscribe_request(
                reread,
                PreparedRequest::new(ProtocolRequest::SubscribeHyperliquidUserFills(query), None),
            )
            .await
            .expect("the fixture socket accepts the second subscription");
        let mut fresh = fresh.into_stream();

        // Only now does the venue publish the very same frame to both streams: each
        // decodes it with the reading it was opened under.
        opening_socket.push(frame.clone());
        fresh_socket.push(frame);
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), established.next())
            .await
            .expect("the fixture delivers the pushed frame")
            .expect("the stream stays open")
            .expect("the established stream decodes the frame");
        let (established_fills, established_size) = decoded_fills(event);
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), fresh.next())
            .await
            .expect("the fixture delivers the pushed frame")
            .expect("the stream stays open")
            .expect("the new stream decodes the frame");
        let (fresh_fills, fresh_size) = decoded_fills(event);

        assert_eq!(
            established_fills, 1,
            "the established stream decodes the one row its own reading resolves"
        );
        assert_eq!(
            fresh_fills, 1,
            "and the stream admitted after the re-read decodes the one its new reading \
             resolves, dropping the row the previous reading named"
        );
        assert_eq!(
            established_size, "1",
            "the established stream resolved the row its own reading names"
        );
        assert_eq!(
            fresh_size, "2",
            "and the stream admitted after the re-read resolved the other row of the same frame"
        );

        drop((established, fresh));
        catalogs.abort();
        opening_socket.shutdown().await;
        fresh_socket.shutdown().await;
    }

    /// The `userFills` frame the venue would push for one address.
    fn wallet_fills_payload(address: &WalletAddress, rows: &[(&str, i64)]) -> String {
        let fills: Vec<serde_json::Value> = rows
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                serde_json::json!({
                    "coin": entry.0,
                    "side": "B",
                    "sz": format!("{}", entry.1),
                    "px": "100",
                    "time": 1_700_000_000_000_i64 + index as i64,
                    "startPosition": "0",
                    "closedPnl": "0",
                    "fee": "0.01",
                    "feeToken": "USDC",
                    "tid": index as u64 + 1,
                    "hash": "0xabc",
                })
            })
            .collect();
        serde_json::json!({
            "channel": "userFills",
            "data": { "user": address.as_str(), "isSnapshot": false, "fills": fills },
        })
        .to_string()
    }

    /// The fresh fills one wallet event carries, and the size of the first of them.
    ///
    /// Each row of the fixture frame carries its own size, so the decoded size names
    /// which row the stream resolved.
    fn decoded_fills(event: crate::adapter::ProtocolEvent) -> (usize, String) {
        let crate::adapter::ProtocolEvent::HyperliquidWalletUpdate(
            NativeHyperliquidWalletUpdate::UserFills { fills, .. },
        ) = event
        else {
            panic!("the fixture pushes one wallet fills event");
        };
        let first = fills
            .first()
            .expect("a stream that decoded a frame with one resolvable row carries it")
            .quantity
            .to_string();
        (fills.len(), first)
    }
}

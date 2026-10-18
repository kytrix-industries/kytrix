use std::sync::Arc;

use market_participants_model::{
    identifier::{MarketParticipantAccountId, WalletAddress},
    listing::route::{VenueListingKey, VenueRoute},
    market::account::AssetBalance,
};
use smol_str::SmolStr;

use super::{
    domain::{
        AccountTrade, HyperliquidHistoricalOrder, HyperliquidLedgerEvent, HyperliquidOpenOrder,
        HyperliquidPerpetualPosition, HyperliquidPerpetualState, HyperliquidSpotPair,
        HyperliquidSpotState, HyperliquidUserFill, Order, Position,
    },
    error::{AccountError, account_error_from_catalog, account_error_from_market_participant},
    hyperliquid::HyperliquidSubscription,
    mapping::{
        PRIMARY_ACCOUNT, RouteAssets, WalletCatalogs, map_account_trade, map_balance,
        map_historical_position, map_hyperliquid_historical_order, map_hyperliquid_open_order,
        map_hyperliquid_perpetual_position, map_hyperliquid_spot_balance,
        map_hyperliquid_user_fill, map_ledger_event, map_order, map_position,
    },
    shared::{MarginMode, TradingSettings},
    streams::{OrderStream, PositionStream},
};
use crate::{
    adapter::{ProtocolRequest, ProtocolResponse},
    catalog::resolved::{EnsuredRoute, ResolvedListing},
    connector::MarketParticipantsConnectorInner,
    market_participants::model::{self, AuthenticatedQuery, TradingSettingsQuery},
    protocol::AuthenticatedTarget,
};

/// Account capability handle backed by the shared market-participants runtime.
pub struct AccountCapability {
    inner: Arc<MarketParticipantsConnectorInner>,
}

impl Clone for AccountCapability {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl AccountCapability {
    pub(crate) fn new(inner: Arc<MarketParticipantsConnectorInner>) -> Self {
        Self { inner }
    }

    /// Builds the authenticated target of one account on one route.
    ///
    /// The account name is validated here so every authenticated operation starts
    /// from a target that exists, and the route is the one the caller names: the
    /// gateway is what decides whether it is registered.
    pub fn target(
        &self,
        route: VenueRoute,
        account: &str,
    ) -> Result<AuthenticatedTarget, AccountError> {
        let account = MarketParticipantAccountId::new(account).map_err(AccountError::Model)?;
        Ok(AuthenticatedTarget::new(account, route))
    }

    /// Builds the authenticated target of one route's configured account.
    pub fn configured_target(
        &self,
        route: VenueRoute,
    ) -> Result<AuthenticatedTarget, AccountError> {
        self.target(route, PRIMARY_ACCOUNT)
    }

    pub async fn balances(
        &self,
        target: AuthenticatedTarget,
    ) -> Result<Vec<AssetBalance>, AccountError> {
        let resolved = self.resolve_target(&target).await?;
        let assets = RouteAssets::of(&resolved);
        match self
            .execute(
                target,
                ProtocolRequest::FetchBalances(AuthenticatedQuery::account_wide(
                    self.configured_target_for(&resolved)?,
                )),
            )
            .await?
        {
            ProtocolResponse::Balances(values) => values
                .iter()
                .map(|value| map_balance(&assets, value))
                .collect(),
            _ => Err(Self::protocol_error("fetch balances")),
        }
    }

    pub async fn positions(
        &self,
        target: AuthenticatedTarget,
    ) -> Result<Vec<Position>, AccountError> {
        let resolved = self.resolve_target(&target).await?;
        let response = self
            .execute(
                target,
                ProtocolRequest::FetchPositions(AuthenticatedQuery::account_wide(
                    self.configured_target_for(&resolved)?,
                )),
            )
            .await?;
        match response {
            ProtocolResponse::Positions(values) => values
                .iter()
                .map(|value| {
                    let listing = self.resolve_listing(&resolved, &value.listing)?;
                    map_position(value, &listing)
                })
                .collect(),
            _ => Err(Self::protocol_error("fetch positions")),
        }
    }

    pub async fn active_orders(
        &self,
        target: AuthenticatedTarget,
        listing: Option<VenueListingKey>,
    ) -> Result<Vec<Order>, AccountError> {
        let resolved = self.resolve_target(&target).await?;
        let query = self.account_query(&resolved, listing)?;
        match self
            .execute(target, ProtocolRequest::FetchActiveOrders(query))
            .await?
        {
            ProtocolResponse::Orders(values) => values
                .iter()
                .map(|value| {
                    let listing = self.resolve_listing(&resolved, &value.listing)?;
                    map_order(value, &listing)
                })
                .collect(),
            _ => Err(Self::protocol_error("fetch active orders")),
        }
    }

    pub async fn order_history(
        &self,
        target: AuthenticatedTarget,
        listing: Option<VenueListingKey>,
        start_timestamp: Option<i64>,
        end_timestamp: Option<i64>,
    ) -> Result<Vec<Order>, AccountError> {
        let resolved = self.resolve_target(&target).await?;
        let query = self.account_query(&resolved, listing)?.with_window(
            start_timestamp,
            end_timestamp,
            1_000,
        );
        match self
            .execute(target, ProtocolRequest::FetchOrderHistory(query))
            .await?
        {
            ProtocolResponse::Orders(values) => values
                .iter()
                .map(|value| {
                    let listing = self.resolve_listing(&resolved, &value.listing)?;
                    map_order(value, &listing)
                })
                .collect(),
            _ => Err(Self::protocol_error("fetch order history")),
        }
    }

    pub async fn account_trades(
        &self,
        target: AuthenticatedTarget,
        listing: Option<VenueListingKey>,
        start_timestamp: Option<i64>,
        end_timestamp: Option<i64>,
    ) -> Result<Vec<AccountTrade>, AccountError> {
        let resolved = self.resolve_target(&target).await?;
        let assets = RouteAssets::of(&resolved);
        let query = self.account_query(&resolved, listing)?.with_window(
            start_timestamp,
            end_timestamp,
            1_000,
        );
        match self
            .execute(target, ProtocolRequest::FetchAccountTrades(query))
            .await?
        {
            ProtocolResponse::AccountTrades(values) => values
                .iter()
                .map(|value| {
                    let listing = self.resolve_listing(&resolved, &value.listing)?;
                    map_account_trade(value, &listing, &assets)
                })
                .collect(),
            _ => Err(Self::protocol_error("fetch account trades")),
        }
    }

    pub async fn position_history(
        &self,
        target: AuthenticatedTarget,
        listing: Option<VenueListingKey>,
        start_timestamp: Option<i64>,
        end_timestamp: Option<i64>,
    ) -> Result<Vec<super::domain::HistoricalPosition>, AccountError> {
        let resolved = self.resolve_target(&target).await?;
        let query = self.account_query(&resolved, listing)?.with_window(
            start_timestamp,
            end_timestamp,
            1_000,
        );
        match self
            .execute(target, ProtocolRequest::FetchPositionHistory(query))
            .await?
        {
            ProtocolResponse::HistoricalPositions(values) => values
                .iter()
                .map(|value| {
                    let listing = self.resolve_listing(&resolved, &value.listing)?;
                    map_historical_position(value, &listing)
                })
                .collect(),
            _ => Err(Self::protocol_error("fetch position history")),
        }
    }

    pub async fn trading_settings(
        &self,
        target: AuthenticatedTarget,
        listing: VenueListingKey,
        margin_mode: MarginMode,
    ) -> Result<TradingSettings, AccountError> {
        let resolved = self.resolve_target(&target).await?;
        let listing = self.resolve_listing(&resolved, &listing)?;
        let query = TradingSettingsQuery {
            target: self.configured_target_for(&resolved)?,
            listing: listing.key.clone(),
            margin_mode,
        };
        query
            .validate_for(listing.instrument.kind())
            .map_err(AccountError::Model)?;
        match self
            .execute(target, ProtocolRequest::FetchTradingSettings(query))
            .await?
        {
            ProtocolResponse::TradingSettings(value) => Ok(TradingSettings {
                position_mode: value.position_mode,
                margin_mode: value.margin_mode,
                leverage: value.leverage,
            }),
            _ => Err(Self::protocol_error("fetch trading settings")),
        }
    }

    pub async fn subscribe_orders(
        &self,
        target: AuthenticatedTarget,
    ) -> Result<OrderStream, AccountError> {
        let resolved = self.ensure(target.route())?;
        let request = ProtocolRequest::SubscribeOrders(AuthenticatedQuery::account_wide(
            self.configured_target_for(&resolved)?,
        ));
        let handle = self.subscribe(target, request).await?;
        Ok(OrderStream::from_handle(handle, resolved))
    }

    pub async fn subscribe_positions(
        &self,
        target: AuthenticatedTarget,
        listing: Option<VenueListingKey>,
    ) -> Result<PositionStream, AccountError> {
        let resolved = self.ensure(target.route())?;
        let query = self.account_query(&resolved, listing)?;
        let handle = self
            .subscribe(target, ProtocolRequest::SubscribePositions(query))
            .await?;
        Ok(PositionStream::from_handle(handle, resolved))
    }

    /// Fetches one monitored wallet's spot state.
    pub async fn hyperliquid_spot_state(
        &self,
        route: VenueRoute,
        address: WalletAddress,
    ) -> Result<HyperliquidSpotState, AccountError> {
        let request = ProtocolRequest::FetchHyperliquidSpotState(
            model::HyperliquidWalletQuery::public(route.clone(), address),
        );
        self.ensure(&route)?;
        match self.execute_route(route, request).await? {
            ProtocolResponse::HyperliquidSpotState(value) => Ok(HyperliquidSpotState {
                balances: value
                    .balances
                    .iter()
                    .map(map_hyperliquid_spot_balance)
                    .collect(),
            }),
            _ => Err(Self::protocol_error("fetch Hyperliquid spot state")),
        }
    }

    /// Fetches one monitored wallet's perpetual state.
    pub async fn hyperliquid_perpetual_state(
        &self,
        route: VenueRoute,
        address: WalletAddress,
    ) -> Result<HyperliquidPerpetualState, AccountError> {
        let resolved = self.ensure(&route)?;
        let request = ProtocolRequest::FetchHyperliquidPerpetualState(
            model::HyperliquidWalletQuery::public(route, address),
        );
        match self
            .execute_route(resolved.route().clone(), request)
            .await?
        {
            ProtocolResponse::HyperliquidPerpetualState(value) => Ok(HyperliquidPerpetualState {
                positions: value
                    .positions
                    .iter()
                    .map(|position| {
                        let listing = self.resolve_listing(&resolved, &position.listing)?;
                        map_hyperliquid_perpetual_position(position, &listing)
                    })
                    .collect::<Result<Vec<HyperliquidPerpetualPosition>, AccountError>>()?,
            }),
            _ => Err(Self::protocol_error("fetch Hyperliquid perpetual state")),
        }
    }

    /// Fetches one monitored wallet's fills.
    ///
    /// A fills answer is venue-wide — `userFills` names the user alone — so it is read
    /// once and decoded against every monitored route's catalog: the first route
    /// addresses the request, and each answered row belongs to the route that published
    /// its listing.
    pub async fn hyperliquid_user_fills(
        &self,
        routes: &[VenueRoute],
        address: WalletAddress,
        aggregate_by_time: bool,
    ) -> Result<Vec<HyperliquidUserFill>, AccountError> {
        let query = model::HyperliquidWalletQuery::public_across(routes.to_vec(), address)
            .map_err(AccountError::Model)?;
        let route = query.route().clone();
        let catalogs = self.decoding_catalogs(&route, routes).await?;
        let request = ProtocolRequest::FetchHyperliquidUserFills {
            query,
            aggregate_by_time,
        };
        match self.execute_route(route, request).await? {
            ProtocolResponse::HyperliquidUserFills(values) => values
                .iter()
                .map(|value| {
                    let (_, listing, assets) = catalogs.resolve(&value.listing)?;
                    map_hyperliquid_user_fill(value, &listing, assets)
                })
                .collect(),
            _ => Err(Self::protocol_error("fetch Hyperliquid user fills")),
        }
    }

    /// Fetches one monitored wallet's fills inside a bounded window.
    pub async fn hyperliquid_user_fills_by_time(
        &self,
        routes: &[VenueRoute],
        address: WalletAddress,
        start_timestamp: i64,
        end_timestamp: i64,
    ) -> Result<Vec<HyperliquidUserFill>, AccountError> {
        let query = model::HyperliquidUserFillsByTimeQuery::for_routes(
            routes.to_vec(),
            address,
            start_timestamp,
            end_timestamp,
        )
        .map_err(AccountError::Model)?;
        query.validate().map_err(AccountError::Model)?;
        let route = query.route().clone();
        let catalogs = self.decoding_catalogs(&route, routes).await?;
        match self
            .execute_route(
                route,
                ProtocolRequest::FetchHyperliquidUserFillsByTime(query),
            )
            .await?
        {
            ProtocolResponse::HyperliquidUserFills(values) => values
                .iter()
                .map(|value| {
                    let (_, listing, assets) = catalogs.resolve(&value.listing)?;
                    map_hyperliquid_user_fill(value, &listing, assets)
                })
                .collect(),
            _ => Err(Self::protocol_error("fetch Hyperliquid user fills by time")),
        }
    }

    /// Fetches one monitored wallet's open orders.
    pub async fn hyperliquid_open_orders(
        &self,
        route: VenueRoute,
        address: WalletAddress,
    ) -> Result<Vec<HyperliquidOpenOrder>, AccountError> {
        let resolved = self.ensure(&route)?;
        let request = ProtocolRequest::FetchHyperliquidOpenOrders(
            model::HyperliquidWalletQuery::public(route, address),
        );
        match self
            .execute_route(resolved.route().clone(), request)
            .await?
        {
            ProtocolResponse::HyperliquidOpenOrders(values) => values
                .iter()
                .map(|value| {
                    let listing = self.resolve_listing(&resolved, &value.listing)?;
                    map_hyperliquid_open_order(value, &listing)
                })
                .collect(),
            _ => Err(Self::protocol_error("fetch Hyperliquid open orders")),
        }
    }

    /// Fetches one monitored wallet's historical orders.
    ///
    /// `historicalOrders` names the user alone — a venue-wide read — so the answer is
    /// decoded against every monitored route's catalog while the first route addresses
    /// the request.
    pub async fn hyperliquid_historical_orders(
        &self,
        routes: &[VenueRoute],
        address: WalletAddress,
    ) -> Result<Vec<HyperliquidHistoricalOrder>, AccountError> {
        let query = model::HyperliquidWalletQuery::public_across(routes.to_vec(), address)
            .map_err(AccountError::Model)?;
        let route = query.route().clone();
        let catalogs = self.decoding_catalogs(&route, routes).await?;
        let request = ProtocolRequest::FetchHyperliquidHistoricalOrders(query);
        match self.execute_route(route, request).await? {
            ProtocolResponse::HyperliquidHistoricalOrders(values) => values
                .iter()
                .map(|value| {
                    let (_, listing, _) = catalogs.resolve(&value.listing)?;
                    map_hyperliquid_historical_order(value, &listing)
                })
                .collect(),
            _ => Err(Self::protocol_error("fetch Hyperliquid historical orders")),
        }
    }

    /// Fetches one monitored wallet's ledger from `start_time` on.
    ///
    /// `userNonFundingLedgerUpdates` names the user alone, so the ledger is decoded
    /// against every monitored route's catalog — the same set the fills read uses.
    pub async fn hyperliquid_user_ledger(
        &self,
        routes: &[VenueRoute],
        address: WalletAddress,
        start_time: i64,
    ) -> Result<Vec<HyperliquidLedgerEvent>, AccountError> {
        let query = model::HyperliquidWalletQuery::public_across(routes.to_vec(), address)
            .map_err(AccountError::Model)?;
        let route = query.route().clone();
        let catalogs = self.decoding_catalogs(&route, routes).await?;
        let request = ProtocolRequest::FetchHyperliquidUserLedger { query, start_time };
        match self.execute_route(route, request).await? {
            ProtocolResponse::HyperliquidLedger(values) => values
                .iter()
                .map(|value| map_ledger_event(value, &catalogs))
                .collect(),
            _ => Err(Self::protocol_error("fetch Hyperliquid user ledger")),
        }
    }

    /// Fetches the spot pairs one Hyperliquid route publishes.
    pub async fn hyperliquid_spot_pairs(
        &self,
        route: VenueRoute,
    ) -> Result<Vec<HyperliquidSpotPair>, AccountError> {
        self.ensure(&route)?;
        match self
            .execute_route(
                route.clone(),
                ProtocolRequest::FetchHyperliquidSpotPairs(model::MarketCatalogQuery { route }),
            )
            .await?
        {
            ProtocolResponse::HyperliquidSpotPairs(values) => Ok(values
                .iter()
                .map(|value| HyperliquidSpotPair {
                    coin: SmolStr::new(value.coin.as_str()),
                    base_token: value.base_token.clone(),
                    quote_token: value.quote_token.clone(),
                    asset_id: value.asset_id,
                })
                .collect()),
            _ => Err(Self::protocol_error("fetch Hyperliquid spot pairs")),
        }
    }

    /// Opens one monitored wallet's fill stream.
    ///
    /// One stream carries the fills of every scope its reader monitors, so the decoding
    /// route set is what the events are resolved with rather than the session's address.
    pub async fn subscribe_hyperliquid_user_fills(
        &self,
        routes: &[VenueRoute],
        address: WalletAddress,
    ) -> Result<HyperliquidSubscription, AccountError> {
        let query = model::HyperliquidWalletQuery::public_across(routes.to_vec(), address)
            .map_err(AccountError::Model)?;
        let route = query.route().clone();
        let catalogs = self.decoding_catalogs(&route, routes).await?;
        let request = ProtocolRequest::SubscribeHyperliquidUserFills(query);
        self.open_hyperliquid_subscription(route, request, catalogs)
            .await
    }

    /// Opens one monitored wallet's ledger stream.
    pub async fn subscribe_hyperliquid_user_ledger(
        &self,
        routes: &[VenueRoute],
        address: WalletAddress,
    ) -> Result<HyperliquidSubscription, AccountError> {
        let query = model::HyperliquidWalletQuery::public_across(routes.to_vec(), address)
            .map_err(AccountError::Model)?;
        let route = query.route().clone();
        let catalogs = self.decoding_catalogs(&route, routes).await?;
        let request = ProtocolRequest::SubscribeHyperliquidUserLedger(query);
        self.open_hyperliquid_subscription(route, request, catalogs)
            .await
    }

    /// Opens one reader's fast asset-context stream.
    ///
    /// The stream carries every scope's contexts, so it is opened once per reader and
    /// decoded against that reader's whole route set.
    pub async fn subscribe_hyperliquid_fast_contexts(
        &self,
        routes: &[VenueRoute],
    ) -> Result<HyperliquidSubscription, AccountError> {
        let query = model::HyperliquidWalletQuery::route_level_across(routes.to_vec())
            .map_err(AccountError::Model)?;
        let route = query.route().clone();
        let catalogs = self.decoding_catalogs(&route, routes).await?;
        let request = ProtocolRequest::SubscribeHyperliquidFastContexts(query);
        self.open_hyperliquid_subscription(route, request, catalogs)
            .await
    }

    async fn open_hyperliquid_subscription(
        &self,
        route: VenueRoute,
        request: ProtocolRequest,
        catalogs: WalletCatalogs,
    ) -> Result<HyperliquidSubscription, AccountError> {
        let context = self.gateway_context(&route, &request);
        let handle = self
            .inner
            .gateway
            .subscribe(&self.inner.venues, request, context, self.inner.catalog())
            .await
            .map_err(account_error_from_market_participant)?;
        Ok(HyperliquidSubscription::new(handle.into_stream(), catalogs))
    }

    /// Reads every route's published catalog for one venue-wide wallet read.
    ///
    /// The set is the caller's monitored routes in their own order, so the first is the
    /// route the request is addressed to. Nothing here reaches the venue: a route
    /// whose catalog is not published fails the whole read with
    /// [`crate::catalog::error::CatalogError::CatalogNotLoaded`], because a set that quietly
    /// dropped one would drop exactly the rows its listings carry — the failure a
    /// monitored address must not hide.
    async fn decoding_catalogs(
        &self,
        target_route: &VenueRoute,
        routes: &[VenueRoute],
    ) -> Result<WalletCatalogs, AccountError> {
        let mut catalogs = Vec::with_capacity(routes.len());
        for route in routes {
            catalogs.push(self.ensure(route)?);
        }
        Ok(WalletCatalogs::new(target_route.clone(), catalogs))
    }

    /// Asserts that one route's catalog is already published, answering the proof.
    fn ensure(&self, route: &VenueRoute) -> Result<EnsuredRoute, AccountError> {
        self.inner
            .catalog()
            .ensure(route)
            .map_err(account_error_from_catalog)?;
        Ok(EnsuredRoute::new(self.inner.catalog_arc(), route.clone()))
    }

    /// Resolves one listing against a catalog this capability already holds.
    fn resolve_listing(
        &self,
        catalog: &EnsuredRoute,
        listing: &VenueListingKey,
    ) -> Result<ResolvedListing, AccountError> {
        catalog
            .resolve_listing(listing)
            .map_err(account_error_from_catalog)
    }

    /// Resolves the listing of one authenticated target's route.
    async fn resolve_target(
        &self,
        target: &AuthenticatedTarget,
    ) -> Result<EnsuredRoute, AccountError> {
        self.ensure(target.route())
    }

    /// Rebuilds the authenticated target of one resolved route.
    fn configured_target_for(
        &self,
        catalog: &EnsuredRoute,
    ) -> Result<AuthenticatedTarget, AccountError> {
        self.configured_target(catalog.route().clone())
    }

    /// Builds one account query, narrowing it to a listing of the same route.
    fn account_query(
        &self,
        catalog: &EnsuredRoute,
        listing: Option<VenueListingKey>,
    ) -> Result<AuthenticatedQuery, AccountError> {
        let target = self.configured_target_for(catalog)?;
        match listing {
            Some(listing) => {
                if &listing.route != catalog.route() {
                    return Err(AccountError::ListingRouteMismatch {
                        listing: Box::new(listing),
                        route: catalog.route().clone(),
                    });
                }
                Ok(AuthenticatedQuery::for_listing(target, listing))
            }
            None => Ok(AuthenticatedQuery::account_wide(target)),
        }
    }

    /// Executes one authenticated request of a target's route.
    async fn execute(
        &self,
        target: AuthenticatedTarget,
        request: ProtocolRequest,
    ) -> Result<ProtocolResponse, AccountError> {
        let route = target.route().clone();
        let context = self.gateway_context(&route, &request);
        self.inner
            .gateway
            .execute(&self.inner.venues, request, context, self.inner.catalog())
            .await
            .map_err(account_error_from_market_participant)
    }

    /// Opens one authenticated subscription of a target's route.
    async fn subscribe(
        &self,
        target: AuthenticatedTarget,
        request: ProtocolRequest,
    ) -> Result<crate::adapter::SubscriptionHandle, AccountError> {
        let route = target.route().clone();
        let context = self.gateway_context(&route, &request);
        self.inner
            .gateway
            .subscribe(&self.inner.venues, request, context, self.inner.catalog())
            .await
            .map_err(account_error_from_market_participant)
    }

    /// Executes one route-scoped request that names no account.
    async fn execute_route(
        &self,
        route: VenueRoute,
        request: ProtocolRequest,
    ) -> Result<ProtocolResponse, AccountError> {
        let context = self.gateway_context(&route, &request);
        self.inner
            .gateway
            .execute(&self.inner.venues, request, context, self.inner.catalog())
            .await
            .map_err(account_error_from_market_participant)
    }

    /// Builds the request context of one capability call.
    ///
    /// The capability is the classifier's answer, so a call site never repeats it.
    fn gateway_context(
        &self,
        route: &VenueRoute,
        request: &ProtocolRequest,
    ) -> crate::protocol::RequestIdentity {
        crate::protocol::RequestIdentity::new(
            route.clone(),
            crate::protocol::classify(request).capability(),
        )
    }

    pub(super) const fn protocol_error(operation: &'static str) -> AccountError {
        AccountError::Protocol { operation }
    }
}

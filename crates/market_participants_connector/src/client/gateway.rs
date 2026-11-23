use std::sync::Arc;

use app_config::market_participants::{
    error::MarketParticipantsConfigError, settings::VenueSettings,
};
use app_config_provider::notification::ConfigNotification;
use market_participants_model::{
    identifier::MarketParticipantAccountId, listing::route::VenueRoute,
};
use network::{
    proxy::ProxyResolver,
    transport::{http::HttpClientClass, hub::TransportHub},
};
use parking_lot::RwLock;
use smol_str::{SmolStr, format_smolstr};
use tokio::{runtime::Handle, task::JoinHandle};

use crate::{
    adapter::{
        Authentication, ExecutionContext, ProtocolRequest, VenueExecutionContext,
        binance::BinanceAdapter, hyperliquid::adapter::implementation::HyperliquidAdapter,
        longbridge::LongbridgeAdapter, okx::OkxAdapter, polymarket::PolymarketAdapter,
    },
    capability::router::CapabilityRouter,
    catalog::{
        resolved::{ResolvedListing, ResolvedListingRef},
        store::Catalog,
    },
    connection::ConnectionPool,
    error::{AuthError, MarketParticipantError},
    protocol::{
        ClassifiedRequest, ListingCommandMatrix, ProtocolResponse, RequestIdentity,
        SubscriptionHandle, classify,
    },
    routing::{
        RegisteredRoutes, RegistryError, RouteCapabilityDescriptor, VenueRouteDescriptor,
        VenueRouteRegistry,
    },
    runtime::{CredentialState, MarketParticipantNetworkSnapshot},
    venue::{
        loaded::LoadedVenues,
        metadata::{VenueMetadata, VenueReading},
    },
};
pub(crate) struct OwnedTask(Option<JoinHandle<()>>);

impl OwnedTask {
    pub(crate) fn new(handle: JoinHandle<()>) -> Self {
        Self(Some(handle))
    }

    pub(crate) async fn cancel_and_wait(mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
            let _ = handle.await;
        }
    }
}

impl Drop for OwnedTask {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

#[derive(Default)]
struct Admission {
    metadata: Option<Arc<VenueMetadata>>,
}

impl Admission {
    fn loaded(
        venues: &LoadedVenues,
        identity: &RequestIdentity,
    ) -> Result<Self, MarketParticipantError> {
        let route = identity.route();
        crate::capability::router::admit(venues, route, identity.capability())?;
        Ok(Self {
            metadata: venues
                .loaded(&route.venue)
                .map(|loaded| Arc::clone(loaded.metadata())),
        })
    }

    fn unchecked(metadata: Option<Arc<VenueMetadata>>) -> Self {
        Self { metadata }
    }

    fn metadata(&self) -> Option<Arc<VenueMetadata>> {
        self.metadata.clone()
    }
}

#[derive(Clone)]
pub struct MarketParticipantRuntime {
    pub transport: Arc<TransportHub>,
    pub connection_pool: Arc<ConnectionPool>,
    proxy_resolver: ProxyResolver,
    pub runtime: Handle,
    pub snapshot: ConfigNotification<MarketParticipantNetworkSnapshot>,
}

impl MarketParticipantRuntime {
    pub fn from_shared_resources(
        transport: Arc<TransportHub>,
        connection_pool: Arc<ConnectionPool>,
        snapshot: ConfigNotification<MarketParticipantNetworkSnapshot>,
        runtime: Handle,
    ) -> Result<Self, MarketParticipantError> {
        if !Arc::ptr_eq(&transport, &connection_pool.transport()) {
            return Err(MarketParticipantError::Configuration(
                "market-participants runtime requires a connection pool sharing its transport"
                    .into(),
            ));
        }
        let resolver_snapshot = snapshot.clone();
        let proxy_resolver = ProxyResolver::new(Arc::new(move || {
            Arc::clone(resolver_snapshot.current().network())
        }));
        Ok(Self {
            connection_pool,
            transport,
            proxy_resolver,
            runtime,
            snapshot,
        })
    }

    pub async fn shutdown(&self) {
        self.connection_pool.close_all().await;
    }
}

#[derive(Clone)]
pub struct MarketParticipantGateway {
    pub router: Arc<CapabilityRouter>,
    pub runtime: MarketParticipantRuntime,
    registry: Arc<RwLock<VenueRouteRegistry>>,
}

impl MarketParticipantGateway {
    pub fn adapter_router(
        config: &MarketParticipantNetworkSnapshot,
    ) -> Result<CapabilityRouter, MarketParticipantError> {
        let binance = BinanceAdapter::from_config(config)?;
        let okx = OkxAdapter::new();
        let hyperliquid = HyperliquidAdapter::from_config(config)?;
        let longbridge = LongbridgeAdapter::new();
        let polymarket = Arc::new(PolymarketAdapter::new());
        Ok(CapabilityRouter::new([
            binance as Arc<dyn crate::adapter::MarketParticipantAdapter>,
            okx as Arc<dyn crate::adapter::MarketParticipantAdapter>,
            hyperliquid as Arc<dyn crate::adapter::MarketParticipantAdapter>,
            longbridge as Arc<dyn crate::adapter::MarketParticipantAdapter>,
            polymarket as Arc<dyn crate::adapter::MarketParticipantAdapter>,
        ]))
    }

    pub fn with_shared_resources(
        router: CapabilityRouter,
        transport: Arc<TransportHub>,
        connection_pool: Arc<ConnectionPool>,
        snapshot: ConfigNotification<MarketParticipantNetworkSnapshot>,
        runtime: Handle,
    ) -> Result<Self, MarketParticipantError> {
        let runtime = MarketParticipantRuntime::from_shared_resources(
            transport,
            Arc::clone(&connection_pool),
            snapshot,
            runtime,
        )?;
        let registry = Arc::new(RwLock::new(VenueRouteRegistry::new()));
        Ok(Self {
            router: Arc::new(router),
            runtime,
            registry,
        })
    }

    pub(crate) fn registered(&self) -> Arc<RegisteredRoutes> {
        Arc::clone(self.registry.read().routes())
    }

    pub(crate) fn registry(&self) -> &Arc<RwLock<VenueRouteRegistry>> {
        &self.registry
    }

    pub(crate) fn plan_venue_routes(
        &self,
        venue: &market_participants_model::identifier::VenueId,
        descriptors: Vec<VenueRouteDescriptor>,
    ) -> Result<RegisteredRoutes, RegistryError> {
        self.registry.read().plan_venue_routes(venue, descriptors)
    }

    pub async fn shutdown(&self) {
        self.runtime.shutdown().await;
    }

    pub async fn execute(
        &self,
        venues: &LoadedVenues,
        request: ProtocolRequest,
        context: RequestIdentity,
        catalog: &Catalog,
    ) -> Result<ProtocolResponse, MarketParticipantError> {
        let routes = self.registered();
        let admission = Admission::loaded(venues, &context)?;
        let (adapter, execution_context, request) = self
            .prepare(&routes, admission, request, &context, catalog)
            .await?;
        adapter.execute(execution_context, request).await
    }

    pub(crate) async fn execute_with_routes(
        &self,
        routes: &RegisteredRoutes,
        metadata: Option<Arc<VenueMetadata>>,
        request: ProtocolRequest,
        context: RequestIdentity,
        catalog: &Catalog,
    ) -> Result<ProtocolResponse, MarketParticipantError> {
        let (adapter, execution_context, request) = self
            .prepare(
                routes,
                Admission::unchecked(metadata),
                request,
                &context,
                catalog,
            )
            .await?;
        adapter.execute(execution_context, request).await
    }

    pub(crate) async fn read_venue(
        &self,
        venue: &market_participants_model::identifier::VenueId,
    ) -> Result<VenueReading, MarketParticipantError> {
        let snapshot = self.runtime.snapshot.current();
        let endpoints = snapshot
            .venue_settings(venue)
            .and_then(|settings| settings.endpoints(settings.primary_scope()))
            .ok_or(MarketParticipantError::VenueUnsupported { venue: *venue })?;
        let (proxy, _network_route_generation) = self
            .runtime
            .proxy_resolver
            .resolve_for_venue_with_generation(
                venue,
                &endpoints.primary.http,
                &self.runtime.runtime,
            )
            .await
            .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{error}")))?;
        let http_client = self
            .runtime
            .transport
            .http()
            .client(&proxy, HttpClientClass::Public)
            .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{error}")))?;
        let adapter = self
            .router
            .adapter(venue)
            .ok_or(MarketParticipantError::VenueUnsupported { venue: *venue })?;
        let context = VenueExecutionContext::new_with_endpoints(*venue, endpoints, http_client);
        adapter.read_venue(context).await
    }

    pub async fn subscribe(
        &self,
        venues: &LoadedVenues,
        request: ProtocolRequest,
        context: RequestIdentity,
        catalog: &Catalog,
    ) -> Result<SubscriptionHandle, MarketParticipantError> {
        let routes = self.registered();
        let admission = Admission::loaded(venues, &context)?;
        let (adapter, execution_context, request) = self
            .prepare(&routes, admission, request, &context, catalog)
            .await?;
        adapter.subscribe(execution_context, request).await
    }

    async fn prepare(
        &self,
        routes: &RegisteredRoutes,
        admission: Admission,
        request: ProtocolRequest,
        context: &RequestIdentity,
        catalog: &Catalog,
    ) -> Result<
        (
            Arc<dyn crate::adapter::MarketParticipantAdapter>,
            ExecutionContext,
            crate::adapter::PreparedRequest,
        ),
        MarketParticipantError,
    > {
        let classified = classify(&request);
        if classified.capability() != context.capability() {
            return Err(MarketParticipantError::CapabilityMismatch {
                expected: classified.capability(),
                actual: context.capability(),
            });
        }
        let route = context.route();
        if classified.target().route() != route {
            let mismatched = classified.target().route().clone();
            return Err(MarketParticipantError::RouteMismatch {
                request: mismatched,
                context: route.clone(),
            });
        }
        let capability = *routes
            .capability_descriptor(route, context.capability())
            .ok_or_else(|| MarketParticipantError::UnsupportedCapability {
                route: route.clone(),
                capability: context.capability(),
            })?;
        let listing = self.resolve_listing(&classified, catalog).await?;
        if let Some(listing) = listing.as_ref()
            && listing.key.route != *route
        {
            return Err(MarketParticipantError::ListingRouteMismatch {
                listing: listing.key.clone(),
                route: route.clone(),
            });
        }
        if let Some(listing) = listing.as_ref() {
            Self::require_listing_command(&classified, listing)?;
        }
        let adapter = self.router.resolve(route, context.capability())?;
        let execution_context = self
            .execution_context(context, &capability, &classified, admission.metadata())
            .await?;
        Ok((
            adapter,
            execution_context,
            crate::adapter::PreparedRequest::new(request, listing.as_ref().map(listing_ref)),
        ))
    }

    async fn resolve_listing(
        &self,
        classified: &ClassifiedRequest,
        catalog: &Catalog,
    ) -> Result<Option<ResolvedListing>, MarketParticipantError> {
        let Some(key) = classified.listing() else {
            return Ok(None);
        };
        catalog
            .resolve_listing(key)
            .map(Some)
            .map_err(|error| MarketParticipantError::Catalog(Box::new(error)))
    }

    fn require_listing_command(
        classified: &ClassifiedRequest,
        listing: &ResolvedListing,
    ) -> Result<(), MarketParticipantError> {
        let Some(command) = classified.command() else {
            return Ok(());
        };
        let as_of = now_millis();
        let within_window = listing.available_from.is_none_or(|from| as_of >= from)
            && listing.available_until.is_none_or(|until| as_of < until);
        if ListingCommandMatrix::permits(
            listing.status,
            command,
            listing.available_from,
            listing.available_until,
            as_of,
        ) {
            return Ok(());
        }
        Err(MarketParticipantError::ListingActionNotAllowed {
            listing: listing.key.clone(),
            status: listing.status,
            command,
            within_window,
        })
    }

    async fn execution_context(
        &self,
        identity: &RequestIdentity,
        capability: &RouteCapabilityDescriptor,
        classified: &ClassifiedRequest,
        metadata: Option<Arc<VenueMetadata>>,
    ) -> Result<ExecutionContext, MarketParticipantError> {
        let route = identity.route();
        let snapshot = self.runtime.snapshot.current();
        let endpoints = snapshot
            .endpoints(&route.venue, &route.scope)
            .ok_or(MarketParticipantError::VenueUnsupported { venue: route.venue })?;
        let (proxy, network_route_generation) = self
            .runtime
            .proxy_resolver
            .resolve_for_venue_with_generation(
                &route.venue,
                &endpoints.primary.http,
                &self.runtime.runtime,
            )
            .await
            .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{error}")))?;
        let http_client = self
            .runtime
            .transport
            .http()
            .client(
                &proxy,
                if capability.needs_auth() {
                    HttpClientClass::Private
                } else {
                    HttpClientClass::Public
                },
            )
            .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{error}")))?;
        let credential_generation = snapshot.credential_generation(&route.venue, &route.scope);
        let context = ExecutionContext::new_with_endpoints(
            identity.clone(),
            endpoints,
            http_client,
            Arc::clone(&self.runtime.connection_pool),
        )
        .with_proxy(proxy, network_route_generation)
        .with_credential_generation(credential_generation)
        .with_venue_metadata(metadata);
        let context = authenticate(&snapshot, context, capability, classified)?;
        Ok(context.with_runtime(self.runtime.snapshot.clone()))
    }
}

fn authenticate(
    snapshot: &MarketParticipantNetworkSnapshot,
    context: ExecutionContext,
    capability: &RouteCapabilityDescriptor,
    classified: &ClassifiedRequest,
) -> Result<ExecutionContext, MarketParticipantError> {
    let Some(account) = classified.authenticated_account() else {
        return Ok(context);
    };
    let route = context.route();
    if !capability.needs_auth() {
        return Err(MarketParticipantError::UnsupportedCapability {
            route,
            capability: capability.capability(),
        });
    }
    let auth = credential_authentication(snapshot, account, &route)?;
    Ok(context.with_authentication(auth))
}

fn credential_authentication(
    snapshot: &MarketParticipantNetworkSnapshot,
    account: &MarketParticipantAccountId,
    route: &VenueRoute,
) -> Result<Authentication, MarketParticipantError> {
    let account_id: SmolStr = account.as_str().into();
    let Some(settings) = snapshot.venue_settings(&route.venue) else {
        return Err(authentication_failed(
            route,
            account,
            AuthError::ApiKeyNotFound,
        ));
    };
    match settings {
        VenueSettings::Binance(_) => {
            match snapshot.resolve_binance_credentials(&route.venue, &route.scope) {
                CredentialState::Configured(credentials) => Ok(Authentication::ApiKey {
                    account_id,
                    api_key: credentials.api_key,
                    secret: credentials.private_key,
                }),
                CredentialState::Unconfigured => Err(authentication_failed(
                    route,
                    account,
                    AuthError::ApiKeyNotFound,
                )),
                CredentialState::Invalid(error) => {
                    Err(authentication_invalid(route, account, error))
                }
            }
        }
        VenueSettings::Okx(_) => {
            match snapshot.resolve_okx_credentials(&route.venue, &route.scope) {
                CredentialState::Configured(credentials) => Ok(Authentication::PassphraseApiKey {
                    account_id,
                    api_key: credentials.api_key,
                    secret: credentials.secret_key,
                    passphrase: credentials.passphrase,
                }),
                CredentialState::Unconfigured => Err(authentication_failed(
                    route,
                    account,
                    AuthError::ApiKeyNotFound,
                )),
                CredentialState::Invalid(error) => {
                    Err(authentication_invalid(route, account, error))
                }
            }
        }
        VenueSettings::Hyperliquid(_) => {
            match snapshot.resolve_hyperliquid_credentials(&route.venue, &route.scope) {
                CredentialState::Configured(credentials) => Ok(Authentication::Wallet {
                    account_id,
                    address: credentials.main_account_address,
                    private_key: credentials.api_agent_private_key,
                }),
                CredentialState::Unconfigured => Err(authentication_failed(
                    route,
                    account,
                    AuthError::ApiKeyNotFound,
                )),
                CredentialState::Invalid(error) => {
                    Err(authentication_invalid(route, account, error))
                }
            }
        }
        VenueSettings::Longbridge(_) => {
            match snapshot.resolve_longbridge_credentials(&route.venue, &route.scope) {
                CredentialState::Configured(credentials) => Ok(Authentication::LongbridgeApiKey {
                    account_id,
                    app_key: credentials.app_key,
                    app_secret: credentials.app_secret,
                    access_token: credentials.access_token,
                }),
                CredentialState::Unconfigured => Err(authentication_failed(
                    route,
                    account,
                    AuthError::ApiKeyNotFound,
                )),
                CredentialState::Invalid(error) => {
                    Err(authentication_invalid(route, account, error))
                }
            }
        }
        VenueSettings::Polymarket(_) => Err(authentication_failed(
            route,
            account,
            AuthError::ApiKeyNotFound,
        )),
    }
}

fn authentication_failed(
    route: &VenueRoute,
    account: &MarketParticipantAccountId,
    source: AuthError,
) -> MarketParticipantError {
    MarketParticipantError::AuthenticationFailed {
        route: route.clone(),
        account: Some(account.as_str().into()),
        source,
    }
}

fn authentication_invalid(
    route: &VenueRoute,
    account: &MarketParticipantAccountId,
    error: MarketParticipantsConfigError,
) -> MarketParticipantError {
    let MarketParticipantsConfigError::IncompleteCredentials { venue, scope } = error;
    authentication_failed(
        route,
        account,
        AuthError::IncompleteCredentials { venue, scope },
    )
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

fn listing_ref(resolved: &ResolvedListing) -> ResolvedListingRef {
    ResolvedListingRef {
        key: resolved.key.clone(),
        generation: resolved.generation,
        native_symbol: resolved.native_symbol.clone(),
        identifiers: resolved.identifiers.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use app_config::{
        market_participants::{
            credentials::{BinanceMarketCredentials, LongbridgeCredentials},
            fixture::{self, FixtureAddress, FixtureInstallation},
            origins::BinanceMarket,
            settings::VenueSettings,
        },
        settings::Config,
    };
    use app_config_provider::notification::ConfigNotification;
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::{
            listing_impl::ListingStatus,
            route::{VenueListingKey, VenueRoute},
        },
        market::{selection::OrderQuantity, units::Side},
        routing::{CapabilityId, IdentifierNormalizationRule},
    };
    use network::transport::hub::{TransportHub, TransportPolicy};

    use super::{Admission, MarketParticipantGateway, credential_authentication};
    use crate::{
        account::shared::{MarginMode, OrderId, OrderKind},
        adapter::{Authentication, PreparedRequest, ProtocolRequest},
        catalog::{
            source::{
                CatalogRouteRegistration, CatalogRouteUpdate, CatalogSourceBinding,
                CatalogSourceId, RouteInputRevision,
            },
            store::Catalog,
            test_support,
        },
        connection::ConnectionPool,
        error::{AuthError, MarketParticipantError},
        market_participants::model::{
            AuthenticatedQuery, CancelOrderCommand, MarketCatalogQuery, OrderBookQuery,
            PlaceOrderCommand, SetLeverageCommand, SetMarginModeCommand, TradingSettingsQuery,
        },
        protocol::{AuthenticatedTarget, RequestIdentity, classify},
        routing::{LONGBRIDGE_US_SCOPE, PERPETUAL_SCOPE},
        runtime::MarketParticipantNetworkSnapshot,
        test_support::{fixture_config, test_handle},
    };

    fn route() -> VenueRoute {
        test_support::spot_route()
    }

    fn other_route() -> VenueRoute {
        test_support::route("binance", PERPETUAL_SCOPE)
    }

    fn longbridge_route() -> VenueRoute {
        VenueRoute::new(
            VenueId::Longbridge,
            VenueScopeId::new(LONGBRIDGE_US_SCOPE).expect("the built-in scope name is canonical"),
        )
    }

    fn listing() -> VenueListingKey {
        test_support::listing_key(&route(), "BTCUSDT")
    }

    fn other_listing() -> VenueListingKey {
        test_support::listing_key(&other_route(), "BTCUSDT")
    }

    fn fixture_origin() -> url::Url {
        "https://fixture.test"
            .parse()
            .expect("the fixture origin is a URL")
    }

    fn account(route: &VenueRoute) -> AuthenticatedTarget {
        AuthenticatedTarget::new(
            market_participants_model::identifier::MarketParticipantAccountId::new("primary")
                .expect("the fixture account name is canonical"),
            route.clone(),
        )
    }

    fn configured(fixture: &FixtureInstallation) -> Config {
        let mut config = fixture_config(fixture);
        let venue = route().venue;
        let Some(VenueSettings::Binance(settings)) = config.venues.get_mut(&venue) else {
            panic!("the fixture venue is configured as Binance");
        };
        for market in BinanceMarket::ALL {
            settings.set_market_credentials(
                market,
                Some(BinanceMarketCredentials {
                    api_key: "fixture-api-key".into(),
                    private_key: "0707070707070707070707070707070707070707070707070707070707070707"
                        .into(),
                    passphrase: "".into(),
                }),
            );
        }
        config
    }

    fn descriptor_rules(route: &VenueRoute) -> Vec<IdentifierNormalizationRule> {
        crate::venue::specs::all_descriptors()
            .iter()
            .find(|descriptor| descriptor.route() == route)
            .expect("the fixture route is a built-in one")
            .identifier_normalization()
            .to_vec()
    }

    struct Fixture {
        gateway: MarketParticipantGateway,
        catalog: Arc<Catalog>,
    }

    impl Fixture {
        async fn new(status: ListingStatus, rules: Vec<IdentifierNormalizationRule>) -> Self {
            let fixture = fixture::venues([(route().venue, FixtureAddress::at(&fixture_origin()))]);
            let snapshot = Arc::new(MarketParticipantNetworkSnapshot::new(configured(&fixture)));
            let notification = ConfigNotification::new(Arc::clone(&snapshot));
            let transport = Arc::new(TransportHub::new(TransportPolicy::default()));
            let pool = Arc::new(ConnectionPool::with_transport(Arc::clone(&transport)));
            let router = MarketParticipantGateway::adapter_router(&snapshot)
                .expect("the built-in adapters build from the fixture configuration");
            let gateway = MarketParticipantGateway::with_shared_resources(
                router,
                transport,
                pool,
                notification,
                test_handle(),
            )
            .expect("the fixture gateway builds");
            let catalog = Arc::new(Catalog::new());
            let binance: Vec<crate::routing::VenueRouteDescriptor> =
                crate::venue::specs::all_descriptors()
                    .into_iter()
                    .filter(|descriptor| descriptor.route().venue == route().venue)
                    .collect();
            let committed = {
                let mut registry = gateway.registry().write();
                registry
                    .plan_venue_routes(&route().venue, binance.clone())
                    .expect("the built-in Binance routes are a valid candidate");
                registry.commit_venue_routes(&route().venue, binance)
            };
            assert!(
                committed.supports(&route(), CapabilityId::MarketCatalog),
                "the fixture venue is registered"
            );
            let source_id = CatalogSourceId::new("gateway-fixture")
                .expect("the fixture source id is canonical");
            let entry = test_support::with_status(
                test_support::entry(
                    &route(),
                    test_support::spot_token("binance", "BTC", "USDT"),
                    "BTCUSDT",
                ),
                status,
            );
            let other_entry = test_support::entry(
                &other_route(),
                test_support::spot_token("binance", "BTC", "USDT"),
                "BTCUSDT",
            );
            let other_rules = descriptor_rules(&other_route());
            catalog
                .commit_fixture_routes(
                    CatalogRouteUpdate::new(
                        [CatalogSourceBinding::new(
                            source_id.clone(),
                            test_support::listing(vec![entry, other_entry]),
                        )],
                        [
                            CatalogRouteRegistration::catalog(
                                route(),
                                source_id.clone(),
                                rules.clone(),
                                vec![CapabilityId::MarketCatalog],
                                RouteInputRevision::of_declaration(
                                    &route(),
                                    Some(&source_id),
                                    &rules,
                                ),
                            ),
                            CatalogRouteRegistration::catalog(
                                other_route(),
                                source_id.clone(),
                                other_rules.clone(),
                                vec![CapabilityId::MarketCatalog],
                                RouteInputRevision::of_declaration(
                                    &other_route(),
                                    Some(&source_id),
                                    &other_rules,
                                ),
                            ),
                        ],
                    ),
                    &test_support::requirements(&[route(), other_route()]),
                )
                .expect("the fixture catalog routes publish");
            for published in [route(), other_route()] {
                catalog
                    .refresh_for_test(&published)
                    .await
                    .expect("the fixture catalog route publishes its catalog");
            }
            Self { gateway, catalog }
        }

        async fn with_status(status: ListingStatus) -> Self {
            Self::new(status, descriptor_rules(&route())).await
        }

        fn context(&self, capability: CapabilityId) -> RequestIdentity {
            RequestIdentity::new(route(), capability)
        }

        async fn prepare(
            &self,
            request: ProtocolRequest,
            capability: CapabilityId,
        ) -> Result<PreparedRequest, MarketParticipantError> {
            self.prepare_with(request, self.context(capability)).await
        }

        async fn prepare_with(
            &self,
            request: ProtocolRequest,
            context: RequestIdentity,
        ) -> Result<PreparedRequest, MarketParticipantError> {
            self.gateway
                .prepare(
                    &self.gateway.registered(),
                    Admission::default(),
                    request,
                    &context,
                    &self.catalog,
                )
                .await
                .map(|(_, _, prepared)| prepared)
        }

        fn markets(&self, route: &VenueRoute) -> ProtocolRequest {
            ProtocolRequest::FetchMarkets(MarketCatalogQuery {
                route: route.clone(),
            })
        }

        fn order_book(&self, listing: &VenueListingKey) -> ProtocolRequest {
            ProtocolRequest::FetchOrderBook(OrderBookQuery {
                listing: listing.clone(),
                depth: 5,
            })
        }

        fn place_order(&self, listing: &VenueListingKey, route: &VenueRoute) -> ProtocolRequest {
            ProtocolRequest::PlaceOrder(PlaceOrderCommand {
                target: account(route),
                listing: listing.clone(),
                side: Side::Buy,
                position_side: None,
                kind: OrderKind::Market,
                quantity: OrderQuantity::AssetUnits {
                    asset: test_support::token("binance", "BTC"),
                    value: market_participants_model::decimal::Quantity::from(1_u8),
                },
                reduce_only: false,
                margin_mode: Some(MarginMode::Cross),
                leverage: None,
                market_max_slippage: None,
            })
        }

        fn active_orders_of(&self, listing: &VenueListingKey) -> ProtocolRequest {
            ProtocolRequest::FetchActiveOrders(AuthenticatedQuery::for_listing(
                account(&route()),
                listing.clone(),
            ))
        }

        fn active_orders(&self) -> ProtocolRequest {
            ProtocolRequest::FetchActiveOrders(AuthenticatedQuery::account_wide(account(&route())))
        }
    }

    #[tokio::test]
    async fn a_capability_that_is_not_the_requests_own_is_refused() {
        let fixture = Fixture::with_status(ListingStatus::Trading).await;
        let error = fixture
            .prepare(fixture.markets(&route()), CapabilityId::Tickers)
            .await
            .expect_err("a route request under another capability must be refused");

        assert_eq!(
            error,
            MarketParticipantError::CapabilityMismatch {
                expected: CapabilityId::MarketCatalog,
                actual: CapabilityId::Tickers,
            }
        );
    }

    #[tokio::test]
    async fn a_request_of_another_route_than_its_target_is_refused() {
        let fixture = Fixture::with_status(ListingStatus::Trading).await;
        let error = fixture
            .prepare(fixture.markets(&other_route()), CapabilityId::MarketCatalog)
            .await
            .expect_err("a request for another route must be refused");

        assert_eq!(
            error,
            MarketParticipantError::RouteMismatch {
                request: other_route(),
                context: route(),
            }
        );
    }

    #[tokio::test]
    async fn a_halted_listing_refuses_a_new_order() {
        let fixture = Fixture::with_status(ListingStatus::Halted).await;
        let error = fixture
            .prepare(
                fixture.place_order(&listing(), &route()),
                CapabilityId::AuthenticatedTrading,
            )
            .await
            .expect_err("a halted listing must refuse a new order");

        assert_eq!(
            error,
            MarketParticipantError::ListingActionNotAllowed {
                listing: listing(),
                status: ListingStatus::Halted,
                command: crate::protocol::CommandKind::NewTrading,
                within_window: true,
            }
        );
    }

    #[tokio::test]
    async fn a_command_carries_the_listing_the_gateway_resolved() {
        let fixture = Fixture::with_status(ListingStatus::Trading).await;
        let prepared = fixture
            .prepare(
                fixture.place_order(&listing(), &route()),
                CapabilityId::AuthenticatedTrading,
            )
            .await
            .expect("a trading listing permits a new order");

        assert_eq!(
            prepared.listing().map(|listing| &listing.key),
            Some(&listing()),
            "a command reaches the adapter with its resolved listing"
        );

        let error = fixture
            .prepare(
                fixture.place_order(&other_listing(), &route()),
                CapabilityId::AuthenticatedTrading,
            )
            .await
            .expect_err("a listing of another route must be refused");

        assert_eq!(
            error,
            MarketParticipantError::ListingRouteMismatch {
                listing: other_listing(),
                route: route(),
            }
        );
    }

    #[tokio::test]
    async fn a_narrowed_account_read_keeps_its_listing() {
        let fixture = Fixture::with_status(ListingStatus::Trading).await;
        let narrowed = fixture
            .prepare(
                fixture.active_orders_of(&listing()),
                CapabilityId::AccountOrders,
            )
            .await
            .expect("an account read of a published listing is prepared");
        let wide = fixture
            .prepare(fixture.active_orders(), CapabilityId::AccountOrders)
            .await
            .expect("an account-wide read is prepared");

        assert_eq!(
            narrowed.listing().map(|listing| &listing.key),
            Some(&listing())
        );
        assert!(
            wide.listing().is_none(),
            "an account-wide read names no listing and must not be narrowed to one"
        );
    }

    #[tokio::test]
    async fn an_authenticated_request_of_a_capability_the_route_lacks_names_it() {
        let fixture = Fixture::with_status(ListingStatus::Trading).await;
        let error = fixture
            .prepare(
                ProtocolRequest::FetchPositions(AuthenticatedQuery::account_wide(
                    account(&route()),
                )),
                CapabilityId::AccountPositions,
            )
            .await
            .expect_err("the fixture route publishes no position read");

        assert_eq!(
            error,
            MarketParticipantError::UnsupportedCapability {
                route: route(),
                capability: CapabilityId::AccountPositions,
            }
        );
    }

    #[test]
    fn a_longbridge_route_answers_its_own_credential_states() {
        let route = longbridge_route();

        let configured = MarketParticipantNetworkSnapshot::new(longbridge_config(Some((
            "longbridge-app-key",
            "longbridge-app-secret",
            "longbridge-access-token",
        ))));
        assert_eq!(
            configured
                .venue_settings(&VenueId::Longbridge)
                .expect("the built-in table configures longbridge")
                .validate_credentials(VenueId::Longbridge, LONGBRIDGE_US_SCOPE),
            Ok(true),
            "the stored set is complete, so the answer below is not about missing values"
        );
        assert_eq!(
            credential_authentication(&configured, account(&route).account(), &route)
                .expect("a complete set authenticates"),
            Authentication::LongbridgeApiKey {
                account_id: "primary".into(),
                app_key: "longbridge-app-key".into(),
                app_secret: "longbridge-app-secret".into(),
                access_token: "longbridge-access-token".into(),
            }
        );

        let unconfigured = MarketParticipantNetworkSnapshot::new(longbridge_config(None));
        assert_eq!(
            credential_authentication(&unconfigured, account(&route).account(), &route)
                .expect_err("a venue with nothing stored authenticates nothing"),
            MarketParticipantError::AuthenticationFailed {
                route: route.clone(),
                account: Some("primary".into()),
                source: AuthError::ApiKeyNotFound,
            }
        );

        let incomplete = MarketParticipantNetworkSnapshot::new(longbridge_config(Some((
            "longbridge-app-key",
            "",
            "longbridge-access-token",
        ))));
        assert_eq!(
            credential_authentication(&incomplete, account(&route).account(), &route)
                .expect_err("a half-filled set is not a credential"),
            MarketParticipantError::AuthenticationFailed {
                route,
                account: Some("primary".into()),
                source: AuthError::IncompleteCredentials {
                    venue: VenueId::Longbridge,
                    scope: None,
                },
            }
        );
    }

    fn longbridge_config(values: Option<(&str, &str, &str)>) -> Config {
        let mut config = Config::default();
        let Some(VenueSettings::Longbridge(settings)) = config.venues.get_mut(&VenueId::Longbridge)
        else {
            panic!("the built-in table configures longbridge as Longbridge");
        };
        if let Some((app_key, app_secret, access_token)) = values {
            settings.credentials = Some(LongbridgeCredentials {
                app_key: smol_str::SmolStr::new(app_key),
                app_secret: smol_str::SmolStr::new(app_secret),
                access_token: smol_str::SmolStr::new(access_token),
            });
        }
        config
    }

    #[test]
    fn the_router_registers_the_longbridge_adapter() {
        let router = MarketParticipantGateway::adapter_router(
            &MarketParticipantNetworkSnapshot::new(longbridge_config(Some((
                "longbridge-app-key",
                "longbridge-app-secret",
                "longbridge-access-token",
            )))),
        )
        .expect("the built-in router builds");

        assert_eq!(
            router
                .adapter(&VenueId::Longbridge)
                .expect("the venue's own adapter is registered")
                .venue(),
            &VenueId::Longbridge
        );
        assert_eq!(
            router
                .adapter(&VenueId::Binance)
                .expect("the other venues keep theirs")
                .venue(),
            &VenueId::Binance
        );
    }

    #[test]
    fn every_command_and_every_narrowed_account_read_names_its_listing() {
        let target = account(&route());
        let key = listing();
        let query = || AuthenticatedQuery::for_listing(account(&route()), listing());
        let requests = [
            ProtocolRequest::PlaceOrder(PlaceOrderCommand {
                target: target.clone(),
                listing: key.clone(),
                side: Side::Buy,
                position_side: None,
                kind: OrderKind::Market,
                quantity: OrderQuantity::AssetUnits {
                    asset: test_support::token("binance", "BTC"),
                    value: market_participants_model::decimal::Quantity::from(1_u8),
                },
                reduce_only: false,
                margin_mode: Some(MarginMode::Cross),
                leverage: None,
                market_max_slippage: None,
            }),
            ProtocolRequest::CancelOrder(CancelOrderCommand {
                target: target.clone(),
                listing: key.clone(),
                order_id: OrderId::normal("7").expect("the fixture order id is valid"),
            }),
            ProtocolRequest::SetMarginMode(SetMarginModeCommand {
                target: target.clone(),
                listing: key.clone(),
                mode: MarginMode::Cross,
            }),
            ProtocolRequest::SetLeverage(SetLeverageCommand {
                target: target.clone(),
                listing: key.clone(),
                margin_mode: MarginMode::Cross,
                position_side: None,
                leverage: std::num::NonZeroU8::new(5).expect("the fixture leverage is non-zero"),
            }),
            ProtocolRequest::FetchTradingSettings(TradingSettingsQuery {
                target,
                listing: key.clone(),
                margin_mode: MarginMode::Cross,
            }),
            ProtocolRequest::SubscribeOrders(query()),
            ProtocolRequest::SubscribePositions(query()),
            ProtocolRequest::FetchBalances(query()),
            ProtocolRequest::FetchActiveOrders(query()),
            ProtocolRequest::FetchOrderHistory(query()),
            ProtocolRequest::FetchPositions(query()),
            ProtocolRequest::FetchAccountTrades(query()),
            ProtocolRequest::FetchPositionHistory(query()),
        ];

        assert_eq!(
            requests.len(),
            13,
            "four commands and nine reads carry a listing"
        );
        for request in &requests {
            let classified = classify(request);
            assert_eq!(
                classified.listing(),
                Some(&key),
                "{classified:?} must name the listing it carries"
            );
        }
    }

    #[tokio::test]
    async fn a_listing_scoped_read_and_a_settings_read_reach_the_adapter_with_their_listing() {
        let fixture = Fixture::with_status(ListingStatus::Trading).await;
        let prepared = fixture
            .prepare(fixture.order_book(&listing()), CapabilityId::OrderBook)
            .await
            .expect("an order-book read of a published listing is prepared");

        assert_eq!(
            prepared.listing().map(|listing| &listing.key),
            Some(&listing())
        );

        let settings = fixture
            .prepare(
                ProtocolRequest::FetchTradingSettings(TradingSettingsQuery {
                    target: account(&route()),
                    listing: listing(),
                    margin_mode: MarginMode::Cross,
                }),
                CapabilityId::AuthenticatedTrading,
            )
            .await
            .expect("a settings read of a published listing is prepared");
        assert_eq!(
            settings.listing().map(|listing| &listing.key),
            Some(&listing()),
            "a command-shaped request carries its listing too"
        );
    }

    #[tokio::test]
    async fn a_listing_the_route_never_published_is_refused() {
        let fixture = Fixture::with_status(ListingStatus::Trading).await;
        let unknown = test_support::listing_key(&route(), "ETHUSDT");
        let error = fixture
            .prepare(fixture.order_book(&unknown), CapabilityId::OrderBook)
            .await
            .expect_err("an unpublished listing must not resolve");

        assert!(
            matches!(error, MarketParticipantError::Catalog(_)),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn an_execution_context_names_the_route_and_the_venues_origins() {
        let fixture = Fixture::with_status(ListingStatus::Trading).await;
        let routes = fixture.gateway.registered();
        let (_, context, _) = fixture
            .gateway
            .prepare(
                &routes,
                Admission::default(),
                fixture.markets(&route()),
                &fixture.context(CapabilityId::MarketCatalog),
                &fixture.catalog,
            )
            .await
            .expect("a catalog request is prepared");

        assert_eq!(
            context.identity(),
            &RequestIdentity::new(route(), CapabilityId::MarketCatalog),
            "the identity is the route and the capability the request was validated under"
        );
        assert_eq!(context.capability(), CapabilityId::MarketCatalog);
        assert_eq!(
            context.http_origin().as_str(),
            "https://fixture.test/",
            "the HTTP origin is the venue configuration's own"
        );
        assert_eq!(
            context.websocket_origin().as_str(),
            "https://fixture.test/",
            "and so is the WebSocket one"
        );
    }

    #[tokio::test]
    async fn the_routes_own_rules_decide_how_a_native_symbol_is_folded() {
        let fixture = Fixture::with_status(ListingStatus::Trading).await;

        assert_eq!(
            fixture
                .catalog
                .resolve_native_symbol(&route(), "btcusdt")
                .expect("the route declared ASCII case folding for its symbols"),
            listing(),
            "a product-qualified rule is the one the route's own symbols fold under"
        );

        for scope in [crate::routing::SPOT_SCOPE, crate::routing::SWAP_SCOPE] {
            let okx = test_support::route(crate::routing::OKX_VENUE, scope);
            let registration = crate::catalog::source::RouteRegistration {
                normalization: descriptor_rules(&okx),
                capabilities: vec![CapabilityId::MarketCatalog],
                revision: RouteInputRevision::describe().build(),
            };
            assert_eq!(
                registration.normalization_mode(
                    market_participants_model::routing::ListingIdentifierKind::Symbol
                ),
                market_participants_model::routing::SymbolNormalization::AsciiCaseInsensitive,
                "{okx} folds its own symbols"
            );
        }

        let deployed = crate::adapter::hyperliquid::catalog::perp_dex_descriptor("xyz")
            .expect("a deployed DEX has a descriptor");
        let registration = crate::catalog::source::RouteRegistration {
            normalization: deployed.identifier_normalization().to_vec(),
            capabilities: vec![CapabilityId::MarketCatalog],
            revision: RouteInputRevision::describe().build(),
        };
        let mode = registration.normalization_mode(
            market_participants_model::routing::ListingIdentifierKind::HyperliquidAsset,
        );
        assert_eq!(
            mode,
            market_participants_model::routing::SymbolNormalization::Exact,
            "a HIP-3 coin is indexed exactly"
        );
        assert_eq!(
            mode.apply("XYZ:abc")
                .expect("an exact rule keeps its input"),
            "XYZ:abc",
            "a differently spelled HIP-3 coin is another coin"
        );
    }
}

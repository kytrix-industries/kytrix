//! The gateway: the only place a protocol request becomes a network operation.
//!
//! Every operation is validated against the registered route registry before it
//! touches a socket: the request's capability must be the target's, the route and
//! listing the request names must be the target's route, and the listing status
//! must permit the command. Which origins a request dials and which credentials it
//! signs with come from the route's **venue** configuration, never from the route's
//! own descriptor.

use std::sync::Arc;

use app_config::market_participants::{
    error::MarketParticipantsConfigError, settings::VenueSettings,
};
use market_participants_model::{
    identifier::MarketParticipantAccountId, listing::route::VenueRoute,
};
use network::{
    proxy::ProxyResolver,
    transport::{http::HttpClientClass, hub::TransportHub},
};
use parking_lot::{Mutex, RwLock};
use smol_str::{SmolStr, format_smolstr};
use tokio::{sync::watch, task::JoinHandle};
use widget_tokio_runtime::runtime::Runtime;

use crate::{
    adapter::{
        Authentication, ExecutionContext, ProtocolRequest, VenueExecutionContext,
        binance::BinanceAdapter, hyperliquid::adapter::implementation::HyperliquidAdapter,
        longbridge::LongbridgeAdapter, okx::OkxAdapter,
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
/// A spawned watcher whose owner can cancel and await it before releasing the
/// underlying resources captured by the task.
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

/// What one request was admitted against, and the metadata that admitted it.
///
/// A request path is admitted by the one routing view, and the commit that
/// admitted it is the only place its venue metadata can come from. A
/// load-time conversation names its own route view instead and carries the
/// reading it belongs to — the metadata a load is about to commit for the catalog
/// it stages, or nothing for the discovery that produces it.
#[derive(Default)]
struct Admission {
    metadata: Option<Arc<VenueMetadata>>,
}

impl Admission {
    /// Returns the admission of one loaded `(route, capability)`.
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

    /// Returns the admission of one load-time conversation.
    fn unchecked(metadata: Option<Arc<VenueMetadata>>) -> Self {
        Self { metadata }
    }

    /// Returns the venue metadata this execution is resolved against.
    fn metadata(&self) -> Option<Arc<VenueMetadata>> {
        self.metadata.clone()
    }
}

#[derive(Clone)]
pub struct MarketParticipantRuntime {
    pub transport: Arc<TransportHub>,
    pub connection_pool: Arc<ConnectionPool>,
    proxy_resolver: ProxyResolver,
    /// The application runtime every request's route resolution runs on.
    ///
    /// A request can arrive from a GPUI task, which has no Tokio context of its
    /// own, so the runtime travels with the gateway instead of being discovered
    /// on the thread that happens to be executing.
    pub runtime: Runtime,
    pub snapshot: watch::Receiver<Arc<MarketParticipantNetworkSnapshot>>,
    snapshot_task: Arc<Mutex<Option<OwnedTask>>>,
}

impl MarketParticipantRuntime {
    pub fn from_shared_resources(
        transport: Arc<TransportHub>,
        connection_pool: Arc<ConnectionPool>,
        snapshot: watch::Receiver<Arc<MarketParticipantNetworkSnapshot>>,
        runtime: Runtime,
    ) -> Result<Self, MarketParticipantError> {
        if !Arc::ptr_eq(&transport, &connection_pool.transport()) {
            return Err(MarketParticipantError::Configuration(
                "market-participants runtime requires a connection pool sharing its transport"
                    .into(),
            ));
        }
        let network_snapshot = Arc::new(snapshot.borrow().network().clone());
        let (network_sender, network_receiver) = watch::channel(network_snapshot);
        let mut snapshot_changes = snapshot.clone();
        let snapshot_task = OwnedTask::new(runtime.run(async move {
            while snapshot_changes.changed().await.is_ok() {
                let _ = network_sender
                    .send_replace(Arc::new(snapshot_changes.borrow().network().clone()));
            }
        }));
        let proxy_resolver = ProxyResolver::new(network_receiver);
        Ok(Self {
            connection_pool,
            transport,
            proxy_resolver,
            runtime,
            snapshot,
            snapshot_task: Arc::new(Mutex::new(Some(snapshot_task))),
        })
    }

    fn take_snapshot_task(&self) -> Option<OwnedTask> {
        self.snapshot_task.lock().take()
    }

    pub async fn shutdown(&self) {
        let snapshot_task = self.take_snapshot_task();
        if let Some(task) = snapshot_task {
            task.cancel_and_wait().await;
        }
        self.connection_pool.close_all().await;
    }
}

/// The single market-participants gateway: registry, adapters and transport.
#[derive(Clone)]
pub struct MarketParticipantGateway {
    pub router: Arc<CapabilityRouter>,
    pub runtime: MarketParticipantRuntime,
    registry: Arc<RwLock<VenueRouteRegistry>>,
    pool_config_task: Arc<Mutex<Option<OwnedTask>>>,
}

impl MarketParticipantGateway {
    /// Builds the adapter registry of one configuration.
    ///
    /// An adapter is constructed once per venue and then answers `supports` per
    /// exact scope: whether a scope is served is never decided by the venue, and a
    /// Hyperliquid HIP-3 DEX or HIP-4 outcome venue is answered by the same
    /// adapter as the core scope without being folded into it.
    pub fn adapter_router(
        config: &MarketParticipantNetworkSnapshot,
    ) -> Result<CapabilityRouter, MarketParticipantError> {
        let binance = BinanceAdapter::from_config(config)?;
        let okx = OkxAdapter::new();
        let hyperliquid = HyperliquidAdapter::from_config(config)?;
        let longbridge = LongbridgeAdapter::new();
        Ok(CapabilityRouter::new([
            binance as Arc<dyn crate::adapter::MarketParticipantAdapter>,
            okx as Arc<dyn crate::adapter::MarketParticipantAdapter>,
            hyperliquid as Arc<dyn crate::adapter::MarketParticipantAdapter>,
            longbridge as Arc<dyn crate::adapter::MarketParticipantAdapter>,
        ]))
    }

    /// Builds a gateway around already-built adapters and shared resources.
    ///
    /// The composition root uses this when adapters are constructed from typed
    /// runtime inputs of their own; the registry starts empty and the root publishes
    /// its routes into it.
    pub fn with_shared_resources(
        router: CapabilityRouter,
        transport: Arc<TransportHub>,
        connection_pool: Arc<ConnectionPool>,
        snapshot: watch::Receiver<Arc<MarketParticipantNetworkSnapshot>>,
        spawn_pool_watcher: bool,
        runtime: Runtime,
    ) -> Result<Self, MarketParticipantError> {
        let application = runtime.clone();
        let runtime = MarketParticipantRuntime::from_shared_resources(
            transport,
            Arc::clone(&connection_pool),
            snapshot.clone(),
            runtime,
        )?;
        let registry = Arc::new(RwLock::new(VenueRouteRegistry::new()));
        let pool_config_task = if spawn_pool_watcher {
            let pool = Arc::clone(&connection_pool);
            Some(OwnedTask::new(application.run(async move {
                let _ = pool.watch_config_changes(snapshot).await;
            })))
        } else {
            None
        };
        Ok(Self {
            router: Arc::new(router),
            runtime,
            registry,
            pool_config_task: Arc::new(Mutex::new(pool_config_task)),
        })
    }

    /// Returns the registered descriptor view.
    ///
    /// This is the plumbing the request path reads a route's capabilities and its
    /// adapter from, never the answer to whether a scope may be addressed: that is
    /// the loaded venues behind `Admission`.
    pub(crate) fn registered(&self) -> Arc<RegisteredRoutes> {
        Arc::clone(self.registry.read().routes())
    }

    /// Returns the route registry a venue commit writes.
    ///
    /// The commit holds this lock across the catalog publication, which is what
    /// makes the descriptor view the request path reads and the catalog routes the
    /// directory publishes two halves of one write instead of two publications
    /// that could be observed apart.
    pub(crate) fn registry(&self) -> &Arc<RwLock<VenueRouteRegistry>> {
        &self.registry
    }

    /// Validates one venue's candidate route set without changing anything.
    ///
    /// The stage of a load validates the route set it is about to commit here, so
    /// the descriptors the commit writes are the ones the catalog registrations
    /// were derived from and the commit itself cannot be refused halfway through.
    pub(crate) fn plan_venue_routes(
        &self,
        venue: &market_participants_model::identifier::VenueId,
        descriptors: Vec<VenueRouteDescriptor>,
    ) -> Result<RegisteredRoutes, RegistryError> {
        self.registry.read().plan_venue_routes(venue, descriptors)
    }

    pub(crate) fn take_snapshot_task(&self) -> Option<OwnedTask> {
        self.runtime.take_snapshot_task()
    }

    pub(crate) fn take_pool_config_task(&self) -> Option<OwnedTask> {
        self.pool_config_task.lock().take()
    }

    /// Closes every connection owned by this gateway.
    pub async fn shutdown(&self) {
        let pool_config_task = self.pool_config_task.lock().take();
        if let Some(task) = pool_config_task {
            task.cancel_and_wait().await;
        }
        self.runtime.shutdown().await;
    }

    /// Executes one validated protocol call.
    ///
    /// `admission` names the view the call is admitted against: the loaded venues
    /// for a request path or a route view built for one call that has to
    /// happen before anything is loaded.
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

    /// Executes one protocol call against an explicitly supplied route view.
    ///
    /// A venue conversation that has to happen before the venue is registered —
    /// its own scope reading, and the catalog a load stages — addresses a route
    /// view built for that one call rather than the registry's: the request is
    /// dialed through the venue's configuration, and the view only says
    /// which capability, endpoint and adapter the request is validated against.
    ///
    /// The metadata is the reading that conversation belongs to: a catalog staged
    /// by a load is resolved against the metadata the same load read, and a
    /// discovery carries none because it is itself what produces it.
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

    /// Reads what one venue publishes, through the venue's own configuration.
    ///
    /// This is the one market-participants call a load makes before the venue is registered:
    /// it addresses the venue rather than a route, so it is dialed through the
    /// venue's **primary scope** — the scope a venue that discovers what it deploys
    /// answers its own non-scoped conversations on — and resolved to the venue's
    /// adapter directly, with no route view, no classification and no admission in
    /// between. The request is public, so it takes the public HTTP client of the
    /// venue's own proxy decision.
    pub(crate) async fn read_venue(
        &self,
        venue: &market_participants_model::identifier::VenueId,
    ) -> Result<VenueReading, MarketParticipantError> {
        let snapshot = self.runtime.snapshot.borrow().clone();
        let origins = snapshot
            .venue_settings(venue)
            .and_then(|settings| settings.origins(settings.primary_scope()))
            .ok_or(MarketParticipantError::VenueUnsupported { venue: *venue })?;
        let (proxy, _network_route_generation) = self
            .runtime
            .proxy_resolver
            .resolve_for_venue_with_generation(venue, &origins.http, &self.runtime.runtime)
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
        let context = VenueExecutionContext::new(*venue, origins, http_client);
        adapter.read_venue(context).await
    }

    /// Opens one validated protocol subscription.
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

    /// Validates one request and resolves everything the adapter needs.
    ///
    /// The listing a request names is resolved here and nowhere else: a command,
    /// a narrowed account read and a listing-scoped fetch all hand the adapter the
    /// same kind of reference, and a request that names no listing gets none.
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
        // The listing route the request itself names is checked against the
        // context route here, whether it came from a listing-scoped request or
        // from the payload of a command or a narrowed account read.
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

    /// Resolves the listing one request encodes with, when it is about one.
    ///
    /// The resolution carries the venue's own typed identifiers together with the
    /// status, the availability window and the generation they were published in,
    /// so the rules a command is checked against and the identity its adapter
    /// encodes with come from one read of the directory — a request can never be
    /// signed under a listing whose status was read from another generation. A
    /// request that names no listing — a route-scoped call, an account-wide read —
    /// resolves nothing at all.
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

    /// Validates one command against the listing's published status and window.
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

    /// Builds the execution context of one validated request.
    ///
    /// The venue metadata of the admission travels with the context: it is the
    /// commit's own value, handed over rather than looked up, so an adapter reads
    /// what the load published and no request path reaches the venue for it.
    async fn execution_context(
        &self,
        identity: &RequestIdentity,
        capability: &RouteCapabilityDescriptor,
        classified: &ClassifiedRequest,
        metadata: Option<Arc<VenueMetadata>>,
    ) -> Result<ExecutionContext, MarketParticipantError> {
        let route = identity.route();
        let snapshot = self.runtime.snapshot.borrow().clone();
        // The origins of a request are its venue's connection settings: the scope's
        // own pair where the venue publishes the scope, and the pair the venue
        // reaches a scope it deploys through — a deployed perpetual DEX is reached
        // through the venue that deployed it, never through an endpoint of its own.
        let origins = snapshot
            .origins(&route.venue, &route.scope)
            .ok_or(MarketParticipantError::VenueUnsupported { venue: route.venue })?;
        let (proxy, network_route_generation) = self
            .runtime
            .proxy_resolver
            .resolve_for_venue_with_generation(&route.venue, &origins.http, &self.runtime.runtime)
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
        let context = ExecutionContext::new(
            identity.clone(),
            origins,
            http_client,
            Arc::clone(&self.runtime.connection_pool),
        )
        .with_proxy(proxy, network_route_generation)
        .with_credential_generation(credential_generation)
        .with_venue_metadata(metadata);
        let context = authenticate(&snapshot, context, capability, classified)?;
        Ok(context.with_runtime_receiver(self.runtime.snapshot.clone()))
    }
}

/// Attaches the credentials one request authenticates with.
///
/// A public wallet read and a route-level public stream never resolve a trading
/// credential; an authenticated target resolves the credentials of its **venue**,
/// which is the credential identity of this build. A request that
/// authenticates against a capability the venue does not declare as needing one is
/// refused as the capability mismatch it is — naming the exact route and capability
/// — instead of being reported as a missing API key, which would describe a
/// credential this route never had.
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

/// Resolves the authentication of one authenticated request.
///
/// The credentials are read per scope: a venue that configures its markets apart
/// signs each with its own set, and one that authenticates as a whole signs every
/// scope — a scope it deploys most of all — with the one set it stores.
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

/// Reports a stored credential set that cannot authenticate anything.
///
/// The venue is carried as the typed identity the set belongs to rather than as the
/// configuration error's own sentence: that sentence spells the canonical identity,
/// and interpolating it would name the venue a second time, in English, inside a
/// message whose venue is already the catalogue's name.
///
/// Every set that reaches here is the venue's *own* shape — the resolution behind it
/// validates each venue's row as that venue — so a set of another venue's shape is a
/// configuration this build cannot produce rather than one a user can write. It is still
/// answered as the invalid credential it would be, because the alternative is a panic on a
/// value that came out of a file.
fn authentication_invalid(
    route: &VenueRoute,
    account: &MarketParticipantAccountId,
    error: MarketParticipantsConfigError,
) -> MarketParticipantError {
    let source = match error {
        MarketParticipantsConfigError::IncompleteCredentials { venue, scope } => {
            AuthError::IncompleteCredentials { venue, scope }
        }
        MarketParticipantsConfigError::MismatchedShape { .. } => AuthError::InvalidCredentials,
    };
    authentication_failed(route, account, source)
}

/// Returns the current wall-clock time in Unix milliseconds.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Projects one resolution into the reference an adapter encodes with.
///
/// The reference is built from the one resolution the status and the window of a
/// command were checked against, so the identity an adapter signs with and the
/// rules it was validated under always name the same catalog generation.
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
            credentials::{BinanceCredentialMarket, LongbridgeCredentials},
            fixture::{self, FixtureAddress, FixtureInstallation},
            settings::VenueSettings,
        },
        settings::Config,
    };
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
    use tokio::sync::watch;
    use widget_tokio_runtime::runtime::Runtime;

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
        test_support::fixture_config,
    };

    /// The fixture route: the built-in Binance spot scope.
    fn route() -> VenueRoute {
        test_support::spot_route()
    }

    /// A second built-in route of the same venue, for cross-route checks.
    fn other_route() -> VenueRoute {
        test_support::route("binance", PERPETUAL_SCOPE)
    }

    /// The route of the one venue this build carries an identity for and no adapter for.
    fn longbridge_route() -> VenueRoute {
        VenueRoute::new(
            VenueId::Longbridge,
            VenueScopeId::new(LONGBRIDGE_US_SCOPE).expect("the built-in scope name is canonical"),
        )
    }

    /// The listing every fixture publishes on the fixture route.
    fn listing() -> VenueListingKey {
        test_support::listing_key(&route(), "BTCUSDT")
    }

    /// The same native symbol as a listing of the other fixture route.
    fn other_listing() -> VenueListingKey {
        test_support::listing_key(&other_route(), "BTCUSDT")
    }

    /// The address every fixture venue's published origins resolve at.
    fn fixture_origin() -> url::Url {
        "https://fixture.test"
            .parse()
            .expect("the fixture origin is a URL")
    }

    /// The account the fixture authenticates as.
    fn account(route: &VenueRoute) -> AuthenticatedTarget {
        AuthenticatedTarget::new(
            market_participants_model::identifier::MarketParticipantAccountId::new("primary")
                .expect("the fixture account name is canonical"),
            route.clone(),
        )
    }

    /// A configuration with one complete Binance credential set per market and the
    /// venue's own fixture origins.
    ///
    /// The credentials are written into the row the fixture built, because a venue's
    /// credentials are a field of that venue's own settings rather than a value beside
    /// them. The private key is the Ed25519 seed the adapter parses, so the credential
    /// resolution under test is the real one rather than a stub; the origins are the
    /// fixture's, so an origin a context carries can only have come from the venue's
    /// own settings.
    fn configured(fixture: &FixtureInstallation) -> Config {
        let mut config = fixture_config(fixture);
        let venue = route().venue;
        let Some(VenueSettings::Binance(settings)) = config.venues.get_mut(&venue) else {
            panic!("the fixture venue is configured as Binance");
        };
        for market in [
            BinanceCredentialMarket::Spot,
            BinanceCredentialMarket::UsdMPerp,
        ] {
            settings.credentials.set_market(
                market,
                app_config::market_participants::credentials::BinanceMarketCredentials {
                    api_key: "fixture-api-key",
                    private_key: "0707070707070707070707070707070707070707070707070707070707070707",
                    passphrase: "",
                },
            );
        }
        config
    }

    /// The identifier normalization rules one built-in route declares.
    fn descriptor_rules(route: &VenueRoute) -> Vec<IdentifierNormalizationRule> {
        crate::venue::specs::all_descriptors()
            .iter()
            .find(|descriptor| descriptor.route() == route)
            .expect("the fixture route is a built-in one")
            .identifier_normalization()
            .to_vec()
    }

    /// A gateway whose Binance spot route publishes one fixture listing.
    struct Fixture {
        gateway: MarketParticipantGateway,
        catalog: Arc<Catalog>,
    }

    impl Fixture {
        /// Builds the fixture with one listing at the given status and the
        /// normalization rules the route's own descriptor declares.
        async fn new(status: ListingStatus, rules: Vec<IdentifierNormalizationRule>) -> Self {
            let fixture = fixture::venues([(route().venue, FixtureAddress::at(&fixture_origin()))]);
            let snapshot = Arc::new(MarketParticipantNetworkSnapshot::new(configured(&fixture)));
            let (_, receiver) = watch::channel(Arc::clone(&snapshot));
            let transport = Arc::new(TransportHub::new(TransportPolicy::default()));
            let pool = Arc::new(ConnectionPool::with_transport(Arc::clone(&transport)));
            let router = MarketParticipantGateway::adapter_router(&snapshot)
                .expect("the built-in adapters build from the fixture configuration");
            let gateway = MarketParticipantGateway::with_shared_resources(
                router,
                transport,
                pool,
                receiver,
                false,
                Runtime::for_test(),
            )
            .expect("the fixture gateway builds");
            let catalog = Arc::new(Catalog::new());
            // The fixture loads Binance the way a load does: the venue's whole
            // route set is planned against the registry and then committed.
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
            // The two catalog routes are rebound to a fixture source, so
            // resolution runs through the real connector path while the venue
            // behind it is the fixture. The second route exists so a listing of
            // another route is a listing the directory really holds.
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
            // The fixture loads the two routes the way a load does: after the
            // registration, each catalog route is fetched and published.
            for published in [route(), other_route()] {
                catalog
                    .refresh_for_test(&published)
                    .await
                    .expect("the fixture catalog route publishes its catalog");
            }
            Self { gateway, catalog }
        }

        /// Builds the fixture with the route's own declared rules.
        async fn with_status(status: ListingStatus) -> Self {
            Self::new(status, descriptor_rules(&route())).await
        }

        /// The request context of one capability of the fixture route.
        fn context(&self, capability: CapabilityId) -> RequestIdentity {
            RequestIdentity::new(route(), capability)
        }

        /// Validates one request against the fixture route's capability.
        async fn prepare(
            &self,
            request: ProtocolRequest,
            capability: CapabilityId,
        ) -> Result<PreparedRequest, MarketParticipantError> {
            self.prepare_with(request, self.context(capability)).await
        }

        /// Validates one request against an explicit context.
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

        /// One market-catalog request of the fixture route.
        fn markets(&self, route: &VenueRoute) -> ProtocolRequest {
            ProtocolRequest::FetchMarkets(MarketCatalogQuery {
                route: route.clone(),
            })
        }

        /// One order-book request about one listing.
        fn order_book(&self, listing: &VenueListingKey) -> ProtocolRequest {
            ProtocolRequest::FetchOrderBook(OrderBookQuery {
                listing: listing.clone(),
                depth: 5,
            })
        }

        /// One place-order command about one listing as one account.
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

        /// One active-order read narrowed to one listing.
        fn active_orders_of(&self, listing: &VenueListingKey) -> ProtocolRequest {
            ProtocolRequest::FetchActiveOrders(AuthenticatedQuery::for_listing(
                account(&route()),
                listing.clone(),
            ))
        }

        /// One account-wide active-order read.
        fn active_orders(&self) -> ProtocolRequest {
            ProtocolRequest::FetchActiveOrders(AuthenticatedQuery::account_wide(account(&route())))
        }
    }

    /// A capability that is not the request's own is refused before anything is
    /// resolved, and the error names both capabilities.
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

    /// A request that names another route than its target is refused, and the
    /// error names both routes.
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

    /// A halted listing refuses a new order, and the error carries the listing,
    /// the status and the command.
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

    /// A command the listing's status permits reaches the adapter with the
    /// listing the gateway resolved, and a command naming a listing of another
    /// route is refused before anything is signed.
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

        // The same command addressed to a listing of another route — under this
        // route's own target — must not be sent as if this route published it.
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

    /// A read narrowed to one listing carries that listing to the adapter, and an
    /// account-wide read carries none: the narrowing is never dropped.
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

    /// A request that authenticates against a capability the route does not declare is
    /// refused as that capability, naming the exact route and capability instead of a
    /// missing API key.
    ///
    /// The fixture route is the built-in Binance spot route, which publishes no position
    /// read: the refusal is `prepare`'s answer to a descriptor that lacks the capability,
    /// not the credential lookup's answer to a set that is absent.
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

    /// A Longbridge route resolves its own credential set, and each of the three
    /// states the settings can be in answers its own way.
    ///
    /// The venue authenticates by an app key, an app secret and an access token, so a
    /// complete set becomes the authentication a client can present; an empty one is
    /// the missing API key; and a half-filled one is the incomplete credential the
    /// operator has to finish. The stored set is asserted complete through the
    /// settings' own validation in the first case, so that answer cannot be read as
    /// "nothing was configured".
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

    /// One Longbridge configuration whose three values are `values`.
    fn longbridge_config(values: Option<(&str, &str, &str)>) -> Config {
        let mut config = Config::default();
        let Some(VenueSettings::Longbridge(settings)) = config.venues.get_mut(&VenueId::Longbridge)
        else {
            panic!("the built-in table configures longbridge as Longbridge");
        };
        if let Some((app_key, app_secret, access_token)) = values {
            settings.credentials = LongbridgeCredentials {
                app_key: smol_str::SmolStr::new(app_key),
                app_secret: smol_str::SmolStr::new(app_secret),
                access_token: smol_str::SmolStr::new(access_token),
            };
        }
        config
    }

    /// The adapter registry answers the venue's route with the venue's own adapter.
    ///
    /// A registered adapter is what decides whether a scope is served at all, so the
    /// venue's presence in the router is what every later lookup of its scope rests
    /// on: a router without it would answer the venue's routes as unsupported however
    /// well its loader registered them.
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

    /// Every request variant that is about one listing — each command and each
    /// account read that can be narrowed to a listing — is classified with the
    /// listing it names, whichever target variant it is classified into.
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

    /// A listing-scoped read and a settings read of one published listing reach
    /// the adapter with the listing the gateway resolved.
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

    /// A listing the route never published is reported by the directory instead
    /// of being sent as an empty identity.
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

    /// The execution context of a request names the route and capability it was
    /// validated under, and the origins its venue configuration gives that scope.
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

    /// A route folds its own native symbols with the rule its descriptor
    /// declares: the Binance scope resolves a lowercase spelling end to end, the
    /// OKX scopes declare the same fold, and a HIP-3 coin stays exact.
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

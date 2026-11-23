use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use ahash::AHashMap;
use app_config_provider::notification::ConfigNotification;
use futures::future::BoxFuture;
use market_participants_model::{
    identifier::{VenueId, VenueScopeId},
    listing::route::VenueRoute,
};
use network::transport::hub::TransportHub;
use parking_lot::Mutex;
use smol_str::SmolStr;
use tokio::runtime::Handle;

use crate::{
    account,
    catalog::{
        batch::{CatalogBatch, CatalogPending},
        error::CatalogError,
        registry::staging::{CatalogStagingHandle, StagedRegistry},
        source::{
            CatalogFetchContext, CatalogRouteRegistration, CatalogRouteRequirements,
            CatalogRouteSet, bindings_of,
        },
        store::Catalog,
    },
    client::{MarketParticipantGateway, OwnedTask},
    connection::ConnectionPool,
    error::MarketParticipantError,
    market, routing,
    runtime::MarketParticipantNetworkSnapshot,
    trading,
    venue::{
        config::{LoadCancellation, VenueConfig, VenueConversation},
        definition::{self, ScopeEntry, VenueDefinition},
        loaded::LoadedVenues,
        metadata::{VenueMetadata, VenueReading},
        specs,
    },
};

pub struct MarketParticipantsConnectorInput {
    transport: Arc<TransportHub>,
    snapshot: ConfigNotification<MarketParticipantNetworkSnapshot>,
    runtime: Option<Handle>,
}

impl MarketParticipantsConnectorInput {
    pub fn new(
        transport: Arc<TransportHub>,
        snapshot: ConfigNotification<MarketParticipantNetworkSnapshot>,
    ) -> Self {
        Self {
            transport,
            snapshot,
            runtime: None,
        }
    }

    pub fn with_runtime(mut self, runtime: Handle) -> Self {
        self.runtime = Some(runtime);
        self
    }
}

#[derive(Clone)]
pub struct MarketParticipantsConnector {
    inner: Arc<MarketParticipantsConnectorInner>,
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum MarketParticipantsConnectorError {
    #[error("MarketParticipantsConnector initialization failed: {0}")]
    Initialization(#[source] MarketParticipantError),
    #[error("a Tokio runtime is required for this market-participant operation")]
    RuntimeUnavailable,
    #[error("route descriptor validation failed: {0}")]
    RouteDescriptors(#[source] routing::RegistryError),
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum VenueLoadError {
    #[error("reading what {venue} publishes failed: {source}")]
    Discovery {
        venue: VenueId,
        #[source]
        source: Box<MarketParticipantError>,
    },
    #[error("{venue} publishes the deployment {dex}, which names no scope this build serves")]
    DiscoveryUnverifiable { venue: VenueId, dex: SmolStr },
    #[error("{venue} named {named} deployments, more than the {limit} a load verifies")]
    DiscoveryLimitExceeded {
        venue: VenueId,
        named: usize,
        limit: usize,
    },
    #[error("registering the scopes of {venue} failed: {source}")]
    Registration {
        venue: VenueId,
        #[source]
        source: Box<routing::RegistryError>,
    },
    #[error("reading the metadata of {venue} failed: {source}")]
    Metadata {
        venue: VenueId,
        scope: Option<VenueScopeId>,
        #[source]
        source: Box<MarketParticipantError>,
    },
    #[error("reading the {scope} directory of {venue} failed: {source}")]
    Catalog {
        venue: VenueId,
        scope: VenueScopeId,
        #[source]
        source: Box<MarketParticipantError>,
    },
    #[error("the {scope} directory of {venue} was refused: {error}")]
    CatalogValidation {
        venue: VenueId,
        scope: VenueScopeId,
        error: crate::catalog::error::CatalogValidationError,
    },
    #[error("{scope} of {venue} changed while it was being loaded")]
    RevisionChanged { venue: VenueId, scope: VenueScopeId },
    #[error("loading {venue} was cancelled")]
    Interrupted { venue: VenueId },
    #[error("this build serves no market for venue {venue}")]
    UnsupportedVenue { venue: VenueId },
}

impl VenueLoadError {
    fn from_commit(venue: &VenueId, scope: VenueScopeId, error: CatalogError) -> Self {
        match error {
            CatalogError::RouteRevisionChanged { route, .. } => Self::RevisionChanged {
                venue: *venue,
                scope: route.scope,
            },
            other => Self::Catalog {
                venue: *venue,
                scope,
                source: Box::new(MarketParticipantError::Catalog(Box::new(other))),
            },
        }
    }
}

impl MarketParticipantsConnectorError {
    fn from_internal(error: MarketParticipantError) -> Self {
        if matches!(error, MarketParticipantError::RuntimeUnavailable) {
            Self::RuntimeUnavailable
        } else {
            Self::Initialization(error)
        }
    }
}

pub(crate) struct MarketParticipantsConnectorInner {
    pub gateway: MarketParticipantGateway,
    pub pool: Arc<ConnectionPool>,
    pub catalog: Arc<Catalog>,
    pub venues: LoadedVenues,
    pub loads: Mutex<AHashMap<VenueId, Arc<LoadFlight>>>,
    pub cancellations: Mutex<AHashMap<VenueId, LoadCancellation>>,
    pub config_task: Mutex<Option<OwnedTask>>,
    pub closed: AtomicBool,
}

impl MarketParticipantsConnectorInner {
    pub(crate) fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub(crate) fn catalog_arc(&self) -> Arc<Catalog> {
        Arc::clone(&self.catalog)
    }

    async fn load_venue(
        &self,
        venue: VenueId,
        cancellation: &LoadCancellation,
    ) -> Result<(), VenueLoadError> {
        let definition = definition::definition_of(venue);
        let flight = self.load_flight(&venue);
        let seen = flight.generation.load(Ordering::Acquire);
        let _flight = flight.lock.lock().await;
        if flight.generation.load(Ordering::Acquire) != seen {
            return Ok(());
        }
        let stage = self
            .stage_venue(definition, cancellation)
            .await
            .inspect_err(|error| log::debug!("loading {venue} failed: {error}"))?;
        self.commit_venue(stage, cancellation)?;
        flight.generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    async fn stage_venue(
        &self,
        definition: &'static VenueDefinition,
        cancellation: &LoadCancellation,
    ) -> Result<StagedVenue, VenueLoadError> {
        let venue = definition.venue;
        let conversation = Arc::new(GatewayConversation {
            gateway: self.gateway.clone(),
            venue,
        });
        let config = VenueConfig::new(venue, conversation);
        let data = definition.loader.load(&config, cancellation).await?;
        if cancellation.is_cancelled() {
            return Err(VenueLoadError::Interrupted { venue });
        }
        let descriptors: Vec<routing::VenueRouteDescriptor> = data
            .scopes
            .iter()
            .map(|entry| specs::descriptor_of_entry(venue, entry))
            .collect();
        let route_plan = Arc::new(
            self.gateway
                .plan_venue_routes(&venue, descriptors.clone())
                .map_err(|source| VenueLoadError::Registration {
                    venue,
                    source: Box::new(source),
                })?,
        );
        let registrations = routing::venue_registrations(&route_plan, &venue);
        let requirements = route_plan.requirements();
        let catalog_plan = {
            let sources = crate::catalog::market_participants::market_participant_catalog_source(
                &self.gateway,
                &self.catalog,
            );
            let bindings = bindings_of(&registrations, sources);
            self.catalog
                .plan_venue_commit(&venue, Some(registrations.clone()), &requirements, bindings)
                .map_err(|source| VenueLoadError::Registration {
                    venue,
                    source: Box::new(routing::RegistryError::Catalog(source)),
                })?
        };
        let metadata = Arc::clone(data.metadata());
        let pending = self
            .stage_catalogs(
                &venue,
                &data.scopes,
                &route_plan,
                &catalog_plan,
                Arc::clone(&metadata),
                cancellation,
            )
            .await?;
        Ok(StagedVenue {
            venue,
            descriptors,
            registrations,
            requirements,
            pending,
            scopes: data.scopes,
            metadata,
        })
    }

    async fn stage_catalogs(
        &self,
        venue: &VenueId,
        scopes: &[ScopeEntry],
        route_view: &Arc<crate::routing::RegisteredRoutes>,
        planned: &CatalogRouteSet,
        metadata: Arc<VenueMetadata>,
        cancellation: &LoadCancellation,
    ) -> Result<Vec<CatalogPending>, VenueLoadError> {
        let mut pending = Vec::with_capacity(scopes.len());
        for entry in scopes {
            let Some(source_id) = entry.catalog_source.as_ref() else {
                continue;
            };
            if cancellation.is_cancelled() {
                return Err(VenueLoadError::Interrupted { venue: *venue });
            }
            let route = VenueRoute::new(*venue, entry.scope.clone());
            let registered =
                planned
                    .registered_route(&route)
                    .ok_or_else(|| VenueLoadError::Catalog {
                        venue: *venue,
                        scope: entry.scope.clone(),
                        source: Box::new(MarketParticipantError::Configuration(
                            "a catalog scope registered no catalog route".into(),
                        )),
                    })?;
            let source: Arc<dyn crate::catalog::source::CatalogSource> = Arc::new(
                crate::catalog::market_participants::MarketParticipantCatalogSource::with_routes(
                    self.gateway.clone(),
                    Arc::clone(&self.catalog),
                    Arc::clone(route_view),
                    Arc::clone(&metadata),
                ),
            );
            let base = self.catalog.generation();
            let staging = CatalogStagingHandle::new(StagedRegistry::new(
                Arc::clone(base.assets()),
                Arc::clone(base.references()),
                Arc::clone(base.outcomes()),
            ));
            let context = CatalogFetchContext::new(registered, staging);
            let batch = source
                .fetch_catalog(context.clone())
                .await
                .map_err(|error| VenueLoadError::Catalog {
                    venue: *venue,
                    scope: entry.scope.clone(),
                    source: Box::new(MarketParticipantError::Catalog(Box::new(
                        CatalogError::SourceFailed {
                            id: source_id.to_smol_str(),
                            error: Box::new(error),
                        },
                    ))),
                })?;
            if cancellation.is_cancelled() {
                return Err(VenueLoadError::Interrupted { venue: *venue });
            }
            let staged = CatalogBatch::pending(&context, batch, &base).map_err(|error| {
                VenueLoadError::CatalogValidation {
                    venue: *venue,
                    scope: entry.scope.clone(),
                    error,
                }
            })?;
            pending.push(staged);
        }
        Ok(pending)
    }

    fn commit_venue(
        &self,
        staged: StagedVenue,
        cancellation: &LoadCancellation,
    ) -> Result<(), VenueLoadError> {
        let venue = staged.venue;
        if cancellation.is_cancelled() {
            return Err(VenueLoadError::Interrupted { venue });
        }
        let scope = staged
            .scopes
            .iter()
            .find(|entry| entry.catalog_source.is_some())
            .map(|entry| entry.scope.clone())
            .unwrap_or_else(|| {
                VenueScopeId::new(crate::routing::SPOT_SCOPE)
                    .expect("the built-in scope name is canonical")
            });
        let sources = crate::catalog::market_participants::market_participant_catalog_source(
            &self.gateway,
            &self.catalog,
        );
        let bindings = bindings_of(&staged.registrations, sources);
        let scopes: Vec<Arc<ScopeEntry>> = staged.scopes.iter().cloned().map(Arc::new).collect();
        let mut venues = self.venues.lock_for_commit();
        let mut registry = self.gateway.registry().write();
        self.catalog
            .commit_venue_with_visibility(
                &venue,
                Some(staged.registrations),
                &staged.requirements,
                staged.pending,
                bindings,
                |generation| {
                    registry.commit_venue_routes(&venue, staged.descriptors);
                    venues.replace(venue, scopes, generation.id(), Arc::clone(&staged.metadata));
                },
            )
            .map_err(|error| VenueLoadError::from_commit(&venue, scope, error))?;
        drop(registry);
        drop(venues);
        Ok(())
    }

    fn load_flight(&self, venue: &VenueId) -> Arc<LoadFlight> {
        let mut loads = self.loads.lock();
        Arc::clone(
            loads
                .entry(*venue)
                .or_insert_with(|| Arc::new(LoadFlight::default())),
        )
    }

    fn cancellation_of(&self, venue: &VenueId) -> LoadCancellation {
        let mut cancellations = self.cancellations.lock();
        cancellations.entry(*venue).or_default().clone()
    }

    fn finish_load(&self, venue: &VenueId) {
        self.cancellations.lock().remove(venue);
    }

    #[cfg(any(test, feature = "test-support"))]
    fn load_fixture_venue(
        &self,
        venue: &VenueId,
        entries: Vec<ScopeEntry>,
        catalogs: &[(VenueRoute, Vec<crate::catalog::batch::CatalogEntry>)],
    ) -> Result<(), MarketParticipantsConnectorError> {
        let sources = crate::catalog::market_participants::market_participant_catalog_source(
            &self.gateway,
            &self.catalog,
        );
        let descriptors: Vec<routing::VenueRouteDescriptor> = entries
            .iter()
            .map(|entry| specs::descriptor_of_entry(*venue, entry))
            .collect();
        let planned_routes = self
            .gateway
            .plan_venue_routes(venue, descriptors.clone())
            .map_err(MarketParticipantsConnectorError::RouteDescriptors)?;
        let registrations = routing::venue_registrations(&planned_routes, venue);
        let requirements = planned_routes.requirements();
        let bindings = bindings_of(&registrations, sources);
        let planned = self
            .catalog
            .plan_venue_commit(venue, Some(registrations.clone()), &requirements, bindings)
            .map_err(|error| {
                MarketParticipantsConnectorError::RouteDescriptors(routing::RegistryError::Catalog(
                    error,
                ))
            })?;
        let mut pending = Vec::with_capacity(entries.len());
        for entry in &entries {
            let Some(_) = entry.catalog_source.as_ref() else {
                continue;
            };
            let route = VenueRoute::new(*venue, entry.scope.clone());
            let registered = planned.registered_route(&route).ok_or_else(|| {
                MarketParticipantsConnectorError::RouteDescriptors(routing::RegistryError::Catalog(
                    CatalogError::RouteNotRegistered {
                        route: route.clone(),
                    },
                ))
            })?;
            let listings = catalogs
                .iter()
                .find(|(known, _)| *known == route)
                .map(|(_, listings)| listings.clone())
                .unwrap_or_default();
            pending.push(
                self.catalog
                    .stage_fixture_listings(&registered, listings)
                    .map_err(|error| {
                        MarketParticipantsConnectorError::RouteDescriptors(
                            routing::RegistryError::Catalog(error),
                        )
                    })?,
            );
        }
        let scopes: Vec<Arc<ScopeEntry>> = entries.into_iter().map(Arc::new).collect();
        let metadata = Arc::new(VenueMetadata::empty());
        let mut venues = self.venues.lock_for_commit();
        let mut registry = self.gateway.registry().write();
        let sources = crate::catalog::market_participants::market_participant_catalog_source(
            &self.gateway,
            &self.catalog,
        );
        let bindings = bindings_of(&registrations, sources);
        self.catalog
            .commit_venue_with_visibility(
                venue,
                Some(registrations),
                &requirements,
                pending,
                bindings,
                |generation| {
                    registry.commit_venue_routes(venue, descriptors);
                    venues.replace(*venue, scopes, generation.id(), Arc::clone(&metadata));
                },
            )
            .map_err(|error| {
                MarketParticipantsConnectorError::RouteDescriptors(routing::RegistryError::Catalog(
                    error,
                ))
            })?;
        drop(registry);
        drop(venues);
        if let Some(entry) = self.venues.loaded(venue) {
            log::debug!(
                "fixture committed {venue} at revision {} generation {} with {} deployments",
                entry.revision(),
                entry.generation(),
                entry.metadata().perp_dexes().len()
            );
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct LoadFlight {
    lock: tokio::sync::Mutex<()>,
    generation: std::sync::atomic::AtomicU64,
}

struct StagedVenue {
    venue: VenueId,
    descriptors: Vec<routing::VenueRouteDescriptor>,
    registrations: Vec<CatalogRouteRegistration>,
    requirements: CatalogRouteRequirements,
    pending: Vec<CatalogPending>,
    scopes: Vec<ScopeEntry>,
    metadata: Arc<VenueMetadata>,
}

struct GatewayConversation {
    gateway: MarketParticipantGateway,
    venue: VenueId,
}

impl VenueConversation for GatewayConversation {
    fn read_venue(&self) -> BoxFuture<'static, Result<VenueReading, MarketParticipantError>> {
        let gateway = self.gateway.clone();
        let venue = self.venue;
        Box::pin(async move { gateway.read_venue(&venue).await })
    }
}

impl MarketParticipantsConnector {
    pub fn new(
        input: MarketParticipantsConnectorInput,
    ) -> Result<Self, MarketParticipantsConnectorError> {
        Self::build(input, Arc::new(Catalog::new()))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn new_with_catalog(
        input: MarketParticipantsConnectorInput,
        catalog: Arc<Catalog>,
    ) -> Result<Self, MarketParticipantsConnectorError> {
        Self::build(input, catalog)
    }

    fn build(
        input: MarketParticipantsConnectorInput,
        catalog: Arc<Catalog>,
    ) -> Result<Self, MarketParticipantsConnectorError> {
        let mut input = input;
        let transport = Arc::clone(&input.transport);
        let snapshot = input.snapshot.clone();
        let runtime = input
            .runtime
            .take()
            .ok_or(MarketParticipantsConnectorError::RuntimeUnavailable)?;
        drop(input);
        catalog.set_runtime(runtime.clone());
        let initial_snapshot = snapshot.current();
        let pool = Arc::new(ConnectionPool::with_transport(Arc::clone(&transport)));
        let config_notification = snapshot.clone();
        let task_runtime = runtime.clone();
        let router = MarketParticipantGateway::adapter_router(&initial_snapshot)
            .map_err(MarketParticipantsConnectorError::from_internal)?;
        let gateway = MarketParticipantGateway::with_shared_resources(
            router,
            transport,
            Arc::clone(&pool),
            snapshot,
            runtime,
        )
        .map_err(MarketParticipantsConnectorError::from_internal)?;
        let invalidating_pool = Arc::clone(&pool);
        let config_task = task_runtime.spawn(async move {
            loop {
                let next = config_notification.wait().await;
                invalidating_pool.invalidate_stale(&next).await;
            }
        });
        let venues = LoadedVenues::new();
        let inner = Arc::new(MarketParticipantsConnectorInner {
            gateway,
            pool,
            catalog,
            venues,
            loads: Mutex::new(AHashMap::new()),
            cancellations: Mutex::new(AHashMap::new()),
            config_task: Mutex::new(Some(OwnedTask::new(config_task))),
            closed: AtomicBool::new(false),
        });
        Ok(Self { inner })
    }

    pub fn account(&self) -> account::provider::AccountCapability {
        account::provider::AccountCapability::new(Arc::clone(&self.inner))
    }

    pub fn market(&self) -> market::provider::MarketCapability {
        market::provider::MarketCapability::new(Arc::clone(&self.inner))
    }

    pub fn trading(&self) -> trading::provider::TradingCapability {
        trading::provider::TradingCapability::new(Arc::clone(&self.inner))
    }

    pub fn supported_venues(&self) -> Vec<VenueId> {
        definition::supported_venues()
    }

    pub fn load_needs_credentials(&self, venue: VenueId) -> bool {
        let definition = definition::definition_of(venue);
        definition.requires_credentials_to_load
            && !self.runtime_snapshot().credentials_configured(&venue)
    }

    fn runtime_snapshot(&self) -> Arc<MarketParticipantNetworkSnapshot> {
        self.inner.gateway.runtime.snapshot.current()
    }

    pub fn venues(&self) -> &LoadedVenues {
        &self.inner.venues
    }

    pub fn catalog(&self) -> &Catalog {
        &self.inner.catalog
    }

    pub fn catalog_arc(&self) -> Arc<Catalog> {
        self.inner.catalog_arc()
    }

    pub fn load_venue(
        &self,
        venue: VenueId,
    ) -> impl Future<Output = Result<(), VenueLoadError>> + Send + 'static {
        let inner = Arc::clone(&self.inner);
        let cancellation = self.inner.cancellation_of(&venue);
        async move {
            let result = inner.load_venue(venue, &cancellation).await;
            inner.finish_load(&venue);
            result
        }
    }

    pub fn cancel_venue_load(&self, venue: &VenueId) {
        if let Some(cancellation) = self.inner.cancellations.lock().get(venue) {
            cancellation.cancel();
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn load_fixture(
        &self,
        venues: &[VenueDefinition],
        scopes: &[(VenueId, Vec<ScopeEntry>)],
        catalogs: &[(VenueRoute, Vec<crate::catalog::batch::CatalogEntry>)],
    ) -> Result<(), MarketParticipantsConnectorError> {
        let mut stated: Vec<(VenueId, Vec<ScopeEntry>)> = venues
            .iter()
            .map(|definition| (definition.venue, specs::scope_entries(definition.venue)))
            .collect();
        for (venue, entries) in scopes {
            match stated.iter_mut().find(|(known, _)| known == venue) {
                Some((_, known)) => *known = entries.clone(),
                None => stated.push((*venue, entries.clone())),
            }
        }
        for (venue, entries) in stated {
            self.inner.load_fixture_venue(&venue, entries, catalogs)?;
        }
        Ok(())
    }

    pub async fn shutdown(&self) {
        if self.inner.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let config_task = self.inner.config_task.lock().take();
        if let Some(task) = config_task {
            task.cancel_and_wait().await;
        }
        self.inner.gateway.shutdown().await;
        self.inner.pool.close_all().await;
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use app_config::{
        market_participants::{
            credentials::LongbridgeCredentials,
            origins::{BinanceHttpOrigin, BinanceMarket, BinanceWebsocketOrigin, OkxRegion},
            settings::{BinanceOrigins, VenueSettings},
        },
        network::ResolvedProxy,
        settings::Config,
    };
    use app_config_provider::notification::ConfigNotification;
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
        routing::CapabilityId,
    };
    use network::transport::hub::{TransportHub, TransportPolicy};
    use smol_str::SmolStr;
    use tokio::{runtime::Handle, time::timeout};

    use super::{MarketParticipantsConnector, MarketParticipantsConnectorInput};
    use crate::{
        connection::{
            Connection, ConnectionKey, ConnectionState, no_response_id,
            physical::ConnectionProtocol,
        },
        runtime::MarketParticipantNetworkSnapshot,
        test_support::test_handle,
    };

    fn binance() -> VenueId {
        VenueId::Binance
    }

    fn spot() -> VenueRoute {
        VenueRoute::new(
            binance(),
            VenueScopeId::new("spot").expect("the fixture scope is canonical"),
        )
    }

    fn root() -> MarketParticipantsConnector {
        let snapshot = Arc::new(MarketParticipantNetworkSnapshot::default());
        let notification = ConfigNotification::new(snapshot);
        MarketParticipantsConnector::new(
            MarketParticipantsConnectorInput::new(
                Arc::new(TransportHub::new(TransportPolicy::default())),
                notification,
            )
            .with_runtime(test_handle()),
        )
        .expect("the root builds from typed runtime inputs")
    }

    #[tokio::test]
    async fn connecting_a_venue_publishes_its_scopes_and_catalog_bindings() {
        let connector = root();

        assert!(
            connector.venues().scopes().is_empty(),
            "a root with the wiring attached holds no venue"
        );
        assert_eq!(
            connector.supported_venues().len(),
            5,
            "the static table lists the venues this build serves"
        );

        connector
            .load_fixture(&crate::test_support::definitions(&[binance()]), &[], &[])
            .expect("the Binance routes register");

        assert!(
            connector.venues().scope(&spot()).is_some(),
            "the venue catalog scope is loaded"
        );
        assert_eq!(
            connector
                .venues()
                .scopes()
                .iter()
                .filter(|route| route.venue == binance())
                .count(),
            2,
            "only the scopes of the venue that was loaded appear"
        );
        let catalog = connector.catalog();
        assert!(
            catalog.source_of(&spot()).is_some(),
            "and its catalog source binding is published with it"
        );
        assert!(
            connector
                .market()
                .limits(&spot(), CapabilityId::MarketCatalog)
                .is_ok(),
            "every capability of a loaded scope answers its own bounds"
        );

        connector.shutdown().await;
    }

    #[tokio::test]
    async fn connecting_a_venue_does_not_touch_another() {
        let connector = root();
        connector
            .load_fixture(&crate::test_support::definitions(&[binance()]), &[], &[])
            .expect("the Binance routes register");
        let okx = VenueId::Okx;

        connector
            .load_fixture(
                &crate::test_support::definitions(std::slice::from_ref(&okx)),
                &[],
                &[],
            )
            .expect("the OKX routes register");

        assert!(connector.venues().scope(&spot()).is_some());

        let catalog = connector.catalog();
        assert!(catalog.source_of(&spot()).is_some());
        assert!(
            catalog
                .source_of(&VenueRoute::new(
                    okx,
                    VenueScopeId::new("swap").expect("the fixture scope is canonical")
                ))
                .is_some()
        );
        connector.shutdown().await;
    }

    #[tokio::test]
    async fn a_venue_whose_reads_sign_needs_its_credentials_to_load() {
        let unconfigured = root();
        for venue in [VenueId::Binance, VenueId::Okx, VenueId::Hyperliquid] {
            assert!(
                !unconfigured.load_needs_credentials(venue),
                "{venue} is read without a credential, so its load is offered as it is"
            );
        }
        assert!(
            unconfigured.load_needs_credentials(VenueId::Longbridge),
            "the venue's whole market signs, and nothing is stored"
        );

        let mut config = Config::default();
        let Some(VenueSettings::Longbridge(settings)) = config.venues.get_mut(&VenueId::Longbridge)
        else {
            panic!("the built-in table configures longbridge as Longbridge");
        };
        settings.credentials = Some(LongbridgeCredentials {
            app_key: SmolStr::new_static("app-key"),
            app_secret: SmolStr::new_static("app-secret"),
            access_token: SmolStr::new_static("access-token"),
        });
        let snapshot = Arc::new(MarketParticipantNetworkSnapshot::new(config));
        let notification = ConfigNotification::new(snapshot);
        let configured = MarketParticipantsConnector::new(
            MarketParticipantsConnectorInput::new(
                Arc::new(TransportHub::new(TransportPolicy::default())),
                notification,
            )
            .with_runtime(test_handle()),
        )
        .expect("the root builds from typed runtime inputs");

        assert!(
            !configured.load_needs_credentials(VenueId::Longbridge),
            "a complete set is what makes the load worth offering"
        );

        unconfigured.shutdown().await;
        configured.shutdown().await;
    }

    const INVALIDATION_BOUND: Duration = Duration::from_secs(5);

    fn fixture_key(
        snapshot: &MarketParticipantNetworkSnapshot,
        route: &VenueRoute,
        protocol: &ConnectionProtocol,
    ) -> ConnectionKey {
        let origins = snapshot
            .origins(&route.venue, &route.scope)
            .expect("every venue this build configures resolves its own scopes");
        ConnectionKey::for_route(
            origins.websocket,
            ResolvedProxy::NoProxy,
            None,
            route.clone(),
            snapshot.network_route_generation(&route.venue),
            protocol,
        )
    }

    fn wait_for_close(runtime: &Handle, connection: &Connection, subject: &str) {
        let closed = runtime.block_on(async {
            timeout(INVALIDATION_BOUND, async {
                while !connection.is_closed() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
        });
        assert!(
            closed.is_ok(),
            "{subject} stayed open after the configuration that no longer serves it was published"
        );
    }

    #[test]
    fn a_moved_projection_closes_only_the_connections_it_no_longer_serves() {
        let runtime = test_handle();
        let mut config = Config::default();
        let notification = ConfigNotification::new(Arc::new(
            MarketParticipantNetworkSnapshot::new(config.clone()),
        ));
        let connector = MarketParticipantsConnector::new(
            MarketParticipantsConnectorInput::new(
                Arc::new(TransportHub::new(TransportPolicy::default())),
                notification.clone(),
            )
            .with_runtime(runtime.clone()),
        )
        .expect("the root builds from typed runtime inputs");
        let snapshot = connector.runtime_snapshot();
        let binance_venue = binance();
        let binance_spot = spot();
        let okx_venue = VenueId::Okx;
        let okx_spot = VenueRoute::new(
            okx_venue,
            VenueScopeId::new("spot").expect("the fixture scope is canonical"),
        );
        let protocol = ConnectionProtocol::new(no_response_id);
        let pool = Arc::clone(&connector.inner.pool);
        let (binance_connection, okx_connection) = runtime.block_on(async {
            let binance_connection = pool
                .get_or_create_fixture(
                    fixture_key(&snapshot, &binance_spot, &protocol),
                    ConnectionState::stateless(),
                )
                .await;
            let okx_connection = pool
                .get_or_create_fixture(
                    fixture_key(&snapshot, &okx_spot, &protocol),
                    ConnectionState::stateless(),
                )
                .await;
            (binance_connection, okx_connection)
        });
        assert!(
            !binance_connection.is_closed() && !okx_connection.is_closed(),
            "both connections are pooled before anything is published"
        );

        config.set_okx_region(&okx_venue, OkxRegion::Europe);
        notification.publish(Arc::new(MarketParticipantNetworkSnapshot::from_previous(
            config.clone(),
            Some(&snapshot),
        )));
        wait_for_close(
            &runtime,
            &okx_connection,
            "the OKX connection of the moved region",
        );
        assert!(
            !binance_connection.is_closed(),
            "a configuration write that did not name Binance kept its connection open"
        );

        config.set_binance_market_origins(
            &binance_venue,
            BinanceMarket::Spot,
            &BinanceOrigins::new(
                BinanceMarket::Spot,
                BinanceHttpOrigin::Api,
                BinanceWebsocketOrigin::StreamAlt,
            ),
        );
        notification.publish(Arc::new(MarketParticipantNetworkSnapshot::from_previous(
            config,
            Some(&snapshot),
        )));
        wait_for_close(
            &runtime,
            &binance_connection,
            "the Binance connection of the moved stream origin",
        );

        runtime.block_on(connector.shutdown());
    }
}
#[cfg(test)]
mod venue_connection_tests {

    use std::{sync::Arc, time::Duration};

    use app_config::market_participants::fixture::{self, FixtureAddress};
    use app_config_provider::notification::ConfigNotification;
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
    };
    use network::transport::hub::TransportPolicy;
    use tokio::time::timeout;

    use crate::{
        connector::{MarketParticipantsConnector, VenueLoadError},
        runtime::MarketParticipantNetworkSnapshot,
        test_support::fixture_config,
    };

    const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);

    fn binance() -> VenueId {
        VenueId::Binance
    }

    fn hyperliquid() -> VenueId {
        VenueId::Hyperliquid
    }

    fn deployed_route(name: &str) -> VenueRoute {
        VenueRoute::new(
            hyperliquid(),
            VenueScopeId::new(smol_str::format_smolstr!("perp-dex/{name}"))
                .expect("a deployed scope name is a canonical identifier"),
        )
    }

    fn connector() -> MarketParticipantsConnector {
        let notification =
            ConfigNotification::new(Arc::new(MarketParticipantNetworkSnapshot::default()));
        crate::test_support::connector_over(notification, TransportPolicy::default()).0
    }

    fn discovery_connector(origin: &str, policy: TransportPolicy) -> MarketParticipantsConnector {
        let origin: url::Url = origin.parse().expect("the fixture origin parses");
        let fixture = fixture::venues([(hyperliquid(), FixtureAddress::at(&origin))]);
        let notification = ConfigNotification::new(Arc::new(
            MarketParticipantNetworkSnapshot::new(fixture_config(&fixture)),
        ));
        crate::test_support::connector_over(notification, policy).0
    }

    #[tokio::test]
    async fn a_registered_venue_binds_its_catalog_sources_in_one_commit() {
        let connector = connector();

        connector
            .load_fixture(
                &crate::test_support::definitions(&[hyperliquid()]),
                &[],
                &[],
            )
            .expect("the declared Hyperliquid routes register");

        let venues = connector.venues();
        let hyperliquid_spot = VenueRoute::new(
            hyperliquid(),
            VenueScopeId::new("core-spot").expect("the built-in scope is canonical"),
        );
        let source_id = connector
            .catalog()
            .source_of(&hyperliquid_spot)
            .expect("a catalog route binds a source")
            .id()
            .clone();
        assert_eq!(
            venues
                .loaded(&hyperliquid())
                .expect("the venue is loaded")
                .scopes()
                .len(),
            2,
            "the venue's declared scopes are the two core ones"
        );
        let catalog = connector.catalog();
        let route = hyperliquid_spot.clone();
        assert!(
            catalog.source_of(&route).is_some(),
            "the registered scope's catalog source is bound"
        );
        assert_eq!(
            catalog
                .registered_route(&route)
                .expect("the scope is a registered catalog route")
                .catalog_source(),
            &source_id
        );
        connector.shutdown().await;
    }

    #[tokio::test]
    async fn a_venue_registration_does_not_touch_another_venue() {
        let connector = connector();

        connector
            .load_fixture(&crate::test_support::definitions(&[binance()]), &[], &[])
            .expect("the Binance routes register");
        connector
            .load_fixture(
                &crate::test_support::definitions(&[hyperliquid()]),
                &[],
                &[],
            )
            .expect("the Hyperliquid routes register");

        let venues = connector.venues();
        let binance_spot =
            VenueRoute::new(binance(), VenueScopeId::new("spot").expect("canonical"));
        let hyperliquid_spot = VenueRoute::new(
            hyperliquid(),
            VenueScopeId::new("core-spot").expect("canonical"),
        );
        assert!(venues.scope(&binance_spot).is_some());
        assert!(venues.scope(&hyperliquid_spot).is_some());
        assert!(
            venues.scope(&deployed_route("xyz")).is_none(),
            "a scope the venue deploys is registered by a load, not by the static table"
        );
        let catalog = connector.catalog();
        assert!(catalog.source_of(&binance_spot).is_some());
        assert!(catalog.source_of(&hyperliquid_spot).is_some());
        connector.shutdown().await;
    }

    #[tokio::test]
    async fn an_empty_registry_holds_no_route() {
        let connector = connector();

        assert!(
            connector.venues().scope(&deployed_route("xyz")).is_none(),
            "an empty registry holds no dynamically deployed scope"
        );
        let catalog = connector.catalog();
        assert!(catalog.source_of(&deployed_route("xyz")).is_none());
        connector.shutdown().await;
    }

    #[tokio::test]
    async fn a_first_connection_that_cannot_discover_revokes_the_venue() {
        let connector = discovery_connector("https://127.0.0.1:1", TransportPolicy::default());

        let error = timeout(DISCOVERY_TIMEOUT, connector.load_venue(hyperliquid()))
            .await
            .expect("the discovery request should not time out")
            .expect_err("nothing answers the venue's discovery endpoint in a test");

        let VenueLoadError::Discovery { source, .. } = error else {
            panic!("discovery reached the venue but was refused before the wire: {error:?}");
        };
        assert!(
            matches!(
                *source,
                crate::error::MarketParticipantError::VenueRequestFailed { .. }
            ),
            "{source:?}"
        );
        let venues = connector.venues();
        assert!(
            venues.entries().is_empty(),
            "a failed first load leaves nothing of the venue loaded"
        );
        assert!(
            venues
                .scope(&VenueRoute::new(
                    hyperliquid(),
                    VenueScopeId::new("core-spot").expect("canonical")
                ))
                .is_none(),
            "and every declared route of that venue with it"
        );
        let catalog = connector.catalog();
        assert!(
            catalog
                .source_of(&VenueRoute::new(
                    hyperliquid(),
                    VenueScopeId::new("core-spot").expect("canonical")
                ))
                .is_none(),
            "the catalog source binding is revoked in the same commit"
        );
        connector.shutdown().await;
    }
}

#[cfg(test)]
mod venue_load_tests {

    use std::{sync::Arc, time::Duration};

    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
    };
    use smol_str::SmolStr;
    use tokio::time::timeout;

    use crate::{
        adapter::hyperliquid::adapter::implementation::MAX_DEPLOYED_PERP_DEXES,
        catalog::{batch::CatalogEntry, store::Catalog},
        connector::{MarketParticipantsConnector, VenueLoadError},
        routing::HYPERLIQUID_VENUE,
        test_support::{
            CatalogFixture, CatalogRoute, binance_spot_market_participant_info, dex_meta,
            info_fixture_server, spot_meta_answer,
        },
        venue::test_support::{fixture_connector, load},
    };

    const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

    fn venue(name: &str) -> VenueId {
        VenueId::from_name(name).expect("the built-in venue identity is canonical")
    }

    fn route(venue: &VenueId, scope: &str) -> VenueRoute {
        VenueRoute::new(
            *venue,
            VenueScopeId::new(scope).expect("the built-in scope is canonical"),
        )
    }

    fn connector_with(fixture: &CatalogFixture) -> (MarketParticipantsConnector, Arc<Catalog>) {
        fixture_connector(fixture.origin(), fixture.origin())
    }

    fn registered(connector: &MarketParticipantsConnector) -> Vec<(SmolStr, SmolStr)> {
        let mut routes: Vec<(SmolStr, SmolStr)> = connector
            .venues()
            .scopes()
            .iter()
            .map(|route| {
                (
                    SmolStr::new(route.venue.as_str()),
                    SmolStr::new(route.scope.as_str()),
                )
            })
            .collect();
        routes.sort();
        routes
    }

    fn symbols_of(catalog: &Catalog, route: &VenueRoute) -> Vec<SmolStr> {
        catalog
            .cached_listings(route)
            .expect("the route should hold a snapshot")
            .iter()
            .filter_map(|entry| {
                entry
                    .listing
                    .native_symbol
                    .as_ref()
                    .map(|symbol| SmolStr::new(symbol.as_str()))
            })
            .collect()
    }

    fn catalogs_of(
        catalog: &Catalog,
        routes: &[VenueRoute],
    ) -> Vec<(VenueRoute, Arc<[CatalogEntry]>)> {
        routes
            .iter()
            .map(|route| {
                let entries = catalog
                    .cached_listings(route)
                    .expect("a loaded venue holds a snapshot for every catalog route");
                (route.clone(), entries)
            })
            .collect()
    }

    #[tokio::test]
    async fn a_refresh_replaces_the_catalogs_of_a_connected_venue() {
        let fixture = CatalogFixture::start().await;
        let (connector, catalog) = connector_with(&fixture);
        let binance = venue("binance");
        let spot = route(&binance, "spot");
        let perpetual = route(&binance, "usd-m-perp");

        load(&connector, &binance)
            .await
            .expect("the first load commits the venue");
        assert_eq!(
            symbols_of(&catalog, &spot),
            [SmolStr::new_static("BTCUSDT")],
            "the venue listed one pair when it was loaded"
        );
        assert_eq!(
            symbols_of(&catalog, &perpetual),
            [SmolStr::new_static("BTCUSDT")]
        );
        let routes_before = registered(&connector);

        fixture.serve(
            CatalogRoute::BinanceSpot,
            &binance_spot_market_participant_info(&["BTCUSDT", "ETHUSDT"]),
        );
        load(&connector, &binance)
            .await
            .expect("the refresh loads the venue again");

        assert_eq!(
            symbols_of(&catalog, &spot),
            [
                SmolStr::new_static("BTCUSDT"),
                SmolStr::new_static("ETHUSDT")
            ],
            "the refreshed snapshot is the one the venue answered this time"
        );
        assert_eq!(
            symbols_of(&catalog, &perpetual),
            [SmolStr::new_static("BTCUSDT")],
            "and every other catalog route of the venue is fetched again with it"
        );
        assert_eq!(
            registered(&connector),
            routes_before,
            "a refresh registers nothing and revokes nothing"
        );
        assert_eq!(
            fixture
                .served()
                .iter()
                .filter(|route| **route == CatalogRoute::BinanceSpot)
                .count(),
            2,
            "the refresh asked the venue for the catalog itself, not for the cache"
        );

        connector.shutdown().await;
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_the_registered_venue_and_its_snapshots() {
        let fixture = CatalogFixture::start().await;
        let (connector, catalog) = connector_with(&fixture);
        let binance = venue("binance");
        let spot = route(&binance, "spot");
        let perpetual = route(&binance, "usd-m-perp");

        load(&connector, &binance)
            .await
            .expect("the first load commits the venue");
        let routes_before = registered(&connector);
        let catalogs_before = catalogs_of(&catalog, &[spot.clone(), perpetual.clone()]);
        let symbols_before: Vec<Vec<SmolStr>> = catalogs_before
            .iter()
            .map(|(route, _)| symbols_of(&catalog, route))
            .collect();

        fixture.refuse(CatalogRoute::BinanceSpot);
        let error = load(&connector, &binance)
            .await
            .expect_err("a refused catalog fails the refresh");

        let VenueLoadError::Catalog {
            venue: failed_venue,
            scope: failed_scope,
            ..
        } = error
        else {
            panic!("the venue answered and refused one of its catalogs: {error:?}");
        };
        assert_eq!(
            failed_venue, binance,
            "the error names the venue it refreshed"
        );
        assert_eq!(
            failed_scope.as_str(),
            "spot",
            "and the scope the venue refused"
        );

        assert_eq!(
            registered(&connector),
            routes_before,
            "the failed refresh leaves the venue registered with exactly the routes it had"
        );
        assert!(
            catalog.source_of(&spot).is_some(),
            "and its catalog source binding survives the failure"
        );
        for ((route, entries), symbols) in catalogs_before.iter().zip(&symbols_before) {
            let now = catalog
                .cached_listings(route)
                .expect("a failed refresh keeps the snapshot the route already had");
            assert!(
                Arc::ptr_eq(&now, entries),
                "the failed refresh replaced the snapshot of {route}"
            );
            assert_eq!(
                &symbols_of(&catalog, route),
                symbols,
                "the catalog of {route} still holds the listings it held"
            );
        }

        connector.shutdown().await;
    }

    #[tokio::test]
    async fn two_venues_loaded_concurrently_keep_their_own_routes_and_catalogs() {
        let fixture = CatalogFixture::start().await;
        fixture.stall(CatalogRoute::OkxSwap);
        let (connector, catalog) = connector_with(&fixture);
        let binance = venue("binance");
        let okx = venue("okx");
        let binance_spot = route(&binance, "spot");
        let okx_swap = route(&okx, "swap");

        let binance_load = tokio::spawn(connector.clone().load_venue(binance));
        let okx_load = tokio::spawn(connector.clone().load_venue(okx));
        timeout(REQUEST_TIMEOUT, fixture.await_stall())
            .await
            .expect("the held catalog route is asked for by the venue that owns it");
        binance_load
            .await
            .expect("the Binance load does not panic")
            .expect("Binance connects");

        assert_eq!(
            registered(&connector),
            [
                (SmolStr::new_static("binance"), SmolStr::new_static("spot")),
                (
                    SmolStr::new_static("binance"),
                    SmolStr::new_static("usd-m-perp")
                ),
            ],
            "a venue mid-load has published nothing"
        );

        fixture.release();
        okx_load
            .await
            .expect("the OKX load does not panic")
            .expect("OKX connects");

        assert_eq!(
            registered(&connector),
            [
                (SmolStr::new_static("binance"), SmolStr::new_static("spot")),
                (
                    SmolStr::new_static("binance"),
                    SmolStr::new_static("usd-m-perp")
                ),
                (SmolStr::new_static("okx"), SmolStr::new_static("spot")),
                (SmolStr::new_static("okx"), SmolStr::new_static("swap")),
            ],
            "the venue that committed last left the other venue's routes alone"
        );
        for (route, expected) in [
            (&binance_spot, vec![SmolStr::new_static("BTCUSDT")]),
            (
                &route(&binance, "usd-m-perp"),
                vec![SmolStr::new_static("BTCUSDT")],
            ),
            (&route(&okx, "spot"), vec![SmolStr::new_static("BTC-USDT")]),
            (&okx_swap, vec![SmolStr::new_static("BTC-USDT-SWAP")]),
        ] {
            assert_eq!(
                symbols_of(&catalog, route),
                expected,
                "the catalog of {route} is the one its own venue published"
            );
            assert!(
                catalog.source_of(route).is_some(),
                "the catalog source of {route} is bound"
            );
        }

        connector.shutdown().await;
    }

    #[tokio::test]
    async fn a_refresh_that_discovers_fewer_scopes_revokes_the_scope_that_disappeared() {
        let both = r#"[null,{"name":"xyz"},{"name":"abc"}]"#.to_owned();
        let one = r#"[null,{"name":"xyz"}]"#.to_owned();
        let (origin, requests, server) = info_fixture_server(
            vec![both, one.clone(), one],
            spot_meta_answer(&["USD", "USDC", "BTC"]),
            dex_meta(&["BTC"]),
            vec![
                ("xyz".to_owned(), dex_meta(&["xyz:ABC"])),
                ("abc".to_owned(), dex_meta(&["abc:DEF"])),
            ],
        )
        .await;
        let (connector, catalog) = fixture_connector(&origin, &origin);
        let hyperliquid = venue(HYPERLIQUID_VENUE);
        let deployed = route(&hyperliquid, "perp-dex/xyz");
        let vanished = route(&hyperliquid, "perp-dex/abc");

        load(&connector, &hyperliquid)
            .await
            .expect("the first load discovers and commits both deployed scopes");
        assert_eq!(
            requests
                .lock()
                .iter()
                .filter(|kind| kind.as_str() == "perpDexs")
                .count(),
            1,
            "one load is one reading of the venue's own deployment table"
        );
        for scope in [&deployed, &vanished] {
            assert!(
                connector.venues().scope(scope).is_some(),
                "the first discovery registered {scope}"
            );
            assert!(
                catalog.source_of(scope).is_some(),
                "the first load bound the catalog source of {scope}"
            );
            assert!(
                catalog.cached_listings(scope).is_some(),
                "the first load fetched the catalog of {scope}"
            );
        }
        assert_eq!(
            symbols_of(&catalog, &vanished),
            [SmolStr::new_static("abc:DEF")]
        );

        load(&connector, &hyperliquid)
            .await
            .expect("the refresh loads the scopes the venue still publishes");

        assert_eq!(
            registered(&connector),
            [
                (
                    SmolStr::new_static("hyperliquid"),
                    SmolStr::new_static("core-perp")
                ),
                (
                    SmolStr::new_static("hyperliquid"),
                    SmolStr::new_static("core-spot")
                ),
                (
                    SmolStr::new_static("hyperliquid"),
                    SmolStr::new_static("perp-dex/xyz")
                ),
            ],
            "the refresh holds exactly the routes the venue still publishes"
        );
        assert!(
            connector.venues().scope(&vanished).is_none(),
            "the scope the venue no longer publishes is revoked in this commit"
        );
        assert!(
            catalog.source_of(&vanished).is_none(),
            "and its catalog source binding is revoked with it"
        );
        assert!(
            catalog.cached_listings(&vanished).is_none(),
            "and the snapshot it held is gone from the directory"
        );
        assert!(
            catalog.source_of(&deployed).is_some(),
            "the scope the venue still publishes keeps its catalog source"
        );
        assert_eq!(
            symbols_of(&catalog, &deployed),
            [SmolStr::new_static("xyz:ABC")],
            "and the refresh published its catalog again"
        );

        load(&connector, &hyperliquid)
            .await
            .expect("the second refresh loads the venue again");
        assert!(
            connector.venues().scope(&vanished).is_none(),
            "a scope that disappeared stays revoked"
        );
        assert!(catalog.cached_listings(&vanished).is_none());
        assert_eq!(
            symbols_of(&catalog, &deployed),
            [SmolStr::new_static("xyz:ABC")]
        );

        connector.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn a_load_commit_is_invisible_until_it_succeeds() {
        let fixture = CatalogFixture::start().await;
        let (connector, catalog) = connector_with(&fixture);
        let okx = venue("okx");
        let swap = route(&okx, "swap");

        fixture.stall(CatalogRoute::OkxSwap);
        let loading = tokio::spawn({
            let connector = connector.clone();
            async move { connector.load_venue(okx).await }
        });
        timeout(REQUEST_TIMEOUT, fixture.await_stall())
            .await
            .expect("the venue asks for the held catalog");

        assert!(
            connector.venues().loaded(&okx).is_none(),
            "a venue mid-stage is not loaded"
        );
        assert!(
            connector.venues().scopes().is_empty(),
            "no scope of a venue mid-stage is in play"
        );
        assert!(
            catalog.source_of(&swap).is_none(),
            "no catalog source of a venue mid-stage is bound"
        );
        assert!(
            catalog.cached_listings(&swap).is_none(),
            "the directory holds nothing of a venue mid-stage"
        );

        fixture.release();
        loading
            .await
            .expect("the load task does not panic")
            .expect("OKX loads once its catalog is answered");

        let entry = connector
            .venues()
            .loaded(&okx)
            .expect("the successful commit is what makes the venue visible");
        assert_eq!(entry.revision(), 1, "the first commit is revision one");
        assert_eq!(
            connector.venues().scopes().len(),
            2,
            "the whole scope set appears at once"
        );
        assert!(
            catalog.cached_listings(&swap).is_some(),
            "and so does the catalog the same commit published"
        );

        let before = Arc::clone(&entry);
        let generation_before = before.generation();
        let scopes_before = connector.venues().scopes();
        let listings_before = catalog
            .cached_listings(&swap)
            .expect("the first commit published the swap catalog");
        fixture.stall(CatalogRoute::OkxSwap);
        let refreshing = tokio::spawn({
            let connector = connector.clone();
            async move { connector.load_venue(okx).await }
        });
        timeout(REQUEST_TIMEOUT, fixture.await_stall())
            .await
            .expect("the refresh asks for the held catalog again");

        let during = connector
            .venues()
            .loaded(&okx)
            .expect("a venue mid-refresh stays loaded");
        assert!(
            Arc::ptr_eq(&before, &during),
            "a reader sees the previous entry while the refresh stages"
        );
        assert_eq!(
            during.revision(),
            1,
            "and the previous revision, because nothing committed"
        );
        assert_eq!(
            during.generation(),
            generation_before,
            "and the generation id the previous commit named"
        );
        assert_eq!(
            connector.venues().scopes(),
            scopes_before,
            "the scope set a reader scans is the previous one"
        );
        assert!(
            Arc::ptr_eq(
                &listings_before,
                &catalog
                    .cached_listings(&swap)
                    .expect("the previous catalog is still published")
            ),
            "and the directory still serves the previous snapshot"
        );

        fixture.release();
        refreshing
            .await
            .expect("the refresh task does not panic")
            .expect("the refresh commits once its catalog is answered");
        let after = connector
            .venues()
            .loaded(&okx)
            .expect("the venue stays loaded");
        assert_eq!(after.revision(), 2, "the refresh committed once");
        assert_ne!(
            after.generation(),
            generation_before,
            "and published a new generation"
        );
        connector.shutdown().await;
    }

    #[tokio::test]
    async fn a_first_load_that_fails_leaves_nothing() {
        let fixture = CatalogFixture::start().await;
        let (connector, catalog) = connector_with(&fixture);
        let binance = venue("binance");
        let spot = route(&binance, "spot");

        fixture.refuse(CatalogRoute::BinanceSpot);
        let error = load(&connector, &binance)
            .await
            .expect_err("a refused catalog fails the first load");
        assert!(
            matches!(&error, VenueLoadError::Catalog { venue, scope, .. }
                if venue == &binance && scope == &spot.scope),
            "the failure names the scope that was refused: {error:?}"
        );

        assert!(
            connector.venues().loaded(&binance).is_none(),
            "a failed first load loads no venue"
        );
        assert!(
            connector.venues().scopes().is_empty(),
            "a failed first load puts no scope of that venue in play"
        );
        assert!(
            !catalog.is_registered(&spot),
            "and registers no catalog route in the directory"
        );
        assert!(
            catalog.source_of(&spot).is_none(),
            "and binds no catalog source"
        );
        assert!(
            catalog.cached_listings(&spot).is_none(),
            "and publishes no snapshot"
        );
        connector.shutdown().await;
    }

    #[tokio::test]
    async fn a_deployment_that_cannot_be_verified_fails_the_whole_load() {
        let (origin, _requests, server) = info_fixture_server(
            vec![r#"[null,{"name":"xyz"},{"name":"bad"}]"#.to_owned()],
            spot_meta_answer(&["USD", "USDC", "BTC"]),
            dex_meta(&["BTC"]),
            vec![
                ("xyz".to_owned(), dex_meta(&["xyz:ABC"])),
                ("bad".to_owned(), dex_meta(&["ABC"])),
            ],
        )
        .await;
        let (connector, catalog) = fixture_connector(&origin, &origin);
        let hyperliquid = venue(HYPERLIQUID_VENUE);

        let error = load(&connector, &hyperliquid)
            .await
            .expect_err("a deployment the reading cannot verify fails the venue");
        assert!(
            matches!(&error, VenueLoadError::DiscoveryUnverifiable { venue, dex }
                if venue == &hyperliquid && dex == "bad"),
            "the failure names the venue and the deployment: {error:?}"
        );
        assert_nothing_was_committed(&connector, &catalog, &hyperliquid, &["xyz", "bad"]);

        connector.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn more_deployments_than_one_reading_verifies_fails_the_whole_load() {
        let claimed = MAX_DEPLOYED_PERP_DEXES + 4;
        let name_of = |index: usize| {
            format!(
                "{}{}",
                (b'a' + (index / 26) as u8) as char,
                (b'a' + (index % 26) as u8) as char
            )
        };
        const OUT_OF_SHAPE: &str = "abcde";
        let named: Vec<String> = (0..claimed - 1)
            .map(|index| format!(r#"{{"name":"{}"}}"#, name_of(index)))
            .chain(std::iter::once(format!(r#"{{"name":"{OUT_OF_SHAPE}"}}"#)))
            .collect();
        assert_eq!(named.len(), claimed, "the fixture names the full set");
        let deployed: Vec<(String, String)> = (0..claimed - 1)
            .map(|index| {
                let name = name_of(index);
                let coin = format!("{name}:ABC");
                (name, dex_meta(&[coin.as_str()]))
            })
            .chain(std::iter::once((
                OUT_OF_SHAPE.to_owned(),
                dex_meta(&[format!("{OUT_OF_SHAPE}:ABC").as_str()]),
            )))
            .collect();
        let (origin, requests, server) = info_fixture_server(
            vec![format!("[null,{}]", named.join(","))],
            spot_meta_answer(&["USD", "USDC"]),
            dex_meta(&["BTC"]),
            deployed,
        )
        .await;
        let (connector, catalog) = fixture_connector(&origin, &origin);
        let hyperliquid = venue(HYPERLIQUID_VENUE);

        let error = load(&connector, &hyperliquid)
            .await
            .expect_err("a venue over the bound fails the whole load");
        assert!(
            matches!(&error, VenueLoadError::DiscoveryLimitExceeded { venue, named, limit }
                if venue == &hyperliquid
                    && *named == claimed
                    && *limit == MAX_DEPLOYED_PERP_DEXES),
            "the failure carries the count and the bound: {error:?}"
        );
        assert_eq!(
            requests.lock().clone(),
            vec!["perpDexs".to_owned()],
            "the bound is decided from the deployment table alone"
        );
        assert_nothing_was_committed(
            &connector,
            &catalog,
            &hyperliquid,
            &[name_of(0).as_str(), name_of(1).as_str()],
        );

        connector.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn a_deployment_outside_the_scope_shape_fails_the_whole_load() {
        let (origin, _requests, server) = info_fixture_server(
            vec![r#"[null,{"name":"abcde"}]"#.to_owned()],
            spot_meta_answer(&["USD", "USDC"]),
            dex_meta(&["BTC"]),
            vec![("abcde".to_owned(), dex_meta(&["abcde:ABC"]))],
        )
        .await;
        let (connector, catalog) = fixture_connector(&origin, &origin);
        let hyperliquid = venue(HYPERLIQUID_VENUE);

        let error = load(&connector, &hyperliquid)
            .await
            .expect_err("a deployment this build cannot address fails the venue");
        assert!(
            matches!(&error, VenueLoadError::DiscoveryUnverifiable { venue, dex }
                if venue == &hyperliquid && dex == "abcde"),
            "the failure names the deployment outside the reserved shape: {error:?}"
        );
        assert_nothing_was_committed(&connector, &catalog, &hyperliquid, &["abcde"]);

        connector.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn a_deployment_whose_collateral_does_not_resolve_fails_the_whole_load() {
        let (origin, _requests, server) = info_fixture_server(
            vec![r#"[null,{"name":"noc"}]"#.to_owned()],
            spot_meta_answer(&["USD", "USDC"]),
            dex_meta(&["BTC"]),
            vec![(
                "noc".to_owned(),
                r#"{"universe":[{"name":"noc:ABC","szDecimals":3,"maxLeverage":10}],"marginTables":[],"collateralToken":7}"#
                    .to_owned(),
            )],
        )
        .await;
        let (connector, catalog) = fixture_connector(&origin, &origin);
        let hyperliquid = venue(HYPERLIQUID_VENUE);

        let error = load(&connector, &hyperliquid)
            .await
            .expect_err("a deployment whose collateral does not resolve fails the venue");
        assert!(
            matches!(&error, VenueLoadError::DiscoveryUnverifiable { venue, dex }
                if venue == &hyperliquid && dex == "noc"),
            "the failure names the deployment, not the conversation: {error:?}"
        );
        assert_nothing_was_committed(&connector, &catalog, &hyperliquid, &["noc"]);

        connector.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn a_core_scope_whose_collateral_does_not_resolve_is_a_discovery_failure() {
        let (origin, _requests, server) = info_fixture_server(
            vec![r#"[null]"#.to_owned()],
            spot_meta_answer(&["USD", "USDC"]),
            r#"{"universe":[{"name":"BTC","szDecimals":3,"maxLeverage":10}],"marginTables":[],"collateralToken":7}"#
                .to_owned(),
            Vec::new(),
        )
        .await;
        let (connector, catalog) = fixture_connector(&origin, &origin);
        let hyperliquid = venue(HYPERLIQUID_VENUE);

        let error = load(&connector, &hyperliquid)
            .await
            .expect_err("the venue's own metadata does not decode");
        assert!(
            matches!(&error, VenueLoadError::Discovery { venue, .. } if venue == &hyperliquid),
            "the core scope is nobody's deployment: {error:?}"
        );
        assert_nothing_was_committed(&connector, &catalog, &hyperliquid, &[]);

        connector.shutdown().await;
        server.abort();
    }

    fn assert_nothing_was_committed(
        connector: &MarketParticipantsConnector,
        catalog: &Catalog,
        venue: &VenueId,
        deployments: &[&str],
    ) {
        assert!(
            connector.venues().loaded(venue).is_none(),
            "a failed load loads no venue"
        );
        assert!(
            connector.venues().scopes().is_empty(),
            "a failed load puts no scope in play"
        );
        for scope in ["core-spot", "core-perp"] {
            assert_no_trace(catalog, &route(venue, scope), scope);
        }
        for dex in deployments {
            let scope = format!("perp-dex/{dex}");
            assert_no_trace(catalog, &route(venue, &scope), &scope);
        }
        let generation = catalog.generation();
        assert!(
            generation.assets().is_empty()
                && generation.references().is_empty()
                && generation.outcomes().is_empty(),
            "and installed no asset, reference or outcome identity"
        );
    }

    fn assert_no_trace(catalog: &Catalog, route: &VenueRoute, scope: &str) {
        assert!(!catalog.is_registered(route), "{scope} is not registered");
        assert!(
            catalog.source_of(route).is_none(),
            "{scope} has no catalog source bound"
        );
        assert!(
            catalog.cached_listings(route).is_none(),
            "{scope} published no snapshot"
        );
    }

    #[tokio::test]
    async fn a_refresh_that_fails_leaves_the_previous_commit_intact() {
        let fixture = CatalogFixture::start().await;
        let (connector, catalog) = connector_with(&fixture);
        let okx = venue("okx");

        load(&connector, &okx).await.expect("OKX connects");
        let before = connector
            .venues()
            .loaded(&okx)
            .expect("the first commit is visible");
        let generation = before.generation();
        let metadata = Arc::clone(before.metadata());
        let scopes = connector.venues().scopes();
        let registered_before = registered(&connector);
        let symbols_before = symbols_of(&catalog, &route(&okx, "spot"));

        fixture.refuse(CatalogRoute::OkxSwap);
        load(&connector, &okx)
            .await
            .expect_err("a refused catalog fails the refresh");

        let after = connector
            .venues()
            .loaded(&okx)
            .expect("the failed refresh keeps the venue loaded");
        assert!(
            Arc::ptr_eq(&before, &after),
            "the entry is the very value the last commit published"
        );
        assert_eq!(
            generation,
            after.generation(),
            "the generation id is the same one"
        );
        assert!(
            Arc::ptr_eq(&metadata, after.metadata()),
            "the metadata is the same value"
        );
        assert_eq!(after.revision(), 1, "the revision did not move");
        assert_eq!(
            connector.venues().scopes().len(),
            scopes.len(),
            "the scope set did not move"
        );
        assert_eq!(
            registered(&connector),
            registered_before,
            "the registered routes did not move"
        );
        assert_eq!(
            symbols_of(&catalog, &route(&okx, "spot")),
            symbols_before,
            "the catalog of a scope the refresh re-read did not move"
        );
        connector.shutdown().await;
    }

    #[tokio::test]
    async fn a_shrinking_refresh_drops_the_vanished_scopes_catalogs_and_metadata() {
        let both = r#"[null,{"name":"xyz"},{"name":"abc"}]"#.to_owned();
        let one = r#"[null,{"name":"xyz"}]"#.to_owned();
        let (origin, _requests, server) = info_fixture_server(
            vec![both, one],
            spot_meta_answer(&["USD", "USDC", "BTC"]),
            dex_meta(&["BTC"]),
            vec![
                ("xyz".to_owned(), dex_meta(&["xyz:ABC"])),
                ("abc".to_owned(), dex_meta(&["abc:DEF"])),
            ],
        )
        .await;
        let (connector, catalog) = fixture_connector(&origin, &origin);
        let hyperliquid = venue(HYPERLIQUID_VENUE);
        let deployed = route(&hyperliquid, "perp-dex/xyz");
        let vanished = route(&hyperliquid, "perp-dex/abc");

        load(&connector, &hyperliquid)
            .await
            .expect("the first load discovers both deployed scopes");
        let entry = connector
            .venues()
            .loaded(&hyperliquid)
            .expect("the first commit is visible");
        assert_eq!(
            entry.metadata().perp_dexes().iter().flatten().count(),
            2,
            "the metadata of the first commit names both deployments"
        );
        let survivor_symbols = symbols_of(&catalog, &deployed);

        load(&connector, &hyperliquid)
            .await
            .expect("the refresh loads the scopes the venue still publishes");

        assert!(
            connector.venues().scope(&vanished).is_none(),
            "the vanished scope left the loaded view"
        );
        assert!(
            catalog.source_of(&vanished).is_none(),
            "and its catalog source binding went with it"
        );
        assert!(
            catalog.cached_listings(&vanished).is_none(),
            "and its published snapshot went with it"
        );
        let entry = connector
            .venues()
            .loaded(&hyperliquid)
            .expect("the venue is still loaded");
        assert_eq!(
            entry
                .metadata()
                .perp_dexes()
                .iter()
                .flatten()
                .map(|dex| dex.name.as_str())
                .collect::<Vec<_>>(),
            vec!["xyz"],
            "the metadata was replaced by the reading that no longer names abc"
        );
        assert_eq!(
            symbols_of(&catalog, &deployed),
            survivor_symbols,
            "the scope the venue still publishes kept its catalog"
        );
        connector.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn concurrent_loads_of_two_venues_do_not_interfere() {
        let fixture = CatalogFixture::start().await;
        let (connector, catalog) = connector_with(&fixture);
        let binance = venue("binance");
        let okx = venue("okx");

        let (binance_load, okx_load) = tokio::join!(
            timeout(REQUEST_TIMEOUT, connector.load_venue(binance)),
            timeout(REQUEST_TIMEOUT, connector.load_venue(okx)),
        );
        binance_load
            .expect("the Binance load does not time out")
            .expect("Binance loads");
        okx_load
            .expect("the OKX load does not time out")
            .expect("OKX loads");

        assert_eq!(
            connector
                .venues()
                .loaded(&binance)
                .expect("Binance is loaded")
                .revision(),
            1,
            "each venue's revision is its own"
        );
        assert_eq!(
            connector
                .venues()
                .loaded(&okx)
                .expect("OKX is loaded")
                .revision(),
            1
        );
        assert_eq!(
            registered(&connector),
            [
                (SmolStr::new_static("binance"), SmolStr::new_static("spot")),
                (
                    SmolStr::new_static("binance"),
                    SmolStr::new_static("usd-m-perp")
                ),
                (SmolStr::new_static("okx"), SmolStr::new_static("spot")),
                (SmolStr::new_static("okx"), SmolStr::new_static("swap")),
            ],
            "each venue holds its own scopes"
        );
        for (route, expected) in [
            (
                route(&binance, "spot"),
                vec![SmolStr::new_static("BTCUSDT")],
            ),
            (
                route(&okx, "swap"),
                vec![SmolStr::new_static("BTC-USDT-SWAP")],
            ),
        ] {
            assert_eq!(
                symbols_of(&catalog, &route),
                expected,
                "the catalog of {route} is the one its own venue published"
            );
        }
        connector.shutdown().await;
    }

    #[tokio::test]
    async fn two_loads_of_the_same_venue_do_not_interleave() {
        let fixture = CatalogFixture::start().await;
        let (connector, catalog) = connector_with(&fixture);
        let okx = venue("okx");

        let first = connector.load_venue(okx);
        let second = connector.load_venue(okx);
        let (first, second) = tokio::join!(
            timeout(REQUEST_TIMEOUT, first),
            timeout(REQUEST_TIMEOUT, second)
        );
        first
            .expect("the first load does not time out")
            .expect("the first load succeeds");
        second
            .expect("the second load does not time out")
            .expect("the second load is answered by the first");

        assert_eq!(
            connector
                .venues()
                .loaded(&okx)
                .expect("OKX is loaded")
                .revision(),
            1,
            "one burst of loads of one venue commits once"
        );
        assert_eq!(
            catalog.route_set().map(|set| set.catalog_routes().len()),
            Some(2),
            "and registers the venue's catalog routes once"
        );
        connector.shutdown().await;
    }

    #[tokio::test]
    async fn a_load_cancelled_mid_stage_leaves_nothing() {
        let fixture = CatalogFixture::start().await;
        let (connector, catalog) = connector_with(&fixture);
        let okx = venue("okx");
        let swap = route(&okx, "swap");

        fixture.stall(CatalogRoute::OkxSwap);
        let loading = tokio::spawn({
            let connector = connector.clone();
            async move { connector.load_venue(okx).await }
        });
        timeout(REQUEST_TIMEOUT, fixture.await_stall())
            .await
            .expect("the venue asks for the held catalog");

        connector.cancel_venue_load(&okx);
        fixture.release();
        let error = loading
            .await
            .expect("the load task does not panic")
            .expect_err("a cancelled load does not commit");
        assert!(
            matches!(&error, VenueLoadError::Interrupted { venue } if venue == &okx),
            "the refusal is the cancellation: {error:?}"
        );
        assert!(connector.venues().loaded(&okx).is_none());
        assert!(connector.venues().scopes().is_empty());
        assert!(catalog.source_of(&swap).is_none());
        assert!(catalog.cached_listings(&swap).is_none());
        connector.shutdown().await;
    }

    #[tokio::test]
    async fn a_load_report_names_the_stage_that_failed() {
        let fixture = CatalogFixture::start().await;
        let (connector, _catalog) = connector_with(&fixture);
        let binance = venue("binance");
        fixture.refuse(CatalogRoute::BinanceSpot);
        let error = load(&connector, &binance)
            .await
            .expect_err("a refused catalog fails the load");
        assert!(
            matches!(&error, VenueLoadError::Catalog { venue, scope, .. }
                if venue == &binance && scope.as_str() == "spot"),
            "a catalog failure names the venue and the scope it failed on: {error:?}"
        );

        let (unreachable, _catalog) = fixture_connector("http://127.0.0.1:1", "http://127.0.0.1:1");
        let hyperliquid = venue(HYPERLIQUID_VENUE);
        let error = load(&unreachable, &hyperliquid)
            .await
            .expect_err("nothing answers the venue's discovery endpoint");
        assert!(
            matches!(&error, VenueLoadError::Discovery { venue, .. } if venue == &hyperliquid),
            "a discovery failure names the venue: {error:?}"
        );
        unreachable.shutdown().await;
        connector.shutdown().await;
    }
}

#[cfg(test)]
mod venue_request_tests {

    use std::sync::Arc;

    use app_config::market_participants::fixture::{self, FixtureAddress};
    use app_config_provider::notification::ConfigNotification;
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
    };
    use parking_lot::Mutex;
    use tokio::time::timeout;

    use crate::{
        connector::MarketParticipantsConnector,
        market::timeframe::TimeFrame,
        routing::CORE_PERP_SCOPE,
        runtime::MarketParticipantNetworkSnapshot,
        test_support::{
            InfoFixture, InfoSocket, dex_meta, fixture_config, info_fixture_server,
            spot_meta_answer,
        },
    };

    const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

    fn deployed_route() -> VenueRoute {
        VenueRoute::new(
            VenueId::Hyperliquid,
            VenueScopeId::new("perp-dex/xyz").expect("the deployed scope name is canonical"),
        )
    }

    async fn discovery_fixture() -> InfoFixture {
        info_fixture_server(
            vec![r#"[null,{"name":"xyz"}]"#.to_owned()],
            spot_meta_answer(&["USD", "USDC", "BTC"]),
            dex_meta(&["BTC"]),
            vec![("xyz".to_owned(), dex_meta(&["xyz:ABC"]))],
        )
        .await
    }

    fn fixture_at(
        http: &url::Url,
        websocket: &str,
    ) -> app_config::market_participants::fixture::FixtureInstallation {
        fixture::venues([(
            VenueId::Hyperliquid,
            FixtureAddress {
                http: http.clone(),
                websocket: websocket.parse().expect("the fixture socket is a URL"),
            },
        )])
    }

    fn connector(
        notification: ConfigNotification<MarketParticipantNetworkSnapshot>,
    ) -> (
        MarketParticipantsConnector,
        Arc<crate::catalog::store::Catalog>,
    ) {
        crate::test_support::connector_over(notification, crate::test_support::plaintext_policy())
    }

    type RequestLog = Arc<Mutex<Vec<String>>>;

    fn served(requests: &RequestLog, kind: &str) -> usize {
        requests
            .lock()
            .iter()
            .filter(|served| served.as_str() == kind)
            .count()
    }

    fn candles(socket: &InfoSocket) -> usize {
        socket
            .frames()
            .iter()
            .filter(|frame| frame.contains("candleSnapshot"))
            .count()
    }

    #[tokio::test]
    async fn discovery_dials_the_venues_own_configuration() {
        let (http_origin, requests, http_server) = discovery_fixture().await;
        let http_origin: url::Url = http_origin.parse().expect("the fixture origin is a URL");
        let socket = InfoSocket::spawn("[]".to_owned(), Vec::new()).await;
        let fixture = fixture_at(&http_origin, socket.url());
        let snapshot = Arc::new(MarketParticipantNetworkSnapshot::new(fixture_config(
            &fixture,
        )));
        let notification = ConfigNotification::new(Arc::clone(&snapshot));
        let (connector, catalog) = connector(notification);
        let hyperliquid = VenueId::Hyperliquid;
        let core_perp =
            VenueScopeId::new(CORE_PERP_SCOPE).expect("the built-in scope identity is canonical");

        timeout(REQUEST_TIMEOUT, connector.load_venue(hyperliquid))
            .await
            .expect("the venue load should not time out")
            .expect("the venue discovers and loads through its own configuration");

        assert_eq!(
            served(&requests, "perpDexs"),
            1,
            "the discovery request reached the venue's own configuration"
        );
        assert_eq!(
            served(&requests, "spotMetaAndAssetCtxs"),
            1,
            "and so did the spot scope's own metadata, which the reading carries"
        );

        let listing_of = |venue: &VenueRoute| {
            let catalog = catalog.clone();
            let route = venue.clone();
            async move {
                catalog
                    .listings(&route)
                    .expect("the loaded scope publishes its catalogue")
                    .first()
                    .expect("the fixture lists one coin on the scope")
                    .listing
                    .key
                    .clone()
            }
        };
        let core_perp_route = VenueRoute::new(hyperliquid, core_perp);
        let core_perp_listing = listing_of(&core_perp_route).await;
        connector
            .market()
            .market(core_perp_route)
            .candlesticks(core_perp_listing, TimeFrame::Minute1)
            .limit(1)
            .fetch()
            .await
            .expect("the venue's own socket answers the core perpetual scope's candle request");
        assert_eq!(
            candles(&socket),
            1,
            "a request that addresses a scope dialled the socket the venue is configured with"
        );

        let deployed = deployed_route();
        let deployed_listing = listing_of(&deployed).await;
        connector
            .market()
            .market(deployed)
            .candlesticks(deployed_listing, TimeFrame::Minute1)
            .limit(1)
            .fetch()
            .await
            .expect("the venue's own socket answers the deployed scope's candle request too");
        assert_eq!(
            candles(&socket),
            2,
            "a scope the settings hold no key for is still reached through the venue's own pair"
        );

        drop(http_server);
        connector.shutdown().await;
    }

    #[tokio::test]
    async fn a_dynamic_scope_inherits_the_venues_connection_config() {
        let (http_origin, requests, http_server) = discovery_fixture().await;
        let http_origin: url::Url = http_origin.parse().expect("the fixture origin is a URL");
        let socket_a = InfoSocket::spawn("[]".to_owned(), Vec::new()).await;
        let socket_b = InfoSocket::spawn("[]".to_owned(), Vec::new()).await;
        let mut fixture = fixture_at(&http_origin, socket_a.url());
        let initial = Arc::new(MarketParticipantNetworkSnapshot::new(fixture_config(
            &fixture,
        )));
        let notification = ConfigNotification::new(Arc::clone(&initial));
        let (connector, catalog) = connector(notification.clone());
        let hyperliquid = VenueId::Hyperliquid;
        let deployed = deployed_route();

        timeout(REQUEST_TIMEOUT, connector.load_venue(hyperliquid))
            .await
            .expect("the venue load should not time out")
            .expect(
                "the venue discovers and loads its deployed scope through its own configuration",
            );
        assert!(
            served(&requests, "perpDexs") > 0,
            "the load read the venue over the HTTP socket it is configured with"
        );

        let listing = catalog
            .listings(&deployed)
            .expect("the deployed scope publishes its catalogue")
            .first()
            .expect("the fixture lists one coin on the deployed scope")
            .listing
            .key
            .clone();
        let candle_request = || {
            connector
                .market()
                .market(deployed.clone())
                .candlesticks(listing.clone(), TimeFrame::Minute1)
                .limit(1)
                .fetch()
        };

        candle_request()
            .await
            .expect("the venue's own socket answers the candle request");
        assert_eq!(
            candles(&socket_a),
            1,
            "the deployed scope dialled the venue's configured socket"
        );
        assert_eq!(
            candles(&socket_b),
            0,
            "and nothing reached the socket the venue is not configured with"
        );

        fixture.repoint([(
            hyperliquid,
            FixtureAddress {
                http: http_origin,
                websocket: socket_b.url().parse().expect("the socket is a URL"),
            },
        )]);
        notification.publish_for_test(Arc::new(MarketParticipantNetworkSnapshot::from_previous(
            fixture_config(&fixture),
            Some(&initial),
        )));

        candle_request()
            .await
            .expect("the venue's new socket answers the candle request too");
        assert_eq!(
            candles(&socket_b),
            1,
            "the same request reached the socket the venue is configured with now"
        );
        assert_eq!(
            candles(&socket_a),
            1,
            "and the socket it used to dial received nothing more"
        );

        connector.shutdown().await;
        http_server.abort();
        socket_a.shutdown().await;
        socket_b.shutdown().await;
    }
}

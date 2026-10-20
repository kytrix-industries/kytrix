//! The application-facing market participants connector.
//!
//! [`MarketParticipantsConnector`] owns one gateway, connection pool, registry, and runtime
//! projection for the application's market-participants capabilities. The `account`,
//! `market`, and `trading` handles are cheap-to-clone siblings that share that
//! state and have no dependency on one another.
//!
//! Nothing here loads a market on its own: [`MarketParticipantsConnector::load_venue`] is
//! the one entry point that stages everything a venue publishes and commits it in
//! one critical section, and the only caller is the user's load action.

use std::{
    cell::Cell,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use ahash::AHashMap;
use app_config::settings::Config;
use app_config_provider::{
    events::{ConfigEventBridge, ConfigTopics},
    manager::ConfigManager,
    notification::ConfigNotification,
};
use futures::future::BoxFuture;
use gpui_kit::{App, AppContext};
use market_participants_model::{
    identifier::{VenueId, VenueScopeId},
    listing::route::VenueRoute,
};
use network::transport::hub::{TransportHub, TransportPolicy};
use parking_lot::Mutex;
use smol_str::SmolStr;
use widget_tokio_runtime::runtime::Runtime;

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
        loaded::{LoadedVenues, VenueEventSource, VenuesUpdated},
        metadata::{VenueMetadata, VenueReading},
        specs,
    },
};

/// Runtime inputs owned by the composition root when constructing an
/// [`MarketParticipantsConnector`]. The root creates its own connection pool and
/// gateway from these inputs; lower-level market-participant types are intentionally
/// absent from this boundary.
pub struct MarketParticipantsConnectorInput {
    transport: Arc<TransportHub>,
    snapshot: ConfigNotification<MarketParticipantNetworkSnapshot>,
    runtime: Option<Runtime>,
}

impl MarketParticipantsConnectorInput {
    /// Creates MarketParticipantsConnector input from the shared transport and the
    /// notification carrying the market-participants runtime projection.
    ///
    /// The application runtime must be attached with [`Self::with_runtime`]
    /// before the market participants root is built: every route resolves its proxy through
    /// it, and a request has to be able to carry that work off whatever executor
    /// it arrived on.
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

    /// Attaches the application runtime every market-participant operation runs on.
    pub fn with_runtime(mut self, runtime: Runtime) -> Self {
        self.runtime = Some(runtime);
        self
    }
}

/// The single application-level owner of market-participants runtime resources.
#[derive(Clone)]
pub struct MarketParticipantsConnector {
    inner: Arc<MarketParticipantsConnectorInner>,
}

/// Errors raised while creating the application `MarketParticipantsConnector`.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum MarketParticipantsConnectorError {
    #[error("MarketParticipantsConnector initialization failed: {0}")]
    Initialization(#[source] MarketParticipantError),
    #[error("a Tokio runtime is required for this market-participant operation")]
    RuntimeUnavailable,
    #[error("the application settings are not installed")]
    ConfigurationUnavailable,
    #[error("route descriptor validation failed: {0}")]
    RouteDescriptors(#[source] routing::RegistryError),
}

/// Why loading one venue failed.
///
/// The variant names the stage that failed and, where a stage is per scope, the
/// scope it failed on, so a caller can tell "the venue would not say what it
/// deploys" from "this scope's directory could not be read" and report the right
/// thing about the right scope. A load that fails at any stage commits nothing:
/// the variants are reports, not states.
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
    /// A venue this build carries an identity for but serves nothing for.
    ///
    /// The identity and the served set are not the same set in principle: a venue whose
    /// loader and capability names have not landed yet would be carried by [`VenueId`]
    /// and answered by no definition, and a load of it would be refused here rather than
    /// panicking. The refusal happens before anything of the venue is read, so it
    /// commits nothing and leaves no trace.
    ///
    /// No [`VenueId`] reaches this variant today: every variant's definition has landed,
    /// so the definition table answers one for each and cannot answer nothing. Nothing in
    /// this build constructs it, and it is kept rather than deleted because the load seam
    /// it belongs to is the one a later phase extends: the day a venue is carried before
    /// its loader, that arm's absence is what this variant says, and
    /// `the_definition_table_covers_every_venue_this_build_serves` is the test that would
    /// fail first — it asserts the lookup answers exactly [`VenueId::ALL`], so a carried
    /// venue without a definition is caught there rather than silently served.
    #[error("this build serves no market for venue {venue}")]
    UnsupportedVenue { venue: VenueId },
}

impl VenueLoadError {
    /// Reports a refused commit as the stage failure it is.
    ///
    /// The commit refuses a batch whose route changed while it was staged, which
    /// is a `(venue, scope)` answer rather than a scope-less one; every other
    /// refusal names the scope whose directory the commit was publishing.
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

impl gpui_kit::Global for MarketParticipantsConnector {}

/// Keeps the market-participants bridge alive for the life of the application.
///
/// The bridge holds `Subscription`s, which are neither `Send` nor `Sync`, while
/// the connector is an `Arc` shared with background work: the subscription lives
/// here, owned by the application, and only the notification it publishes into
/// travels with the connector.
struct MarketParticipantsConfigBridge {
    _bridge: ConfigEventBridge,
}

impl gpui_kit::Global for MarketParticipantsConfigBridge {}

/// The state every market-participants capability handle shares.
///
/// The fields are public because the `account`, `market` and `trading` handles are
/// built from this one value and read the gateway, the pool and the route
/// projections directly rather than through a second layer. The type itself is
/// crate-visible, so no field reaches an external caller.
pub(crate) struct MarketParticipantsConnectorInner {
    pub gateway: MarketParticipantGateway,
    pub pool: Arc<ConnectionPool>,
    /// The shared catalogue this root loads a venue's catalogs through and every
    /// reader answers from: the root owns it, so there is one directory and one
    /// publication, and no attachment step exists.
    pub catalog: Arc<Catalog>,
    /// The one routing view: which venues a load has committed and what each
    /// commit published. Catalog content is not answered here — it is answered
    /// by the published generation of the catalogue.
    pub venues: LoadedVenues,
    /// The single-flight boundary of one venue's load.
    ///
    /// Two loads of the same venue never interleave their stages: the second
    /// waits for the first to commit, so a venue's revision advances once per
    /// load and no stage ever plans against a registration another stage is
    /// about to replace.
    pub loads: Mutex<AHashMap<VenueId, Arc<LoadFlight>>>,
    /// The cancellation of each venue's in-flight load.
    ///
    /// The action that started a load is the one that stops it — a row the user
    /// left while its venue was loading — so the token is kept per venue and
    /// observed by the stage between its steps.
    pub cancellations: Mutex<AHashMap<VenueId, LoadCancellation>>,
    /// The task that turns a committed venue update into an emission.
    ///
    /// A load runs on the application runtime and a subscriber lives on the
    /// application's executor, so the announcement crosses between the two here.
    pub event_task: Mutex<Option<gpui_kit::Task<()>>>,
    /// The receiver of the boundary this root owns before an application
    /// attaches its own.
    pub event_receiver: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<VenuesUpdated>>>,
    /// The task that invalidates pooled connections whenever the runtime
    /// projection is published again.
    ///
    /// Only [`MarketParticipantsConnector::install`] starts it: a root built from
    /// typed runtime inputs has no configuration owner to wait on.
    pub config_task: Mutex<Option<OwnedTask>>,
    pub closed: AtomicBool,
}

impl MarketParticipantsConnectorInner {
    /// Returns the shared catalogue every capability reads through.
    pub(crate) fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Returns the shared catalogue by pointer, for a proof that outlives one
    /// borrow of this root.
    pub(crate) fn catalog_arc(&self) -> Arc<Catalog> {
        Arc::clone(&self.catalog)
    }

    /// Loads one venue: stages everything it publishes and commits it in one step.
    ///
    /// The load is the one writer of the directory, the route registry and the
    /// venue table, and it is a transaction: the whole scope set, every
    /// catalog of every scope that publishes one, and the venue's metadata are
    /// read and validated first, and nothing becomes visible until the commit's
    /// single critical section writes all three views together. A first load
    /// that fails leaves nothing behind, and a refresh that fails leaves the
    /// previous commit byte for byte as it was.
    ///
    /// The stage is serial — the "first failure stops the load" rule: the venues
    /// publish a handful of scopes each, and reporting which scope failed is worth
    /// more than overlapping the reads.
    ///
    /// A venue the definition table answers no definition for would be refused before
    /// anything is read, naming the venue rather than defaulting it to another venue's
    /// definition; the table answers one for every venue this build carries, so no
    /// [`VenueId`] takes that path today.
    async fn load_venue(
        &self,
        venue: VenueId,
        cancellation: &LoadCancellation,
    ) -> Result<(), VenueLoadError> {
        let definition = definition::definition_of(venue);
        // One load of one venue at a time: a second load of the same venue
        // waits for the first to commit rather than interleaving a second stage
        // with it, so a venue's revision advances exactly once per load.
        let flight = self.load_flight(&venue);
        let seen = flight.generation.load(Ordering::Acquire);
        let _flight = flight.lock.lock().await;
        // A load that waited for another load of the same venue is that load's
        // answer: it commits nothing a second time, so the venue's revision
        // advances exactly once per burst of concurrent loads.
        if flight.generation.load(Ordering::Acquire) != seen {
            return Ok(());
        }
        let stage = self
            .stage_venue(definition, cancellation)
            .await
            .inspect_err(|error| log::debug!("loading {venue} failed: {error}"))?;
        self.commit_venue(stage, cancellation)?;
        // The load this burst of callers waited for is what answers them: the
        // counter moves only after a commit actually happened.
        flight.generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Reads everything one venue publishes, changing nothing.
    ///
    /// The loader answers the scope set and the metadata; every catalog scope is
    /// then fetched through the route the same answer registers, so a batch is
    /// validated against the registration it will be published under. Any failure
    /// drops the whole stage: the batches are private pending values until the
    /// commit installs them, so a refused stage leaves no asset, reference or
    /// outcome identity behind.
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
        // The candidate is validated against every other venue's routes before
        // anything is fetched, so the registration each batch is staged under is
        // the one the commit will write.
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

    /// Fetches and stages every catalog one venue's scope set publishes.
    ///
    /// A scope with no catalog source publishes no directory, so it contributes
    /// nothing here; a scope whose fetch or validation fails fails the whole load
    /// and names itself, because a venue is loaded whole or not at all.
    ///
    /// The fetch is resolved against the metadata the same load read: a catalog is
    /// derived from the venue's own tables, so staging it is a read of the reading
    /// rather than a second conversation, and the venue is asked nothing here.
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

    /// Commits one staged venue as a single indivisible publication.
    ///
    /// The four views the commit owns — the venue table, the gateway's route
    /// registry, the catalogue's route registry and the publication pointer — are
    /// locked in that order and released together, and the venue's entry is
    /// replaced inside that same critical section, so a reader observes either the
    /// previous commit or this one. Nothing in the section awaits.
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
        let revision = Cell::new(0_u64);
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
                    revision.set(venues.replace(
                        venue,
                        scopes,
                        generation.id(),
                        Arc::clone(&staged.metadata),
                    ));
                },
            )
            .map_err(|error| VenueLoadError::from_commit(&venue, scope, error))?;
        drop(registry);
        drop(venues);
        // The lock is released before the commit is announced, so a subscriber
        // that reads the venue table in its handler cannot deadlock against the
        // commit that woke it.
        self.venues.announce(venue, revision.get());
        Ok(())
    }

    /// Returns the single-flight lock of one venue's load.
    fn load_flight(&self, venue: &VenueId) -> Arc<LoadFlight> {
        let mut loads = self.loads.lock();
        Arc::clone(
            loads
                .entry(*venue)
                .or_insert_with(|| Arc::new(LoadFlight::default())),
        )
    }

    /// Records the cancellation of one venue's in-flight load.
    fn cancellation_of(&self, venue: &VenueId) -> LoadCancellation {
        let mut cancellations = self.cancellations.lock();
        cancellations.entry(*venue).or_default().clone()
    }

    /// Empties one venue's cancellation once its load is over.
    fn finish_load(&self, venue: &VenueId) {
        self.cancellations.lock().remove(venue);
    }

    /// Commits one fixture venue without a venue conversation.
    ///
    /// The fixture's scope set and listings travel the same commit as a load's:
    /// the routes are validated and planned, the listings are staged as the
    /// pending snapshots of the routes that publish a catalog, and the commit
    /// writes the venue table, the route registry and the directory pointer in one
    /// critical section. The differences are the ones that make it a fixture: no
    /// request is sent and the metadata is empty.
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
        let revision = Cell::new(0_u64);
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
                    revision.set(venues.replace(
                        *venue,
                        scopes,
                        generation.id(),
                        Arc::clone(&metadata),
                    ));
                },
            )
            .map_err(|error| {
                MarketParticipantsConnectorError::RouteDescriptors(routing::RegistryError::Catalog(
                    error,
                ))
            })?;
        drop(registry);
        drop(venues);
        self.venues.announce(*venue, revision.get());
        // The fixture commit is recorded the way the load transaction records one:
        // the entry it published, the generation it pinned and the deployments its
        // metadata named.
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

/// The single-flight boundary of one venue's load.
///
/// The lock serializes the loads of one venue, and the counter records how many
/// of them have committed: a load that waited for the lock and finds the counter
/// moved was answered by the load it waited for, so it commits nothing again.
#[derive(Default)]
pub(crate) struct LoadFlight {
    lock: tokio::sync::Mutex<()>,
    generation: std::sync::atomic::AtomicU64,
}

/// Everything one staged load will commit.
struct StagedVenue {
    venue: VenueId,
    descriptors: Vec<routing::VenueRouteDescriptor>,
    registrations: Vec<CatalogRouteRegistration>,
    requirements: CatalogRouteRequirements,
    pending: Vec<CatalogPending>,
    scopes: Vec<ScopeEntry>,
    metadata: Arc<VenueMetadata>,
}

/// The venue conversation a loader dials through.
///
/// It holds the gateway and the venue identity, so a discovery request is built,
/// dialed and decoded inside the gateway exactly as any other market-participants call is; the
/// one thing it does not need is a registered route, because the conversation
/// addresses the venue itself.
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
    /// Builds the shared market participants root from the installed application services
    /// and installs it in GPUI global state.
    ///
    /// The connector subscribes the market-participants runtime projection to the
    /// settings manager's network and venue events, so a settings change
    /// reconfigures the market-participants runtime without rebuilding the
    /// connector; a background task then closes every pooled connection the new
    /// projection no longer serves.
    ///
    /// # Errors
    ///
    /// Fails when the application Tokio runtime or the settings manager is not
    /// installed, or when the market-participants runtime cannot be built from the current
    /// settings.
    pub fn install(cx: &mut App) -> Result<(), MarketParticipantsConnectorError> {
        let runtime = widget_tokio_runtime::global::try_handle(cx)
            .ok_or(MarketParticipantsConnectorError::RuntimeUnavailable)?;
        let manager = cx
            .try_global::<ConfigManager>()
            .ok_or(MarketParticipantsConnectorError::ConfigurationUnavailable)?;
        let config = manager.read();
        let initial = Arc::new(MarketParticipantNetworkSnapshot::new(Config::clone(
            &config,
        )));
        let notification = ConfigNotification::new(initial);
        let bridge = ConfigEventBridge::install(
            cx,
            ConfigTopics::NETWORK_AND_VENUES,
            notification.clone(),
            Arc::new(
                |config: &Config, previous: &MarketParticipantNetworkSnapshot| {
                    MarketParticipantNetworkSnapshot::from_previous(
                        Config::clone(config),
                        Some(previous),
                    )
                },
            ),
        );

        let transport = Arc::new(TransportHub::new(TransportPolicy::default()));
        let connector = Self::new(
            MarketParticipantsConnectorInput::new(transport, notification.clone())
                .with_runtime(runtime.clone()),
        )?;
        let pool = Arc::clone(&connector.inner.pool);
        let config_task = runtime.run(async move {
            loop {
                let snapshot = notification.wait().await;
                pool.invalidate_stale(&snapshot).await;
            }
        });
        *connector.inner.config_task.lock() = Some(OwnedTask::new(config_task));

        cx.set_global(MarketParticipantsConfigBridge { _bridge: bridge });
        connector.attach_events(cx);
        cx.set_global(connector);

        Ok(())
    }

    /// Attaches the venue event boundary to this root.
    ///
    /// A root built from typed runtime inputs — a test's, an embedder's — has no
    /// application until this is called, so it has no boundary to announce a
    /// commit through. Attaching one is what makes a load legal there: a commit
    /// with no boundary attached reports rather than commits silently.
    pub fn attach_events(&self, cx: &mut App) {
        let events = cx.new(|_| VenueEventSource);
        self.inner.venues.attach_events(events.clone());
        let Some(mut receiver) = self.inner.event_receiver.lock().take() else {
            log::error!("the venue event boundary of this root is already attached");
            return;
        };
        // A load runs on the application runtime, which holds no GPUI context,
        // so the announcement travels back as a message and is emitted on the
        // entity here, on the application own executor.
        let task = cx.spawn(move |cx: &mut gpui_kit::AsyncApp| {
            let mut cx = cx.clone();
            async move {
                while let Some(update) = receiver.recv().await {
                    cx.update_entity(&events, |_, cx| cx.emit(update));
                }
            }
        });
        *self.inner.event_task.lock() = Some(task);
    }

    /// Builds the shared market participants root from typed runtime inputs.
    ///
    /// The root owns its catalogue: a fetch of a venue's catalogs and every read
    /// of the published generation answer from the same [`Catalog`], and the
    /// application runtime is attached to it here so a fetch driven from a GPUI
    /// task crosses the same blocking boundary as one driven by a service.
    pub fn new(
        input: MarketParticipantsConnectorInput,
    ) -> Result<Self, MarketParticipantsConnectorError> {
        Self::build(input, Arc::new(Catalog::new()))
    }

    /// Builds a root over an existing catalogue.
    ///
    /// This is the test seam beside [`Self::new`]: a fixture builds a catalogue,
    /// seeds it and hands it here, so the root reads and publishes into the
    /// directory the fixture drives. Production builds its own and never shares
    /// one.
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
        // The root owns the venue event boundary from the moment it exists:
        // every commit is announced into it, and an application that installs
        // the root attaches the entity that turns those announcements into
        // emissions. A commit can therefore never be silently unannounced.
        let venues = LoadedVenues::new();
        let (event_sender, event_receiver) = tokio::sync::mpsc::unbounded_channel();
        venues.attach_update_sink(event_sender);
        let inner = Arc::new(MarketParticipantsConnectorInner {
            gateway,
            pool,
            catalog,
            venues,
            loads: Mutex::new(AHashMap::new()),
            cancellations: Mutex::new(AHashMap::new()),
            event_task: Mutex::new(None),
            event_receiver: Mutex::new(Some(event_receiver)),
            config_task: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        Ok(Self { inner })
    }

    /// Returns the account capability sharing this `MarketParticipantsConnector` runtime.
    pub fn account(&self) -> account::provider::AccountCapability {
        account::provider::AccountCapability::new(Arc::clone(&self.inner))
    }

    /// Returns the market capability sharing this `MarketParticipantsConnector` runtime.
    pub fn market(&self) -> market::provider::MarketCapability {
        market::provider::MarketCapability::new(Arc::clone(&self.inner))
    }

    /// Returns the trading capability sharing this `MarketParticipantsConnector` runtime.
    pub fn trading(&self) -> trading::provider::TradingCapability {
        trading::provider::TradingCapability::new(Arc::clone(&self.inner))
    }

    /// Returns every venue this build serves, in declaration order.
    ///
    /// The list is the static definition table's answer: the market menu draws
    /// one row per venue and the settings page one group per venue, whether or
    /// not anything is loaded. Whether a venue is loaded is [`Self::venues`]'s
    /// answer — the two are deliberately different questions.
    pub fn supported_venues(&self) -> Vec<VenueId> {
        definition::supported_venues()
    }

    /// Reports whether a load of this venue has to be refused for want of credentials.
    ///
    /// A venue that declares every read signs and stores no complete credential set is
    /// one a load would leave unreadable: the load itself succeeds — its directory is
    /// public — and every pane that then read the market would fail on the credential.
    /// A venue whose reads are public answers false and is offered unconditionally.
    ///
    /// This is the one read of the static table a caller outside this crate has, and it
    /// exists because the interface that asks the question may not read the table
    /// itself: whether a venue *needs* credentials is a definition's field, and
    /// whether it *has* them is the runtime snapshot's answer.
    pub fn load_needs_credentials(&self, venue: VenueId) -> bool {
        let definition = definition::definition_of(venue);
        definition.requires_credentials_to_load
            && !self.runtime_snapshot().credentials_configured(&venue)
    }

    /// Returns the runtime snapshot this root's requests resolve against.
    ///
    /// It is the shared projection read once: the snapshot every request is
    /// resolved with is the one this answers from, so an answer about a venue's
    /// credentials is about the configuration that would serve its next request.
    fn runtime_snapshot(&self) -> Arc<MarketParticipantNetworkSnapshot> {
        self.inner.gateway.runtime.snapshot.current()
    }

    /// Returns the loaded venues.
    ///
    /// This is the one routing view: which venues are loaded, which scopes are in
    /// play and what each scope supports all come from it. It never answers
    /// catalog content, which is the published generation of the catalogue.
    pub fn venues(&self) -> &LoadedVenues {
        &self.inner.venues
    }

    /// Returns the shared catalogue.
    ///
    /// This is the one read face of the directory: a caller names it, a listing
    /// or an asset, and reads whatever the published generation answers, without
    /// fetching anything. Writing it is not reachable from outside this crate —
    /// a venue's catalogs are published by [`Self::load_venue`] alone.
    pub fn catalog(&self) -> &Catalog {
        &self.inner.catalog
    }

    /// Returns the shared catalogue by pointer.
    ///
    /// The same read face as [`Self::catalog`], for a holder that outlives one
    /// borrow of this root — a service, a dialog or an entity that keeps the
    /// directory across calls.
    pub fn catalog_arc(&self) -> Arc<Catalog> {
        self.inner.catalog_arc()
    }

    /// Loads one venue: everything it publishes, in one transaction.
    ///
    /// The load stages the venue's whole scope set, every catalog of every scope
    /// that publishes one, and its metadata, and commits them together with the
    /// venue's routes in one critical section. A first load that fails
    /// leaves nothing behind; a refresh that fails leaves the previous commit
    /// exactly as it was, down to the revision the venue's entry carries.
    ///
    /// A venue this build declares no definition for is refused before anything is
    /// read: the definition table is what says a venue can be loaded at all. Every
    /// [`VenueId`] this build carries has an entry there — the identity and the served
    /// set are one set today — so no load reaches that refusal; the lookup answers a
    /// definition rather than an option for exactly that reason, and a venue the identity
    /// gained before its loader landed would be the one that separated them again.
    ///
    /// The returned future is the load itself. Cancelling it — see
    /// [`Self::cancel_venue_load`] — stops the stage before its next step and
    /// commits nothing.
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

    /// Cancels the load of one venue, if one is in flight.
    ///
    /// The cancellation is observed between the stage's steps, so the request
    /// already under way finishes and nothing after it starts. A load that was
    /// cancelled commits nothing and answers [`VenueLoadError::Interrupted`].
    pub fn cancel_venue_load(&self, venue: &VenueId) {
        if let Some(cancellation) = self.inner.cancellations.lock().get(venue) {
            cancellation.cancel();
        }
    }

    /// Loads a fixture market without a venue answering.
    ///
    /// This is the one seam a test drives a market through, and it replaces the
    /// three former registration seams: the venues named are committed exactly as
    /// [`Self::load_venue`] commits them — their scope set, their catalogs and
    /// their metadata become visible in one step, the venue table, the route
    /// registry and the directory pointer together — except that no request is
    /// sent, no discovery runs and the catalogs are the fixture's own listings
    /// instead of a fetch.
    ///
    /// `scopes` states a scope set a fixture owns — a scope no static table has,
    /// like a discovered `perp-dex/*` — one entry per venue; a venue whose scopes
    /// are not stated loads the ones its definition publishes. `catalogs` carries
    /// the fixture listings of the routes that publish one. A venue named in
    /// neither list keeps whatever it committed before, and a venue whose stated
    /// scope set is empty is unloaded.
    ///
    /// Production never calls it: a market is loaded by the user's own action, and
    /// that action is `load_venue`.
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

    /// Closes the shared gateway and stops the root-owned invalidation task.
    pub async fn shutdown(&self) {
        if self.inner.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let config_task = self.inner.config_task.lock().take();
        if let Some(task) = config_task {
            task.cancel_and_wait().await;
        }
        // The catalogue belongs to this root: shutting the root down drops the
        // last reference to it and nothing publishes into it again.
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
    use app_config_provider::{manager::ConfigManager, notification::ConfigNotification};
    use gpui_kit::TestAppContext;
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
        routing::CapabilityId,
    };
    use network::transport::hub::{TransportHub, TransportPolicy};
    use smol_str::SmolStr;
    use tokio::time::timeout;
    use widget_tokio_runtime::{global, runtime::Runtime};

    use super::{MarketParticipantsConnector, MarketParticipantsConnectorInput};
    use crate::{
        connection::{
            Connection, ConnectionKey, ConnectionState, no_response_id,
            physical::ConnectionProtocol,
        },
        runtime::MarketParticipantNetworkSnapshot,
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

    /// A root with its own catalogue and no venue loaded.
    fn root() -> MarketParticipantsConnector {
        let snapshot = Arc::new(MarketParticipantNetworkSnapshot::default());
        let notification = ConfigNotification::new(snapshot);
        MarketParticipantsConnector::new(
            MarketParticipantsConnectorInput::new(
                Arc::new(TransportHub::new(TransportPolicy::default())),
                notification,
            )
            .with_runtime(Runtime::for_test()),
        )
        .expect("the root builds from typed runtime inputs")
    }

    /// The static table is not a registration, and loading a venue publishes both
    /// of its scopes together with its catalog source bindings.
    #[tokio::test]
    async fn connecting_a_venue_publishes_its_scopes_and_catalog_bindings() {
        let connector = root();

        assert!(
            connector.venues().scopes().is_empty(),
            "a root with the wiring attached holds no venue"
        );
        assert_eq!(
            connector.supported_venues().len(),
            4,
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

    /// Loading one venue leaves another venue's slot exactly as it was.
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

    /// A venue whose every read signs reports the load it needs credentials for, and
    /// one whose reads are public never does.
    ///
    /// This is the one read of the static table a caller outside this crate has: the
    /// interface that offers a load asks it before starting one, and what it answers is
    /// the definition's own requirement beside the runtime snapshot's stored set. The
    /// three market participants answer `false` whether or not anything is stored — their
    /// directories are public — while the venue whose market is behind its credentials
    /// answers `true` until a complete set is stored and `false` once it is.
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
        settings.credentials = LongbridgeCredentials {
            app_key: SmolStr::new_static("app-key"),
            app_secret: SmolStr::new_static("app-secret"),
            access_token: SmolStr::new_static("access-token"),
        };
        let snapshot = Arc::new(MarketParticipantNetworkSnapshot::new(config));
        let notification = ConfigNotification::new(snapshot);
        let configured = MarketParticipantsConnector::new(
            MarketParticipantsConnectorInput::new(
                Arc::new(TransportHub::new(TransportPolicy::default())),
                notification,
            )
            .with_runtime(Runtime::for_test()),
        )
        .expect("the root builds from typed runtime inputs");

        assert!(
            !configured.load_needs_credentials(VenueId::Longbridge),
            "a complete set is what makes the load worth offering"
        );

        unconfigured.shutdown().await;
        configured.shutdown().await;
    }

    /// How long one publication may take to reach the pool.
    ///
    /// The invalidation task is one runtime hop away from the interface, so the bound is
    /// generous; the wait itself is a poll that returns as soon as the connection it
    /// watches closes, and this is only what fails a publication that never arrives.
    const INVALIDATION_BOUND: Duration = Duration::from_secs(5);

    /// The key of one venue connection, carrying what the snapshot serves its scope now.
    ///
    /// The origin and the route generation are the transport dimensions invalidation
    /// re-reads off the published snapshot, so a key built from the installed one is
    /// exactly what a later publication can move.
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

    /// Waits for the invalidation task to close one pooled connection.
    ///
    /// The task runs on the application runtime rather than on the executor that owns the
    /// test, so the wait is a bounded poll on that runtime: it returns as soon as the task
    /// closed the connection, and names the connection that stayed open when no
    /// publication reached it.
    fn wait_for_close(runtime: &Runtime, connection: &Connection, subject: &str) {
        let closed = runtime.handle().block_on(async {
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

    /// The configuration notification has exactly one consumer — the task `install`
    /// starts — and a publication that moves a venue closes the pooled connections of
    /// that venue and no other.
    ///
    /// Both entries are built from what the installed snapshot serves, so whether each
    /// survives is decided by the projection the bridge publishes. The first write moves
    /// OKX's region: the OKX connection closes, which is how the test knows the task
    /// consumed the publication, and Binance's stays open, which is the negative an
    /// invalidation that closed every venue would fail. The second write moves Binance's
    /// stream origin, and Binance's connection closes with it.
    #[gpui_kit::test]
    fn installing_the_root_starts_the_task_that_invalidates_connections(cx: &mut TestAppContext) {
        cx.update(|cx| {
            global::install(cx).expect("the application Tokio runtime installs");
            let manager = ConfigManager::in_memory(Config::default(), cx);
            cx.set_global(manager);
            MarketParticipantsConnector::install(cx)
                .expect("the root installs over the installed configuration manager");
        });
        let connector = cx.update(|cx| cx.global::<MarketParticipantsConnector>().clone());
        let runtime = cx.update(|cx| global::handle(cx));
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
        let (binance_connection, okx_connection) = runtime.handle().block_on(async {
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

        // A write that names OKX's region closes the OKX connection and leaves Binance's:
        // the task demonstrably consumed the publication, and it invalidated only the
        // venue the publication moved.
        cx.update_global::<ConfigManager, _>(|manager, cx| {
            let landed = manager.update(
                |config| config.set_okx_region(&okx_venue, OkxRegion::Europe),
                cx,
            );
            assert!(landed, "the configured OKX row takes a service region");
        });
        wait_for_close(
            &runtime,
            &okx_connection,
            "the OKX connection of the moved region",
        );
        assert!(
            !binance_connection.is_closed(),
            "a configuration write that did not name Binance kept its connection open"
        );

        // A write that names the venue itself closes that venue's connection.
        cx.update_global::<ConfigManager, _>(|manager, cx| {
            let landed = manager.update(
                |config| {
                    config.set_binance_market_origins(
                        &binance_venue,
                        BinanceMarket::Spot,
                        &BinanceOrigins::new(
                            BinanceMarket::Spot,
                            BinanceHttpOrigin::Api,
                            BinanceWebsocketOrigin::StreamAlt,
                        ),
                    )
                },
                cx,
            );
            assert!(
                landed,
                "the configured Binance spot market takes its other stream"
            );
        });
        wait_for_close(
            &runtime,
            &binance_connection,
            "the Binance connection of the moved stream origin",
        );

        runtime.handle().block_on(connector.shutdown());
    }
}
#[cfg(test)]
mod venue_connection_tests {
    //! Unit tests of loading one venue.
    //!
    //! What a load commits is one candidate set per venue: the venue's statically
    //! declared routes, their catalog source bindings and their registered catalog
    //! routes become visible together, the venues are independent of one another, and
    //! a first load that cannot finish revokes the slot it started so a registered
    //! venue always has all of its catalogs loaded.

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

    /// A discovery conversation is bounded so a test cannot hang on a dead origin.
    const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);

    /// The Binance venue identity.
    fn binance() -> VenueId {
        VenueId::Binance
    }

    /// The Hyperliquid venue identity.
    fn hyperliquid() -> VenueId {
        VenueId::Hyperliquid
    }

    /// The route of one builder-deployed perpetual DEX scope.
    fn deployed_route(name: &str) -> VenueRoute {
        VenueRoute::new(
            hyperliquid(),
            VenueScopeId::new(smol_str::format_smolstr!("perp-dex/{name}"))
                .expect("a deployed scope name is a canonical identifier"),
        )
    }

    /// A root whose catalog wiring is attached and nothing else.
    fn connector() -> MarketParticipantsConnector {
        let notification =
            ConfigNotification::new(Arc::new(MarketParticipantNetworkSnapshot::default()));
        crate::test_support::connector_over(notification, TransportPolicy::default()).0
    }

    /// A root whose Hyperliquid published addresses resolve at a local fixture.
    ///
    /// Where a request goes is the venue's own settings, so a test decides it through the venue
    /// the request belongs to: pointing the venue at a socket nobody listens on is what makes a
    /// discovery fail before the wire. The configuration the root is built over carries those
    /// addresses, so there is nothing to hold beside it.
    fn discovery_connector(origin: &str, policy: TransportPolicy) -> MarketParticipantsConnector {
        let origin: url::Url = origin.parse().expect("the fixture origin parses");
        let fixture = fixture::venues([(hyperliquid(), FixtureAddress::at(&origin))]);
        let notification = ConfigNotification::new(Arc::new(
            MarketParticipantNetworkSnapshot::new(fixture_config(&fixture)),
        ));
        crate::test_support::connector_over(notification, policy).0
    }

    /// Registering a venue publishes its declared routes, their catalog source
    /// bindings and their registered catalog routes in one commit, and answers the
    /// read handle every capability reads the directory through.
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
        // The catalog itself is as usable as any other route's: the source is bound in
        // the catalogue and the route is registered there as a catalog
        // route.
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

    /// One venue's registration leaves every other venue's slot exactly as it was.
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

    /// An empty registry holds no route and no catalog source binding at all: that is
    /// the state a program starts in.
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

    /// A first load that cannot read the venue's market list revokes the slot it
    /// published: the registry never holds a venue whose catalogs were not loaded.
    ///
    /// The discovery conversation is addressable end to end — the request is resolved to
    /// the venue's adapter and dialed on the venue configuration's own origin, with no
    /// route published first — so an origin nothing answers reports the connection
    /// failure rather than a capability, route or registry failure.
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
    //! Unit tests of what loading a venue does to the directory over time.
    //!
    //! A load is the only thing that fetches a catalog, so these tests drive it: a
    //! refresh replaces the snapshots of the venue it refreshes, a refresh that
    //! discovers fewer scopes revokes the scopes that disappeared together with their
    //! catalogs, a refresh that fails leaves every committed route and every cached
    //! snapshot as it was, and two venues loaded at once keep their own routes and their
    //! own catalogs whichever of them commits first.
    //!
    //! The venue behind the requests is the catalogue fixture of this crate's test
    //! support — a real socket answering each venue's own protocol — so every request
    //! travels through the real gateway, registry and adapter dispatch, and a test
    //! decides per catalog route what the venue answers, refuses or holds back.

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

    /// One venue conversation is bounded so a test cannot hang on a fixture that never
    /// answers.
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

    /// One built-in venue identity.
    fn venue(name: &str) -> VenueId {
        VenueId::from_name(name).expect("the built-in venue identity is canonical")
    }

    /// The route of one built-in scope.
    fn route(venue: &VenueId, scope: &str) -> VenueRoute {
        VenueRoute::new(
            *venue,
            VenueScopeId::new(scope).expect("the built-in scope is canonical"),
        )
    }

    /// A root whose Binance and OKX catalogues are the fixture.
    fn connector_with(fixture: &CatalogFixture) -> (MarketParticipantsConnector, Arc<Catalog>) {
        fixture_connector(fixture.origin(), fixture.origin())
    }

    /// The loaded scopes of the root, as `(venue, scope)` pairs in sorted order.
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

    /// The native symbols one route publishes, in the order its venue listed them.
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

    /// The snapshots one venue holds in the directory right now, route by route.
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

    /// A refresh replaces the catalogs of the venue it refreshes.
    ///
    /// The venue answers the second load with a market list the first one did not carry,
    /// so the snapshot the directory serves afterwards can only come from the refresh: a
    /// refresh that served the cache, or that committed nothing, would leave the first
    /// answer in place.
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

        // The venue lists a second pair from here on, and only the refresh asks it again.
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

    /// A refresh that cannot read a catalog changes nothing that was already committed:
    /// the venue keeps its slot, and every snapshot it holds is the one it held.
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

        // The venue refuses the first catalog the route order reaches, so no catalog of
        // this load can be committed at all.
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

    /// Two venues loaded at once each end up registered with their own routes and their
    /// own catalogs, whichever of them commits first.
    ///
    /// The second venue's catalog request is held back, so the first venue finishes while
    /// the second is mid-load and the second commits into a registry that already holds
    /// the first: a commit that assembled its candidate from the whole registry instead
    /// of its own slot would lose the venue that finished first, and one that overwrote
    /// the other's catalogs would be read back here.
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

        // Nothing of the venue that is still mid-load is visible yet: its routes,
        // its catalogs and its metadata are committed together, so a reader sees
        // the venue that already committed and nothing of the one being loaded.
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

    /// a scope the venue stops publishing is revoked by the refresh that
    /// discovered fewer scopes, and stays revoked.
    ///
    /// One load is one reading of the venue's tables, so the fixture answers the
    /// first load with two builder-deployed DEXs and every later one with the single
    /// DEX the venue still publishes: the second load's candidate set is strictly
    /// smaller than the set the first load committed. The
    /// disappeared scope must be gone from the registry, from the catalog source
    /// bindings and from the directory in that same commit, while the scope the
    /// venue still publishes keeps its route and its catalog.
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

        // The venue publishes one DEX from here on, so this load's candidate set is
        // smaller than the one the first load committed.
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

        // The venue still publishes one DEX, so the next refresh cannot bring the
        // revoked scope back: nothing but a discovery that names it again would.
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

    /// A commit is visible only when it succeeds: while its stage is held,
    /// every reader — the venue table, the route registry and the directory —
    /// still answers the state before it.
    #[tokio::test]
    async fn a_load_commit_is_invisible_until_it_succeeds() {
        let fixture = CatalogFixture::start().await;
        let (connector, catalog) = connector_with(&fixture);
        let okx = venue("okx");
        let swap = route(&okx, "swap");

        // The venue holds the swap catalog, so the load is stopped inside its stage.
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

        // The same holds for a refresh: while its stage is held, every reader
        // still answers the commit before it — the previous entry, the previous
        // generation it names and the previous catalogs.
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

    /// A first load that fails leaves nothing behind: no venue entry, no
    /// registered route, no bound source and no published snapshot.
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

    /// A load whose reading cannot verify a deployment the venue named fails whole
    /// the error names the deployment, and nothing of that venue — no
    /// entry, no scope, no route binding, no snapshot, no identity — is left behind.
    ///
    /// The venue names two deployments and one of them publishes coins that are not
    /// its own. Registering the other would publish a scope set short of what the
    /// venue said, so the load reports the deployment instead of skipping it, and
    /// because the failure happens while staging, the commit never runs.
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

    /// A load whose venue names more deployments than one reading verifies fails
    /// whole the error carries the count and the bound, and nothing of that
    /// venue is left behind.
    ///
    /// The bound is this build's own answer about how much work one remote answer may
    /// cause. It is decided from the deployment table alone — the fixture is asked for
    /// `perpDexs` and nothing else — so a venue over it costs one request and is
    /// refused rather than truncated.
    ///
    /// One of the names is deliberately outside the scope shape the venue reserves:
    /// the bound counts **every** name the venue published, so a name refused for its
    /// shape cannot slip out of the count the refusal reports.
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
        // The one name of this fixture that has no scope shape.
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

    /// A deployment whose name has no scope shape this build reserves fails the
    /// whole load.
    ///
    /// The venue reserves two-to-four lowercase letters for a deployed scope, so a
    /// venue naming anything else publishes a deployment this build cannot address.
    /// The name is read and counted like every other one: a name dropped before the
    /// reading would leave a load successful with that scope silently missing, and
    /// would not even be part of the bound the reading checks.
    #[tokio::test]
    async fn a_deployment_outside_the_scope_shape_fails_the_whole_load() {
        let (origin, _requests, server) = info_fixture_server(
            vec![r#"[null,{"name":"abcde"}]"#.to_owned()],
            spot_meta_answer(&["USD", "USDC"]),
            dex_meta(&["BTC"]),
            // The metadata itself verifies: only the scope name is out of shape.
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

    /// A deployment whose collateral is not a spot token fails the whole load
    /// it is reported like any other unverifiable deployment.
    ///
    /// The failure is the venue's own answer rather than a transport one — its
    /// metadata names a collateral index nothing resolves — so it is the reading's
    /// refusal, not the conversation's, that the load reports. The deployment's name
    /// is deliberately one the venue reserves a scope shape for: a name outside the
    /// shape is refused earlier, and a fixture with one would pin that refusal while
    /// claiming to pin this one.
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

    /// The core scope's own collateral failing to resolve is a discovery failure:
    /// the core scope is not a deployment, so there is no deployment to name
    /// and nothing to leave out — the venue's own metadata does not decode.
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

    /// Asserts that a load of one venue committed nothing at all.
    ///
    /// The venue table holds no entry for it, no scope is in play, and the directory
    /// published neither a route binding, a source nor a snapshot for any of the
    /// deployments the reading named — and no identity was installed with them.
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

    /// Asserts one scope left no trace in the published directory.
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

    /// A refresh that fails leaves the previous commit byte for byte as it was:
    /// the entry, the generation it pins, its metadata and its revision are
    /// the values the last successful commit published.
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

    /// A refresh that discovers fewer scopes drops the vanished scope, its catalog
    /// and its metadata in the same commit, and keeps the survivors as they were.
    #[tokio::test]
    async fn a_shrinking_refresh_drops_the_vanished_scopes_catalogs_and_metadata() {
        let both = r#"[null,{"name":"xyz"},{"name":"abc"}]"#.to_owned();
        let one = r#"[null,{"name":"xyz"}]"#.to_owned();
        let (origin, _requests, server) = info_fixture_server(
            // One load is one reading: the first names two deployments, the refresh one.
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

    /// Two venues loaded at once do not interfere: each ends up with its own
    /// scopes, its own revision and its own catalogs.
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

    /// Two loads of one venue that overlap commit once: the second waits for
    /// the first and is answered by it, so the revision advances exactly one.
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

    /// A load cancelled mid-stage leaves nothing: the stage stops before
    /// its next step and answers [`VenueLoadError::Interrupted`].
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

    /// The report names the stage that failed and, where a stage is per scope, the
    /// scope it failed on.
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

        // A discovery that never reaches the venue is the discovery stage's own
        // failure, and it names the venue rather than a scope: nothing of the
        // venue's scope set is known when it fails.
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
    //! Unit tests of where one request goes.
    //!
    //! A request reaches the origin its own venue's configuration gives the scope, and
    //! a scope the configuration does not name — a deployed perpetual DEX most of all —
    //! is reached through the same venue's default. Nothing is inferred from a venue
    //! name: the fixture answers on two sockets, and the test moves the venue's
    //! configuration from one to the other between two identical requests.

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

    /// One venue conversation is bounded so a test cannot hang on a fixture that never
    /// answers.
    const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

    /// The route of the deployed scope both requests are about.
    fn deployed_route() -> VenueRoute {
        VenueRoute::new(
            VenueId::Hyperliquid,
            VenueScopeId::new("perp-dex/xyz").expect("the deployed scope name is canonical"),
        )
    }

    /// One HTTP info fixture answering the discovery conversation of `xyz`.
    async fn discovery_fixture() -> InfoFixture {
        info_fixture_server(
            vec![r#"[null,{"name":"xyz"}]"#.to_owned()],
            spot_meta_answer(&["USD", "USDC", "BTC"]),
            dex_meta(&["BTC"]),
            vec![("xyz".to_owned(), dex_meta(&["xyz:ABC"]))],
        )
        .await
    }

    /// Installs one Hyperliquid fixture whose requests are sent to `http` and whose
    /// streams are opened on `websocket`.
    ///
    /// A venue's two roles are one setting, so a fixture that serves one protocol per
    /// socket names them apart — the load of a venue is an HTTP conversation and its
    /// streams are WebSocket ones.
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

    /// A root whose Hyperliquid configuration is the snapshot `notification` carries.
    fn connector(
        notification: ConfigNotification<MarketParticipantNetworkSnapshot>,
    ) -> (
        MarketParticipantsConnector,
        Arc<crate::catalog::store::Catalog>,
    ) {
        crate::test_support::connector_over(notification, crate::test_support::plaintext_policy())
    }

    /// One fixture's request log, in arrival order.
    type RequestLog = Arc<Mutex<Vec<String>>>;

    /// How many requests of one `info` type one HTTP fixture has served.
    fn served(requests: &RequestLog, kind: &str) -> usize {
        requests
            .lock()
            .iter()
            .filter(|served| served.as_str() == kind)
            .count()
    }

    /// How many candle requests one socket fixture has received.
    fn candles(socket: &InfoSocket) -> usize {
        socket
            .frames()
            .iter()
            .filter(|frame| frame.contains("candleSnapshot"))
            .count()
    }

    /// The discovery conversation is dialed through the venue's own settings, and a
    /// scope the venue deploys is reached through the very same pair.
    ///
    /// A load asks what the venue deploys before any scope of it is registered, so the
    /// reading addresses the venue rather than a scope: it is dialed on the venue's
    /// primary scope — the one a venue that discovers what it deploys answers its own
    /// non-scoped conversations on. A scope the venue deploys answers the settings with
    /// no key of its own, so it is reached through that same pair rather than through an
    /// address beside it.
    ///
    /// The two requests after the load are what keep the first half from passing by
    /// accident: the very same configuration sends a `core-perp` candle request to the
    /// venue's stream socket and a deployed scope's candle request to the same socket, so
    /// the configuration the reading arrived through is proven live rather than merely
    /// written down.
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

    /// A scope the settings hold no key for is reached through its venue's own
    /// connection configuration.
    ///
    /// The venue is loaded over HTTP and its deployed scope inherits that configuration:
    /// the candle request for `perp-dex/xyz` arrives at the venue's own stream socket,
    /// because the scope is not a connection of its own. Moving the venue's streams to a
    /// second fixture and issuing the same request again sends it there instead — the
    /// second half is what keeps the first from being met by a request that never left
    /// the process.
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

        // The deployed scope's own listing, as the catalogue the load published holds it.
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

        // The fixture answers the candle request with an empty set, so the call itself
        // succeeds: what this test reads is where the request went.
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

        // The venue's streams move to the second fixture, and the same request has to
        // follow them: the settings are where a deployed scope's connection comes from.
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

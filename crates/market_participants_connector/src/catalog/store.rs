use std::sync::Arc;

use market_participants_model::{
    display::values::InstrumentDisplayString,
    identifier::NativeSymbol,
    listing::{
        native::{ListingIdentifier, OutcomeEventIdentifier, OutcomeGroupIdentifier},
        route::{VenueListingKey, VenueRoute},
    },
    outcome::{OutcomeEvent, OutcomeEventId, OutcomeGroup, OutcomeGroupId},
    routing::{ListingIdentifierKind, SymbolNormalization},
};
use parking_lot::RwLock;
use smol_str::SmolStr;
use tokio::runtime::Handle;

#[cfg(any(test, feature = "test-support"))]
use crate::catalog::{
    batch::CatalogBatch,
    error::SourceError,
    registry::staging::CatalogStagingHandle,
    source::{CatalogFetchContext, CatalogSource},
};
use crate::catalog::{
    batch::{CatalogEntry, CatalogPending},
    display::{catalog::DisplaySnapshot, types::DisplayError},
    error::{CatalogError, ListingInstrumentError},
    generation::{CatalogGeneration, CatalogGenerationId, CatalogGenerationPublisher},
    resolved::{ResolvedListing, ResolvedListingRef},
    source::{
        CatalogRouteRegistration, CatalogRouteRequirements, CatalogRouteSet, CatalogRouteUpdate,
        CatalogSourceBinding, CatalogSourceRegistry, RegisteredCatalogRoute,
    },
};

pub struct Catalog {
    shared: Arc<CatalogShared>,
}

struct CatalogShared {
    routes: RwLock<CatalogSourceRegistry>,
    publication: RwLock<Arc<CatalogGeneration>>,
    publisher: CatalogGenerationPublisher,
    runtime: RwLock<Option<Handle>>,
}

impl Catalog {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(CatalogShared::new()),
        }
    }

    pub(crate) fn set_runtime(&self, runtime: Handle) {
        *self.shared.runtime.write() = Some(runtime);
    }

    pub fn generation(&self) -> Arc<CatalogGeneration> {
        Arc::clone(&self.shared.publication.read())
    }

    pub fn cached_display(&self) -> DisplaySnapshot {
        DisplaySnapshot::new(Arc::clone(&self.shared.publication.read()))
    }

    pub fn listing_instrument(
        &self,
        listing: &VenueListingKey,
    ) -> Result<InstrumentDisplayString, ListingInstrumentError> {
        self.listings(&listing.route)?;
        let captured = self.cached_display();
        captured.listing_instrument(listing).map_err(Into::into)
    }

    pub fn cached_listing_instrument(
        &self,
        listing: &VenueListingKey,
    ) -> Result<InstrumentDisplayString, DisplayError> {
        self.cached_display().listing_instrument(listing)
    }

    pub(crate) fn with_generation<T>(&self, lookup: impl FnOnce(&CatalogGeneration) -> T) -> T {
        let publication = self.shared.publication.read();
        lookup(&publication)
    }

    pub fn generation_id(&self) -> CatalogGenerationId {
        self.shared.publication.read().id()
    }

    pub fn route_set(&self) -> Option<crate::catalog::source::CatalogRouteSet> {
        self.shared.routes.read().route_set().cloned()
    }

    pub fn is_registered(&self, route: &VenueRoute) -> bool {
        self.shared.routes.read().is_registered(route)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn install_registries(
        &self,
        assets: Arc<crate::catalog::registry::asset::AssetRegistry>,
    ) -> Result<CatalogGenerationId, CatalogError> {
        let _routes = self.shared.routes.read();
        let mut publication = self.shared.publication.write();
        let base: Arc<CatalogGeneration> = Arc::clone(&publication);
        let references = Arc::clone(base.references());
        let outcomes = Arc::clone(base.outcomes());
        let next = base.as_ref().clone().with_registries(
            assets,
            references,
            outcomes,
            &self.shared.publisher,
        )?;
        let id = next.id();
        *publication = Arc::new(next);
        Ok(id)
    }

    pub fn source_of(&self, route: &VenueRoute) -> Option<CatalogSourceBinding> {
        self.shared.routes.read().source_of(route).cloned()
    }

    pub fn registered_route(
        &self,
        route: &VenueRoute,
    ) -> Result<RegisteredCatalogRoute, CatalogError> {
        self.shared.routes.read().require_registered_route(route)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn commit_fixture_routes(
        &self,
        update: CatalogRouteUpdate,
        catalog_routes: &CatalogRouteRequirements,
    ) -> Result<CatalogGenerationId, CatalogError> {
        self.shared
            .commit_candidate(update, catalog_routes, &self.shared.publisher)
    }

    pub(crate) fn plan_venue_commit(
        &self,
        venue: &market_participants_model::identifier::VenueId,
        registrations: Option<Vec<CatalogRouteRegistration>>,
        catalog_routes: &CatalogRouteRequirements,
        bindings: Vec<CatalogSourceBinding>,
    ) -> Result<CatalogRouteSet, CatalogError> {
        let routes = self.shared.routes.read();
        let current = routes.route_set().cloned();
        CatalogShared::candidate_for_venue(
            &routes,
            current.as_ref(),
            venue,
            registrations,
            catalog_routes,
            bindings,
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn stage_fixture_listings(
        &self,
        route: &RegisteredCatalogRoute,
        listings: Vec<CatalogEntry>,
    ) -> Result<CatalogPending, CatalogError> {
        let base = self.generation();
        let staging =
            CatalogStagingHandle::new(crate::catalog::registry::staging::StagedRegistry::new(
                Arc::clone(base.assets()),
                Arc::clone(base.references()),
                Arc::clone(base.outcomes()),
            ));
        let context = CatalogFetchContext::new(route.clone(), staging);
        let batch = crate::catalog::test_support::fixture_batch(
            CatalogBatch {
                listings,
                ..CatalogBatch::default()
            },
            &context,
        );
        CatalogBatch::pending(&context, batch, &base)
            .map_err(|error| CatalogError::invalid_catalog(route.route().clone(), error))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn publish_venue_commit(
        &self,
        venue: &market_participants_model::identifier::VenueId,
        registrations: Option<Vec<CatalogRouteRegistration>>,
        catalog_routes: &CatalogRouteRequirements,
        pending: Vec<CatalogPending>,
        bindings: Vec<CatalogSourceBinding>,
    ) -> Result<CatalogGenerationId, CatalogError> {
        self.commit_venue_with_visibility(
            venue,
            registrations,
            catalog_routes,
            pending,
            bindings,
            |_| {},
        )
    }

    pub(crate) fn commit_venue_with_visibility(
        &self,
        venue: &market_participants_model::identifier::VenueId,
        registrations: Option<Vec<CatalogRouteRegistration>>,
        catalog_routes: &CatalogRouteRequirements,
        pending: Vec<CatalogPending>,
        bindings: Vec<CatalogSourceBinding>,
        visible: impl FnOnce(&Arc<CatalogGeneration>),
    ) -> Result<CatalogGenerationId, CatalogError> {
        let id = self.shared.commit_venue(
            venue,
            registrations,
            catalog_routes,
            &pending,
            bindings,
            &self.shared.publisher,
            visible,
        )?;
        Ok(id)
    }

    pub fn listings(&self, route: &VenueRoute) -> Result<Arc<[CatalogEntry]>, CatalogError> {
        match self.cached(route) {
            Some(listings) => Ok(listings),
            None => Err(self.not_loaded(route)),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn publish_batch_for_test(
        &self,
        route: &VenueRoute,
        batch: CatalogBatch,
    ) -> Result<Arc<[CatalogEntry]>, CatalogError> {
        let context = self.fetch_context_for_test(route)?;
        let base = self.generation();
        let pending = CatalogBatch::pending(&context, batch, &base)
            .map_err(|error| CatalogError::invalid_catalog(route.clone(), error))?;
        self.commit_single_pending(route, pending)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn fetch_context_for_test(
        &self,
        route: &VenueRoute,
    ) -> Result<CatalogFetchContext, CatalogError> {
        let registered = self.registered_route(route)?;
        let base = self.generation();
        Ok(CatalogFetchContext::new(
            registered,
            CatalogStagingHandle::new(crate::catalog::registry::staging::StagedRegistry::new(
                Arc::clone(base.assets()),
                Arc::clone(base.references()),
                Arc::clone(base.outcomes()),
            )),
        ))
    }

    pub fn cached_listings(&self, route: &VenueRoute) -> Option<Arc<[CatalogEntry]>> {
        self.cached(route)
    }

    pub fn contains(&self, listing: &VenueListingKey) -> Result<bool, CatalogError> {
        self.listings(&listing.route)?;
        Ok(self.with_generation(|generation| {
            crate::catalog::generation::listing_entry(generation, listing).is_some()
        }))
    }

    pub fn listing(&self, listing: &VenueListingKey) -> Result<Arc<CatalogEntry>, CatalogError> {
        self.listings(&listing.route)?;
        self.with_generation(|generation| {
            crate::catalog::generation::resolve_listing_entry(generation, listing)
        })
    }

    pub fn cached_listing(&self, listing: &VenueListingKey) -> Option<Arc<CatalogEntry>> {
        self.with_generation(|generation| {
            crate::catalog::generation::listing_entry(generation, listing)
        })
    }

    pub fn listings_for(
        &self,
        instrument: &market_participants_model::settlement::InstrumentId,
    ) -> Arc<[VenueListingKey]> {
        self.shared
            .publication
            .read()
            .listings_for(instrument)
            .cloned()
            .unwrap_or_else(|| Arc::from(Vec::new()))
    }

    pub fn resolve_listing(
        &self,
        listing: &VenueListingKey,
    ) -> Result<ResolvedListing, CatalogError> {
        self.listings(&listing.route)?;
        self.with_generation(|generation| {
            crate::catalog::generation::resolve_listing(generation, listing)
        })
    }

    pub fn resolve_listing_ref(
        &self,
        listing: &VenueListingKey,
    ) -> Result<ResolvedListingRef, CatalogError> {
        self.listings(&listing.route)?;
        self.with_generation(|generation| {
            crate::catalog::generation::resolve_listing_ref(generation, listing)
        })
    }

    pub fn native_symbol(&self, listing: &VenueListingKey) -> Option<NativeSymbol> {
        self.with_generation(|generation| {
            crate::catalog::generation::listing_entry(generation, listing)?
                .listing
                .native_symbol
                .clone()
        })
    }

    pub fn resolve_native_symbol(
        &self,
        route: &VenueRoute,
        symbol: &str,
    ) -> Result<VenueListingKey, CatalogError> {
        let native = NativeSymbol::new(symbol).map_err(|_| CatalogError::NativeSymbolInvalid {
            route: route.clone(),
            symbol: SmolStr::new(symbol),
        })?;
        self.listings(route)?;
        let normalization = self.symbol_normalization(route)?;
        self.with_generation(|generation| {
            let snapshot = generation.route_snapshot(route).ok_or_else(|| {
                CatalogError::RouteNotRegistered {
                    route: route.clone(),
                }
            })?;
            crate::catalog::generation::resolve_native_symbol(snapshot, normalization, &native)
        })
    }

    pub(crate) fn symbol_normalization(
        &self,
        route: &VenueRoute,
    ) -> Result<SymbolNormalization, CatalogError> {
        Ok(self
            .registered_route(route)?
            .registration()
            .normalization_mode(ListingIdentifierKind::Symbol))
    }

    pub(crate) fn listing_identifier_normalization(
        &self,
        route: &VenueRoute,
        identifier: &ListingIdentifier,
    ) -> Result<SymbolNormalization, CatalogError> {
        Ok(self
            .registered_route(route)?
            .registration()
            .identifier_normalization_mode(identifier))
    }

    pub fn resolve_listing_identifier(
        &self,
        route: &VenueRoute,
        identifier: &ListingIdentifier,
    ) -> Result<VenueListingKey, CatalogError> {
        self.listings(route)?;
        let normalization = self.listing_identifier_normalization(route, identifier)?;
        self.with_generation(|generation| {
            let snapshot = generation.route_snapshot(route).ok_or_else(|| {
                CatalogError::RouteNotRegistered {
                    route: route.clone(),
                }
            })?;
            crate::catalog::generation::resolve_native_listing(snapshot, normalization, identifier)
        })
    }

    pub fn resolve_group_identifier(
        &self,
        route: &VenueRoute,
        identifier: &OutcomeGroupIdentifier,
    ) -> Result<OutcomeGroupId, CatalogError> {
        self.listings(route)?;
        self.with_generation(|generation| {
            let snapshot = generation.route_snapshot(route).ok_or_else(|| {
                CatalogError::RouteNotRegistered {
                    route: route.clone(),
                }
            })?;
            crate::catalog::generation::resolve_native_group(generation, snapshot, identifier)
        })
    }

    pub fn resolve_event_identifier(
        &self,
        route: &VenueRoute,
        identifier: &OutcomeEventIdentifier,
    ) -> Result<OutcomeEventId, CatalogError> {
        self.listings(route)?;
        self.with_generation(|generation| {
            let snapshot = generation.route_snapshot(route).ok_or_else(|| {
                CatalogError::RouteNotRegistered {
                    route: route.clone(),
                }
            })?;
            crate::catalog::generation::resolve_native_event(generation, snapshot, identifier)
        })
    }

    pub fn outcome_group(&self, group: &OutcomeGroupId) -> Option<Arc<OutcomeGroup>> {
        self.shared
            .publication
            .read()
            .outcomes()
            .group(group)
            .cloned()
            .map(Arc::new)
    }

    pub fn outcome_event_for(
        &self,
        event: &market_participants_model::identifier::PolymarketEventId,
    ) -> Option<OutcomeEventId> {
        self.generation().outcomes().native_event(event).cloned()
    }

    pub fn outcome_event(&self, event: &OutcomeEventId) -> Option<Arc<OutcomeEvent>> {
        self.shared
            .publication
            .read()
            .outcomes()
            .event(event)
            .cloned()
            .map(Arc::new)
    }

    pub fn ensure(&self, route: &VenueRoute) -> Result<(), CatalogError> {
        self.listings(route)?;
        Ok(())
    }

    fn cached(&self, route: &VenueRoute) -> Option<Arc<[CatalogEntry]>> {
        let routes = self.shared.routes.read();
        let revision = routes.route_entry(route)?.registration().revision();
        let publication = self.shared.publication.read();
        let snapshot = publication.route_snapshot(route)?;
        (snapshot.route_revision() == revision).then(|| Arc::clone(snapshot.entries()))
    }

    fn not_loaded(&self, route: &VenueRoute) -> CatalogError {
        if self.shared.routes.read().is_registered(route) {
            return CatalogError::CatalogNotLoaded {
                route: route.clone(),
            };
        }
        CatalogError::RouteNotRegistered {
            route: route.clone(),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) async fn refresh_for_test(
        &self,
        route: &VenueRoute,
    ) -> Result<Arc<[CatalogEntry]>, CatalogError> {
        let registered = self.registered_route(route)?;
        let source = self
            .source_of(route)
            .ok_or_else(|| CatalogError::RouteNotRegistered {
                route: route.clone(),
            })?;
        let base = self.generation();
        let staging =
            CatalogStagingHandle::new(crate::catalog::registry::staging::StagedRegistry::new(
                Arc::clone(base.assets()),
                Arc::clone(base.references()),
                Arc::clone(base.outcomes()),
            ));
        let context = CatalogFetchContext::new(registered.clone(), staging);
        let batch: CatalogBatch = self
            .fetch_from_source(source.source(), context.clone())
            .await
            .map_err(|error| CatalogError::source_failed(source.id().to_smol_str(), error))?;
        let pending = match CatalogBatch::pending(&context, batch, &base) {
            Ok(pending) => pending,
            Err(error) => {
                return Err(CatalogError::invalid_catalog(route.clone(), error));
            }
        };
        self.commit_single_pending(route, pending)
    }

    #[cfg(any(test, feature = "test-support"))]
    async fn fetch_from_source(
        &self,
        source: &Arc<dyn CatalogSource>,
        context: CatalogFetchContext,
    ) -> Result<CatalogBatch, SourceError> {
        let Some(runtime) = self.shared.runtime.read().clone() else {
            return source.fetch_catalog(context).await;
        };
        runtime
            .spawn(source.fetch_catalog(context))
            .await
            .unwrap_or(Err(SourceError::Lost))
    }

    #[cfg(any(test, feature = "test-support"))]
    fn commit_single_pending(
        &self,
        route: &VenueRoute,
        pending: CatalogPending,
    ) -> Result<Arc<[CatalogEntry]>, CatalogError> {
        let listings = Arc::clone(pending.entries());
        let requirements = self.current_requirements();
        self.publish_venue_commit(&route.venue, None, &requirements, vec![pending], Vec::new())?;
        Ok(listings)
    }

    #[cfg(any(test, feature = "test-support"))]
    fn current_requirements(&self) -> CatalogRouteRequirements {
        self.shared
            .routes
            .read()
            .route_set()
            .map(|set| {
                CatalogRouteRequirements::from_catalog_routes(set.catalog_routes().iter().cloned())
            })
            .unwrap_or_default()
    }
}

impl CatalogShared {
    fn new() -> Self {
        let publisher = CatalogGenerationPublisher::new();
        let publication = Arc::new(CatalogGeneration::empty(&publisher));
        Self {
            routes: RwLock::new(CatalogSourceRegistry::new()),
            publication: RwLock::new(publication),
            publisher,
            runtime: RwLock::new(None),
        }
    }

    fn commit_venue(
        &self,
        venue: &market_participants_model::identifier::VenueId,
        registrations: Option<Vec<CatalogRouteRegistration>>,
        catalog_routes: &CatalogRouteRequirements,
        pending: &[CatalogPending],
        bindings: Vec<CatalogSourceBinding>,
        publisher: &CatalogGenerationPublisher,
        visible: impl FnOnce(&Arc<CatalogGeneration>),
    ) -> Result<CatalogGenerationId, CatalogError> {
        let mut routes = self.routes.write();
        let mut publication = self.publication.write();
        let current = routes.route_set().cloned();
        let candidate = CatalogShared::candidate_for_venue(
            &routes,
            current.as_ref(),
            venue,
            registrations,
            catalog_routes,
            bindings,
        )?;
        for staged in pending {
            let registered = candidate.registered_route(staged.route()).ok_or_else(|| {
                CatalogError::RouteNotRegistered {
                    route: staged.route().clone(),
                }
            })?;
            if registered.route_revision() != staged.revision() {
                return Err(CatalogError::RouteRevisionChanged {
                    route: staged.route().clone(),
                    staged: Box::new(staged.revision()),
                    current: Box::new(registered.route_revision()),
                });
            }
        }
        let revoked: Vec<VenueRoute> = current
            .as_ref()
            .map(|set| {
                set.catalog_routes()
                    .iter()
                    .filter(|route| {
                        route.venue == *venue && !candidate.routes().contains_key(*route)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let next = Arc::new(publication.with_venue_commit(pending, &revoked, publisher)?);
        let id = next.id();
        routes.commit(candidate);
        *publication = Arc::clone(&next);
        visible(&next);
        Ok(id)
    }

    #[cfg(any(test, feature = "test-support"))]
    fn commit_candidate(
        &self,
        update: CatalogRouteUpdate,
        catalog_routes: &CatalogRouteRequirements,
        publisher: &CatalogGenerationPublisher,
    ) -> Result<CatalogGenerationId, CatalogError> {
        let mut routes = self.routes.write();
        let mut publication = self.publication.write();
        let current = routes.route_set().cloned();
        let candidate = routes.plan(update, catalog_routes)?;
        let revoked: Vec<VenueRoute> = current
            .as_ref()
            .map(|set| {
                set.catalog_routes()
                    .iter()
                    .filter(|route| !candidate.routes().contains_key(*route))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let next = Arc::new(publication.with_venue_commit(&[], &revoked, publisher)?);
        let id = next.id();
        routes.commit(candidate);
        if !revoked.is_empty() {
            *publication = Arc::clone(&next);
        }
        Ok(id)
    }

    fn candidate_for_venue(
        registry: &CatalogSourceRegistry,
        current: Option<&CatalogRouteSet>,
        venue: &market_participants_model::identifier::VenueId,
        registrations: Option<Vec<CatalogRouteRegistration>>,
        catalog_routes: &CatalogRouteRequirements,
        bindings: Vec<CatalogSourceBinding>,
    ) -> Result<CatalogRouteSet, CatalogError> {
        let _ = catalog_routes;
        let mut sources: Vec<CatalogSourceBinding> = bindings;
        let mut routes: Vec<CatalogRouteRegistration> = Vec::new();
        if let Some(current) = current {
            for binding in current.sources().values() {
                if !sources
                    .iter()
                    .any(|candidate| candidate.id() == binding.id())
                {
                    sources.push(binding.clone());
                }
            }
            for (route, entry) in current.routes() {
                match &registrations {
                    Some(_) if route.venue == *venue => {}
                    _ => routes.push(entry.as_registration()),
                }
            }
        }
        match registrations {
            Some(registrations) => routes.extend(registrations),
            None => {}
        }
        let claimed = CatalogRouteRequirements::new(routes.iter().flat_map(|registration| {
            registration
                .capabilities()
                .iter()
                .map(|capability| (registration.route().clone(), *capability))
        }));
        let update = CatalogRouteUpdate::new(sources, routes);
        registry.plan(update, &claimed)
    }
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
}

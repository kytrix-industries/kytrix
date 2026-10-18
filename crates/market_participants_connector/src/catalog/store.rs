//! The shared catalogue and its publication rules.
//!
//! The catalogue is one published generation plus the registered sources and
//! routes that may replace it. Every read here is answered by the published
//! [`CatalogGeneration`], which is one immutable value behind one pointer, and a
//! route with no published snapshot is answered with
//! [`CatalogError::CatalogNotLoaded`] rather than by a request: nothing in this
//! module fetches. A catalogue is only ever written through the crate-private
//! verbs below, and the one caller is the venue load transaction of
//! [`crate::connector`].

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
use widget_tokio_runtime::runtime::Runtime;

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

/// The shared catalogue: one published generation plus the registered sources
/// and routes that may replace it.
///
/// The type is public because its reads are: every reader — a widget, a pane, a
/// background service — holds an `Arc<Catalog>` and answers from the published
/// generation. Every verb that changes what it holds is crate-private, so the
/// value a caller outside this crate can name is a read-only view: the only way
/// to reach any of them is the venue load transaction of
/// [`crate::connector::MarketParticipantsConnector`], which this crate owns alone.
pub struct Catalog {
    shared: Arc<CatalogShared>,
}

/// Everything a fetch touches.
struct CatalogShared {
    /// The registered catalog sources and routes. Held only for the short
    /// sections that read or replace the whole map; never across an await.
    routes: RwLock<CatalogSourceRegistry>,
    /// The published generation, replaced in one indivisible step.
    ///
    /// The pointer is what is replaced and what a read captures: a generation is
    /// immutable once built, so taking one out of the lock is an `Arc` clone
    /// rather than a copy of every map it holds, and a reader that keeps the
    /// pointer keeps reading the publication it captured whatever is published
    /// afterwards.
    publication: RwLock<Arc<CatalogGeneration>>,
    /// The one generation clock this catalog owns.
    publisher: CatalogGenerationPublisher,
    /// The runtime a fixture-driven fetch crosses its blocking boundary on.
    ///
    /// A fetch reaches the venue through the market-participants gateway, which resolves the
    /// route's proxy through [`network::proxy`]: that resolution must run its OS and
    /// PAC work on a real blocking pool, and a caller's thread that runs no runtime
    /// — a GPUI test's frame thread — has none to give it. The runtime therefore
    /// travels with the catalogue instead of being discovered on whichever thread
    /// happens to drive the fetch. Production fetches do not read it: the connection
    /// action drives a venue's own fetch on the load transaction's runtime.
    runtime: RwLock<Option<Runtime>>,
}

impl Catalog {
    /// Creates a catalogue with no registered source and an empty generation.
    pub fn new() -> Self {
        Self {
            shared: Arc::new(CatalogShared::new()),
        }
    }

    /// Attaches the runtime a fixture-driven fetch crosses its blocking boundary on.
    ///
    /// [`crate::connector::MarketParticipantsConnector::new`] calls this when it builds the
    /// catalogue, so a fetch driven through `Self::refresh_for_test` resolves the
    /// venue's proxy on the application's runtime instead of on the caller's
    /// executor. A catalogue without one — a unit test, a non-GPUI embedder — runs
    /// that fetch on the caller's executor, which is what such a caller has always
    /// done.
    ///
    /// The verb is crate-private like every other write on this catalogue: the
    /// type is public because its reads are, and no caller outside this crate can
    /// reach a state change through it.
    pub(crate) fn set_runtime(&self, runtime: Runtime) {
        *self.shared.runtime.write() = Some(runtime);
    }

    /// Returns the currently published generation.
    ///
    /// Every read of this value observes one whole generation: a route refresh,
    /// a revocation and the global identity index it belongs to are all replaced
    /// together. The answer is the published pointer itself, not a copy of what
    /// it holds, so a caller that keeps it keeps reading the publication it
    /// captured however many times the catalogue is published afterwards.
    pub fn generation(&self) -> Arc<CatalogGeneration> {
        Arc::clone(&self.shared.publication.read())
    }

    /// Captures the published generation one display read is answered from.
    ///
    /// The capture is the pointer the publication lock holds, so the whole
    /// operation reads one publication and no value of another generation can
    /// slip into it — see [`DisplaySnapshot`]. Nothing here fetches: a route that
    /// was never loaded is a snapshot with no display for its listings, which is
    /// answered as a missing listing rather than by a request.
    pub fn cached_display(&self) -> DisplaySnapshot {
        DisplaySnapshot::new(Arc::clone(&self.shared.publication.read()))
    }

    /// Reads `listing`'s name from the published generation, without fetching.
    ///
    /// The read is answered from the published generation alone: a listing whose
    /// route was never loaded is [`CatalogError::CatalogNotLoaded`],
    /// and the name is read from a single capture of the publication, so the
    /// listing and the instrument it implements belong to one generation.
    pub fn listing_instrument(
        &self,
        listing: &VenueListingKey,
    ) -> Result<InstrumentDisplayString, ListingInstrumentError> {
        self.listings(&listing.route)?;
        let captured = self.cached_display();
        captured.listing_instrument(listing).map_err(Into::into)
    }

    /// Reads one listing's name from the published generation, without fetching.
    ///
    /// A listing the published generation never named is
    /// [`DisplayError::ListingNotFound`]: the read names no generation of its
    /// own, so a listing that simply is not published is not a mismatch. A whole
    /// list of rows wants [`Self::cached_display`] instead, so that every row is
    /// named by one capture.
    pub fn cached_listing_instrument(
        &self,
        listing: &VenueListingKey,
    ) -> Result<InstrumentDisplayString, DisplayError> {
        self.cached_display().listing_instrument(listing)
    }

    /// Runs one resolution against the currently published generation.
    ///
    /// A resolution only reads the generation, so it is answered under the
    /// publication read lock instead of taking the pointer and resolving outside
    /// it. No other lock is taken while this one is held.
    pub(crate) fn with_generation<T>(&self, lookup: impl FnOnce(&CatalogGeneration) -> T) -> T {
        let publication = self.shared.publication.read();
        lookup(&publication)
    }

    /// Returns the id of the currently published generation.
    pub fn generation_id(&self) -> CatalogGenerationId {
        self.shared.publication.read().id()
    }

    /// Returns the registered catalog sources and routes.
    pub fn route_set(&self) -> Option<crate::catalog::source::CatalogRouteSet> {
        self.shared.routes.read().route_set().cloned()
    }

    /// Returns whether the route is a registered catalog route.
    pub fn is_registered(&self, route: &VenueRoute) -> bool {
        self.shared.routes.read().is_registered(route)
    }

    /// Replaces the canonical asset registry the published generation carries.
    ///
    /// The registry is created through the registry boundary; this only makes it
    /// part of a new generation. No route snapshot changes, but the publication is
    /// still a new one: the display catalog is rebuilt against the registries being
    /// installed under a fresh [`CatalogGenerationId`], and a publication the
    /// display gate refuses keeps the previous generation and returns the typed
    /// error.
    ///
    /// Nothing is signalled: a registry-only update publishes no route snapshot, so
    /// a subscriber that reads the catalogue again sees the same listings it saw
    /// before.
    ///
    /// This is a test seam and exists only in builds with this crate's
    /// `test-support` feature: a fixture stages the terms of the assets its own
    /// vocabulary cannot recover, while production only ever publishes a registry
    /// as part of the batch publication that carries the snapshots it was
    /// validated against.
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

    /// Returns the registered source of one route.
    pub fn source_of(&self, route: &VenueRoute) -> Option<CatalogSourceBinding> {
        self.shared.routes.read().source_of(route).cloned()
    }

    /// Returns the registered registration of one route.
    pub fn registered_route(
        &self,
        route: &VenueRoute,
    ) -> Result<RegisteredCatalogRoute, CatalogError> {
        self.shared.routes.read().require_registered_route(route)
    }

    /// Replaces the whole source and route maps in one atomic update.
    ///
    /// This is a test seam: a fixture that states a candidate route set drives the
    /// catalog's own validation rather than publishing beside it. The candidate is
    /// the complete state — a route absent from it is revoked, and the whole map is
    /// published as one generation — because that is the shape a fixture expresses:
    /// "these are the routes, and nothing else". Production never publishes this
    /// way; a venue commit is the one entry there.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn commit_fixture_routes(
        &self,
        update: CatalogRouteUpdate,
        catalog_routes: &CatalogRouteRequirements,
    ) -> Result<CatalogGenerationId, CatalogError> {
        self.shared
            .commit_candidate(update, catalog_routes, &self.shared.publisher)
    }

    /// Validates one venue's candidate commit without changing anything.
    ///
    /// The stage of a load validates the route set it is about to register and
    /// needs the registrations the commit will publish under — the input revision
    /// of every route, which is what a fetched batch is staged against. The
    /// candidate is assembled under one reading of the registry, so it is the same
    /// merge the commit will perform.
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

    /// Stages one fixture's listings as a route's pending snapshot.
    ///
    /// A fixture publishes the listings a venue fetch would have produced, through
    /// the same validation and staging as the fetched batch, so a fixture snapshot
    /// is the same kind of value a load commits. Nothing in production stages this
    /// way, which is why the verb exists only in builds with this crate's
    /// `test-support` feature.
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

    /// Publishes one venue's commit for a fixture, without a visibility callback.
    ///
    /// This is the fixture seam beside [`Self::commit_venue_with_visibility`]: the
    /// same commit, with `registrations` as the venue's whole candidate route set.
    /// `None` leaves the venue's registered routes exactly as they are, which is
    /// what a refresh of an already registered venue's catalogs commits. Nothing in
    /// production publishes through it — the connection action calls
    /// [`Self::commit_venue_with_visibility`] directly — which is why the verb
    /// exists only in builds with this crate's `test-support` feature.
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

    /// Publishes one venue's commit and hands the new generation to a visibility
    /// callback before the critical section ends.
    ///
    /// This is the one write entry of the catalogue: a venue's whole route set, its
    /// staged catalog batches and the derived content they belong to are committed
    /// in one critical section, so a reader observes either the previous commit or
    /// this one. The catalogue's own locks are taken in the fixed order
    /// **catalog route registry → publication** and released together; the caller
    /// holds its venue table's write lock — the first lock of the commit order — and
    /// a root that also writes its gateway's route registry takes that one in
    /// between.
    ///
    /// The venue table of the market participants root is replaced inside the same critical
    /// section as the publication pointer, so a reader that resolves the route set
    /// and then the directory never observes one commit's routes beside another
    /// commit's catalogs. The callback is the only way to reach that window, and it
    /// runs while both write locks are held.
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

    /// The catalog for `route`, from the published generation.
    ///
    /// The read never fetches: a route with no published snapshot is
    /// [`CatalogError::CatalogNotLoaded`], which is a waiting state a
    /// caller retries after the connection action has loaded the route, while a
    /// route that is not registered at all is
    /// [`CatalogError::RouteNotRegistered`]. The two are never
    /// collapsed into one answer: "this route has no catalog yet" and "this route
    /// publishes no catalog" are different things for a caller to do about.
    pub fn listings(&self, route: &VenueRoute) -> Result<Arc<[CatalogEntry]>, CatalogError> {
        match self.cached(route) {
            Some(listings) => Ok(listings),
            None => Err(self.not_loaded(route)),
        }
    }

    /// Publishes a batch a fixture built as `route`'s snapshot, without a fetch.
    ///
    /// The batch is validated and staged exactly as a fetched one is, so a
    /// fixture snapshot is the same kind of value a load publishes; only the
    /// source is missing. Nothing in production publishes this way, which is why
    /// the verb exists only in builds with this crate's `test-support` feature.
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

    /// The staging view one fixture batch of `route` is published through.
    ///
    /// A test seam like [`Self::publish_batch_for_test`]: a fixture that carries
    /// its own terms stages them against the same base a fetch would, so its
    /// snapshot is built by the catalog's own validation rather than beside it.
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

    /// Whatever is published for `route` right now, without loading anything.
    pub fn cached_listings(&self, route: &VenueRoute) -> Option<Arc<[CatalogEntry]>> {
        self.cached(route)
    }

    /// Whether the published generation holds the listing.
    ///
    /// The read never fetches: a listing of a route with no published snapshot is
    /// refused as [`CatalogError::CatalogNotLoaded`].
    pub fn contains(&self, listing: &VenueListingKey) -> Result<bool, CatalogError> {
        self.listings(&listing.route)?;
        Ok(self.with_generation(|generation| {
            crate::catalog::generation::listing_entry(generation, listing).is_some()
        }))
    }

    /// The entry of one listing, from the published generation.
    pub fn listing(&self, listing: &VenueListingKey) -> Result<Arc<CatalogEntry>, CatalogError> {
        self.listings(&listing.route)?;
        self.with_generation(|generation| {
            crate::catalog::generation::resolve_listing_entry(generation, listing)
        })
    }

    /// The entry of one listing from the published generation, without loading.
    pub fn cached_listing(&self, listing: &VenueListingKey) -> Option<Arc<CatalogEntry>> {
        self.with_generation(|generation| {
            crate::catalog::generation::listing_entry(generation, listing)
        })
    }

    /// Every listing of one canonical instrument across all published routes.
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

    /// The listing, its generation and its rules as one resolution result.
    ///
    /// The resolution is taken from one generation: the entry and the generation
    /// id it carries are read under the same publication, so a resolution never
    /// claims a generation its entry does not come from. Nothing here fetches: a
    /// route with no published snapshot is
    /// [`CatalogError::CatalogNotLoaded`].
    pub fn resolve_listing(
        &self,
        listing: &VenueListingKey,
    ) -> Result<ResolvedListing, CatalogError> {
        self.listings(&listing.route)?;
        self.with_generation(|generation| {
            crate::catalog::generation::resolve_listing(generation, listing)
        })
    }

    /// A protocol-facing projection of one listing.
    pub fn resolve_listing_ref(
        &self,
        listing: &VenueListingKey,
    ) -> Result<ResolvedListingRef, CatalogError> {
        self.listings(&listing.route)?;
        self.with_generation(|generation| {
            crate::catalog::generation::resolve_listing_ref(generation, listing)
        })
    }

    /// The native symbol of one listing, without loading its route.
    pub fn native_symbol(&self, listing: &VenueListingKey) -> Option<NativeSymbol> {
        self.with_generation(|generation| {
            crate::catalog::generation::listing_entry(generation, listing)?
                .listing
                .native_symbol
                .clone()
        })
    }

    /// Resolves a native symbol to its listing on one route.
    ///
    /// The symbol is normalized with the route's own rule for bare symbols
    /// before the lookup, so a route that declared ASCII case folding resolves
    /// `btcusdt` and `BTCUSDT` to the same listing and a route that declared
    /// exact matching keeps them apart. The answer is the one the resolver of
    /// [`crate::catalog::resolved::EnsuredRoute`] gives: both call
    /// `crate::catalog::generation::resolve_native_symbol` under the same rule.
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

    /// The symbol normalization rule of one registered route.
    ///
    /// This is the one place that decides which rule a bare native symbol is
    /// looked up under, so both symbol resolvers ask for it here instead of
    /// spelling the rule out on their own side.
    pub(crate) fn symbol_normalization(
        &self,
        route: &VenueRoute,
    ) -> Result<SymbolNormalization, CatalogError> {
        Ok(self
            .registered_route(route)?
            .registration()
            .normalization_mode(ListingIdentifierKind::Symbol))
    }

    /// The normalization rule of one typed listing identifier on a registered route.
    ///
    /// This is the one place that decides which rule a typed identifier is looked
    /// up under, at the typed-identifier and product granularity the route
    /// declared it at, so the index a route published and every lookup against it
    /// are folded alike.
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

    /// Resolves a typed listing identifier to its listing on one route.
    ///
    /// A scoped identifier — a Hyperliquid asset or outcome — only names a
    /// listing of this route's own scope, and a listing only another route
    /// publishes is refused. The identifier is folded with the route's own rule for
    /// its typed identifier and product before the lookup, so a spelling the route
    /// folds names the same listing as the one it was published under. The answer is
    /// the one the resolver of [`crate::catalog::resolved::EnsuredRoute`] gives:
    /// both call `crate::catalog::generation::resolve_native_listing`.
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

    /// Resolves a native outcome group identifier to its canonical group.
    ///
    /// The group must be reachable from *this* route's own index: either one of
    /// its native group lookups names it, or one of its listings trades a leg of
    /// the group. A scoped identifier only names a group of this route's own
    /// scope, and a group only another route publishes is refused. The answer is
    /// the one the resolver of
    /// [`crate::catalog::resolved::EnsuredRoute`] gives: both call
    /// `crate::catalog::generation::resolve_native_group`.
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

    /// Resolves a native outcome event identifier to its canonical event.
    ///
    /// The event must be reachable from *this* route's own index: either one of
    /// its native event lookups names it, or the route publishes the canonical
    /// event a lookup registered elsewhere names. An event only another route
    /// publishes is refused, and the answer is the one the resolver of
    /// [`crate::catalog::resolved::EnsuredRoute`] gives: both call
    /// `crate::catalog::generation::resolve_native_event`.
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

    /// Returns one already published outcome group.
    pub fn outcome_group(&self, group: &OutcomeGroupId) -> Option<Arc<OutcomeGroup>> {
        self.shared
            .publication
            .read()
            .outcomes()
            .group(group)
            .cloned()
            .map(Arc::new)
    }

    /// Returns the canonical event a venue-native event identifier names.
    ///
    /// The lookup is the generation's own: an event the published generation
    /// never registered answers `None`.
    pub fn outcome_event_for(
        &self,
        event: &market_participants_model::identifier::PolymarketEventId,
    ) -> Option<OutcomeEventId> {
        self.generation().outcomes().native_event(event).cloned()
    }

    /// Returns one already published outcome event index.
    pub fn outcome_event(&self, event: &OutcomeEventId) -> Option<Arc<OutcomeEvent>> {
        self.shared
            .publication
            .read()
            .outcomes()
            .event(event)
            .cloned()
            .map(Arc::new)
    }

    /// Asserts that `route` already has a published snapshot.
    ///
    /// The call never fetches: a route nothing has loaded is
    /// [`CatalogError::CatalogNotLoaded`], the same answer
    /// [`Self::listings`] gives. Loading a route is the connection action's
    /// alone.
    pub fn ensure(&self, route: &VenueRoute) -> Result<(), CatalogError> {
        self.listings(route)?;
        Ok(())
    }

    /// Returns what is published for a route right now, when anything is.
    ///
    /// The route map is read first, so the read path uses the same lock order as
    /// every publication path. A route that is no longer registered is *absent*,
    /// whatever the published generation still holds for it: between the route map
    /// replacement and the generation replacement the old snapshot must not be
    /// served as if the route were current. A snapshot published under another
    /// declaration of the same route is absent for the same reason: it was built
    /// under inputs the registration no longer carries, so serving it would answer
    /// a read with a catalog of the route the route used to be.
    fn cached(&self, route: &VenueRoute) -> Option<Arc<[CatalogEntry]>> {
        let routes = self.shared.routes.read();
        let revision = routes.route_entry(route)?.registration().revision();
        let publication = self.shared.publication.read();
        let snapshot = publication.route_snapshot(route)?;
        (snapshot.route_revision() == revision).then(|| Arc::clone(snapshot.entries()))
    }

    /// The refusal one read gives for a route it can serve nothing for.
    ///
    /// The two states are kept apart: a route that is not registered
    /// publishes no catalog at all, while a registered route with no snapshot is
    /// one the connection action has not loaded yet.
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

    /// Fetches `route` and commits the batch as the route's venue published
    /// it.
    ///
    /// This is a fixture seam: a test drives one route's own fetch and
    /// publication, which production reaches through the venue commit only.
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

    /// Asks one source for its catalogue, on the application runtime.
    ///
    /// A fetch reaches the venue through the market-participants gateway, which resolves the
    /// route's proxy through [`network::proxy`]: that resolution crosses into a
    /// blocking pool, and a caller's thread that runs no runtime — a GPUI task —
    /// has none to give it. Running the fetch on the runtime instead of on the
    /// caller's executor is therefore what makes a cold read work from a pane,
    /// and it is also what keeps the venue's IO off the frame thread.
    ///
    /// A catalogue with no runtime attached — a unit test, an embedder without
    /// GPUI — runs the fetch on the caller's executor, which is what such a
    /// caller has always done.
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
            .run(source.fetch_catalog(context))
            .await
            .unwrap_or(Err(SourceError::Lost))
    }

    /// Commits one staged batch as the route's venue published it.
    ///
    /// A fixture seam: the registration and the input revision the batch was
    /// staged under are checked again inside the venue commit, while the route
    /// registry and the published generation are both held.
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

    /// The route requirements the current registry already satisfies.
    ///
    /// A publication that changes no route set — a catalog refresh of an already
    /// registered venue — still states which of its routes must bind a catalog
    /// source, and the answer is the set the registry itself holds.
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
    /// Creates the empty shared state: no route, no source, one empty generation.
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

    /// Commits one venue's whole publication in one critical section.
    ///
    /// The lock order is fixed: the route registry is taken for writing first and
    /// the publication second, and both are released together. The caller already
    /// holds the venue table's write lock, and a root that also writes its gateway's
    /// route registry takes that one before this call, so the order **venue table →
    /// gateway route registry → catalog route registry → publication** holds, and no
    /// path in this module takes these two in the other order.
    ///
    /// Inside the section the candidate route set — every other venue's
    /// registrations plus this venue's, or this venue's unchanged when it published
    /// no route set — is validated and committed, every staged batch is checked
    /// against the registration it was staged for, the generation is built with the
    /// new snapshots installed and the vanished scopes removed, the publication
    /// pointer is replaced, and the new generation is handed to `visible` before
    /// the locks are released. Nothing here awaits.
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
        // Every staged batch is checked against the registration it was fetched
        // under: a batch whose route changed while it was staged is refused whole
        // rather than published under inputs it does not belong to.
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
        // The generation is built before anything is written: a publication the
        // display gate refuses leaves the route registry, the publication pointer
        // and every reader exactly as they were, rather than replacing the
        // route map with a candidate whose snapshots were never published.
        //
        // The order of the five steps below is the transaction contract, and
        // `commit_candidate` (the fixture seam) repeats it: keep the two in step.
        let next = Arc::new(publication.with_venue_commit(pending, &revoked, publisher)?);
        let id = next.id();
        routes.commit(candidate);
        *publication = Arc::clone(&next);
        visible(&next);
        Ok(id)
    }

    /// Commits a whole candidate route map as one publication.
    ///
    /// This is the fixture seam's commit: the candidate is the complete state, so a
    /// route it does not name is revoked together with its snapshot, and the whole
    /// publication is one generation. A venue commit does not go through here — it
    /// carries every other venue's registrations over instead of replacing them.
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
        // The generation is built before anything is written, exactly as a venue
        // commit does: a candidate whose publication the display gate refuses
        // leaves the route map, the pointer and every reader as they were,
        // rather than replacing the route map with snapshots nobody published.
        //
        // The order of these steps is the transaction contract, and `commit_venue`
        // (the venue commit) repeats it: keep the two in step.
        let next = Arc::new(publication.with_venue_commit(&[], &revoked, publisher)?);
        let id = next.id();
        routes.commit(candidate);
        if !revoked.is_empty() {
            *publication = Arc::clone(&next);
        }
        Ok(id)
    }

    /// Builds the candidate route set of one venue's commit.
    ///
    /// Every other venue's registrations are carried over as they stand, so two
    /// venues committing concurrently never overwrite one another: the merge
    /// happens inside the registry's write lock, never in a caller's snapshot. The
    /// registry is passed in rather than read: the commit already holds it for
    /// writing, and taking it again would deadlock.
    fn candidate_for_venue(
        registry: &CatalogSourceRegistry,
        current: Option<&CatalogRouteSet>,
        venue: &market_participants_model::identifier::VenueId,
        registrations: Option<Vec<CatalogRouteRegistration>>,
        catalog_routes: &CatalogRouteRequirements,
        bindings: Vec<CatalogSourceBinding>,
    ) -> Result<CatalogRouteSet, CatalogError> {
        // The caller set is only its own verdict while staging; the candidate
        // below is validated against the requirements derived from it.
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
            None => {
                // A venue that publishes no route set keeps the registrations it
                // holds; one that holds none is not registered at all.
            }
        }
        // The requirements are derived from the candidate itself: a set a caller
        // assembled while staging can be stale — another venue may have
        // committed since — and the candidate is what the commit publishes.
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

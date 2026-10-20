//! The published catalog generation and its indexes.
//!
//! A generation is immutable once published. Every index a consumer reads —
//! per-route listings, native identifiers, outcome relations, the global
//! instrument index and the display aliases — belongs to the same generation, so
//! a lookup can never observe a route that has been refreshed while the global
//! index still describes the previous snapshot. Publication is one atomic store
//! of the new generation, and the id of a generation comes from the single
//! publisher the whole catalog shares.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use ahash::{AHashMap, AHashSet};
use market_participants_model::{
    error::ModelError,
    identifier::NativeSymbol,
    listing::{
        native::{ListingIdentifier, OutcomeEventIdentifier, OutcomeGroupIdentifier},
        route::{VenueListingKey, VenueRoute},
        venue::HyperliquidAssetProduct,
    },
    outcome::{
        OutcomeEvent, OutcomeEventId, OutcomeGroup, OutcomeGroupId, OutcomeRiskPool,
        OutcomeRiskPoolId,
    },
    routing::{
        IdentifierNormalizationProduct, IdentifierNormalizationRule, ListingIdentifierKind,
        SymbolNormalization, normalization_mode,
    },
    settlement::{InstrumentId, InstrumentLineageEdge},
};

use crate::catalog::{
    batch::{CatalogEntry, CatalogPending},
    display::{
        catalog::{DisplayCatalog, DisplayResolver},
        publication::{PublicationInput, RouteDisplayMaterial, build_display_catalog},
        types::DisplayError,
    },
    error::{CatalogError, CatalogValidationError},
    registry::{asset::AssetRegistry, outcome::OutcomeRegistry, reference::ReferenceRegistry},
    resolved::{ResolvedListing, ResolvedListingRef},
    source::RouteInputRevision,
};

/// The identity of one published catalog generation.
///
/// The value identifies a runtime snapshot inside one catalog. It is not a
/// business identity and is never persisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CatalogGenerationId(u64);

impl CatalogGenerationId {
    /// Returns the numeric value of this generation.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for CatalogGenerationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "generation:{}", self.0)
    }
}

/// The process-wide generation clock.
///
/// A catalog owns exactly one [`CatalogGenerationPublisher`] and every handle
/// reading that catalog shares it. The counter itself is process-wide
/// rather than owned by one catalog, because a generation id must never be
/// handed out twice: two catalogs publishing at the same time would otherwise
/// both call their first generation `1` and mint ids that collide.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// The single generation clock of one catalog.
///
/// The value carries no state of its own: every publisher draws from the
/// process-wide counter, so ids are unique across every catalog in the process
/// while a catalog still shares one publisher with all of its handles.
#[derive(Clone, Copy, Debug, Default)]
pub struct CatalogGenerationPublisher;

impl CatalogGenerationPublisher {
    /// Creates a publisher.
    pub const fn new() -> Self {
        Self
    }

    /// Allocates the next generation id.
    pub fn publish(&self) -> CatalogGenerationId {
        CatalogGenerationId(NEXT_GENERATION.fetch_add(1, Ordering::AcqRel))
    }
}

/// The immutable content of one catalog at one instant.
#[derive(Clone, Debug)]
pub struct CatalogGeneration {
    id: CatalogGenerationId,
    route_snapshots: AHashMap<VenueRoute, Arc<RouteSnapshot>>,
    listings_by_instrument: AHashMap<InstrumentId, Arc<[VenueListingKey]>>,
    display_catalog: DisplayCatalog,
    outcome_events: Arc<OutcomeRegistry>,
    assets: Arc<AssetRegistry>,
    references: Arc<ReferenceRegistry>,
    instrument_lineage: Arc<[InstrumentLineageEdge]>,
    outcome_group_lineage: Arc<[market_participants_model::outcome::OutcomeGroupLineageEdge]>,
    outcome_risk_pool_lineage:
        Arc<[market_participants_model::outcome::OutcomeRiskPoolLineageEdge]>,
}

impl CatalogGeneration {
    /// Creates the empty first generation of a catalog.
    pub fn empty(publisher: &CatalogGenerationPublisher) -> Self {
        let route_snapshots: AHashMap<VenueRoute, Arc<RouteSnapshot>> = AHashMap::new();
        Self {
            id: publisher.publish(),
            route_snapshots,
            listings_by_instrument: AHashMap::new(),
            display_catalog: DisplayCatalog::default(),
            outcome_events: Arc::new(OutcomeRegistry::new()),
            assets: Arc::new(AssetRegistry::new()),
            references: Arc::new(ReferenceRegistry::new()),
            instrument_lineage: Arc::from(Vec::new()),
            outcome_group_lineage: Arc::from(Vec::new()),
            outcome_risk_pool_lineage: Arc::from(Vec::new()),
        }
    }

    /// Returns the identity of this generation.
    pub const fn id(&self) -> CatalogGenerationId {
        self.id
    }

    /// Replaces the registry maps of this generation and rebuilds its display.
    ///
    /// Used when canonical registries are published outside a route fetch. The
    /// generation id advances, because the publication is a new one: the display
    /// catalog is rebuilt against the registries being installed, and a
    /// publication whose display collides or whose metadata is missing is refused
    /// with the generation left exactly as it was.
    pub fn with_registries(
        self,
        assets: Arc<AssetRegistry>,
        references: Arc<ReferenceRegistry>,
        outcomes: Arc<OutcomeRegistry>,
        publisher: &CatalogGenerationPublisher,
    ) -> Result<Self, CatalogError> {
        // The display catalog carries the id of the generation it is published
        // in, so the id is drawn before the catalog is built and consumed by the
        // store: a refused build leaves the counter advanced and the published
        // generation untouched, and an id is never handed out twice.
        let id = publisher.publish();
        let display_catalog = build_display(&self.route_snapshots, &assets, &references, &outcomes)
            .map_err(|error| display_rejected(self.reporting_route(), error))?;
        Ok(Self {
            id,
            route_snapshots: self.route_snapshots,
            listings_by_instrument: self.listings_by_instrument,
            display_catalog,
            outcome_events: outcomes,
            assets,
            references,
            instrument_lineage: self.instrument_lineage,
            outcome_group_lineage: self.outcome_group_lineage,
            outcome_risk_pool_lineage: self.outcome_risk_pool_lineage,
        })
    }

    /// Returns a route this generation publishes, for a failure that names one.
    ///
    /// A registry-only publication replaces the canonical registries without
    /// naming a route, so the failure is reported under a route of the generation
    /// it refused rather than under an invented one. A generation that publishes
    /// no snapshot at all has no route to name, and the refusal is then the whole
    /// catalog's rather than a route's.
    fn reporting_route(&self) -> Option<VenueRoute> {
        let mut routes: Vec<&VenueRoute> = self.route_snapshots.keys().collect();
        routes.sort();
        routes.first().map(|route| (*route).clone())
    }

    /// Returns the display catalog of this generation.
    pub fn display_catalog(&self) -> &DisplayCatalog {
        &self.display_catalog
    }

    /// Binds a display resolver to this generation.
    ///
    /// The resolver reads the values this generation already published and never
    /// formats, looks a term up or reaches another generation, so every value it
    /// answers belongs to the same publication.
    pub fn display_resolver(&self) -> DisplayResolver<'_> {
        DisplayResolver::new(self)
    }

    /// Returns the route snapshots of this generation.
    pub fn route_snapshots(&self) -> &AHashMap<VenueRoute, Arc<RouteSnapshot>> {
        &self.route_snapshots
    }

    /// Returns the snapshot of one route.
    pub fn route_snapshot(&self, route: &VenueRoute) -> Option<&Arc<RouteSnapshot>> {
        self.route_snapshots.get(route)
    }

    /// Returns every listing of one canonical instrument across all routes.
    pub fn listings_for(&self, instrument: &InstrumentId) -> Option<&Arc<[VenueListingKey]>> {
        self.listings_by_instrument.get(instrument)
    }

    /// Returns the outcome registry of this generation.
    pub fn outcomes(&self) -> &Arc<OutcomeRegistry> {
        &self.outcome_events
    }

    /// Returns the asset registry of this generation.
    pub fn assets(&self) -> &Arc<AssetRegistry> {
        &self.assets
    }

    /// Returns the reference registry of this generation.
    pub fn references(&self) -> &Arc<ReferenceRegistry> {
        &self.references
    }

    /// Returns the outcome groups of this generation.
    pub fn outcome_groups(&self) -> &AHashMap<OutcomeGroupId, OutcomeGroup> {
        self.outcome_events.groups()
    }

    /// Returns the outcome risk pools of this generation.
    pub fn outcome_risk_pools(&self) -> &AHashMap<OutcomeRiskPoolId, OutcomeRiskPool> {
        self.outcome_events.risk_pools()
    }

    /// Returns the outcome events of this generation.
    pub fn outcome_events(&self) -> &AHashMap<OutcomeEventId, OutcomeEvent> {
        self.outcome_events.events()
    }

    /// Returns the outcome group lineage of this generation.
    pub fn outcome_group_lineage(
        &self,
    ) -> &[market_participants_model::outcome::OutcomeGroupLineageEdge] {
        &self.outcome_group_lineage
    }

    /// Returns the outcome risk pool lineage of this generation.
    pub fn outcome_risk_pool_lineage(
        &self,
    ) -> &[market_participants_model::outcome::OutcomeRiskPoolLineageEdge] {
        &self.outcome_risk_pool_lineage
    }

    /// Installs a validated batch as the snapshot of its route.
    ///
    /// The new generation keeps every other route snapshot, drops the snapshot
    /// the batch replaces, recomputes the global identity index, inherits the
    /// registry maps with the batch's own terms merged in, drops the outcome
    /// events no surviving route reaches any more, and appends the batch's
    /// lineage to the lineage the generation already carries — a successor
    /// relation stays true for as long as the catalog exists, which is what lets
    /// a risk pool that references a replaced group be checked against the
    /// group's successor in a later generation.
    pub fn with_pending(
        &self,
        pending: CatalogPending,
        publisher: &CatalogGenerationPublisher,
    ) -> Result<Self, CatalogError> {
        let id = publisher.publish();
        self.with_pending_at(pending, id)
    }

    /// Installs a validated batch under an already drawn generation id.
    ///
    /// The one-generation commit of a venue draws its id before it starts folding
    /// the venue's batches in, so the whole commit is published as one generation
    /// rather than one per batch.
    fn with_pending_at(
        &self,
        pending: CatalogPending,
        id: CatalogGenerationId,
    ) -> Result<Self, CatalogError> {
        let route = pending.route().clone();
        let referenced = crate::catalog::batch::ReferencedTerms::collect(&pending, self);
        let (assets, references, mut outcomes) = crate::catalog::batch::staged_registries(
            self,
            pending.pending_assets(),
            pending.pending_references(),
            pending.pending_outcomes(),
            &referenced,
        );
        let mut instrument_lineage: Vec<InstrumentLineageEdge> = self.instrument_lineage.to_vec();
        instrument_lineage.extend(pending.instrument_lineage.iter().cloned());
        let mut outcome_group_lineage: Vec<
            market_participants_model::outcome::OutcomeGroupLineageEdge,
        > = self.outcome_group_lineage.to_vec();
        outcome_group_lineage.extend(pending.outcome_group_lineage.iter().cloned());
        let mut outcome_risk_pool_lineage: Vec<
            market_participants_model::outcome::OutcomeRiskPoolLineageEdge,
        > = self.outcome_risk_pool_lineage.to_vec();
        outcome_risk_pool_lineage.extend(pending.outcome_risk_pool_lineage.iter().cloned());
        let mut route_snapshots = self.route_snapshots.clone();
        route_snapshots.insert(route.clone(), Arc::new(pending.into_snapshot()));
        // The event lifetime is a property of the whole surviving catalog, not of the batch that is publishing: the reachable set is recomputed from every
        // surviving snapshot, so a route that publishes nothing for an event never
        // unpublishes it for the route that does.
        let reachable = ReachableOutcomes::of(&route_snapshots, &outcomes);
        let live_events: AHashSet<OutcomeEventId> = reachable
            .published_events
            .iter()
            .chain(reachable.events.iter())
            .cloned()
            .collect();
        Arc::get_mut(&mut outcomes)
            .expect("a freshly built registry is not shared yet")
            .retain_events(&live_events);
        // The display catalog is built from the surviving snapshots and the
        // registries this publication installs, so a refusal leaves the previous
        // generation — route snapshots, registries and display alike — untouched.
        let display_catalog = build_display(&route_snapshots, &assets, &references, &outcomes)
            .map_err(|error| display_rejected(Some(route), error))?;
        Ok(Self {
            id,
            listings_by_instrument: index_instruments(&route_snapshots),
            display_catalog,
            route_snapshots,
            outcome_events: outcomes,
            assets,
            references,
            instrument_lineage: instrument_lineage.into(),
            outcome_group_lineage: outcome_group_lineage.into(),
            outcome_risk_pool_lineage: outcome_risk_pool_lineage.into(),
        })
    }

    /// Publishes the catalog with a set of routes revoked, under an already drawn
    /// generation id.
    fn with_revoked_routes_at(
        &self,
        revoked: &[VenueRoute],
        id: CatalogGenerationId,
    ) -> Result<Self, CatalogError> {
        let mut route_snapshots = self.route_snapshots.clone();
        for route in revoked {
            route_snapshots.remove(route);
        }
        let reachable = ReachableOutcomes::of(&route_snapshots, &self.outcome_events);
        let mut groups: AHashMap<OutcomeGroupId, OutcomeGroup> = AHashMap::new();
        for (id, group) in self.outcome_events.groups() {
            if reachable.groups.contains(id) {
                groups.insert(*id, group.clone());
            }
        }
        let mut pools: AHashMap<OutcomeRiskPoolId, OutcomeRiskPool> = AHashMap::new();
        for (id, pool) in self.outcome_events.risk_pools() {
            if reachable.pools.contains(id) {
                pools.insert(*id, pool.clone());
            }
        }
        let mut events: AHashMap<OutcomeEventId, OutcomeEvent> = AHashMap::new();
        for (id, event) in self.outcome_events.events() {
            if reachable.events.contains(id) {
                events.insert(*id, event.clone());
            }
        }
        // A surviving snapshot only keeps the event indexes it still publishes, so
        // the route that lost its last referencing listing loses the event with it.
        // The native lookup maps are rebuilt from the same set, so a route never
        // resolves an event its generation no longer carries.
        let mut rebuilt_events: Vec<(VenueRoute, Arc<RouteSnapshot>)> = Vec::new();
        for (route, snapshot) in &route_snapshots {
            let kept: AHashSet<OutcomeEventId> = snapshot
                .published_events
                .intersection(&reachable.events)
                .cloned()
                .collect();
            let mut native_events: AHashMap<
                market_participants_model::identifier::PolymarketEventId,
                OutcomeEventId,
            > = AHashMap::new();
            for (native, id) in &snapshot.native_events {
                if kept.contains(id) {
                    native_events.insert(native.clone(), *id);
                }
            }
            let mut native_event_groups: AHashMap<
                market_participants_model::identifier::PolymarketEventId,
                Vec<OutcomeGroupId>,
            > = AHashMap::new();
            for (native, _) in &native_events {
                if let Some(groups) = snapshot.native_event_groups.get(native) {
                    let kept_groups: Vec<OutcomeGroupId> = groups
                        .iter()
                        .filter(|group| reachable.groups.contains(group))
                        .cloned()
                        .collect();
                    if !kept_groups.is_empty() {
                        native_event_groups.insert(native.clone(), kept_groups);
                    }
                }
            }
            // A route whose event indexes survive this publication unchanged keeps
            // the snapshot it already had, `Arc` and all. Rebuilding it would
            // deep-copy every listing key of the route's lookup maps for no
            // difference in content, and a venue commit rebuilds *every* surviving
            // route, so the copy is what one commit costs the whole catalog.
            let unchanged = kept == snapshot.published_events
                && native_events == snapshot.native_events
                && native_event_groups == snapshot.native_event_groups;
            if unchanged {
                rebuilt_events.push((route.clone(), Arc::clone(snapshot)));
                continue;
            }
            let mut next = snapshot.as_ref().clone();
            next.published_events = kept;
            next.native_events = native_events;
            next.native_event_groups = native_event_groups;
            rebuilt_events.push((route.clone(), Arc::new(next)));
        }
        let route_snapshots: AHashMap<VenueRoute, Arc<RouteSnapshot>> =
            rebuilt_events.into_iter().collect();
        let outcomes =
            OutcomeRegistry::from_relations(&self.outcome_events, groups, pools, events, &[], &[]);
        let outcomes = Arc::new(outcomes);
        // The display catalog is rebuilt from the survivors with the same rules,
        // so the names of the listings that stayed never change because another
        // route left.
        let display_catalog =
            build_display(&route_snapshots, &self.assets, &self.references, &outcomes).map_err(
                |error| {
                    display_rejected(
                        revoked.first().cloned().or_else(|| self.reporting_route()),
                        error,
                    )
                },
            )?;
        Ok(Self {
            id,
            listings_by_instrument: index_instruments(&route_snapshots),
            display_catalog,
            route_snapshots,
            outcome_events: outcomes,
            assets: Arc::clone(&self.assets),
            references: Arc::clone(&self.references),
            instrument_lineage: self.instrument_lineage.clone(),
            outcome_group_lineage: self.outcome_group_lineage.clone(),
            outcome_risk_pool_lineage: self.outcome_risk_pool_lineage.clone(),
        })
    }

    /// Publishes one venue's whole commit as a single generation.
    ///
    /// This is the catalog side of a venue commit: the batches the venue's
    /// load staged are installed as their scopes' snapshots, the scopes the venue
    /// no longer publishes are removed, and the derived content — the global
    /// instrument index, the reachable outcomes and the display catalog — is
    /// recomputed from the survivors. One call consumes exactly one generation id,
    /// so a reader observes either the generation before the commit or the one
    /// after it and never a mixture of the two.
    ///
    /// The caller holds the registry and publication write locks, so nothing can
    /// be published between the snapshots being folded in and the pointer being
    /// replaced.
    pub fn with_venue_commit(
        &self,
        pending: &[CatalogPending],
        revoked: &[VenueRoute],
        publisher: &CatalogGenerationPublisher,
    ) -> Result<Self, CatalogError> {
        if pending.is_empty() && revoked.is_empty() {
            return Ok(self.clone());
        }
        let id = publisher.publish();
        let mut next = self.with_revoked_routes_at(revoked, id)?;
        for batch in pending {
            next = next.with_pending_at(batch.clone(), id)?;
        }
        Ok(next)
    }

    /// Returns the set of outcome groups and risk pools the surviving listings
    /// still reference.
    pub fn reachable_outcomes(&self) -> (AHashSet<OutcomeGroupId>, AHashSet<OutcomeRiskPoolId>) {
        let reachable = ReachableOutcomes::of(&self.route_snapshots, &self.outcome_events);
        (reachable.groups, reachable.pools)
    }
}

/// The outcome objects a set of route snapshots still reaches.
///
/// A group survives while a surviving listing trades one of its legs; a pool or
/// event survives while one of those groups takes part in it, or — for an event —
/// while a surviving route publishes it. Everything else is dropped by the
/// publication that removed the last reference. `published_events` keeps the raw
/// event indexes of the surviving snapshots next to the set that also reaches a
/// group, so a caller can express "this event is indexed but nothing reaches it".
struct ReachableOutcomes {
    groups: AHashSet<OutcomeGroupId>,
    pools: AHashSet<OutcomeRiskPoolId>,
    published_events: AHashSet<OutcomeEventId>,
    events: AHashSet<OutcomeEventId>,
}

impl ReachableOutcomes {
    fn of(
        route_snapshots: &AHashMap<VenueRoute, Arc<RouteSnapshot>>,
        outcomes: &Arc<OutcomeRegistry>,
    ) -> Self {
        let mut groups: AHashSet<OutcomeGroupId> = AHashSet::new();
        let mut pools: AHashSet<OutcomeRiskPoolId> = AHashSet::new();
        let mut published_events: AHashSet<OutcomeEventId> = AHashSet::new();
        for snapshot in route_snapshots.values() {
            groups.extend(snapshot.outcome_listings.keys().cloned());
            published_events.extend(snapshot.published_events.iter().cloned());
        }
        let mut events = published_events.clone();
        // An event or pool keeps the groups it indexes reachable, and a group
        // keeps the pool it takes part in reachable. Apply both rules until they
        // settle, so a group that only survives through a pool survives as long
        // as that pool does.
        loop {
            let mut grew = false;
            for (id, event) in outcomes.events() {
                if !events.contains(id) {
                    continue;
                }
                if event.groups.is_empty() && event.risk_pools.is_empty() {
                    continue;
                }
                if !event.groups.is_empty() && !event.groups.iter().any(|g| groups.contains(g)) {
                    continue;
                }
                for group in &event.groups {
                    grew |= groups.insert(*group);
                }
                for pool in &event.risk_pools {
                    grew |= pools.insert(*pool);
                }
            }
            for (id, pool) in outcomes.risk_pools() {
                if !pools.contains(id) {
                    continue;
                }
                for group in &pool.groups {
                    grew |= groups.insert(*group);
                }
            }
            if !grew {
                break;
            }
        }
        // An event survives only while one of its groups or pools survives.
        events.retain(|id| {
            outcomes.events().get(id).is_some_and(|event| {
                event.groups.iter().any(|group| groups.contains(group))
                    || event.risk_pools.iter().any(|pool| pools.contains(pool))
            })
        });
        Self {
            groups,
            pools,
            published_events,
            events,
        }
    }
}

/// The immutable snapshot of one route's listings.
#[derive(Clone, Debug)]
pub struct RouteSnapshot {
    route: VenueRoute,
    route_revision: RouteInputRevision,
    entries: Arc<[CatalogEntry]>,
    native_keys: AHashMap<smol_str::SmolStr, VenueListingKey>,
    listing_identifiers: AHashMap<ListingIdentifier, VenueListingKey>,
    outcome_listings: AHashMap<OutcomeGroupId, VenueListingKey>,
    native_groups: AHashMap<OutcomeGroupIdentifier, OutcomeGroupId>,
    native_events:
        AHashMap<market_participants_model::identifier::PolymarketEventId, OutcomeEventId>,
    /// The event-level index:
    /// `(VenueRoute, PolymarketEventId) -> OutcomeEventId -> [OutcomeGroupId]`.
    native_event_groups:
        AHashMap<market_participants_model::identifier::PolymarketEventId, Vec<OutcomeGroupId>>,
    /// The event indexes this route publishes.
    published_events: AHashSet<OutcomeEventId>,
    /// The display material this route staged with its listings.
    display_material: Arc<RouteDisplayMaterial>,
    /// The identifier normalization rules of this route.
    normalization: Arc<[IdentifierNormalizationRule]>,
}

impl RouteSnapshot {
    /// Builds the immutable snapshot of one validated batch.
    pub(crate) fn from_pending(pending: CatalogPending) -> Self {
        Self::from_pending_ref(&pending)
    }

    /// Builds the snapshot a validated batch would publish, without consuming it.
    ///
    /// The display gate needs the candidate snapshot of the generation it is
    /// about to accept, so it builds one and drops it; the publication builds the
    /// real one from the same staged content.
    pub(crate) fn from_pending_ref(pending: &CatalogPending) -> Self {
        Self {
            route: pending.route().clone(),
            route_revision: pending.revision(),
            entries: Arc::clone(pending.entries()),
            native_keys: pending.native_keys().clone(),
            listing_identifiers: pending.listing_identifiers().clone(),
            outcome_listings: pending.outcome_listings().clone(),
            native_groups: pending.native_groups().clone(),
            native_events: pending.native_events().clone(),
            native_event_groups: pending.native_event_groups().clone(),
            published_events: pending.published_events().clone(),
            display_material: Arc::clone(pending.display_material()),
            normalization: Arc::clone(pending.normalization()),
        }
    }

    /// Returns the display material this route staged with its listings.
    pub fn display_material(&self) -> &Arc<RouteDisplayMaterial> {
        &self.display_material
    }

    /// Returns the normalization mode this route declared for one identifier kind.
    ///
    /// the display gate asks the route for the mode of the typed
    /// identifier it is about to compare, so the comparison folds a value exactly
    /// the way the route's own lookup index folded it. A kind the route declared
    /// no rule for is `Exact`.
    pub fn normalization_mode(&self, kind: ListingIdentifierKind) -> SymbolNormalization {
        let product = self.declared_product(kind);
        normalization_mode(&self.normalization, kind, product).unwrap_or(SymbolNormalization::Exact)
    }

    /// Returns the one product this route declared for one identifier kind.
    ///
    /// A kind the route declared no product for, or declared two products for,
    /// has no single product of its own and answers `None`.
    fn declared_product(
        &self,
        kind: ListingIdentifierKind,
    ) -> Option<IdentifierNormalizationProduct> {
        let mut products = self
            .normalization
            .iter()
            .filter(|rule| rule.identifier == kind)
            .filter_map(|rule| rule.product);
        let product = products.next()?;
        products.next().is_none().then_some(product)
    }

    /// Returns the route this snapshot belongs to.
    pub fn route(&self) -> &VenueRoute {
        &self.route
    }

    /// Returns the input revision this snapshot was fetched under.
    pub const fn route_revision(&self) -> RouteInputRevision {
        self.route_revision
    }

    /// Returns every listing of this route in canonical order.
    pub fn entries(&self) -> &Arc<[CatalogEntry]> {
        &self.entries
    }

    /// Returns the entry of one listing key.
    ///
    /// The entries are held in canonical key order — the order
    /// [`listing_order`](market_participants_model::listing::navigation::listing_order)
    /// defines, which is the order a batch is staged in and the order every listing
    /// list is drawn in — so the lookup is a binary search over the snapshot's own
    /// array. A map from key to position would answer the same question at the cost
    /// of one more entry per listing, and the array is already the index.
    pub fn listing(&self, key: &VenueListingKey) -> Option<&CatalogEntry> {
        self.entries
            .binary_search_by(|entry| {
                market_participants_model::listing::navigation::listing_order(
                    &entry.listing.key,
                    key,
                )
            })
            .ok()
            .map(|index| &self.entries[index])
    }

    /// Returns the listing a normalized native symbol names.
    pub fn listing_by_native_key(&self, key: &str) -> Option<&VenueListingKey> {
        self.native_keys.get(key)
    }

    /// Returns the listing a typed identifier names.
    pub fn listing_by_identifier(
        &self,
        identifier: &ListingIdentifier,
    ) -> Option<&VenueListingKey> {
        self.listing_identifiers.get(identifier)
    }

    /// Returns the listing of one canonical outcome group.
    pub fn listing_by_outcome_group(&self, group: &OutcomeGroupId) -> Option<&VenueListingKey> {
        self.outcome_listings.get(group)
    }

    /// Returns the outcome group a native group identifier names on this route.
    pub fn group_by_native_identifier(
        &self,
        identifier: &OutcomeGroupIdentifier,
    ) -> Option<&OutcomeGroupId> {
        self.native_groups.get(identifier)
    }

    /// Returns the outcome event a native event identifier names on this route.
    ///
    /// Only the lookups this route publishes are answered, so a native event
    /// identifier of another route resolves here only when this route publishes
    /// the event it names.
    pub fn event_by_native_identifier(
        &self,
        event: &market_participants_model::identifier::PolymarketEventId,
    ) -> Option<&OutcomeEventId> {
        self.native_events.get(event)
    }

    /// Returns the native event lookups this route publishes.
    pub fn native_events(
        &self,
    ) -> &AHashMap<market_participants_model::identifier::PolymarketEventId, OutcomeEventId> {
        &self.native_events
    }

    /// Returns the groups the event of one native event identifier carries on
    /// this route, per the event-level index.
    pub fn event_groups(
        &self,
        event: &market_participants_model::identifier::PolymarketEventId,
    ) -> Option<&[OutcomeGroupId]> {
        self.native_event_groups.get(event).map(Vec::as_slice)
    }

    /// Returns the whole event-level index of this route.
    pub fn native_event_groups(
        &self,
    ) -> &AHashMap<market_participants_model::identifier::PolymarketEventId, Vec<OutcomeGroupId>>
    {
        &self.native_event_groups
    }

    /// Returns the event indexes this route publishes.
    pub fn published_events(&self) -> &AHashSet<OutcomeEventId> {
        &self.published_events
    }
}

/// Resolves a native event identifier against one route's published indexes.
///
/// This is the single implementation behind both the resolver of
/// [`crate::catalog::resolved::EnsuredRoute`] and the resolver of
/// [`crate::catalog::store::Catalog`], so the two can never disagree about the same
/// `(route, identifier)`. The event must be reachable from the route's own
/// index: either one of its native event lookups names it, or the route publishes
/// the canonical event a shared lookup names — for instance when another route's
/// adapter registered the same event under a second native identifier. An event
/// only another route publishes is not reachable and is refused as
/// [`CatalogError::NativeEventIdentifierNotFound`].
pub(crate) fn resolve_native_event(
    generation: &CatalogGeneration,
    snapshot: &RouteSnapshot,
    identifier: &OutcomeEventIdentifier,
) -> Result<OutcomeEventId, CatalogError> {
    let OutcomeEventIdentifier::PolymarketEvent(native) = identifier;
    snapshot
        .event_by_native_identifier(native)
        .cloned()
        .or_else(|| {
            generation
                .outcomes()
                .native_event(native)
                .filter(|event| snapshot.published_events().contains(event))
                .cloned()
        })
        .ok_or_else(|| CatalogError::NativeEventIdentifierNotFound {
            route: snapshot.route().clone(),
            event: native.clone(),
        })
}

/// Resolves a native outcome group identifier against one route's published
/// indexes.
///
/// This is the single implementation behind both the resolver of
/// [`crate::catalog::resolved::EnsuredRoute`] and the resolver of
/// [`crate::catalog::store::Catalog`], so the two can never disagree about the same
/// `(route, identifier)`. A native group identifier that carries a scope — a
/// Hyperliquid question — only ever names a group of the route's own
/// [`market_participants_model::identifier::VenueScopeId`], exactly as a scoped listing
/// identifier does: a question of another scope is refused as
/// [`CatalogError::NativeIdentifierRouteMismatch`] before any lookup,
/// whoever registered it. The group must then be reachable from the route's own
/// index: either one of its native group lookups names it, or one of its listings
/// trades a leg of the group — for instance when another route's adapter
/// registered the same group under a second native identifier. A group only
/// another route publishes is refused.
pub(crate) fn resolve_native_group(
    generation: &CatalogGeneration,
    snapshot: &RouteSnapshot,
    identifier: &OutcomeGroupIdentifier,
) -> Result<OutcomeGroupId, CatalogError> {
    if let OutcomeGroupIdentifier::HyperliquidQuestion { scope, .. } = identifier
        && scope != &snapshot.route().scope
    {
        return Err(CatalogError::NativeIdentifierRouteMismatch {
            route: snapshot.route().clone(),
            identifier: Box::new(ListingIdentifier::HyperliquidOutcome {
                scope: scope.clone(),
                outcome_id: market_participants_model::listing::venue::HyperliquidOutcomeId(0),
                asset_id: market_participants_model::listing::venue::HyperliquidAssetId(0),
            }),
        });
    }
    snapshot
        .group_by_native_identifier(identifier)
        .cloned()
        .or_else(|| {
            generation
                .outcomes()
                .native_group(identifier)
                .filter(|group| snapshot.listing_by_outcome_group(group).is_some())
                .cloned()
        })
        .ok_or_else(|| CatalogError::NativeGroupIdentifierNotFound {
            route: snapshot.route().clone(),
            identifier: Box::new(identifier.clone()),
        })
}

/// Resolves a typed listing identifier against one route's published indexes.
///
/// This is the single implementation behind both the resolver of
/// [`crate::catalog::resolved::EnsuredRoute`] and the resolver of
/// [`crate::catalog::store::Catalog`], so the two can never disagree about the same
/// `(route, identifier)`. A scoped identifier — a Hyperliquid asset or outcome —
/// only ever names a listing of the route's own
/// [`market_participants_model::identifier::VenueScopeId`]: an identifier of another scope
/// is refused as [`CatalogError::NativeIdentifierRouteMismatch`]
/// before any lookup, whoever registered it.
///
/// The identifier is folded with the route's own rule for its typed identifier and
/// product — the rule the route's index was built under — so a spelling the route
/// folds names the same listing on both sides. An identifier the rule cannot
/// produce names no listing of this route, which is refused as
/// [`CatalogError::NativeIdentifierNotFound`]. Nothing else is
/// consulted: a listing another route publishes is not reachable from this route's
/// own index and is refused the same way.
pub(crate) fn resolve_native_listing(
    snapshot: &RouteSnapshot,
    normalization: SymbolNormalization,
    identifier: &ListingIdentifier,
) -> Result<VenueListingKey, CatalogError> {
    if let Some(scope) = identifier.scope()
        && scope != &snapshot.route().scope
    {
        return Err(CatalogError::NativeIdentifierRouteMismatch {
            route: snapshot.route().clone(),
            identifier: Box::new(identifier.clone()),
        });
    }
    normalize_listing_identifier(identifier, normalization)
        .ok()
        .as_ref()
        .and_then(|normalized| snapshot.listing_by_identifier(normalized))
        .cloned()
        .ok_or_else(|| CatalogError::NativeIdentifierNotFound {
            route: snapshot.route().clone(),
            identifier: Box::new(identifier.clone()),
        })
}

/// Resolves a native symbol against one route's published indexes.
///
/// This is the single implementation behind both the resolver of
/// [`crate::catalog::resolved::EnsuredRoute`] and the resolver of
/// [`crate::catalog::store::Catalog`], so the two can never disagree about the same
/// `(route, symbol)`. The native index of a route is built from the keys the
/// route's own normalization rule produces, so the lookup applies that same rule
/// [`SymbolNormalization::apply`]: a spelling the route folds names the same
/// listing on both sides, and a symbol the route cannot normalize is refused as
/// [`CatalogError::NativeSymbolInvalid`] before any lookup. A symbol
/// the route does not register — its own or an alias of one of its listings — is
/// refused as [`CatalogError::NativeSymbolNotFound`].
pub(crate) fn resolve_native_symbol(
    snapshot: &RouteSnapshot,
    normalization: SymbolNormalization,
    symbol: &NativeSymbol,
) -> Result<VenueListingKey, CatalogError> {
    let normalized =
        normalization
            .apply(symbol.as_str())
            .map_err(|_| CatalogError::NativeSymbolInvalid {
                route: snapshot.route().clone(),
                symbol: symbol.as_str().into(),
            })?;
    snapshot
        .listing_by_native_key(&normalized)
        .cloned()
        .ok_or_else(|| CatalogError::NativeSymbolNotFound {
            route: snapshot.route().clone(),
            symbol: symbol.clone(),
        })
}

/// The entry of one listing in one generation, without loading its route.
///
/// Every reader of a published listing is this lookup: the loading readers fetch
/// the route first and then answer from the generation they were handed, and the
/// cached readers answer from the current one. A listing of a route the generation
/// does not carry is absent, whatever another route publishes.
pub(crate) fn listing_entry(
    generation: &CatalogGeneration,
    listing: &VenueListingKey,
) -> Option<Arc<CatalogEntry>> {
    let snapshot = generation.route_snapshot(&listing.route)?;
    let entry = snapshot.listing(listing)?;
    Some(Arc::new(entry.clone()))
}

/// The entry of one published listing, or the refusal that names it.
pub(crate) fn resolve_listing_entry(
    generation: &CatalogGeneration,
    listing: &VenueListingKey,
) -> Result<Arc<CatalogEntry>, CatalogError> {
    listing_entry(generation, listing).ok_or_else(|| CatalogError::ListingNotFound {
        listing: Box::new(listing.clone()),
    })
}

/// Resolves one listing into the value a command validates against.
///
/// This is the single implementation behind both the synchronous resolver of
/// [`crate::catalog::resolved::EnsuredRoute`] and the asynchronous resolver of
/// [`crate::catalog::store::Catalog`]: the entry and the generation id a resolution
/// carries are read under one generation, so the resolution can never claim a
/// generation its entry does not come from.
pub(crate) fn resolve_listing(
    generation: &CatalogGeneration,
    listing: &VenueListingKey,
) -> Result<ResolvedListing, CatalogError> {
    let entry = resolve_listing_entry(generation, listing)?;
    Ok(ResolvedListing::from_entry(generation.id(), entry))
}

/// Resolves one listing into the projection an adapter encodes with.
pub(crate) fn resolve_listing_ref(
    generation: &CatalogGeneration,
    listing: &VenueListingKey,
) -> Result<ResolvedListingRef, CatalogError> {
    let entry = resolve_listing_entry(generation, listing)?;
    Ok(ResolvedListingRef::from_entry(generation.id(), entry))
}

/// The key one typed identifier is indexed and looked up under.
///
/// The kind comes from the variant and the product from its own fields, so the
/// rule the route declared for that pair is the one applied to the value the
/// variant carries. The index a route publishes and every lookup against it are
/// built here, which is what makes a spelling the route folds name the same
/// listing on both sides.
///
/// A variant that carries no symbol of its own — a Hyperliquid asset or outcome, a
/// Polymarket token, a future series — is its own key: the route never folds a
/// value the identifier does not have, so those identifiers stay exactly as they
/// were published.
pub(crate) fn normalize_listing_identifier(
    identifier: &ListingIdentifier,
    mode: SymbolNormalization,
) -> Result<ListingIdentifier, ModelError> {
    use market_participants_model::identifier::{
        EquityLocalCode, LocalSymbol, MarketParticipantSeriesId, OccSymbol,
    };

    let Some(value) = identifier.symbol_value() else {
        return Ok(identifier.clone());
    };
    let value = mode.apply(value)?;
    Ok(match identifier {
        ListingIdentifier::Symbol(_) => ListingIdentifier::Symbol(NativeSymbol::new(&value)?),
        ListingIdentifier::BinanceInstrument { product, .. } => {
            ListingIdentifier::BinanceInstrument {
                symbol: NativeSymbol::new(&value)?,
                product: *product,
            }
        }
        ListingIdentifier::OkxInstrument {
            instrument_type, ..
        } => ListingIdentifier::OkxInstrument {
            instrument_id: NativeSymbol::new(&value)?,
            instrument_type: *instrument_type,
        },
        ListingIdentifier::EquityCode { mic, .. } => ListingIdentifier::EquityCode {
            mic: mic.clone(),
            local_code: EquityLocalCode::new(&value)?,
        },
        ListingIdentifier::OccSymbol(_) => ListingIdentifier::OccSymbol(OccSymbol::new(&value)?),
        ListingIdentifier::MarketParticipantSeries(_) => {
            ListingIdentifier::MarketParticipantSeries(MarketParticipantSeriesId::new(&value)?)
        }
        ListingIdentifier::OptionLocalSymbol(_) => {
            ListingIdentifier::OptionLocalSymbol(LocalSymbol::new(&value)?)
        }
        ListingIdentifier::HyperliquidAsset { .. }
        | ListingIdentifier::HyperliquidOutcome { .. }
        | ListingIdentifier::PolymarketToken(_)
        | ListingIdentifier::FutureSeries(_) => identifier.clone(),
    })
}

/// The typed identifiers a listing is looked up by.
pub fn listing_identifiers_of(
    listing: &market_participants_model::listing::listing_impl::VenueListing,
) -> Vec<ListingIdentifier> {
    use market_participants_model::listing::identifiers::{
        HyperliquidListingIdentifiers, VenueListingIdentifiers,
    };
    let mut identifiers: Vec<ListingIdentifier> = Vec::with_capacity(2);
    match &listing.identifiers {
        VenueListingIdentifiers::Hyperliquid(HyperliquidListingIdentifiers::Perpetual {
            dex,
            asset_id,
            ..
        }) => identifiers.push(ListingIdentifier::HyperliquidAsset {
            scope: dex.clone(),
            product: HyperliquidAssetProduct::Perpetual,
            asset_id: *asset_id,
        }),
        VenueListingIdentifiers::Hyperliquid(HyperliquidListingIdentifiers::Spot {
            asset_id,
            ..
        }) => identifiers.push(ListingIdentifier::HyperliquidAsset {
            scope: listing.key.route.scope.clone(),
            product: HyperliquidAssetProduct::Spot,
            asset_id: *asset_id,
        }),
        VenueListingIdentifiers::Hyperliquid(HyperliquidListingIdentifiers::Outcome {
            venue,
            outcome_id,
            asset_id,
            ..
        }) => identifiers.push(ListingIdentifier::HyperliquidOutcome {
            scope: venue.clone(),
            outcome_id: *outcome_id,
            asset_id: *asset_id,
        }),
        VenueListingIdentifiers::Polymarket(metadata) => identifiers.push(
            ListingIdentifier::PolymarketToken(metadata.clob_token_id.clone()),
        ),
        VenueListingIdentifiers::Equity(metadata) => {
            identifiers.push(ListingIdentifier::EquityCode {
                mic: metadata.mic.clone(),
                local_code: metadata.local_code.clone(),
            })
        }
        VenueListingIdentifiers::Binance(metadata) => {
            if let Some(symbol) = &listing.native_symbol {
                identifiers.push(ListingIdentifier::BinanceInstrument {
                    symbol: symbol.clone(),
                    product: metadata.product,
                });
            }
        }
        VenueListingIdentifiers::Okx(metadata) => {
            if let Some(symbol) = &listing.native_symbol {
                identifiers.push(ListingIdentifier::OkxInstrument {
                    instrument_id: symbol.clone(),
                    instrument_type: metadata.instrument_type,
                });
            }
        }
        VenueListingIdentifiers::FutureSeries(series) => {
            identifiers.push(ListingIdentifier::FutureSeries(series.clone()));
        }
        VenueListingIdentifiers::OptionSeries(series) => {
            if let Some(occ) = &series.occ_symbol {
                identifiers.push(ListingIdentifier::OccSymbol(occ.clone()));
            }
            if let Some(id) = &series.market_participant_series_id {
                identifiers.push(ListingIdentifier::MarketParticipantSeries(id.clone()));
            }
            if let Some(symbol) = &series.local_symbol {
                identifiers.push(ListingIdentifier::OptionLocalSymbol(symbol.clone()));
            }
        }
    }
    identifiers
}

/// Indexes every surviving listing key by its canonical instrument.
fn index_instruments(
    route_snapshots: &AHashMap<VenueRoute, Arc<RouteSnapshot>>,
) -> AHashMap<InstrumentId, Arc<[VenueListingKey]>> {
    let mut index: AHashMap<InstrumentId, Vec<VenueListingKey>> = AHashMap::new();
    for snapshot in route_snapshots.values() {
        for entry in snapshot.entries.iter() {
            index
                .entry(entry.listing.instrument_id)
                .or_default()
                .push(entry.listing.key.clone());
        }
    }
    index
        .into_iter()
        .map(|(instrument, mut keys)| {
            keys.sort();
            keys.dedup();
            (instrument, Arc::from(keys))
        })
        .collect()
}

/// Builds the display catalog of one publication.
///
/// This is the single entry the three publication paths share — a staged batch, a
/// revocation and a registry-only update — so all of them build the same catalog
/// from the same facts with the same naming gate, and a refusal is reported as a
/// typed catalog failure instead of a partially replaced generation.
fn build_display(
    route_snapshots: &AHashMap<VenueRoute, Arc<RouteSnapshot>>,
    assets: &Arc<AssetRegistry>,
    references: &Arc<ReferenceRegistry>,
    outcomes: &Arc<OutcomeRegistry>,
) -> Result<DisplayCatalog, DisplayError> {
    build_display_catalog(PublicationInput {
        snapshots: route_snapshots,
        assets,
        references,
        outcomes,
    })
}

/// Wraps a display failure with the route whose publication was refused.
///
/// A publication that revoked every route has none left to name, and neither has a
/// generation that never had one, so the parameter is an `Option` because the *state*
/// is one — not because a refusal is expected there. A display catalog built over no
/// candidate route has no listing to refuse: the only lookups it runs are over the
/// identities the registries themselves enumerate, and a registry built the way this
/// crate builds them cannot fail them. The route-less arm is therefore the honest
/// total answer for a state the type allows, and no publication path reaches it today:
/// the state is askable in principle — a generation can be built with no route and its
/// registries replaced — but the answer cannot be a refusal.
fn display_rejected(route: Option<VenueRoute>, error: DisplayError) -> CatalogError {
    let error = CatalogValidationError::DisplayRejected {
        listing: None,
        error: Box::new(error),
    };
    match route {
        Some(route) => CatalogError::invalid_catalog(route, error),
        None => CatalogError::InvalidRegistries {
            error: Box::new(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
    };

    use crate::catalog::{
        display::types::DisplayError,
        error::{CatalogError, CatalogValidationError},
        generation::display_rejected,
        test_support,
    };

    /// A display failure with no route to hang it on is the whole catalog's.
    ///
    /// This pins the *mapping*, not a production path: a display catalog built over
    /// no candidate route has nothing to refuse, so no publication reaches the
    /// route-less arm — see `display_rejected` for the argument. The arm still has
    /// to answer, because the state it describes is one the type allows, and the
    /// route-carrying arm is pinned by a real publication in `catalog_tests`.
    #[test]
    fn a_display_failure_without_a_route_should_be_the_whole_catalogs_refusal() {
        let error = DisplayError::MissingAssetTerms {
            asset: Box::new(test_support::token("binance", "BTC")),
        };

        match display_rejected(None, error) {
            CatalogError::InvalidRegistries { error } => match error.as_ref() {
                CatalogValidationError::DisplayRejected { listing, .. } => {
                    assert!(
                        listing.is_none(),
                        "a route-less refusal names no listing: {listing:?}"
                    );
                }
                other => panic!("expected a display refusal, got {other:?}"),
            },
            other => panic!("expected the catalog's own refusal, got {other:?}"),
        }
    }

    /// The same failure on a route is that route's refusal.
    #[test]
    fn a_display_failure_on_a_route_should_be_that_routes_refusal() {
        let route = VenueRoute::new(
            VenueId::Binance,
            VenueScopeId::new("spot").expect("the fixture scope is canonical"),
        );
        let error = DisplayError::MissingAssetTerms {
            asset: Box::new(test_support::token("binance", "BTC")),
        };

        match display_rejected(Some(route.clone()), error) {
            CatalogError::InvalidCatalog { route: named, .. } => {
                assert_eq!(named, route, "the refusal names the route it came from");
            }
            other => panic!("expected the route's refusal, got {other:?}"),
        }
    }
}

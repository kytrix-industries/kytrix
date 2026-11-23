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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CatalogGenerationId(u64);

impl CatalogGenerationId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for CatalogGenerationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "generation:{}", self.0)
    }
}

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Default)]
pub struct CatalogGenerationPublisher;

impl CatalogGenerationPublisher {
    pub const fn new() -> Self {
        Self
    }

    pub fn publish(&self) -> CatalogGenerationId {
        CatalogGenerationId(NEXT_GENERATION.fetch_add(1, Ordering::AcqRel))
    }
}

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

    pub const fn id(&self) -> CatalogGenerationId {
        self.id
    }

    pub fn with_registries(
        self,
        assets: Arc<AssetRegistry>,
        references: Arc<ReferenceRegistry>,
        outcomes: Arc<OutcomeRegistry>,
        publisher: &CatalogGenerationPublisher,
    ) -> Result<Self, CatalogError> {
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

    fn reporting_route(&self) -> Option<VenueRoute> {
        let mut routes: Vec<&VenueRoute> = self.route_snapshots.keys().collect();
        routes.sort();
        routes.first().map(|route| (*route).clone())
    }

    pub fn display_catalog(&self) -> &DisplayCatalog {
        &self.display_catalog
    }

    pub fn display_resolver(&self) -> DisplayResolver<'_> {
        DisplayResolver::new(self)
    }

    pub fn route_snapshots(&self) -> &AHashMap<VenueRoute, Arc<RouteSnapshot>> {
        &self.route_snapshots
    }

    pub fn route_snapshot(&self, route: &VenueRoute) -> Option<&Arc<RouteSnapshot>> {
        self.route_snapshots.get(route)
    }

    pub fn listings_for(&self, instrument: &InstrumentId) -> Option<&Arc<[VenueListingKey]>> {
        self.listings_by_instrument.get(instrument)
    }

    pub fn outcomes(&self) -> &Arc<OutcomeRegistry> {
        &self.outcome_events
    }

    pub fn assets(&self) -> &Arc<AssetRegistry> {
        &self.assets
    }

    pub fn references(&self) -> &Arc<ReferenceRegistry> {
        &self.references
    }

    pub fn outcome_groups(&self) -> &AHashMap<OutcomeGroupId, OutcomeGroup> {
        self.outcome_events.groups()
    }

    pub fn outcome_risk_pools(&self) -> &AHashMap<OutcomeRiskPoolId, OutcomeRiskPool> {
        self.outcome_events.risk_pools()
    }

    pub fn outcome_events(&self) -> &AHashMap<OutcomeEventId, OutcomeEvent> {
        self.outcome_events.events()
    }

    pub fn outcome_group_lineage(
        &self,
    ) -> &[market_participants_model::outcome::OutcomeGroupLineageEdge] {
        &self.outcome_group_lineage
    }

    pub fn outcome_risk_pool_lineage(
        &self,
    ) -> &[market_participants_model::outcome::OutcomeRiskPoolLineageEdge] {
        &self.outcome_risk_pool_lineage
    }

    pub fn with_pending(
        &self,
        pending: CatalogPending,
        publisher: &CatalogGenerationPublisher,
    ) -> Result<Self, CatalogError> {
        let id = publisher.publish();
        self.with_pending_at(pending, id)
    }

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

    pub fn reachable_outcomes(&self) -> (AHashSet<OutcomeGroupId>, AHashSet<OutcomeRiskPoolId>) {
        let reachable = ReachableOutcomes::of(&self.route_snapshots, &self.outcome_events);
        (reachable.groups, reachable.pools)
    }
}

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
    native_event_groups:
        AHashMap<market_participants_model::identifier::PolymarketEventId, Vec<OutcomeGroupId>>,
    published_events: AHashSet<OutcomeEventId>,
    display_material: Arc<RouteDisplayMaterial>,
    normalization: Arc<[IdentifierNormalizationRule]>,
}

impl RouteSnapshot {
    pub(crate) fn from_pending(pending: CatalogPending) -> Self {
        Self::from_pending_ref(&pending)
    }

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

    pub fn display_material(&self) -> &Arc<RouteDisplayMaterial> {
        &self.display_material
    }

    pub fn normalization_mode(&self, kind: ListingIdentifierKind) -> SymbolNormalization {
        let product = self.declared_product(kind);
        normalization_mode(&self.normalization, kind, product).unwrap_or(SymbolNormalization::Exact)
    }

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

    pub fn route(&self) -> &VenueRoute {
        &self.route
    }

    pub const fn route_revision(&self) -> RouteInputRevision {
        self.route_revision
    }

    pub fn entries(&self) -> &Arc<[CatalogEntry]> {
        &self.entries
    }

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

    pub fn listing_by_native_key(&self, key: &str) -> Option<&VenueListingKey> {
        self.native_keys.get(key)
    }

    pub fn listing_by_identifier(
        &self,
        identifier: &ListingIdentifier,
    ) -> Option<&VenueListingKey> {
        self.listing_identifiers.get(identifier)
    }

    pub fn listing_by_outcome_group(&self, group: &OutcomeGroupId) -> Option<&VenueListingKey> {
        self.outcome_listings.get(group)
    }

    pub fn group_by_native_identifier(
        &self,
        identifier: &OutcomeGroupIdentifier,
    ) -> Option<&OutcomeGroupId> {
        self.native_groups.get(identifier)
    }

    pub fn event_by_native_identifier(
        &self,
        event: &market_participants_model::identifier::PolymarketEventId,
    ) -> Option<&OutcomeEventId> {
        self.native_events.get(event)
    }

    pub fn native_events(
        &self,
    ) -> &AHashMap<market_participants_model::identifier::PolymarketEventId, OutcomeEventId> {
        &self.native_events
    }

    pub fn event_groups(
        &self,
        event: &market_participants_model::identifier::PolymarketEventId,
    ) -> Option<&[OutcomeGroupId]> {
        self.native_event_groups.get(event).map(Vec::as_slice)
    }

    pub fn native_event_groups(
        &self,
    ) -> &AHashMap<market_participants_model::identifier::PolymarketEventId, Vec<OutcomeGroupId>>
    {
        &self.native_event_groups
    }

    pub fn published_events(&self) -> &AHashSet<OutcomeEventId> {
        &self.published_events
    }
}

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

pub(crate) fn listing_entry(
    generation: &CatalogGeneration,
    listing: &VenueListingKey,
) -> Option<Arc<CatalogEntry>> {
    let snapshot = generation.route_snapshot(&listing.route)?;
    let entry = snapshot.listing(listing)?;
    Some(Arc::new(entry.clone()))
}

pub(crate) fn resolve_listing_entry(
    generation: &CatalogGeneration,
    listing: &VenueListingKey,
) -> Result<Arc<CatalogEntry>, CatalogError> {
    listing_entry(generation, listing).ok_or_else(|| CatalogError::ListingNotFound {
        listing: Box::new(listing.clone()),
    })
}

pub(crate) fn resolve_listing(
    generation: &CatalogGeneration,
    listing: &VenueListingKey,
) -> Result<ResolvedListing, CatalogError> {
    let entry = resolve_listing_entry(generation, listing)?;
    Ok(ResolvedListing::from_entry(generation.id(), entry))
}

pub(crate) fn resolve_listing_ref(
    generation: &CatalogGeneration,
    listing: &VenueListingKey,
) -> Result<ResolvedListingRef, CatalogError> {
    let entry = resolve_listing_entry(generation, listing)?;
    Ok(ResolvedListingRef::from_entry(generation.id(), entry))
}

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
            ListingIdentifier::PolymarketToken(metadata.asset_id.clone()),
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

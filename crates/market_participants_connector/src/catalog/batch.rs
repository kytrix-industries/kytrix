use std::sync::Arc;

use ahash::{AHashMap, AHashSet};
use market_participants_model::{
    asset::{AssetId, IndexId, UnderlyingRef},
    display::lexical::{ComponentText, normalize_component_text},
    error::ModelError,
    identifier::PolymarketEventId,
    instrument::{Instrument, InstrumentTerms},
    listing::{
        identifiers::{EchoValidationStatus, VenueListingIdentifiers},
        listing_impl::{ListingSnapshot, VenueListing},
        native::{ListingIdentifier, OutcomeGroupIdentifier},
        route::{VenueListingKey, VenueRoute},
        rules::{ProductTradingRules, TradingRules, VenueTradingPolicy},
        venue::{PriceBasis, VenueListingId},
    },
    outcome::{
        OutcomeEvent, OutcomeEventId, OutcomeGroup, OutcomeGroupId, OutcomeRiskPool,
        OutcomeRiskPoolId, validate_outcome_group_lineage, validate_outcome_risk_pool_lineage,
        validate_pool_successors, validate_risk_pool_membership,
    },
    reference::{ExternalReferenceId, PriceReferenceId, PriceReferenceTerms},
    routing::{IdentifierNormalizationRule, ListingIdentifierKind},
    settlement::{ContractSize, DeliveryMode, InstrumentLineageEdge, validate_lineage},
};
use smol_str::SmolStr;

use crate::catalog::{
    display::{
        publication::{PublicationInput, RouteDisplayMaterial, StagedAssetCode},
        types::DisplayError,
    },
    error::CatalogValidationError,
    generation::{
        CatalogGeneration, RouteSnapshot, listing_identifiers_of, normalize_listing_identifier,
    },
    registry::{
        asset::{AssetRegistry, AssetRegistryPending, AssetTerms},
        outcome::{OutcomeRegistry, OutcomeRegistryPending},
        reference::{ReferenceRegistry, ReferenceRegistryPending},
    },
    source::{CatalogFetchContext, RegisteredCatalogRoute, RouteInputRevision},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogEntry {
    pub instrument: Instrument,
    pub listing: VenueListing,
    pub snapshot: ListingSnapshot,
}

#[derive(Clone, Debug, Default)]
pub struct CatalogBatch {
    pub listings: Vec<CatalogEntry>,
    pub outcome_events: Vec<OutcomeEvent>,
    pub outcome_groups: Vec<OutcomeGroup>,
    pub outcome_risk_pools: Vec<OutcomeRiskPool>,
    pub asset_terms: Vec<AssetTerms>,
    pub reference_terms: Vec<PriceReferenceTerms>,
    pub asset_codes: Vec<StagedAssetCode>,
    pub instrument_lineage: Vec<InstrumentLineageEdge>,
    pub outcome_group_lineage: Vec<market_participants_model::outcome::OutcomeGroupLineageEdge>,
    pub outcome_risk_pool_lineage:
        Vec<market_participants_model::outcome::OutcomeRiskPoolLineageEdge>,
}

impl CatalogBatch {
    pub fn pending(
        context: &CatalogFetchContext,
        batch: Self,
        base: &CatalogGeneration,
    ) -> Result<CatalogPending, CatalogValidationError> {
        CatalogPending::stage(context, batch, base)
    }
}

#[derive(Clone, Debug)]
pub struct CatalogPending {
    route: VenueRoute,
    revision: RouteInputRevision,
    entries: Arc<[CatalogEntry]>,
    native_keys: AHashMap<SmolStr, VenueListingKey>,
    listing_identifiers: AHashMap<ListingIdentifier, VenueListingKey>,
    outcome_listings: AHashMap<OutcomeGroupId, VenueListingKey>,
    native_groups: AHashMap<OutcomeGroupIdentifier, OutcomeGroupId>,
    native_events: AHashMap<PolymarketEventId, OutcomeEventId>,
    native_event_groups: AHashMap<PolymarketEventId, Vec<OutcomeGroupId>>,
    published_events: AHashSet<OutcomeEventId>,
    pub outcome_events: Vec<OutcomeEvent>,
    pub outcome_groups: Vec<OutcomeGroup>,
    pub outcome_risk_pools: Vec<OutcomeRiskPool>,
    pub instrument_lineage: Vec<InstrumentLineageEdge>,
    pub outcome_group_lineage: Vec<market_participants_model::outcome::OutcomeGroupLineageEdge>,
    pub outcome_risk_pool_lineage:
        Vec<market_participants_model::outcome::OutcomeRiskPoolLineageEdge>,
    pub retired_groups: AHashSet<OutcomeGroupId>,
    pub retired_pools: AHashSet<OutcomeRiskPoolId>,
    display_material: Arc<RouteDisplayMaterial>,
    normalization: Arc<[IdentifierNormalizationRule]>,
    pending_assets: AssetRegistryPending,
    pending_references: ReferenceRegistryPending,
    pending_outcomes: OutcomeRegistryPending,
}

impl CatalogPending {
    fn stage(
        context: &CatalogFetchContext,
        batch: CatalogBatch,
        base: &CatalogGeneration,
    ) -> Result<Self, CatalogValidationError> {
        let route = context.route();
        let venue_route = route.route().clone();
        let mut entries: Vec<CatalogEntry> = batch
            .listings
            .into_iter()
            .map(|entry| validate_entry(&venue_route, entry))
            .collect::<Result<_, _>>()?;
        entries.sort_by(|left, right| {
            market_participants_model::listing::navigation::listing_order(
                &left.listing.key,
                &right.listing.key,
            )
        });

        let mut shared_products: AHashMap<Arc<ProductTradingRules>, ()> = AHashMap::new();
        let mut shared_policies: AHashMap<Arc<VenueTradingPolicy>, ()> = AHashMap::new();
        intern_rules(&mut entries, &mut shared_products, &mut shared_policies);

        let mut native_keys: AHashMap<SmolStr, VenueListingKey> =
            AHashMap::with_capacity(entries.len() * 2);
        let mut listing_identifiers: AHashMap<ListingIdentifier, VenueListingKey> =
            AHashMap::with_capacity(entries.len() * 2);
        let mut outcome_listings: AHashMap<OutcomeGroupId, VenueListingKey> = AHashMap::new();
        let mut listing_ids: AHashSet<VenueListingId> = AHashSet::with_capacity(entries.len());
        for entry in &entries {
            if !listing_ids.insert(entry.listing.key.listing_id) {
                return Err(CatalogValidationError::DuplicateListingId {
                    route: venue_route,
                    listing_id: entry.listing.key.listing_id,
                });
            }
            index_entry(
                route,
                entry,
                &mut native_keys,
                &mut listing_identifiers,
                &mut outcome_listings,
            )?;
        }

        let (mut pending_assets, mut pending_references, mut pending_outcomes) =
            context.staging().with(|staging| {
                (
                    staging.pending_assets().clone(),
                    staging.pending_references().clone(),
                    staging.pending_outcomes().clone(),
                )
            });
        for terms in &batch.asset_terms {
            pending_assets.stage(terms.clone());
        }
        for terms in &batch.reference_terms {
            let _ = pending_references.register_price_reference(terms.clone());
        }
        for group in batch.outcome_groups.clone() {
            let id = group.id;
            pending_outcomes
                .register_group(group, &[])
                .map_err(|error| CatalogValidationError::OutcomeGroupIncomplete {
                    group: id,
                    error: Box::new(error.into_model()),
                })?;
        }
        for pool in &batch.outcome_risk_pools {
            pending_outcomes.risk_pools.insert(pool.id, pool.clone());
        }
        for event in &batch.outcome_events {
            pending_outcomes.events.insert(event.id, event.clone());
        }

        let retired_groups: AHashSet<OutcomeGroupId> = base
            .outcome_group_lineage()
            .iter()
            .chain(batch.outcome_group_lineage.iter())
            .map(|edge| edge.predecessor)
            .collect();
        let retired_pools: AHashSet<OutcomeRiskPoolId> = base
            .outcome_risk_pool_lineage()
            .iter()
            .chain(batch.outcome_risk_pool_lineage.iter())
            .map(|edge| edge.predecessor)
            .collect();

        let batch_group_ids: AHashSet<OutcomeGroupId> =
            batch.outcome_groups.iter().map(|group| group.id).collect();
        let batch_event_ids: AHashSet<OutcomeEventId> =
            batch.outcome_events.iter().map(|event| event.id).collect();
        let base_outcomes = base.outcomes();
        let mut published_event_ids: AHashSet<OutcomeEventId> = batch_event_ids;
        for entry in &entries {
            let VenueListingIdentifiers::Polymarket(identifiers) = &entry.listing.identifiers
            else {
                continue;
            };
            let Some(event) = &identifiers.event_id else {
                continue;
            };
            let id = resolve_declared_event(event, &pending_outcomes, base_outcomes);
            if let Some(id) = id {
                published_event_ids.insert(id);
            }
        }
        let native_groups: AHashMap<OutcomeGroupIdentifier, OutcomeGroupId> = pending_outcomes
            .lookups
            .iter()
            .filter(|lookup| batch_group_ids.contains(lookup.group()))
            .map(|lookup| (lookup.identifier(), *lookup.group()))
            .collect();
        let mut native_events: AHashMap<PolymarketEventId, OutcomeEventId> = AHashMap::new();
        for (native, event) in pending_outcomes.event_lookups.iter() {
            if published_event_ids.contains(event) {
                native_events.insert(native.clone(), *event);
            }
        }
        for (native, event) in base_outcomes.event_lookups() {
            if published_event_ids.contains(event) {
                native_events.insert(native.clone(), *event);
            }
        }
        let mut native_event_groups: AHashMap<PolymarketEventId, Vec<OutcomeGroupId>> =
            AHashMap::with_capacity(published_event_ids.len());
        for event in pending_outcomes
            .events
            .values()
            .chain(base_outcomes.events().values())
        {
            if !published_event_ids.contains(&event.id) {
                continue;
            }
            let Some(native) = PolymarketEventId::new(event.native_event.as_str()).ok() else {
                continue;
            };
            native_events.insert(native.clone(), event.id);
            native_event_groups
                .entry(native)
                .or_default()
                .extend(event.groups.iter().cloned());
        }
        for groups in native_event_groups.values_mut() {
            groups.sort();
            groups.dedup();
        }

        let pending = Self {
            route: venue_route,
            revision: route.route_revision(),
            entries: entries.into(),
            native_keys,
            listing_identifiers,
            outcome_listings,
            native_groups,
            native_events,
            native_event_groups,
            published_events: published_event_ids,
            outcome_events: batch.outcome_events,
            outcome_groups: batch.outcome_groups,
            outcome_risk_pools: batch.outcome_risk_pools,
            instrument_lineage: batch.instrument_lineage,
            outcome_group_lineage: batch.outcome_group_lineage,
            outcome_risk_pool_lineage: batch.outcome_risk_pool_lineage,
            retired_groups,
            retired_pools,
            display_material: Arc::new(RouteDisplayMaterial {
                asset_codes: batch.asset_codes.into(),
            }),
            normalization: Arc::from(route.registration().normalization.clone()),
            pending_assets,
            pending_references,
            pending_outcomes,
        };
        pending.validate_against_generation(base)?;
        Ok(pending)
    }

    pub(crate) fn display_material(&self) -> &Arc<RouteDisplayMaterial> {
        &self.display_material
    }

    pub(crate) fn normalization(&self) -> &Arc<[IdentifierNormalizationRule]> {
        &self.normalization
    }

    fn validate_against_generation(
        &self,
        base: &CatalogGeneration,
    ) -> Result<(), CatalogValidationError> {
        let held = HeldIdentities::new(self, base);
        self.validate_outcome_closure(&held)?;
        self.validate_outcome_price_terms(&held)?;
        validate_lineage(&self.instrument_lineage, |field| {
            ModelError::InstrumentLineageInvalid { field }
        })
        .map_err(|error| CatalogValidationError::InstrumentLineageInvalid {
            error: Box::new(error),
        })?;
        validate_outcome_group_lineage(&self.outcome_group_lineage).map_err(|_| {
            CatalogValidationError::OutcomeGroupLineageInvalid {
                group: self
                    .outcome_group_lineage
                    .first()
                    .map(|edge| edge.successor)
                    .unwrap_or_else(|| held.any_group()),
            }
        })?;
        validate_outcome_risk_pool_lineage(&self.outcome_risk_pool_lineage).map_err(|_| {
            CatalogValidationError::OutcomeRiskPoolLineageInvalid {
                pool: self
                    .outcome_risk_pool_lineage
                    .first()
                    .map(|edge| edge.successor)
                    .unwrap_or_else(|| held.any_pool()),
            }
        })?;
        let mut successors: AHashSet<&OutcomeRiskPoolId> =
            AHashSet::with_capacity(self.outcome_risk_pool_lineage.len());
        for edge in &self.outcome_risk_pool_lineage {
            if !held.base_pool(&edge.predecessor) || !held.batch_pool(&edge.successor) {
                return Err(CatalogValidationError::OutcomeRiskPoolLineageInvalid {
                    pool: if held.base_pool(&edge.predecessor) {
                        edge.successor
                    } else {
                        edge.predecessor
                    },
                });
            }
            if !successors.insert(&edge.successor) {
                return Err(CatalogValidationError::OutcomeRiskPoolLineageInvalid {
                    pool: edge.successor,
                });
            }
        }
        let mut group_successors: AHashSet<&OutcomeGroupId> =
            AHashSet::with_capacity(self.outcome_group_lineage.len());
        for edge in &self.outcome_group_lineage {
            if !held.has_group(&edge.successor) || !held.has_group(&edge.predecessor) {
                return Err(CatalogValidationError::OutcomeGroupLineageInvalid {
                    group: if held.has_group(&edge.successor) {
                        edge.predecessor
                    } else {
                        edge.successor
                    },
                });
            }
            if !group_successors.insert(&edge.successor) {
                return Err(CatalogValidationError::OutcomeGroupLineageInvalid {
                    group: edge.successor,
                });
            }
        }
        validate_pool_successors(&held.group_edges, &held.pool_edges, &held.all_pools()).map_err(
            |_| CatalogValidationError::OutcomeRiskPoolLineageInvalid {
                pool: held.unsuccessored_pool().unwrap_or_else(|| held.any_pool()),
            },
        )?;
        for pool in &held.current_pools {
            for group in &pool.groups {
                if held.retired_groups.contains(group) {
                    return Err(CatalogValidationError::OutcomeRiskPoolIncomplete {
                        pool: pool.id,
                        error: Box::new(ModelError::OutcomeRiskPoolUnresolved {
                            field: "risk pool references a replaced group",
                        }),
                    });
                }
            }
        }
        for event in &self.outcome_events {
            for group in &event.groups {
                if held.retired_groups.contains(group) {
                    return Err(CatalogValidationError::OutcomeEventIncomplete {
                        event: event.id,
                        group: Some(*group),
                        pool: None,
                    });
                }
            }
            for pool in &event.risk_pools {
                if held.retired_pools.contains(pool) {
                    return Err(CatalogValidationError::OutcomeEventIncomplete {
                        event: event.id,
                        group: None,
                        pool: Some(*pool),
                    });
                }
            }
        }
        validate_risk_pool_membership(&held.current_pools).map_err(|error| {
            CatalogValidationError::AmbiguousRiskPoolMembership {
                group: match error {
                    ModelError::OutcomeRiskPoolIncomplete { pool, .. } => held
                        .group_of_pool(&pool)
                        .unwrap_or_else(|| held.any_group()),
                    _ => held.any_group(),
                },
            }
        })?;
        self.validate_display(base)
    }

    fn validate_outcome_price_terms(
        &self,
        held: &HeldIdentities,
    ) -> Result<(), CatalogValidationError> {
        for entry in self.entries.iter() {
            let InstrumentTerms::Outcome(terms) = entry.instrument.terms() else {
                continue;
            };
            let Some(group) = held.group(terms.group()) else {
                continue;
            };
            let price = &entry.snapshot.rules.common.price;
            if price.quote_asset != group.settlement_asset {
                return Err(CatalogValidationError::OutcomePriceQuoteMismatch {
                    listing: Box::new(entry.listing.key.clone()),
                    expected: Box::new(group.settlement_asset.clone()),
                    found: Box::new(price.quote_asset.clone()),
                });
            }
            if price.basis != PriceBasis::FractionOfCollateral {
                return Err(CatalogValidationError::OutcomePriceBasisUnsupported {
                    listing: Box::new(entry.listing.key.clone()),
                    basis: Box::new(price.basis.clone()),
                });
            }
        }
        Ok(())
    }

    fn validate_display(&self, base: &CatalogGeneration) -> Result<(), CatalogValidationError> {
        let referenced = crate::catalog::batch::ReferencedTerms::collect(self, base);
        let (assets, references, outcomes) = crate::catalog::batch::staged_registries(
            base,
            &self.pending_assets,
            &self.pending_references,
            &self.pending_outcomes,
            &referenced,
        );
        for entry in self.entries.iter() {
            validate_reference_roles(entry, &references)?;
        }
        let mut route_snapshots = base.route_snapshots().clone();
        let snapshot = Arc::new(RouteSnapshot::from_pending_ref(self));
        route_snapshots.insert(self.route.clone(), Arc::clone(&snapshot));
        for entry in self.entries.iter() {
            let listing = &entry.listing;
            let echoed = entry
                .listing
                .validate_echoed_identifiers(|kind| snapshot.normalization_mode(kind))
                .map_err(|error| CatalogValidationError::DisplayRejected {
                    listing: Some(Box::new(listing.key.clone())),
                    error: Box::new(DisplayError::ListingIdentifierEcho {
                        listing: Box::new(listing.key.clone()),
                        kind: listing.identifiers.primary_kind(),
                        error: Box::new(error),
                    }),
                })?;
            if let EchoValidationStatus::Mismatch { kind, value } = echoed {
                return Err(CatalogValidationError::DisplayRejected {
                    listing: Some(Box::new(listing.key.clone())),
                    error: Box::new(DisplayError::ListingIdentifierMismatch {
                        listing: Box::new(listing.key.clone()),
                        kind,
                        value,
                    }),
                });
            }
            let refused = |error: DisplayError| CatalogValidationError::DisplayRejected {
                listing: Some(Box::new(listing.key.clone())),
                error: Box::new(error),
            };
            if let Some(value) = outcome_label_echo(entry, &outcomes).map_err(refused)? {
                return Err(refused(DisplayError::ListingIdentifierMismatch {
                    listing: Box::new(listing.key.clone()),
                    kind: ListingIdentifierKind::PolymarketToken,
                    value,
                }));
            }
        }
        crate::catalog::display::publication::build_display_catalog(PublicationInput {
            snapshots: &route_snapshots,
            assets: &assets,
            references: &references,
            outcomes: &outcomes,
        })
        .map(|_| ())
        .map_err(|error| CatalogValidationError::DisplayRejected {
            listing: None,
            error: Box::new(error),
        })
    }

    fn validate_outcome_closure(
        &self,
        held: &HeldIdentities,
    ) -> Result<(), CatalogValidationError> {
        for entry in self.entries.iter() {
            if let InstrumentTerms::Outcome(terms) = entry.instrument.terms()
                && !held.has_group(terms.group())
            {
                return Err(CatalogValidationError::IncompleteOutcomeSet {
                    route: self.route.clone(),
                    listing: Box::new(entry.listing.key.clone()),
                    kind: "outcome group",
                    count: 1,
                });
            }
        }
        for group in &self.outcome_groups {
            if let Err(error) = group.validate() {
                return Err(CatalogValidationError::OutcomeGroupIncomplete {
                    group: group.id,
                    error: Box::new(error),
                });
            }
        }
        for entry in self.entries.iter() {
            let InstrumentTerms::Outcome(terms) = entry.instrument.terms() else {
                continue;
            };
            let Some(group) = held.group(terms.group()) else {
                continue;
            };
            if group.side(terms.side()).is_none()
                || !group
                    .member(terms.member())
                    .is_some_and(|member| member.sides.iter().any(|side| side == terms.side()))
            {
                return Err(CatalogValidationError::OutcomeListingLegUnknown {
                    listing: Box::new(entry.listing.key.clone()),
                    group: *terms.group(),
                    member: *terms.member(),
                    side: *terms.side(),
                });
            }
        }
        for pool in &self.outcome_risk_pools {
            if let Err(error) = pool.validate() {
                return Err(CatalogValidationError::OutcomeRiskPoolIncomplete {
                    pool: pool.id,
                    error: Box::new(error),
                });
            }
            for group in &pool.groups {
                if !held.has_group(group) {
                    return Err(CatalogValidationError::OutcomeRiskPoolIncomplete {
                        pool: pool.id,
                        error: Box::new(ModelError::OutcomeRiskPoolUnresolved {
                            field: "risk pool member group",
                        }),
                    });
                }
            }
            for conversion in &pool.conversions {
                for leg in std::iter::once(&conversion.input)
                    .chain(conversion.outputs.iter().map(|output| &output.leg))
                {
                    if !held.has_group(&leg.group) {
                        return Err(CatalogValidationError::OutcomeRiskPoolIncomplete {
                            pool: pool.id,
                            error: Box::new(ModelError::OutcomeRiskPoolUnresolved {
                                field: "conversion leg group",
                            }),
                        });
                    }
                }
            }
        }
        for event in &self.outcome_events {
            for group in &event.groups {
                if !held.has_group(group) {
                    return Err(CatalogValidationError::OutcomeEventIncomplete {
                        event: event.id,
                        group: Some(*group),
                        pool: None,
                    });
                }
            }
            for pool in &event.risk_pools {
                if !held.has_pool(pool) {
                    return Err(CatalogValidationError::OutcomeEventIncomplete {
                        event: event.id,
                        group: None,
                        pool: Some(*pool),
                    });
                }
            }
        }
        for entry in self.entries.iter() {
            let VenueListingIdentifiers::Polymarket(identifiers) = &entry.listing.identifiers
            else {
                continue;
            };
            let Some(event) = &identifiers.event_id else {
                continue;
            };
            if !self.native_events.contains_key(event) {
                return Err(CatalogValidationError::ListingEventUnknown {
                    listing: Box::new(entry.listing.key.clone()),
                    event: event.clone(),
                });
            }
        }
        Ok(())
    }

    pub(crate) fn into_snapshot(self) -> RouteSnapshot {
        RouteSnapshot::from_pending(self)
    }

    pub fn route(&self) -> &VenueRoute {
        &self.route
    }

    pub fn revision(&self) -> RouteInputRevision {
        self.revision
    }

    pub fn entries(&self) -> &Arc<[CatalogEntry]> {
        &self.entries
    }

    pub fn native_keys(&self) -> &AHashMap<SmolStr, VenueListingKey> {
        &self.native_keys
    }

    pub fn listing_identifiers(&self) -> &AHashMap<ListingIdentifier, VenueListingKey> {
        &self.listing_identifiers
    }

    pub fn outcome_listings(&self) -> &AHashMap<OutcomeGroupId, VenueListingKey> {
        &self.outcome_listings
    }

    pub fn native_groups(&self) -> &AHashMap<OutcomeGroupIdentifier, OutcomeGroupId> {
        &self.native_groups
    }

    pub fn native_events(&self) -> &AHashMap<PolymarketEventId, OutcomeEventId> {
        &self.native_events
    }

    pub fn native_event_groups(&self) -> &AHashMap<PolymarketEventId, Vec<OutcomeGroupId>> {
        &self.native_event_groups
    }

    pub fn published_events(&self) -> &AHashSet<OutcomeEventId> {
        &self.published_events
    }

    pub fn pending_assets(&self) -> &AssetRegistryPending {
        &self.pending_assets
    }

    pub fn pending_references(&self) -> &ReferenceRegistryPending {
        &self.pending_references
    }

    pub fn pending_outcomes(&self) -> &OutcomeRegistryPending {
        &self.pending_outcomes
    }
}

struct HeldIdentities {
    batch_groups: AHashSet<OutcomeGroupId>,
    base_groups: AHashSet<OutcomeGroupId>,
    group_terms: AHashMap<OutcomeGroupId, OutcomeGroup>,
    batch_pools: AHashSet<OutcomeRiskPoolId>,
    base_pools: AHashSet<OutcomeRiskPoolId>,
    pools: Vec<OutcomeRiskPool>,
    current_pools: Vec<OutcomeRiskPool>,
    retired_groups: AHashSet<OutcomeGroupId>,
    retired_pools: AHashSet<OutcomeRiskPoolId>,
    group_edges: Vec<market_participants_model::outcome::OutcomeGroupLineageEdge>,
    pool_edges: Vec<market_participants_model::outcome::OutcomeRiskPoolLineageEdge>,
    first_group: Option<OutcomeGroupId>,
    first_pool: Option<OutcomeRiskPoolId>,
}

impl HeldIdentities {
    fn new(pending: &CatalogPending, base: &CatalogGeneration) -> Self {
        let batch_groups: AHashSet<OutcomeGroupId> = pending
            .outcome_groups
            .iter()
            .map(|group| group.id)
            .collect();
        let batch_pools: AHashSet<OutcomeRiskPoolId> = pending
            .outcome_risk_pools
            .iter()
            .map(|pool| pool.id)
            .collect();
        let pools: Vec<OutcomeRiskPool> = base
            .outcome_risk_pools()
            .values()
            .chain(pending.outcome_risk_pools.iter())
            .cloned()
            .collect();
        let current_pools: Vec<OutcomeRiskPool> = pools
            .iter()
            .filter(|pool| !pending.retired_pools.contains(&pool.id))
            .cloned()
            .collect();
        let mut group_edges = base.outcome_group_lineage().to_vec();
        group_edges.extend(pending.outcome_group_lineage.iter().cloned());
        let mut pool_edges = base.outcome_risk_pool_lineage().to_vec();
        pool_edges.extend(pending.outcome_risk_pool_lineage.iter().cloned());
        Self {
            first_group: pending
                .outcome_groups
                .first()
                .map(|group| group.id)
                .or_else(|| base.outcome_groups().keys().next().cloned()),
            first_pool: pending
                .outcome_risk_pools
                .first()
                .map(|pool| pool.id)
                .or_else(|| base.outcome_risk_pools().keys().next().cloned()),
            base_groups: base.outcome_groups().keys().cloned().collect(),
            group_terms: {
                let mut terms: AHashMap<OutcomeGroupId, OutcomeGroup> =
                    base.outcome_groups().clone();
                for group in &pending.outcome_groups {
                    terms.insert(group.id, group.clone());
                }
                terms
            },
            base_pools: base.outcome_risk_pools().keys().cloned().collect(),
            batch_groups,
            batch_pools,
            pools,
            current_pools,
            retired_groups: pending.retired_groups.clone(),
            retired_pools: pending.retired_pools.clone(),
            group_edges,
            pool_edges,
        }
    }

    fn has_group(&self, group: &OutcomeGroupId) -> bool {
        self.batch_groups.contains(group) || self.base_groups.contains(group)
    }

    fn group(&self, group: &OutcomeGroupId) -> Option<&OutcomeGroup> {
        self.group_terms.get(group)
    }

    fn has_pool(&self, pool: &OutcomeRiskPoolId) -> bool {
        self.batch_pools.contains(pool) || self.base_pools.contains(pool)
    }

    fn base_pool(&self, pool: &OutcomeRiskPoolId) -> bool {
        self.base_pools.contains(pool)
    }

    fn batch_pool(&self, pool: &OutcomeRiskPoolId) -> bool {
        self.batch_pools.contains(pool)
    }

    fn all_pools(&self) -> Vec<OutcomeRiskPool> {
        self.pools.clone()
    }

    fn group_of_pool(&self, pool: &OutcomeRiskPoolId) -> Option<OutcomeGroupId> {
        self.pools
            .iter()
            .find(|candidate| &candidate.id == pool)
            .and_then(|candidate| candidate.groups.first().cloned())
    }

    fn unsuccessored_pool(&self) -> Option<OutcomeRiskPoolId> {
        let replaced: AHashSet<&OutcomeGroupId> = self
            .group_edges
            .iter()
            .map(|edge| &edge.predecessor)
            .collect();
        self.pools
            .iter()
            .find(|pool| {
                pool.groups.iter().any(|group| replaced.contains(group))
                    && !self
                        .pool_edges
                        .iter()
                        .any(|edge| edge.predecessor == pool.id)
            })
            .map(|pool| pool.id)
    }

    fn any_group(&self) -> OutcomeGroupId {
        self.first_group
            .expect("a refusal always has a batch and a generation to name")
    }

    fn any_pool(&self) -> OutcomeRiskPoolId {
        self.first_pool
            .expect("a refusal always has a batch and a generation to name")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReferenceRole {
    Conversion,
    Settlement,
}

impl ReferenceRole {
    const fn field(self) -> &'static str {
        match self {
            Self::Conversion => "conversion reference",
            Self::Settlement => "cash settlement reference",
        }
    }
}

struct ReferenceDirection {
    role: ReferenceRole,
    underlying: AssetId,
    quote: AssetId,
    basis: PriceBasis,
}

impl ReferenceDirection {
    fn matches(&self, terms: &PriceReferenceTerms) -> bool {
        terms.underlying == UnderlyingRef::Asset(self.underlying.clone())
            && terms.quote_asset == self.quote
            && terms.basis == self.basis
    }
}

fn validate_reference_roles(
    entry: &CatalogEntry,
    references: &ReferenceRegistry,
) -> Result<(), CatalogValidationError> {
    let mut checks: Vec<ReferenceCheck> = Vec::new();
    match entry.instrument.terms() {
        InstrumentTerms::Perpetual(terms) => {
            if let Some(conversion) = terms.conversion_reference() {
                checks.push(conversion_direction(
                    *conversion,
                    terms.settlement_asset(),
                    terms.price_quote(),
                ));
            }
        }
        InstrumentTerms::Future(terms) => {
            checks.extend(settlement_direction(
                terms.delivery(),
                terms.underlying().reference(),
                terms.price_quote(),
            ));
            if let Some(conversion) = terms.delivery().cash_conversion_reference()
                && let Some(asset) = terms.delivery().cash_asset()
            {
                checks.push(conversion_direction(
                    *conversion,
                    asset,
                    terms.price_quote(),
                ));
            }
        }
        InstrumentTerms::Option(terms) => {
            checks.extend(settlement_direction(
                terms.delivery(),
                terms.underlying().reference(),
                terms.price_quote(),
            ));
            if let Some(conversion) = terms.conversion_reference()
                && let Some(asset) = terms.delivery().cash_asset()
            {
                checks.push(conversion_direction(
                    *conversion,
                    asset,
                    terms.price_quote(),
                ));
            }
        }
        InstrumentTerms::Spot(_) | InstrumentTerms::Outcome(_) => {}
    }
    for check in checks {
        let Some(terms) = references.price_reference(&check.reference) else {
            return Err(CatalogValidationError::DisplayRejected {
                listing: Some(Box::new(entry.listing.key.clone())),
                error: Box::new(DisplayError::MissingReferenceTerms {
                    reference: check.reference,
                }),
            });
        };
        if !check.direction.matches(terms) {
            return Err(CatalogValidationError::DisplayRejected {
                listing: Some(Box::new(entry.listing.key.clone())),
                error: Box::new(DisplayError::ReferenceDirectionMismatch {
                    reference: check.reference,
                    field: check.direction.role.field(),
                }),
            });
        }
    }
    Ok(())
}

fn outcome_label_echo(
    entry: &CatalogEntry,
    outcomes: &OutcomeRegistry,
) -> Result<Option<SmolStr>, DisplayError> {
    let InstrumentTerms::Outcome(terms) = entry.instrument.terms() else {
        return Ok(None);
    };
    let Some(label) = entry.listing.protocol_outcome_label() else {
        return Ok(None);
    };
    let missing = || DisplayError::MissingOutcomeTerms {
        group: *terms.group(),
    };
    let group = outcomes.group(terms.group()).ok_or_else(missing)?;
    let protocol = normalize_component_text(ComponentText::FreeText(label))
        .map_err(|error| DisplayError::Format { error })?;
    let member = group.member(terms.member()).ok_or_else(missing)?;
    let side = group.side(terms.side()).ok_or_else(missing)?;
    let echoed = [&member.label, &side.label].into_iter().any(|registry| {
        normalize_component_text(ComponentText::FreeText(registry))
            .is_ok_and(|registry| registry == protocol)
    });
    Ok((!echoed).then_some(protocol))
}

struct ReferenceCheck {
    reference: PriceReferenceId,
    direction: ReferenceDirection,
}

fn conversion_direction(
    reference: PriceReferenceId,
    settlement_asset: &AssetId,
    price_quote: &AssetId,
) -> ReferenceCheck {
    ReferenceCheck {
        reference,
        direction: ReferenceDirection {
            role: ReferenceRole::Conversion,
            underlying: settlement_asset.clone(),
            quote: price_quote.clone(),
            basis: PriceBasis::PerAssetUnit,
        },
    }
}

fn settlement_direction(
    delivery: &DeliveryMode,
    underlying: &UnderlyingRef,
    price_quote: &AssetId,
) -> Vec<ReferenceCheck> {
    let DeliveryMode::Cash { reference, .. } = delivery else {
        return Vec::new();
    };
    let direction = match underlying {
        UnderlyingRef::Asset(underlying) => ReferenceDirection {
            role: ReferenceRole::Settlement,
            underlying: underlying.clone(),
            quote: price_quote.clone(),
            basis: PriceBasis::PerAssetUnit,
        },
        UnderlyingRef::Index(_) | UnderlyingRef::External(_) => return Vec::new(),
    };
    vec![ReferenceCheck {
        reference: *reference,
        direction,
    }]
}
fn intern_rules(
    entries: &mut [CatalogEntry],
    products: &mut AHashMap<Arc<ProductTradingRules>, ()>,
    policies: &mut AHashMap<Arc<VenueTradingPolicy>, ()>,
) {
    let mut product_of: AHashMap<ProductTradingRules, Arc<ProductTradingRules>> =
        AHashMap::with_capacity(products.capacity());
    let mut policy_of: AHashMap<VenueTradingPolicy, Arc<VenueTradingPolicy>> =
        AHashMap::with_capacity(policies.capacity());
    for entry in entries.iter_mut() {
        let rules = &mut entry.snapshot.rules;
        let existing = product_of.get(rules.product.as_ref());
        rules.product = match existing {
            Some(shared) => Arc::clone(shared),
            None => {
                let shared = Arc::clone(&rules.product);
                product_of.insert(shared.as_ref().clone(), Arc::clone(&shared));
                shared
            }
        };
        let existing = policy_of.get(rules.common.venue_policy.as_ref());
        rules.common.venue_policy = match existing {
            Some(shared) => Arc::clone(shared),
            None => {
                let shared = Arc::clone(&rules.common.venue_policy);
                policy_of.insert(shared.as_ref().clone(), Arc::clone(&shared));
                shared
            }
        };
    }
    products.extend(product_of.into_values().map(|shared| (shared, ())));
    policies.extend(policy_of.into_values().map(|shared| (shared, ())));
}

fn validate_entry(
    route: &VenueRoute,
    entry: CatalogEntry,
) -> Result<CatalogEntry, CatalogValidationError> {
    if &entry.listing.key.route != route {
        return Err(CatalogValidationError::ListingRouteMismatch {
            route: route.clone(),
            listing: Box::new(entry.listing.key),
        });
    }
    if entry.snapshot.key != entry.listing.key {
        return Err(CatalogValidationError::SnapshotKeyMismatch {
            listing: Box::new(entry.listing.key.clone()),
            snapshot: Box::new(entry.snapshot.key),
        });
    }
    if &entry.listing.instrument_id != entry.instrument.id() {
        return Err(CatalogValidationError::InstrumentTermsMismatch {
            listing: Box::new(entry.listing.key.clone()),
            declared: entry.listing.instrument_id,
            computed: *entry.instrument.id(),
        });
    }
    entry
        .listing
        .validate_for(&entry.instrument)
        .map_err(|error| match error {
            ModelError::InstrumentTermsMismatch { .. } => {
                CatalogValidationError::InstrumentTermsMismatch {
                    listing: Box::new(entry.listing.key.clone()),
                    declared: entry.listing.instrument_id,
                    computed: *entry.instrument.id(),
                }
            }
            other => CatalogValidationError::InvalidListingIdentifiers {
                listing: Box::new(entry.listing.key.clone()),
                kind: entry.listing.identifiers.primary_kind(),
                error: Box::new(other),
            },
        })?;
    entry
        .snapshot
        .validate(0)
        .map_err(|error| CatalogValidationError::InvalidTradingRules {
            listing: Box::new(entry.listing.key.clone()),
            error: Box::new(error),
        })?;
    let rules_kind = entry.snapshot.rules.product.instrument_kind();
    if rules_kind != entry.instrument.kind() {
        return Err(CatalogValidationError::InstrumentKindMismatch {
            listing: Box::new(entry.listing.key.clone()),
            declared: entry.instrument.kind(),
            kind: rules_kind,
        });
    }
    let outcome = matches!(entry.instrument.terms(), InstrumentTerms::Outcome(_));
    if outcome {
        entry.snapshot.rules.validate().map_err(|error| {
            CatalogValidationError::InvalidTradingRules {
                listing: Box::new(entry.listing.key.clone()),
                error: Box::new(error),
            }
        })?;
        entry
            .snapshot
            .rules
            .common
            .quantity
            .unit
            .validate_for_instrument(&entry.instrument)
            .map_err(|error| CatalogValidationError::InvalidTradingRules {
                listing: Box::new(entry.listing.key.clone()),
                error: Box::new(error),
            })?;
    } else {
        entry
            .snapshot
            .validate_for(&entry.instrument)
            .map_err(|error| CatalogValidationError::InvalidTradingRules {
                listing: Box::new(entry.listing.key.clone()),
                error: Box::new(error),
            })?;
    }
    Ok(entry)
}

fn index_entry(
    route: &RegisteredCatalogRoute,
    entry: &CatalogEntry,
    native_keys: &mut AHashMap<SmolStr, VenueListingKey>,
    listing_identifiers: &mut AHashMap<ListingIdentifier, VenueListingKey>,
    outcome_listings: &mut AHashMap<OutcomeGroupId, VenueListingKey>,
) -> Result<(), CatalogValidationError> {
    let venue_route = route.route().clone();
    let display_keys: Vec<SmolStr> = entry
        .listing
        .normalized_keys(|kind| route.registration().normalization_mode(kind))
        .map_err(
            |error| CatalogValidationError::IdentifierNormalizationMismatch {
                listing: Box::new(entry.listing.key.clone()),
                kind: entry.listing.identifiers.primary_kind(),
                error: Box::new(error),
            },
        )?;
    let mut normalized_keys: Vec<SmolStr> = Vec::with_capacity(display_keys.len() + 2);
    normalized_keys.extend(display_keys);
    for typed in listing_identifiers_of(&entry.listing) {
        let mode = route.registration().identifier_normalization_mode(&typed);
        let folded = normalize_listing_identifier(&typed, mode).map_err(|error| {
            CatalogValidationError::IdentifierNormalizationMismatch {
                listing: Box::new(entry.listing.key.clone()),
                kind: typed.kind(),
                error: Box::new(error),
            }
        })?;
        if let Some(applied) = folded.symbol_value() {
            normalized_keys.push(SmolStr::new(applied));
        }
        listing_identifiers.insert(folded, entry.listing.key.clone());
    }
    normalized_keys.sort();
    normalized_keys.dedup();
    for key in normalized_keys {
        insert_native_key(&venue_route, native_keys, key, &entry.listing.key)?;
    }
    if let InstrumentTerms::Outcome(terms) = entry.instrument.terms() {
        outcome_listings.insert(*terms.group(), entry.listing.key.clone());
    }
    Ok(())
}

fn insert_native_key(
    route: &VenueRoute,
    native_keys: &mut AHashMap<SmolStr, VenueListingKey>,
    key: SmolStr,
    listing: &VenueListingKey,
) -> Result<(), CatalogValidationError> {
    if native_keys.insert(key.clone(), listing.clone()).is_some() {
        return Err(CatalogValidationError::DuplicateNativeIdentifier {
            route: route.clone(),
            identifier: key,
        });
    }
    Ok(())
}

pub(crate) fn resolve_declared_event(
    declared: &PolymarketEventId,
    pending_events: &OutcomeRegistryPending,
    base: &OutcomeRegistry,
) -> Option<OutcomeEventId> {
    for (native, id) in pending_events.event_lookups.iter() {
        if native == declared {
            return Some(*id);
        }
    }
    for (native, id) in base.event_lookups() {
        if native == declared {
            return Some(*id);
        }
    }
    for candidate in pending_events.events.values().chain(base.events().values()) {
        if candidate.native_event.as_str() == declared.as_str() {
            return Some(candidate.id);
        }
    }
    None
}

pub(crate) fn declared_events(
    entries: &[CatalogEntry],
    pending_events: &OutcomeRegistryPending,
    base: &OutcomeRegistry,
) -> AHashSet<OutcomeEventId> {
    let mut declared: AHashSet<OutcomeEventId> = AHashSet::new();
    for entry in entries {
        let VenueListingIdentifiers::Polymarket(identifiers) = &entry.listing.identifiers else {
            continue;
        };
        let Some(event) = &identifiers.event_id else {
            continue;
        };
        if let Some(id) = resolve_declared_event(event, pending_events, base) {
            declared.insert(id);
        }
    }
    declared
}

#[derive(Debug, Default)]
pub struct ReferencedTerms {
    pub assets: AHashSet<AssetId>,
    pub indices: AHashSet<IndexId>,
    pub external_references: AHashSet<ExternalReferenceId>,
    pub price_references: AHashSet<PriceReferenceId>,
    pub groups: AHashSet<OutcomeGroupId>,
    pub pools: AHashSet<OutcomeRiskPoolId>,
    pub events: AHashSet<OutcomeEventId>,
    pub retired_groups: AHashSet<OutcomeGroupId>,
    pub retired_pools: AHashSet<OutcomeRiskPoolId>,
}

impl ReferencedTerms {
    pub(crate) fn collect(pending: &CatalogPending, base: &CatalogGeneration) -> Self {
        let mut referenced = Self::default();
        for entry in pending.entries.iter() {
            collect_instrument(&entry.instrument, &mut referenced);
            collect_rules(&entry.snapshot.rules, &mut referenced.assets);
        }
        for group in &pending.outcome_groups {
            referenced.groups.insert(group.id);
            referenced.assets.insert(group.settlement_asset.clone());
        }
        for event in &pending.outcome_events {
            referenced.groups.extend(event.groups.iter().cloned());
            referenced.pools.extend(event.risk_pools.iter().cloned());
        }
        referenced.events =
            declared_events(&pending.entries, &pending.pending_outcomes, base.outcomes());
        for pool in &pending.outcome_risk_pools {
            referenced.pools.insert(pool.id);
            referenced.assets.insert(pool.settlement_asset.clone());
        }
        for edge in &pending.outcome_group_lineage {
            referenced.groups.insert(edge.predecessor);
            referenced.groups.insert(edge.successor);
        }
        for edge in &pending.outcome_risk_pool_lineage {
            referenced.pools.insert(edge.predecessor);
            referenced.pools.insert(edge.successor);
        }
        referenced.retired_groups = pending.retired_groups.clone();
        referenced.retired_pools = pending.retired_pools.clone();
        referenced
    }
}

fn collect_instrument(instrument: &Instrument, referenced: &mut ReferencedTerms) {
    match instrument.terms() {
        InstrumentTerms::Spot(terms) => {
            referenced.assets.insert(terms.base().clone());
            referenced.assets.insert(terms.quote().clone());
        }
        InstrumentTerms::Perpetual(terms) => {
            collect_derivative(terms.underlying(), referenced);
            referenced.assets.insert(terms.price_quote().clone());
            referenced.assets.insert(terms.settlement_asset().clone());
            collect_contract_size(terms.contract_size(), &mut referenced.assets);
            if let Some(conversion) = terms.conversion_reference() {
                referenced.price_references.insert(*conversion);
            }
        }
        InstrumentTerms::Future(terms) => {
            collect_derivative(terms.underlying(), referenced);
            referenced.assets.insert(terms.price_quote().clone());
            collect_contract_size(terms.contract_size(), &mut referenced.assets);
            collect_delivery(terms.delivery(), referenced);
        }
        InstrumentTerms::Option(terms) => {
            collect_derivative(terms.underlying(), referenced);
            referenced.assets.insert(terms.price_quote().clone());
            referenced
                .assets
                .insert(terms.strike().quote_asset().clone());
            collect_contract_size(terms.contract_size(), &mut referenced.assets);
            collect_delivery(terms.delivery(), referenced);
            if let Some(conversion) = terms.conversion_reference() {
                referenced.price_references.insert(*conversion);
            }
        }
        InstrumentTerms::Outcome(_) => {}
    }
}

pub(crate) fn collect_derivative(
    underlying: &market_participants_model::asset::DerivativeUnderlying,
    referenced: &mut ReferencedTerms,
) {
    match underlying.reference() {
        UnderlyingRef::Asset(asset) => {
            referenced.assets.insert(asset.clone());
        }
        UnderlyingRef::Index(index) => {
            referenced.indices.insert(*index);
        }
        UnderlyingRef::External(external) => {
            referenced.external_references.insert(*external);
        }
    }
    referenced
        .price_references
        .insert(*underlying.price_reference());
}

fn collect_contract_size(size: &ContractSize, assets: &mut AHashSet<AssetId>) {
    if let Some(asset) = size.denomination() {
        assets.insert(asset.clone());
    }
    if let Some(asset) = size.quote_asset() {
        assets.insert(asset.clone());
    }
}

fn collect_delivery(
    delivery: &market_participants_model::settlement::DeliveryMode,
    referenced: &mut ReferencedTerms,
) {
    if let Some(asset) = delivery.cash_asset() {
        referenced.assets.insert(asset.clone());
    }
    for leg in delivery.deliveries() {
        referenced.assets.insert(leg.asset().clone());
    }
    if let Some(reference) = delivery.cash_reference() {
        referenced.price_references.insert(*reference);
    }
    if let Some(conversion) = delivery.cash_conversion_reference() {
        referenced.price_references.insert(*conversion);
    }
}

fn collect_rules(rules: &TradingRules, assets: &mut AHashSet<AssetId>) {
    assets.insert(rules.common.price.quote_asset.clone());
    if let Some(notional) = &rules.common.notional {
        assets.insert(notional.asset.clone());
    }
    if let Some(asset) = rules.common.quantity.unit.asset() {
        assets.insert(asset.clone());
    }
}

pub fn publish_assets(
    base: &Arc<AssetRegistry>,
    pending: &AssetRegistryPending,
    referenced: &ReferencedTerms,
) -> AssetRegistry {
    let mut currencies = AHashMap::new();
    let mut crypto_tokens = AHashMap::new();
    let mut securities = AHashMap::new();
    let mut share_classes = AHashMap::new();
    let mut commodities = AHashMap::new();
    for asset in base.assets() {
        match &asset {
            AssetId::Currency(id) => {
                if let Some(terms) = base.currency(id) {
                    currencies.insert(*id, terms.clone());
                }
            }
            AssetId::CryptoToken(id) => {
                if let Some(terms) = base.crypto_token(id) {
                    crypto_tokens.insert(*id, terms.clone());
                }
            }
            AssetId::Security(id) => {
                if let Some(terms) = base.security(id) {
                    securities.insert(*id, terms.clone());
                }
                if let Some(terms) = base.share_class(id) {
                    share_classes.insert(*id, terms.clone());
                }
            }
            AssetId::Commodity(id) => {
                if let Some(terms) = base.commodity(id) {
                    commodities.insert(*id, terms.clone());
                }
            }
        }
    }
    for (id, terms) in &pending.currencies {
        if referenced.assets.contains(&AssetId::Currency(*id)) {
            currencies.insert(*id, terms.clone());
        }
    }
    for (id, terms) in &pending.crypto_tokens {
        if referenced.assets.contains(&AssetId::CryptoToken(*id)) {
            crypto_tokens.insert(*id, terms.clone());
        }
    }
    for (id, terms) in &pending.securities {
        if referenced.assets.contains(&AssetId::Security(*id)) {
            securities.insert(*id, terms.clone());
            if let Some(share_class) = pending.share_classes.get(id) {
                share_classes.insert(*id, share_class.clone());
            }
        }
    }
    for (id, terms) in &pending.commodities {
        if referenced.assets.contains(&AssetId::Commodity(*id)) {
            commodities.insert(*id, terms.clone());
        }
    }
    AssetRegistry::from_terms(
        currencies,
        crypto_tokens,
        securities,
        share_classes,
        commodities,
    )
}

pub(crate) fn staged_registries(
    base: &CatalogGeneration,
    pending_assets: &AssetRegistryPending,
    pending_references: &ReferenceRegistryPending,
    pending_outcomes: &OutcomeRegistryPending,
    referenced: &ReferencedTerms,
) -> (
    Arc<AssetRegistry>,
    Arc<ReferenceRegistry>,
    Arc<OutcomeRegistry>,
) {
    let assets = publish_assets(base.assets(), pending_assets, referenced);
    let references = pending_references.publish(base.references(), referenced);
    let outcomes = pending_outcomes.publish(base.outcomes(), referenced);
    (Arc::new(assets), Arc::new(references), Arc::new(outcomes))
}

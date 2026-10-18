//! The catalog batch a source returns, and the one validator that accepts or
//! refuses it.
//!
//! A batch is refused as a whole: a listing whose snapshot describes another key,
//! whose instrument terms do not produce the identity it declares, whose typed
//! identifiers contradict its own symbol, whose rules do not hold for its
//! instrument, or that references an outcome group, member, side, risk pool or
//! event the batch does not carry, never replaces the previous snapshot. An
//! empty batch is not a failure: a registered route may legitimately list
//! nothing.

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
        rules::TradingRules,
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

/// One canonical instrument together with the venue listing that implements it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogEntry {
    /// The canonical instrument the listing implements.
    pub instrument: Instrument,
    /// The venue listing identity and its typed native identifiers.
    pub listing: VenueListing,
    /// The current rules, status and availability window of the listing.
    pub snapshot: ListingSnapshot,
}

/// Everything one source fetch produces for one route.
#[derive(Clone, Debug, Default)]
pub struct CatalogBatch {
    /// The listings of the fetched route.
    pub listings: Vec<CatalogEntry>,
    /// The outcome events the listings reference.
    pub outcome_events: Vec<OutcomeEvent>,
    /// The complete outcome groups the listings reference.
    pub outcome_groups: Vec<OutcomeGroup>,
    /// The complete risk pools the listings and events reference.
    pub outcome_risk_pools: Vec<OutcomeRiskPool>,
    /// The canonical terms of the assets the listings are denominated in.
    ///
    /// A canonical identity is a digest of its terms, so a listing names an
    /// `AssetId` and never the terms behind it. The display contract has to render
    /// those terms, so the batch that carries the listing carries them too, and the
    /// publication installs them in the asset registry of its generation.
    pub asset_terms: Vec<AssetTerms>,
    /// The canonical terms of the price references the listings are quoted through.
    ///
    /// A reference identity is a digest of its own terms the same way an asset
    /// identity is, and the display contract renders the reference's scope and
    /// direction from those terms, so the batch carries them beside the identity
    /// its listings name.
    pub reference_terms: Vec<PriceReferenceTerms>,
    /// The display code this batch verified for every asset whose own terms state
    /// none.
    ///
    /// The staging key is the canonical asset, so one asset states one code
    /// whatever listing of the batch names it: a contract token keeps its chain and
    /// its address on its own terms, and the ticker of a security is a listing fact.
    pub asset_codes: Vec<StagedAssetCode>,
    /// The instrument lineage edges this batch publishes.
    pub instrument_lineage: Vec<InstrumentLineageEdge>,
    /// The outcome group lineage edges this batch publishes.
    pub outcome_group_lineage: Vec<market_participants_model::outcome::OutcomeGroupLineageEdge>,
    /// The outcome risk pool lineage edges this batch publishes.
    pub outcome_risk_pool_lineage:
        Vec<market_participants_model::outcome::OutcomeRiskPoolLineageEdge>,
}

impl CatalogBatch {
    /// Validates the batch against one fetch and stages its result.
    ///
    /// Every canonical identity the batch needs must have been registered
    /// through the fetch context's staging view, or already be published in
    /// `base`; everything else is a refusal, and nothing is published.
    pub fn pending(
        context: &CatalogFetchContext,
        batch: Self,
        base: &CatalogGeneration,
    ) -> Result<CatalogPending, CatalogValidationError> {
        CatalogPending::stage(context, batch, base)
    }
}

/// A validated batch, ready to be published as a route snapshot.
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
    /// The `(PolymarketEventId) -> OutcomeEventId -> [OutcomeGroupId]` index of
    /// this batch, per the event index.
    native_event_groups: AHashMap<PolymarketEventId, Vec<OutcomeGroupId>>,
    /// The event indexes this route publishes: the batch's events plus the ones
    /// its own listings declare.
    ///
    /// This is the set the route's native event index is built from, and it is
    /// also what the generation reads to decide whether an event survives: an
    /// event stays in the published registry while one *surviving* route snapshot
    /// declares it, so one route's publication never drops another route's event
    /// (`ReferencedTerms::collect` reads `declared_events` for the same reason).
    /// An event the batch carries without any listing naming it is indexed on this
    /// route but is never published by `generation.outcomes()`.
    published_events: AHashSet<OutcomeEventId>,
    /// The outcome events of this batch.
    pub outcome_events: Vec<OutcomeEvent>,
    /// The outcome groups of this batch.
    pub outcome_groups: Vec<OutcomeGroup>,
    /// The risk pools of this batch.
    pub outcome_risk_pools: Vec<OutcomeRiskPool>,
    /// The instrument lineage of this batch.
    pub instrument_lineage: Vec<InstrumentLineageEdge>,
    /// The outcome group lineage of this batch.
    pub outcome_group_lineage: Vec<market_participants_model::outcome::OutcomeGroupLineageEdge>,
    /// The outcome risk pool lineage of this batch.
    pub outcome_risk_pool_lineage:
        Vec<market_participants_model::outcome::OutcomeRiskPoolLineageEdge>,
    /// The groups this batch replaces, which the current generation stops indexing.
    pub retired_groups: AHashSet<OutcomeGroupId>,
    /// The risk pools this batch replaces, which the current generation stops indexing.
    pub retired_pools: AHashSet<OutcomeRiskPoolId>,
    /// The display material this batch stages with its listings.
    display_material: Arc<RouteDisplayMaterial>,
    /// The identifier normalization rules of this route.
    ///
    /// The display gate of a later publication reads them from the snapshot,
    /// because a revocation or a registry-only update rebuilds the display of a
    /// surviving route from its snapshot alone and has to fold the same typed
    /// identifiers the same way.
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

        let mut native_keys: AHashMap<SmolStr, VenueListingKey> =
            AHashMap::with_capacity(entries.len() * 2);
        let mut listing_identifiers: AHashMap<ListingIdentifier, VenueListingKey> =
            AHashMap::with_capacity(entries.len() * 2);
        let mut outcome_listings: AHashMap<OutcomeGroupId, VenueListingKey> = AHashMap::new();
        let mut listing_ids: AHashSet<VenueListingId> = AHashSet::with_capacity(entries.len());
        for entry in &entries {
            if !listing_ids.insert(entry.listing.key.listing_id.clone()) {
                return Err(CatalogValidationError::DuplicateListingId {
                    route: venue_route,
                    listing_id: entry.listing.key.listing_id.clone(),
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
        // The canonical terms of the assets this batch is denominated in reach the
        // asset registry of the publication, so a listing's `AssetId` always finds
        // the terms the display contract renders.
        for terms in &batch.asset_terms {
            pending_assets.stage(terms.clone());
        }
        // A price reference identity is a digest of its own terms, so the batch
        // that names one carries the terms the display contract renders its scope
        // and direction from.
        for terms in &batch.reference_terms {
            let _ = pending_references.register_price_reference(terms.clone());
        }
        // The batch carries the complete outcome relations it references, so they
        // are staged here whether or not the source also registered them by hand.
        // An identity that neither the batch nor a published listing references
        // still never reaches the new registry.
        //
        // Groups go through the registry boundary rather than into the pending map
        // directly: `OutcomeGroup::validate` is what enforces the model's limit of
        // 128 Unicode scalar values on a question, and a group that skipped it
        // would publish a question no formatter may render.
        for group in batch.outcome_groups.clone() {
            let id = group.id.clone();
            pending_outcomes
                .register_group(group, &[])
                .map_err(|error| CatalogValidationError::OutcomeGroupIncomplete {
                    group: id,
                    error: Box::new(error.into_model()),
                })?;
        }
        for pool in &batch.outcome_risk_pools {
            pending_outcomes
                .risk_pools
                .insert(pool.id.clone(), pool.clone());
        }
        for event in &batch.outcome_events {
            pending_outcomes
                .events
                .insert(event.id.clone(), event.clone());
        }

        // A successor replaces its predecessor in the *current* generation: the
        // predecessor stays in the historical generation and in the lineage
        // graph, but it is no longer indexed, which is what keeps a group from
        // belonging to two current risk pools.
        let retired_groups: AHashSet<OutcomeGroupId> = base
            .outcome_group_lineage()
            .iter()
            .chain(batch.outcome_group_lineage.iter())
            .map(|edge| edge.predecessor.clone())
            .collect();
        let retired_pools: AHashSet<OutcomeRiskPoolId> = base
            .outcome_risk_pool_lineage()
            .iter()
            .chain(batch.outcome_risk_pool_lineage.iter())
            .map(|edge| edge.predecessor.clone())
            .collect();

        let batch_group_ids: AHashSet<OutcomeGroupId> = batch
            .outcome_groups
            .iter()
            .map(|group| group.id.clone())
            .collect();
        let batch_event_ids: AHashSet<OutcomeEventId> = batch
            .outcome_events
            .iter()
            .map(|event| event.id.clone())
            .collect();
        // One event set for the whole publication: a batch event is published for
        // the route while one of the route's own listings names it, and an event of
        // an earlier generation stays published only while a listing still names
        // it. The native identifier of an event is its own `native_event` value,
        // which is what such a listing declares, so the native lookup map and the
        // event-level index are both keyed by it. A source may additionally
        // register extra native lookups, and the base generation keeps the lookups
        // its own events were published under.
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
            .map(|lookup| (lookup.identifier(), lookup.group().clone()))
            .collect();
        let mut native_events: AHashMap<PolymarketEventId, OutcomeEventId> = AHashMap::new();
        for (native, event) in pending_outcomes.event_lookups.iter() {
            if published_event_ids.contains(event) {
                native_events.insert(native.clone(), event.clone());
            }
        }
        for (native, event) in base_outcomes.event_lookups() {
            if published_event_ids.contains(event) {
                native_events.insert(native.clone(), event.clone());
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
            native_events.insert(native.clone(), event.id.clone());
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

    /// Returns the display material this batch staged.
    pub(crate) fn display_material(&self) -> &Arc<RouteDisplayMaterial> {
        &self.display_material
    }

    /// Returns the identifier normalization rules this batch was fetched under.
    pub(crate) fn normalization(&self) -> &Arc<[IdentifierNormalizationRule]> {
        &self.normalization
    }

    /// Validates the cross-object invariants of the staged batch.
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
                    .map(|edge| edge.successor.clone())
                    .unwrap_or_else(|| held.any_group()),
            }
        })?;
        validate_outcome_risk_pool_lineage(&self.outcome_risk_pool_lineage).map_err(|_| {
            CatalogValidationError::OutcomeRiskPoolLineageInvalid {
                pool: self
                    .outcome_risk_pool_lineage
                    .first()
                    .map(|edge| edge.successor.clone())
                    .unwrap_or_else(|| held.any_pool()),
            }
        })?;
        let mut successors: AHashSet<&OutcomeRiskPoolId> =
            AHashSet::with_capacity(self.outcome_risk_pool_lineage.len());
        for edge in &self.outcome_risk_pool_lineage {
            if !held.base_pool(&edge.predecessor) || !held.batch_pool(&edge.successor) {
                return Err(CatalogValidationError::OutcomeRiskPoolLineageInvalid {
                    pool: if held.base_pool(&edge.predecessor) {
                        edge.successor.clone()
                    } else {
                        edge.predecessor.clone()
                    },
                });
            }
            if !successors.insert(&edge.successor) {
                return Err(CatalogValidationError::OutcomeRiskPoolLineageInvalid {
                    pool: edge.successor.clone(),
                });
            }
        }
        let mut group_successors: AHashSet<&OutcomeGroupId> =
            AHashSet::with_capacity(self.outcome_group_lineage.len());
        for edge in &self.outcome_group_lineage {
            if !held.has_group(&edge.successor) || !held.has_group(&edge.predecessor) {
                return Err(CatalogValidationError::OutcomeGroupLineageInvalid {
                    group: if held.has_group(&edge.successor) {
                        edge.predecessor.clone()
                    } else {
                        edge.successor.clone()
                    },
                });
            }
            if !group_successors.insert(&edge.successor) {
                return Err(CatalogValidationError::OutcomeGroupLineageInvalid {
                    group: edge.successor.clone(),
                });
            }
        }
        // A group successor must be accompanied by a successor of every risk pool
        // that references the replaced group. The pools and the group lineage of
        // the generation being replaced take part in that check, because a pool
        // published by an earlier generation keeps referencing its groups.
        validate_pool_successors(&held.group_edges, &held.pool_edges, &held.all_pools()).map_err(
            |_| CatalogValidationError::OutcomeRiskPoolLineageInvalid {
                pool: held.unsuccessored_pool().unwrap_or_else(|| held.any_pool()),
            },
        )?;
        // A retired group or pool leaves the current generation, so nothing that
        // stays current may still reference it.
        for pool in &held.current_pools {
            for group in &pool.groups {
                if held.retired_groups.contains(group) {
                    return Err(CatalogValidationError::OutcomeRiskPoolIncomplete {
                        pool: pool.id.clone(),
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
                        event: event.id.clone(),
                        group: Some(group.clone()),
                        pool: None,
                    });
                }
            }
            for pool in &event.risk_pools {
                if held.retired_pools.contains(pool) {
                    return Err(CatalogValidationError::OutcomeEventIncomplete {
                        event: event.id.clone(),
                        group: None,
                        pool: Some(pool.clone()),
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
        // The naming gate runs last: it rebuilds the whole catalogue-global decision
        // over the generation this batch would create, so a missing registry entry, a
        // wrong reference direction, an asset no code was resolved for, a staged code
        // two entries disagree on or a contradicted identifier echo refuses the batch
        // before the generation is replaced.
        self.validate_display(base)
    }

    /// Validates the price terms of every outcome listing in the batch.
    ///
    /// `CommonTradingRules.price.quote_asset` must equal the outcome
    /// group's `settlement_asset`, exactly as it must equal a spot listing's
    /// `quote` or a derivative's `price_quote`. The model's own price-terms entry
    /// (`listing::navigation::expected_price_terms`) has no answer for an outcome
    /// instrument — an outcome is not quoted per asset unit or per contract — so
    /// the grouping that answers it is the one this layer already holds.
    ///
    /// An outcome token is priced as a fraction of the collateral it settles in,
    /// which is `PriceBasis::FractionOfCollateral`: it is not an amount of an
    /// asset (`PerAssetUnit`), not an amount per contract (`PerContract`, an
    /// outcome instrument has no contract size) and not per index point (which
    /// requires an index).
    fn validate_outcome_price_terms(
        &self,
        held: &HeldIdentities,
    ) -> Result<(), CatalogValidationError> {
        for entry in self.entries.iter() {
            let InstrumentTerms::Outcome(terms) = entry.instrument.terms() else {
                continue;
            };
            // A group the batch does not carry is already refused as an
            // incomplete outcome set, so there is nothing to compare against.
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

    /// Validates the conversion references and the display decisions of the batch.
    ///
    /// The conversion direction is fixed: a conversion reference's registry terms
    /// must track the settlement asset as the underlying, state the price quote as
    /// the quote asset and be expressed per asset unit. `PerpetualTerms`,
    /// `DeliveryMode::Cash` and `OptionTerms` deliberately carry only the identity
    /// — an identity is a one-way digest — and leave this check, which needs the
    /// registry, to the publisher.
    ///
    /// The display half then runs the same catalogue-global pass the publication
    /// itself runs, over the generation this batch would create: a missing
    /// reference, an asset no code was resolved for, a staged code two entries
    /// disagree on or a name whose details do not render it back refuses the batch
    /// here with the typed display error.
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
        // The echo check runs here rather than in the naming pass: a listing
        // whose typed identifiers contradict its declared native symbols has no
        // verified name, and refusing the batch is what keeps the disagreement from
        // being published away. The comparison is the model's own low-level one;
        // the rest of the model's publication gate is already answered above, and
        // its price-terms cross-check deliberately has no answer for an outcome
        // listing, so it is not re-run here.
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
            // The typed protocol label of an outcome listing only echoes the label
            // of the registry leg it trades. A label that disagrees refuses the
            // publication, and the main name is never built from it.
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

    /// Validates that every referenced outcome object is present.
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
                    group: group.id.clone(),
                    error: Box::new(error),
                });
            }
        }
        // A listing only ever trades one leg of the group it names, and the group
        // is the authority on which legs exist. Checking the leg against the
        // *other* listings of the batch would only compare the batch with itself,
        // so the check is against the group's own members and sides.
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
                    group: terms.group().clone(),
                    member: terms.member().clone(),
                    side: terms.side().clone(),
                });
            }
        }
        for pool in &self.outcome_risk_pools {
            if let Err(error) = pool.validate() {
                return Err(CatalogValidationError::OutcomeRiskPoolIncomplete {
                    pool: pool.id.clone(),
                    error: Box::new(error),
                });
            }
            for group in &pool.groups {
                if !held.has_group(group) {
                    return Err(CatalogValidationError::OutcomeRiskPoolIncomplete {
                        pool: pool.id.clone(),
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
                            pool: pool.id.clone(),
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
                        event: event.id.clone(),
                        group: Some(group.clone()),
                        pool: None,
                    });
                }
            }
            for pool in &event.risk_pools {
                if !held.has_pool(pool) {
                    return Err(CatalogValidationError::OutcomeEventIncomplete {
                        event: event.id.clone(),
                        group: None,
                        pool: Some(pool.clone()),
                    });
                }
            }
        }
        // The event index is keyed by the native event of the listing's venue.
        // A listing that declares one must name an event this route already
        // knows, or the index would key an event the generation does not carry.
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

    /// Builds the immutable snapshot this batch publishes.
    pub(crate) fn into_snapshot(self) -> RouteSnapshot {
        RouteSnapshot::from_pending(self)
    }

    /// Returns the route this batch was fetched for.
    pub fn route(&self) -> &VenueRoute {
        &self.route
    }

    /// Returns the revision this batch was fetched under.
    pub fn revision(&self) -> RouteInputRevision {
        self.revision
    }

    /// Returns the validated entries.
    pub fn entries(&self) -> &Arc<[CatalogEntry]> {
        &self.entries
    }

    /// Returns the normalized native key index.
    pub fn native_keys(&self) -> &AHashMap<SmolStr, VenueListingKey> {
        &self.native_keys
    }

    /// Returns the typed identifier index.
    pub fn listing_identifiers(&self) -> &AHashMap<ListingIdentifier, VenueListingKey> {
        &self.listing_identifiers
    }

    /// Returns the outcome listings by canonical group.
    pub fn outcome_listings(&self) -> &AHashMap<OutcomeGroupId, VenueListingKey> {
        &self.outcome_listings
    }

    /// Returns the native group lookups this route published.
    pub fn native_groups(&self) -> &AHashMap<OutcomeGroupIdentifier, OutcomeGroupId> {
        &self.native_groups
    }

    /// Returns the native event lookups this route published.
    pub fn native_events(&self) -> &AHashMap<PolymarketEventId, OutcomeEventId> {
        &self.native_events
    }

    /// Returns the `PolymarketEventId -> [OutcomeGroupId]` index of this route.
    pub fn native_event_groups(&self) -> &AHashMap<PolymarketEventId, Vec<OutcomeGroupId>> {
        &self.native_event_groups
    }

    /// Returns the event indexes this batch publishes.
    pub fn published_events(&self) -> &AHashSet<OutcomeEventId> {
        &self.published_events
    }

    /// Returns the staged asset terms.
    pub fn pending_assets(&self) -> &AssetRegistryPending {
        &self.pending_assets
    }

    /// Returns the staged reference terms.
    pub fn pending_references(&self) -> &ReferenceRegistryPending {
        &self.pending_references
    }

    /// Returns the staged outcome relations.
    pub fn pending_outcomes(&self) -> &OutcomeRegistryPending {
        &self.pending_outcomes
    }
}

/// Everything the batch and the previous generation can resolve.
///
/// The *current* sets are the union of what the previous generation published and
/// what the batch carries, minus the identities a lineage edge retired: a group
/// or pool that got a successor stays in the lineage graph and in the historical
/// generation, but the current generation indexes only its successor.
struct HeldIdentities {
    batch_groups: AHashSet<OutcomeGroupId>,
    base_groups: AHashSet<OutcomeGroupId>,
    group_terms: AHashMap<OutcomeGroupId, OutcomeGroup>,
    batch_pools: AHashSet<OutcomeRiskPoolId>,
    base_pools: AHashSet<OutcomeRiskPoolId>,
    /// Every pool of the previous generation and the batch, retired ones included.
    pools: Vec<OutcomeRiskPool>,
    /// The pools that stay current.
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
            .map(|group| group.id.clone())
            .collect();
        let batch_pools: AHashSet<OutcomeRiskPoolId> = pending
            .outcome_risk_pools
            .iter()
            .map(|pool| pool.id.clone())
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
                .map(|group| group.id.clone())
                .or_else(|| base.outcome_groups().keys().next().cloned()),
            first_pool: pending
                .outcome_risk_pools
                .first()
                .map(|pool| pool.id.clone())
                .or_else(|| base.outcome_risk_pools().keys().next().cloned()),
            base_groups: base.outcome_groups().keys().cloned().collect(),
            group_terms: {
                let mut terms: AHashMap<OutcomeGroupId, OutcomeGroup> =
                    base.outcome_groups().clone();
                for group in &pending.outcome_groups {
                    terms.insert(group.id.clone(), group.clone());
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

    /// Returns the complete group terms of one held group.
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

    /// Returns the first member group of one pool, for a refusal that must name it.
    fn group_of_pool(&self, pool: &OutcomeRiskPoolId) -> Option<OutcomeGroupId> {
        self.pools
            .iter()
            .find(|candidate| &candidate.id == pool)
            .and_then(|candidate| candidate.groups.first().cloned())
    }

    /// Returns the first pool whose replaced member group has no pool successor.
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
            .map(|pool| pool.id.clone())
    }

    fn any_group(&self) -> OutcomeGroupId {
        self.first_group
            .clone()
            .expect("a refusal always has a batch and a generation to name")
    }

    fn any_pool(&self) -> OutcomeRiskPoolId {
        self.first_pool
            .clone()
            .expect("a refusal always has a batch and a generation to name")
    }
}

/// The role a contract requires one of its price references to state.
///
/// The two roles fix one direction each, and the two are deliberately not the same
/// rule, so the role is what a refusal names and what a check is made against:
///
/// * [`ReferenceRole::Conversion`] is the reference that turns the settlement asset
///   into the price quote: its underlying is `S` (the cash asset), its quote is `Q`
///   (the contract price quote) and its basis is per asset unit.
/// * [`ReferenceRole::Settlement`] is the observation the settlement price is read
///   from. It observes the *same economic quantity* the contract tracks — the
///   contract's own underlying `U`, priced in the contract's own quote `Q` — so the
///   two references share one `(underlying, quote, basis)` scope and differ only in
///   the observation terms behind their identities.
///
/// A role is an enum rather than a string because the two are different obligations
/// of one contract, not two spellings of one check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReferenceRole {
    /// The conversion between the settlement asset and the price quote.
    Conversion,
    /// The observation a cash settlement reads its price from.
    Settlement,
}

impl ReferenceRole {
    /// Returns the field name a refusal of this role reports.
    const fn field(self) -> &'static str {
        match self {
            Self::Conversion => "conversion reference",
            Self::Settlement => "cash settlement reference",
        }
    }
}

/// The direction one role requires of a reference's registered terms.
///
/// A conversion reference is quoted in the price quote because it converts the
/// settlement asset into it. A settlement observation is quoted in the price quote
/// for the other reason: it observes what the contract tracks, and what the contract
/// tracks is priced in the contract's own quote. The two roles therefore agree on the
/// quote and differ in the underlying, and a contract whose settlement asset is the
/// asset it tracks (`S == U`) is the case where both halves coincide.
///
/// The basis follows the underlying in both roles: an asset quantity is valued per
/// asset unit, and an index-point contract is the only shape whose size states
/// another basis.
struct ReferenceDirection {
    /// The role the reference is checked in.
    role: ReferenceRole,
    /// The asset the reference has to track as its own underlying.
    underlying: AssetId,
    /// The asset the reference has to state as its quote.
    quote: AssetId,
    /// The basis the reference has to state.
    basis: PriceBasis,
}

impl ReferenceDirection {
    /// Returns whether one registered reference states exactly this direction.
    fn matches(&self, terms: &PriceReferenceTerms) -> bool {
        terms.underlying == UnderlyingRef::Asset(self.underlying.clone())
            && terms.quote_asset == self.quote
            && terms.basis == self.basis
    }
}

/// Validates the direction of every price reference one listing names.
///
/// `PerpetualTerms`, `DeliveryMode::Cash` and `OptionTerms` deliberately carry only
/// the identity — an identity is a one-way digest — and leave this check, which needs
/// the registry, to the publisher.
fn validate_reference_roles(
    entry: &CatalogEntry,
    references: &ReferenceRegistry,
) -> Result<(), CatalogValidationError> {
    let mut checks: Vec<ReferenceCheck> = Vec::new();
    match entry.instrument.terms() {
        InstrumentTerms::Perpetual(terms) => {
            if let Some(conversion) = terms.conversion_reference() {
                checks.push(conversion_direction(
                    conversion.clone(),
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
                    conversion.clone(),
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
                    conversion.clone(),
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

/// Returns the protocol outcome label a listing failed to echo, when it failed.
///
/// The typed protocol label of an outcome listing is compared with the registry
/// label of the leg the listing trades, after the free-text pipeline both of them
/// go through, and a disagreement is a mismatch. The protocol names a leg by the
/// member it belongs to and by the side it is, so the label is accepted when it
/// echoes either of them. The protocol label is never the name: the registry label
/// is what the grammar renders.
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
        group: terms.group().clone(),
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

/// One reference together with the direction its contract requires it to state.
struct ReferenceCheck {
    reference: PriceReferenceId,
    direction: ReferenceDirection,
}

/// Returns the conversion direction of one reference: `S -> Q` per asset unit.
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

/// Returns the settlement reference check one delivery implies.
///
/// The settlement observation has the direction of the price reference: the
/// settlement price of a contract is a price of what the contract tracks, stated in
/// the contract's own quote. Its direction is therefore `underlying == U` and
/// `quote_asset == Q`, and it is deliberately *not* the conversion direction, whose
/// underlying is the settlement asset. The settlement asset is not read here at all:
/// it is the conversion reference, checked beside this one, that states it.
///
/// The basis follows the underlying, because the observation is a price of what the
/// contract tracks: an asset underlying is observed per asset unit and an index
/// underlying per index point of that same index.
fn settlement_direction(
    delivery: &DeliveryMode,
    underlying: &UnderlyingRef,
    price_quote: &AssetId,
) -> Vec<ReferenceCheck> {
    let DeliveryMode::Cash { reference, .. } = delivery else {
        return Vec::new();
    };
    // An observation is always a price of an asset: an index is not an asset, so a
    // contract whose underlying is an index or an external reference names no
    // settlement observation direction this rule can state.
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
        reference: reference.clone(),
        direction,
    }]
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
            declared: entry.listing.instrument_id.clone(),
            computed: entry.instrument.id().clone(),
        });
    }
    entry
        .listing
        .validate_for(&entry.instrument)
        .map_err(|error| match error {
            ModelError::InstrumentTermsMismatch { .. } => {
                CatalogValidationError::InstrumentTermsMismatch {
                    listing: Box::new(entry.listing.key.clone()),
                    declared: entry.listing.instrument_id.clone(),
                    computed: entry.instrument.id().clone(),
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
    // An outcome listing is priced per outcome token in its group's settlement
    // asset, so the model's price-terms cross-check — which asks for the single
    // quote asset and basis of a listing — has no answer for it and refuses the
    // instrument kind outright. The remaining rule invariants are still checked
    // here: the rules themselves, the product family and the traded quantity unit
    // against the exact group, member and side of the listed instrument.
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

/// Inserts every native lookup key of one validated entry.
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
        // One rule for one typed identifier on both sides of the index: the key
        // stored here is the one a lookup folds to, so a spelling the route folds
        // names the same listing either way. The folded value is also what the
        // native key space holds, which is where a collision between two listings
        // is caught.
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
        outcome_listings.insert(terms.group().clone(), entry.listing.key.clone());
    }
    Ok(())
}

/// Inserts one normalized native key, refusing a collision.
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

/// Resolves the native event identifier a listing declares to the event index of
/// the batch or of the published generation it names.
pub(crate) fn resolve_declared_event(
    declared: &PolymarketEventId,
    pending_events: &OutcomeRegistryPending,
    base: &OutcomeRegistry,
) -> Option<OutcomeEventId> {
    for (native, id) in pending_events.event_lookups.iter() {
        if native == declared {
            return Some(id.clone());
        }
    }
    for (native, id) in base.event_lookups() {
        if native == declared {
            return Some(id.clone());
        }
    }
    for candidate in pending_events.events.values().chain(base.events().values()) {
        if candidate.native_event.as_str() == declared.as_str() {
            return Some(candidate.id.clone());
        }
    }
    None
}

/// The event indexes a batch publishes for the route it belongs to.
///
/// A batch event is published while one of the route's own listings names it,
/// and an event of an earlier generation stays published only while a listing
/// still names it. Both cases resolve the declared native event identifier to the
/// canonical event identity the batch or the published generation already holds.
/// Whether such an event survives the whole publication is decided by the
/// reachability of every surviving route snapshot
/// ([`crate::catalog::generation::CatalogGeneration::with_pending`]), never by this batch
/// alone.
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

/// The canonical terms one batch actually references.
#[derive(Debug, Default)]
pub struct ReferencedTerms {
    /// The assets the batch's listings and outcome relations are denominated in.
    pub assets: AHashSet<AssetId>,
    /// The index references the batch's derivatives track.
    pub indices: AHashSet<IndexId>,
    /// The external references the batch's derivatives track.
    pub external_references: AHashSet<ExternalReferenceId>,
    /// The price references the batch's derivatives are quoted through.
    pub price_references: AHashSet<PriceReferenceId>,
    /// The outcome groups the batch publishes.
    pub groups: AHashSet<OutcomeGroupId>,
    /// The risk pools the batch publishes.
    pub pools: AHashSet<OutcomeRiskPoolId>,
    /// The outcome events the batch publishes.
    ///
    /// The batch's events reach the new registry while one of the batch's own
    /// listings declares them; an event a listing declares without the batch
    /// carrying it resolves to the identity the generation already publishes.
    pub events: AHashSet<OutcomeEventId>,
    /// The groups a lineage edge retired.
    pub retired_groups: AHashSet<OutcomeGroupId>,
    /// The risk pools a lineage edge retired.
    pub retired_pools: AHashSet<OutcomeRiskPoolId>,
}

impl ReferencedTerms {
    /// Collects every identity the batch's published content refers to.
    pub(crate) fn collect(pending: &CatalogPending, base: &CatalogGeneration) -> Self {
        let mut referenced = Self::default();
        for entry in pending.entries.iter() {
            collect_instrument(&entry.instrument, &mut referenced);
            collect_rules(&entry.snapshot.rules, &mut referenced.assets);
        }
        for group in &pending.outcome_groups {
            referenced.groups.insert(group.id.clone());
            referenced.assets.insert(group.settlement_asset.clone());
        }
        for event in &pending.outcome_events {
            referenced.groups.extend(event.groups.iter().cloned());
            referenced.pools.extend(event.risk_pools.iter().cloned());
        }
        // An event is published for as long as one of the route's own listings
        // names it; a batch that carries an event no listing declares publishes
        // nothing for it.
        referenced.events =
            declared_events(&pending.entries, &pending.pending_outcomes, base.outcomes());
        for pool in &pending.outcome_risk_pools {
            referenced.pools.insert(pool.id.clone());
            referenced.assets.insert(pool.settlement_asset.clone());
        }
        for edge in &pending.outcome_group_lineage {
            referenced.groups.insert(edge.predecessor.clone());
            referenced.groups.insert(edge.successor.clone());
        }
        for edge in &pending.outcome_risk_pool_lineage {
            referenced.pools.insert(edge.predecessor.clone());
            referenced.pools.insert(edge.successor.clone());
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
            // The conversion reference states the direction `S -> Q`, so it is a
            // reference the publication must resolve. Leaving it out of the
            // referenced set would drop its registry entry and publish an
            // instrument whose conversion reference names an identity no registry
            // carries.
            if let Some(conversion) = terms.conversion_reference() {
                referenced.price_references.insert(conversion.clone());
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
                referenced.price_references.insert(conversion.clone());
            }
        }
        InstrumentTerms::Outcome(_) => {}
    }
}

/// Collects what a derivative tracks: an asset, an index or an external
/// reference, plus the price reference it is quoted through.
pub(crate) fn collect_derivative(
    underlying: &market_participants_model::asset::DerivativeUnderlying,
    referenced: &mut ReferencedTerms,
) {
    match underlying.reference() {
        UnderlyingRef::Asset(asset) => {
            referenced.assets.insert(asset.clone());
        }
        UnderlyingRef::Index(index) => {
            referenced.indices.insert(index.clone());
        }
        UnderlyingRef::External(external) => {
            referenced.external_references.insert(external.clone());
        }
    }
    referenced
        .price_references
        .insert(underlying.price_reference().clone());
}

fn collect_contract_size(size: &ContractSize, assets: &mut AHashSet<AssetId>) {
    if let Some(asset) = size.denomination() {
        assets.insert(asset.clone());
    }
    if let Some(asset) = size.quote_asset() {
        assets.insert(asset.clone());
    }
}

/// Collects every reference a delivery names, whichever role it plays.
///
/// A cash delivery names two references and both are the publication's own
/// obligation: the settlement observation is what the settlement price is read from,
/// and the conversion is what turns the settlement asset into the price quote. The
/// registry keeps only the references the batch's listings track, so a delivery that
/// names a reference the set omits publishes an instrument whose reference names an
/// identity no registry carries — for either role.
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
        referenced.price_references.insert(reference.clone());
    }
    if let Some(conversion) = delivery.cash_conversion_reference() {
        referenced.price_references.insert(conversion.clone());
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

/// Publishes the asset registry of one batch.
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
                    currencies.insert(id.clone(), terms.clone());
                }
            }
            AssetId::CryptoToken(id) => {
                if let Some(terms) = base.crypto_token(id) {
                    crypto_tokens.insert(id.clone(), terms.clone());
                }
            }
            AssetId::Security(id) => {
                if let Some(terms) = base.security(id) {
                    securities.insert(id.clone(), terms.clone());
                }
                if let Some(terms) = base.share_class(id) {
                    share_classes.insert(id.clone(), terms.clone());
                }
            }
            AssetId::Commodity(id) => {
                if let Some(terms) = base.commodity(id) {
                    commodities.insert(id.clone(), terms.clone());
                }
            }
        }
    }
    for (id, terms) in &pending.currencies {
        if referenced.assets.contains(&AssetId::Currency(id.clone())) {
            currencies.insert(id.clone(), terms.clone());
        }
    }
    for (id, terms) in &pending.crypto_tokens {
        if referenced
            .assets
            .contains(&AssetId::CryptoToken(id.clone()))
        {
            crypto_tokens.insert(id.clone(), terms.clone());
        }
    }
    for (id, terms) in &pending.securities {
        if referenced.assets.contains(&AssetId::Security(id.clone())) {
            securities.insert(id.clone(), terms.clone());
            if let Some(share_class) = pending.share_classes.get(id) {
                share_classes.insert(id.clone(), share_class.clone());
            }
        }
    }
    for (id, terms) in &pending.commodities {
        if referenced.assets.contains(&AssetId::Commodity(id.clone())) {
            commodities.insert(id.clone(), terms.clone());
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

/// The staged registries of one batch, built from the base and the batch terms.
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

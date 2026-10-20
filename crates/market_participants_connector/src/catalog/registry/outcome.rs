//! The single creation boundary of canonical outcome relation identities.
//!
//! An adapter hands this registry verified group terms — question, expiry,
//! topology, settlement asset, resolution rule, fraction rule and the canonical
//! member/side semantic topology — and the native condition, market, question
//! and token identifiers that only ever build a native lookup. The registry
//! normalizes the member, side and conversion order first and derives the
//! identities afterwards, so the same settlement semantics always produce the
//! same identity and a renamed question produces a new one.

use ahash::{AHashMap, AHashSet};
use market_participants_model::{
    identifier::{CtfConditionId, HyperliquidQuestionId, PolymarketEventId, PolymarketMarketId},
    listing::native::OutcomeGroupIdentifier,
    outcome::{
        OutcomeEvent, OutcomeEventId, OutcomeGroup, OutcomeGroupId, OutcomeGroupTerms,
        OutcomeRiskPool, OutcomeRiskPoolId, OutcomeRiskPoolTerms,
    },
};

use crate::catalog::error::RegistryError;

/// One native lookup key of a canonical outcome group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupLookup {
    /// A Polymarket CTF condition.
    Condition {
        /// The condition identity.
        condition: CtfConditionId,
        /// The group it names.
        group: OutcomeGroupId,
    },
    /// A Polymarket market.
    Market {
        /// The market identity.
        market: PolymarketMarketId,
        /// The group it names.
        group: OutcomeGroupId,
    },
    /// A Hyperliquid HIP-4 question.
    Question {
        /// The route scope the question belongs to.
        scope: market_participants_model::identifier::VenueScopeId,
        /// The question identity.
        question: HyperliquidQuestionId,
        /// The group it names.
        group: OutcomeGroupId,
    },
}

impl GroupLookup {
    /// Returns the native identifier this lookup was registered under.
    pub fn identifier(&self) -> OutcomeGroupIdentifier {
        match self {
            Self::Condition { condition, .. } => {
                OutcomeGroupIdentifier::PolymarketCondition(condition.clone())
            }
            Self::Market { market, .. } => OutcomeGroupIdentifier::PolymarketMarket(market.clone()),
            Self::Question {
                scope, question, ..
            } => OutcomeGroupIdentifier::HyperliquidQuestion {
                scope: scope.clone(),
                question: question.clone(),
            },
        }
    }

    /// Returns the group this lookup names.
    pub fn group(&self) -> &OutcomeGroupId {
        match self {
            Self::Condition { group, .. }
            | Self::Market { group, .. }
            | Self::Question { group, .. } => group,
        }
    }
}

/// The outcome relations of one catalog generation.
#[derive(Clone, Debug, Default)]
pub struct OutcomeRegistry {
    groups: AHashMap<OutcomeGroupId, OutcomeGroup>,
    risk_pools: AHashMap<OutcomeRiskPoolId, OutcomeRiskPool>,
    events: AHashMap<OutcomeEventId, OutcomeEvent>,
    conditions: AHashMap<CtfConditionId, OutcomeGroupId>,
    markets: AHashMap<PolymarketMarketId, OutcomeGroupId>,
    questions: AHashMap<HyperliquidQuestionId, OutcomeGroupId>,
    native_events: AHashMap<PolymarketEventId, OutcomeEventId>,
}

impl OutcomeRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a registry that holds exactly the given relations.
    ///
    /// The native lookup maps are the base lookups plus the staged ones, each
    /// kept only while the group it names survives, so a published registry
    /// never keeps a lookup key of a dropped group.
    pub fn from_relations(
        base: &OutcomeRegistry,
        groups: AHashMap<OutcomeGroupId, OutcomeGroup>,
        risk_pools: AHashMap<OutcomeRiskPoolId, OutcomeRiskPool>,
        events: AHashMap<OutcomeEventId, OutcomeEvent>,
        lookups: &[GroupLookup],
        event_lookups: &[(PolymarketEventId, OutcomeEventId)],
    ) -> Self {
        let mut registry = Self {
            groups,
            risk_pools,
            events,
            ..Self::default()
        };
        for (condition, group) in &base.conditions {
            if registry.groups.contains_key(group) {
                registry.conditions.insert(condition.clone(), *group);
            }
        }
        for (market, group) in &base.markets {
            if registry.groups.contains_key(group) {
                registry.markets.insert(market.clone(), *group);
            }
        }
        for (question, group) in &base.questions {
            if registry.groups.contains_key(group) {
                registry.questions.insert(question.clone(), *group);
            }
        }
        for lookup in lookups {
            if !registry.groups.contains_key(lookup.group()) {
                continue;
            }
            match lookup {
                GroupLookup::Condition { condition, group } => {
                    registry.conditions.insert(condition.clone(), *group);
                }
                GroupLookup::Market { market, group } => {
                    registry.markets.insert(market.clone(), *group);
                }
                GroupLookup::Question {
                    question, group, ..
                } => {
                    registry.questions.insert(question.clone(), *group);
                }
            }
        }
        for (native, event) in &base.native_events {
            if registry.events.contains_key(event) {
                registry.native_events.insert(native.clone(), *event);
            }
        }
        for (native, event) in event_lookups {
            if registry.events.contains_key(event) {
                registry.native_events.insert(native.clone(), *event);
            }
        }
        registry
    }

    /// Returns the registered group.
    pub fn group(&self, id: &OutcomeGroupId) -> Option<&OutcomeGroup> {
        self.groups.get(id)
    }

    /// Returns the registered risk pool.
    pub fn risk_pool(&self, id: &OutcomeRiskPoolId) -> Option<&OutcomeRiskPool> {
        self.risk_pools.get(id)
    }

    /// Returns the registered event.
    pub fn event(&self, id: &OutcomeEventId) -> Option<&OutcomeEvent> {
        self.events.get(id)
    }

    /// Returns the group a native group identifier names.
    pub fn native_group(&self, identifier: &OutcomeGroupIdentifier) -> Option<&OutcomeGroupId> {
        match identifier {
            OutcomeGroupIdentifier::HyperliquidQuestion { question, .. } => {
                self.questions.get(question)
            }
            OutcomeGroupIdentifier::PolymarketMarket(market) => self.markets.get(market),
            OutcomeGroupIdentifier::PolymarketCondition(condition) => {
                self.conditions.get(condition)
            }
        }
    }

    /// Returns the event a native event identifier names.
    pub fn native_event(&self, event: &PolymarketEventId) -> Option<&OutcomeEventId> {
        self.native_events.get(event)
    }

    /// Returns every registered group.
    pub fn groups(&self) -> &AHashMap<OutcomeGroupId, OutcomeGroup> {
        &self.groups
    }

    /// Returns every registered risk pool.
    pub fn risk_pools(&self) -> &AHashMap<OutcomeRiskPoolId, OutcomeRiskPool> {
        &self.risk_pools
    }

    /// Returns every registered event.
    pub fn events(&self) -> &AHashMap<OutcomeEventId, OutcomeEvent> {
        &self.events
    }

    /// Returns every registered native event lookup.
    pub fn event_lookups(&self) -> impl Iterator<Item = (&PolymarketEventId, &OutcomeEventId)> {
        self.native_events.iter()
    }

    /// Returns the number of registered groups.
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    /// Returns `true` when no outcome relation is registered.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.risk_pools.is_empty() && self.events.is_empty()
    }

    /// Derives the canonical identity of a group from its already fixed terms.
    pub fn group_id(terms: &OutcomeGroupTerms) -> OutcomeGroupId {
        terms.group_id()
    }

    /// Derives the canonical identity of a risk pool from its normalized terms.
    pub fn risk_pool_id(terms: &OutcomeRiskPoolTerms) -> OutcomeRiskPoolId {
        terms.risk_pool_id()
    }

    /// Keeps only the events the surviving routes still reach.
    ///
    /// The set comes from the reachability of every surviving route snapshot, so
    /// one route's publication never drops another route's event: an event leaves
    /// the current generation with the last surviving listing that declares it.
    /// The native event lookups are kept from the same set, so a route never
    /// resolves an event this registry no longer carries.
    pub fn retain_events(&mut self, live: &AHashSet<OutcomeEventId>) {
        self.events.retain(|id, _| live.contains(id));
        self.native_events.retain(|_, event| live.contains(event));
    }
}

/// The outcome terms staged by one in-flight catalog batch.
#[derive(Clone, Debug, Default)]
pub struct OutcomeRegistryPending {
    /// The staged groups.
    pub groups: AHashMap<OutcomeGroupId, OutcomeGroup>,
    /// The staged risk pools.
    pub risk_pools: AHashMap<OutcomeRiskPoolId, OutcomeRiskPool>,
    /// The staged events.
    pub events: AHashMap<OutcomeEventId, OutcomeEvent>,
    /// The staged native group lookups.
    pub lookups: Vec<GroupLookup>,
    /// The staged native event lookups.
    pub event_lookups: Vec<(PolymarketEventId, OutcomeEventId)>,
}

impl OutcomeRegistryPending {
    /// Creates an empty pending view.
    pub fn new() -> Self {
        Self::default()
    }

    /// Validates and stages a group, together with its native lookup keys.
    ///
    /// The identity is derived from the group's own canonical terms, so a
    /// question or member label change produces a new identity instead of
    /// overwriting the existing one.
    pub fn register_group(
        &mut self,
        group: OutcomeGroup,
        lookups: &[OutcomeGroupIdentifier],
    ) -> Result<OutcomeGroupId, RegistryError> {
        group.validate().map_err(RegistryError::from)?;
        let id = group.id;
        for lookup in lookups {
            self.lookups.push(match lookup {
                OutcomeGroupIdentifier::HyperliquidQuestion { scope, question } => {
                    GroupLookup::Question {
                        scope: scope.clone(),
                        question: question.clone(),
                        group: id,
                    }
                }
                OutcomeGroupIdentifier::PolymarketMarket(market) => GroupLookup::Market {
                    market: market.clone(),
                    group: id,
                },
                OutcomeGroupIdentifier::PolymarketCondition(condition) => GroupLookup::Condition {
                    condition: condition.clone(),
                    group: id,
                },
            });
        }
        self.groups.insert(id, group);
        Ok(id)
    }

    /// Stages an event index and its native event lookup.
    pub fn register_event(
        &mut self,
        event: OutcomeEvent,
        native_event: Option<&PolymarketEventId>,
    ) -> OutcomeEventId {
        let id = event.id;
        if let Some(native) = native_event {
            self.event_lookups.push((native.clone(), id));
        }
        self.events.insert(id, event);
        id
    }

    /// Returns `true` when nothing was staged.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
            && self.risk_pools.is_empty()
            && self.events.is_empty()
            && self.lookups.is_empty()
            && self.event_lookups.is_empty()
    }

    /// Publishes this pending view on top of the generation's registry.
    ///
    /// Only the groups and pools the batch actually references reach the new
    /// registry, so a staged identity that no published listing or event uses is
    /// dropped instead of becoming an orphan. An event is staged while one of the
    /// batch's own listings declares it; whether it survives the whole publication
    /// is decided by the reachability of every surviving route snapshot, which the
    /// generation applies after this merge.
    pub fn publish(
        &self,
        base: &OutcomeRegistry,
        referenced: &crate::catalog::batch::ReferencedTerms,
    ) -> OutcomeRegistry {
        let mut groups: AHashMap<OutcomeGroupId, OutcomeGroup> = base.groups().clone();
        for (id, group) in &self.groups {
            if referenced.groups.contains(id) {
                groups.insert(*id, group.clone());
            }
        }
        let mut pools: AHashMap<OutcomeRiskPoolId, OutcomeRiskPool> = base.risk_pools().clone();
        for (id, pool) in &self.risk_pools {
            if referenced.pools.contains(id) {
                pools.insert(*id, pool.clone());
            }
        }
        let mut events: AHashMap<OutcomeEventId, OutcomeEvent> = base.events().clone();
        for (id, event) in &self.events {
            if referenced.events.contains(id) {
                events.insert(*id, event.clone());
            }
        }
        // A successor replaces its predecessor in the current generation: the
        // predecessor stays in the lineage graph and in the generation that
        // published it, but it is no longer indexed here.
        for retired in &referenced.retired_groups {
            groups.remove(retired);
        }
        for retired in &referenced.retired_pools {
            pools.remove(retired);
        }
        let lookups: Vec<GroupLookup> = self
            .lookups
            .iter()
            .filter(|lookup| groups.contains_key(lookup.group()))
            .cloned()
            .collect();
        OutcomeRegistry::from_relations(base, groups, pools, events, &lookups, &self.event_lookups)
    }
}

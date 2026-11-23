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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupLookup {
    Condition {
        condition: CtfConditionId,
        group: OutcomeGroupId,
    },
    Market {
        market: PolymarketMarketId,
        group: OutcomeGroupId,
    },
    Question {
        scope: market_participants_model::identifier::VenueScopeId,
        question: HyperliquidQuestionId,
        group: OutcomeGroupId,
    },
}

impl GroupLookup {
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

    pub fn group(&self) -> &OutcomeGroupId {
        match self {
            Self::Condition { group, .. }
            | Self::Market { group, .. }
            | Self::Question { group, .. } => group,
        }
    }
}

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
    pub fn new() -> Self {
        Self::default()
    }

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

    pub fn group(&self, id: &OutcomeGroupId) -> Option<&OutcomeGroup> {
        self.groups.get(id)
    }

    pub fn risk_pool(&self, id: &OutcomeRiskPoolId) -> Option<&OutcomeRiskPool> {
        self.risk_pools.get(id)
    }

    pub fn event(&self, id: &OutcomeEventId) -> Option<&OutcomeEvent> {
        self.events.get(id)
    }

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

    pub fn native_event(&self, event: &PolymarketEventId) -> Option<&OutcomeEventId> {
        self.native_events.get(event)
    }

    pub fn groups(&self) -> &AHashMap<OutcomeGroupId, OutcomeGroup> {
        &self.groups
    }

    pub fn risk_pools(&self) -> &AHashMap<OutcomeRiskPoolId, OutcomeRiskPool> {
        &self.risk_pools
    }

    pub fn events(&self) -> &AHashMap<OutcomeEventId, OutcomeEvent> {
        &self.events
    }

    pub fn event_lookups(&self) -> impl Iterator<Item = (&PolymarketEventId, &OutcomeEventId)> {
        self.native_events.iter()
    }

    pub fn len(&self) -> usize {
        self.groups.len()
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.risk_pools.is_empty() && self.events.is_empty()
    }

    pub fn group_id(terms: &OutcomeGroupTerms) -> OutcomeGroupId {
        terms.group_id()
    }

    pub fn risk_pool_id(terms: &OutcomeRiskPoolTerms) -> OutcomeRiskPoolId {
        terms.risk_pool_id()
    }

    pub fn retain_events(&mut self, live: &AHashSet<OutcomeEventId>) {
        self.events.retain(|id, _| live.contains(id));
        self.native_events.retain(|_, event| live.contains(event));
    }
}

#[derive(Clone, Debug, Default)]
pub struct OutcomeRegistryPending {
    pub groups: AHashMap<OutcomeGroupId, OutcomeGroup>,
    pub risk_pools: AHashMap<OutcomeRiskPoolId, OutcomeRiskPool>,
    pub events: AHashMap<OutcomeEventId, OutcomeEvent>,
    pub lookups: Vec<GroupLookup>,
    pub event_lookups: Vec<(PolymarketEventId, OutcomeEventId)>,
}

impl OutcomeRegistryPending {
    pub fn new() -> Self {
        Self::default()
    }

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

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
            && self.risk_pools.is_empty()
            && self.events.is_empty()
            && self.lookups.is_empty()
            && self.event_lookups.is_empty()
    }

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

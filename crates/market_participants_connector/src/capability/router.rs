use std::sync::Arc;

use ahash::AHashMap;
use market_participants_model::{
    identifier::VenueId, listing::route::VenueRoute, routing::CapabilityId,
};

use crate::{
    adapter::MarketParticipantAdapter, error::MarketParticipantError, venue::loaded::LoadedVenues,
};

pub(crate) fn admits(venues: &LoadedVenues, route: &VenueRoute, capability: CapabilityId) -> bool {
    venues.scope(route).is_some_and(|entry| {
        entry
            .capabilities
            .iter()
            .any(|support| support.capability == capability)
    })
}

pub(crate) fn admit(
    venues: &LoadedVenues,
    route: &VenueRoute,
    capability: CapabilityId,
) -> Result<(), MarketParticipantError> {
    if admits(venues, route, capability) {
        return Ok(());
    }
    match venues.scope(route) {
        Some(_) => Err(MarketParticipantError::UnsupportedCapability {
            route: route.clone(),
            capability,
        }),
        None => Err(MarketParticipantError::VenueNotLoaded {
            venue: route.venue,
            scope: route.scope.clone(),
        }),
    }
}

#[derive(Clone)]
pub struct CapabilityRouter {
    adapters: Arc<AHashMap<VenueId, Arc<dyn MarketParticipantAdapter>>>,
}

impl CapabilityRouter {
    pub fn new(adapters: impl IntoIterator<Item = Arc<dyn MarketParticipantAdapter>>) -> Self {
        let mut registry = AHashMap::with_capacity(3);
        for adapter in adapters {
            registry.insert(*adapter.venue(), adapter);
        }
        Self {
            adapters: Arc::new(registry),
        }
    }

    pub fn resolve(
        &self,
        route: &VenueRoute,
        capability: CapabilityId,
    ) -> Result<Arc<dyn MarketParticipantAdapter>, MarketParticipantError> {
        self.adapter(&route.venue)
            .ok_or_else(|| MarketParticipantError::UnsupportedCapability {
                route: route.clone(),
                capability,
            })
    }

    pub fn adapter(&self, venue: &VenueId) -> Option<Arc<dyn MarketParticipantAdapter>> {
        self.adapters.get(venue).cloned()
    }

    pub fn limits(
        &self,
        venues: &LoadedVenues,
        route: &VenueRoute,
        capability: CapabilityId,
    ) -> Result<crate::adapter::AdapterLimits, MarketParticipantError> {
        admit(venues, route, capability)?;
        self.resolve(route, capability)
            .map(|adapter| adapter.limits(route))
    }
}

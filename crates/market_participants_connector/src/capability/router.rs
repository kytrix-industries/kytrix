//! The adapter registry, keyed by venue.
//!
//! An adapter is registered once per venue and implements that venue's wire. What a
//! scope serves is not this registry's answer — it is the venue loader's, published
//! as the scope's own entry — so this type never invents a route and never falls
//! back from a HIP-3 or HIP-4 scope to a core one.

use std::sync::Arc;

use ahash::AHashMap;
use market_participants_model::{
    identifier::VenueId, listing::route::VenueRoute, routing::CapabilityId,
};

use crate::{
    adapter::MarketParticipantAdapter, error::MarketParticipantError, venue::loaded::LoadedVenues,
};

/// Whether the one routing view admits one capability of one route.
///
/// A scope is askable exactly when its venue is loaded and the entry its last
/// commit published declares the capability: there is no second view to read and
/// no intermediate state between "this scope exists" and "this scope is usable".
pub(crate) fn admits(venues: &LoadedVenues, route: &VenueRoute, capability: CapabilityId) -> bool {
    venues.scope(route).is_some_and(|entry| {
        entry
            .capabilities
            .iter()
            .any(|support| support.capability == capability)
    })
}

/// Refuses one `(route, capability)` the one routing view does not admit.
///
/// "Not loaded" and "not supported" are two different answers: a scope no
/// commit published is a market the user has not loaded, and it is refused as
/// exactly that, while a scope a commit did publish without the capability is a
/// request this build does not serve. The refusal happens before the request is
/// encoded and before any listing is resolved, so an unloaded scope costs no
/// request at all.
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

/// The adapters of a runtime, keyed by their static venue identity.
#[derive(Clone)]
pub struct CapabilityRouter {
    adapters: Arc<AHashMap<VenueId, Arc<dyn MarketParticipantAdapter>>>,
}

impl CapabilityRouter {
    /// Registers one adapter per venue.
    ///
    /// The adapter's own `venue()` is only its static attribution: whether a scope
    /// is served at all is [`LoadedVenues`]'s answer, and this registry is only
    /// asked which adapter serves it.
    pub fn new(adapters: impl IntoIterator<Item = Arc<dyn MarketParticipantAdapter>>) -> Self {
        let mut registry = AHashMap::with_capacity(3);
        for adapter in adapters {
            registry.insert(*adapter.venue(), adapter);
        }
        Self {
            adapters: Arc::new(registry),
        }
    }

    /// Returns the adapter of one route a caller admitted.
    ///
    /// Admission — whether the scope is loaded and declares the capability — happens
    /// before this call, so a route no venue publishes never reaches here. Which
    /// capabilities a scope serves is the venue's loader's answer, published as the
    /// scope's own entry; the adapter implements the wire for the scopes that exist,
    /// so there is no second support matrix here that could disagree with it, and a
    /// venue without an adapter fails closed.
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

    /// Returns the adapter of one venue.
    ///
    /// This is the one lookup that names no scope: the discovery conversation a load
    /// opens addresses the venue itself, before any of its scopes is registered.
    pub fn adapter(&self, venue: &VenueId) -> Option<Arc<dyn MarketParticipantAdapter>> {
        self.adapters.get(venue).cloned()
    }

    /// Returns the venue-specific bounds of one loaded route.
    ///
    /// The bound is the adapter's own answer for the scope, so a HIP-3 DEX or a
    /// HIP-4 outcome venue answers for itself instead of inheriting the core
    /// venue's numbers. A scope that is not loaded is not askable at all, and it is
    /// refused as **that** rather than as an unsupported capability: a market nobody
    /// loaded and a market this build does not serve are the two answers, and a
    /// caller acts on them differently — the first waits for a load, the second gives
    /// up on the scope.
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

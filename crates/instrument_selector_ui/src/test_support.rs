//! The loaded-scope fixtures the crate's test modules share.
//!
//! A selector and the control that opens it are both handed a snapshot of what the
//! loaded venues answer, and two modules of this crate ask for one: the selector,
//! which draws the loaded venues as its column, and the button, which reads how many
//! scopes a venue has loaded to state the route a listing is on. The fixtures
//! therefore live here rather than being rebuilt beside each of them.

use std::sync::Arc;

use market_participants_model::{listing::route::VenueRoute, routing::CapabilityId};

use crate::instrument_selector::LoadedScope;

/// One loaded scope of a fixture that publishes a catalog and carries `capabilities`.
pub(crate) fn scope(route: VenueRoute, capabilities: &[CapabilityId]) -> LoadedScope {
    let mut carried = Vec::with_capacity(capabilities.len() + 1);
    carried.push(CapabilityId::MarketCatalog);
    carried.extend_from_slice(capabilities);

    LoadedScope {
        route,
        capabilities: carried,
        provides_catalog: true,
    }
}

/// A caller's snapshot of the loaded scopes.
pub(crate) fn loaded(routes: Vec<LoadedScope>) -> Arc<[LoadedScope]> {
    routes.into()
}

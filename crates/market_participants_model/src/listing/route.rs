//! The venue namespace and the unique key of one listing inside it.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    identifier::{VenueId, VenueScopeId},
    listing::venue::VenueListingId,
};

/// A venue namespace: the venue identity plus its API/account scope.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct VenueRoute {
    #[serde(rename = "venue")]
    pub venue: VenueId,
    #[serde(rename = "scope")]
    pub scope: VenueScopeId,
}

impl VenueRoute {
    /// Builds a route from a venue and a scope.
    pub fn new(venue: VenueId, scope: VenueScopeId) -> Self {
        Self { venue, scope }
    }
}

impl fmt::Display for VenueRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.venue, self.scope)
    }
}

/// The unique key of one tradable listing inside one route.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct VenueListingKey {
    #[serde(rename = "route")]
    pub route: VenueRoute,
    #[serde(rename = "listing_id")]
    pub listing_id: VenueListingId,
}

impl VenueListingKey {
    /// Builds a listing key.
    pub fn new(route: VenueRoute, listing_id: VenueListingId) -> Self {
        Self { route, listing_id }
    }
}

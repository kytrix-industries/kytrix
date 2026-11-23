use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    identifier::{VenueId, VenueScopeId},
    listing::venue::VenueListingId,
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct VenueRoute {
    #[serde(rename = "venue")]
    pub venue: VenueId,
    #[serde(rename = "scope")]
    pub scope: VenueScopeId,
}

impl VenueRoute {
    pub fn new(venue: VenueId, scope: VenueScopeId) -> Self {
        Self { venue, scope }
    }
}

impl fmt::Display for VenueRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.venue, self.scope)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct VenueListingKey {
    #[serde(rename = "route")]
    pub route: VenueRoute,
    #[serde(rename = "listing_id")]
    pub listing_id: VenueListingId,
}

impl VenueListingKey {
    pub fn new(route: VenueRoute, listing_id: VenueListingId) -> Self {
        Self { route, listing_id }
    }
}

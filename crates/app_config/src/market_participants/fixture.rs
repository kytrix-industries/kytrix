use std::collections::BTreeMap;

use market_participants_model::identifier::VenueId;
use url::Url;

use crate::market_participants::settings::{FixturePair, VenueSettings, default_venues};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixtureAddress {
    pub http: Url,
    pub websocket: Url,
}

impl FixtureAddress {
    pub fn at(address: &Url) -> Self {
        Self {
            http: address.clone(),
            websocket: address.clone(),
        }
    }

    pub fn bindings(&self) -> FixturePair {
        FixturePair {
            http: self.http.clone(),
            websocket: self.websocket.clone(),
        }
    }
}

#[must_use = "the configuration carries the fixture addresses, so a root built from it dials them"]
pub fn venues(pointed: impl IntoIterator<Item = (VenueId, FixtureAddress)>) -> FixtureInstallation {
    let mut venues = default_venues();
    for (venue, address) in pointed {
        let Some(settings) = venues.get_mut(&venue) else {
            continue;
        };
        settings.repoint(&address);
    }
    FixtureInstallation { venues }
}

#[derive(Clone, Debug)]
pub struct FixtureInstallation {
    venues: BTreeMap<VenueId, VenueSettings>,
}

impl FixtureInstallation {
    pub fn venues(&self) -> &BTreeMap<VenueId, VenueSettings> {
        &self.venues
    }

    pub fn into_venues(self) -> BTreeMap<VenueId, VenueSettings> {
        self.venues
    }

    pub fn repoint(&mut self, pointed: impl IntoIterator<Item = (VenueId, FixtureAddress)>) {
        for (venue, address) in pointed {
            let Some(settings) = self.venues.get_mut(&venue) else {
                continue;
            };
            settings.repoint(&address);
        }
    }
}

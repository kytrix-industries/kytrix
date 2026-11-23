use std::sync::Arc;

use market_participants_connector::{
    catalog::store::Catalog, connector::MarketParticipantsConnector,
    venue::definition::venues_supporting,
};
use market_participants_model::{
    identifier::VenueId, listing::route::VenueRoute, routing::CapabilityId,
};

pub(super) fn event_venues() -> Vec<VenueId> {
    venues_supporting(CapabilityId::PredictionEvents)
}

pub(super) fn book_venues() -> Vec<VenueId> {
    venues_supporting(CapabilityId::PredictionOrderBook)
}

pub(super) trait MarketDirectory {
    fn any_event_market_loaded(&self) -> bool;

    fn is_loaded(&self, route: &VenueRoute) -> bool;

    fn catalog(&self) -> Option<Arc<Catalog>>;
}

pub(super) struct ConnectorDirectory(MarketParticipantsConnector);

impl ConnectorDirectory {
    pub(super) const fn new(connector: MarketParticipantsConnector) -> Self {
        Self(connector)
    }
}

impl MarketDirectory for ConnectorDirectory {
    fn any_event_market_loaded(&self) -> bool {
        let listed = event_venues();
        self.0.venues().entries().iter().any(|(route, entry)| {
            listed.contains(&route.venue)
                && entry
                    .capabilities
                    .iter()
                    .any(|support| support.capability == CapabilityId::PredictionEvents)
        })
    }

    fn is_loaded(&self, route: &VenueRoute) -> bool {
        self.0.venues().scope(route).is_some()
    }

    fn catalog(&self) -> Option<Arc<Catalog>> {
        Some(self.0.catalog_arc())
    }
}

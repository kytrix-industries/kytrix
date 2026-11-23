use std::sync::Arc;

use instrument_selector_ui::instrument_selector::LoadedScope;
use market_participants_connector::{
    catalog::{display::catalog::DisplaySnapshot, error::CatalogError, store::Catalog},
    connector::MarketParticipantsConnector,
    venue::definition::venues_supporting,
};
use market_participants_model::{
    identifier::VenueId,
    listing::route::{VenueListingKey, VenueRoute},
    routing::CapabilityId,
};

pub(super) fn market_venues() -> Vec<VenueId> {
    venues_supporting(CapabilityId::Candlesticks)
}

pub(super) enum ListingAnswer {
    Listed,
    Terminal,
    Missing,
    Unserved,
    Unknown(CatalogError),
}

pub(super) trait MarketDirectory {
    fn loaded_scopes(&self) -> Arc<[LoadedScope]>;

    fn any_market_loaded(&self) -> bool;

    fn is_loaded(&self, route: &VenueRoute) -> bool;

    fn offers_candlesticks(&self, route: &VenueRoute) -> bool;

    fn listing(&self, listing: &VenueListingKey) -> ListingAnswer;

    fn display(&self) -> DisplaySnapshot;
}

pub(super) struct ConnectorDirectory(MarketParticipantsConnector);

impl ConnectorDirectory {
    pub(super) const fn new(connector: MarketParticipantsConnector) -> Self {
        Self(connector)
    }
}

impl MarketDirectory for ConnectorDirectory {
    fn loaded_scopes(&self) -> Arc<[LoadedScope]> {
        LoadedScope::all(self.0.venues())
    }

    fn any_market_loaded(&self) -> bool {
        market_venues()
            .iter()
            .any(|venue| self.0.venues().loaded(venue).is_some())
    }

    fn is_loaded(&self, route: &VenueRoute) -> bool {
        self.0.venues().scope(route).is_some()
    }

    fn offers_candlesticks(&self, route: &VenueRoute) -> bool {
        self.0.venues().scope(route).is_some_and(|scope| {
            scope
                .capabilities
                .iter()
                .any(|support| support.capability == CapabilityId::Candlesticks)
        })
    }

    fn listing(&self, listing: &VenueListingKey) -> ListingAnswer {
        listing_answer(self.0.catalog(), listing)
    }

    fn display(&self) -> DisplaySnapshot {
        self.0.catalog().cached_display()
    }
}

pub(super) fn listing_answer(catalog: &Catalog, listing: &VenueListingKey) -> ListingAnswer {
    match catalog.contains(listing) {
        Ok(false) => ListingAnswer::Missing,
        Err(error) if proves_absent(&error) => ListingAnswer::Unserved,
        Err(error) => ListingAnswer::Unknown(error),
        Ok(true) => match catalog.cached_listing(listing) {
            Some(entry) if entry.snapshot.status.is_terminal() => ListingAnswer::Terminal,
            _ => ListingAnswer::Listed,
        },
    }
}

pub(super) fn proves_absent(error: &CatalogError) -> bool {
    matches!(
        error,
        CatalogError::RouteNotRegistered { .. }
            | CatalogError::ListingNotFound { .. }
            | CatalogError::ListingRouteMismatch { .. }
    )
}

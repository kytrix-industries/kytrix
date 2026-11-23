use std::sync::Arc;

use instrument_selector_ui::instrument_selector::{LoadedScope, offers_route};
use market_participants_connector::{
    catalog::{
        batch::CatalogEntry, display::catalog::DisplaySnapshot, error::CatalogError, store::Catalog,
    },
    connector::MarketParticipantsConnector,
    venue::definition::venues_supporting,
};
use market_participants_model::{
    identifier::VenueId,
    listing::route::{VenueListingKey, VenueRoute},
    routing::CapabilityId,
};

pub(super) fn market_venues() -> Vec<VenueId> {
    venues_supporting(CapabilityId::OrderBook)
}

pub(super) trait MarketDirectory {
    fn loaded_scopes(&self) -> Arc<[LoadedScope]>;

    fn any_market_loaded(&self) -> bool;

    fn is_loaded(&self, route: &VenueRoute) -> bool;

    fn contains(&self, listing: &VenueListingKey) -> Result<bool, CatalogError>;

    fn listing(&self, listing: &VenueListingKey) -> Option<Arc<CatalogEntry>>;

    fn catalog(&self) -> Arc<Catalog>;

    fn order_book_depth(&self, route: &VenueRoute) -> Option<u16>;

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

    fn contains(&self, listing: &VenueListingKey) -> Result<bool, CatalogError> {
        self.0.catalog().contains(listing)
    }

    fn listing(&self, listing: &VenueListingKey) -> Option<Arc<CatalogEntry>> {
        self.0.catalog().cached_listing(listing)
    }

    fn catalog(&self) -> Arc<Catalog> {
        self.0.catalog_arc()
    }

    fn order_book_depth(&self, route: &VenueRoute) -> Option<u16> {
        self.0.market().order_book_depth(route).ok()
    }

    fn display(&self) -> DisplaySnapshot {
        self.0.catalog().cached_display()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ListingSupport {
    Offered,
    Withdrawn,
    Unknown,
}

pub(super) fn listing_support(
    listing: &VenueListingKey,
    offered: Result<bool, CatalogError>,
    routes: &[LoadedScope],
    directory: &dyn MarketDirectory,
) -> ListingSupport {
    if !offers_route(routes, &listing.route, CapabilityId::OrderBook) {
        return ListingSupport::Withdrawn;
    }
    match offered {
        Ok(true) => match directory.listing(listing) {
            Some(entry) if entry.snapshot.status.is_terminal() => ListingSupport::Withdrawn,
            _ => ListingSupport::Offered,
        },
        Ok(false) => ListingSupport::Withdrawn,
        Err(CatalogError::RouteNotRegistered { .. }) => ListingSupport::Withdrawn,
        Err(error) => {
            log::warn!(
                "asking whether `{}` is still published failed; the book asks for the stream \
                 anyway: {error}",
                listing.listing_id
            );
            ListingSupport::Unknown
        }
    }
}

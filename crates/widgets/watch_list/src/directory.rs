use std::sync::Arc;

use instrument_selector_ui::instrument_selector::LoadedScope;
use market_participants_connector::{
    catalog::{batch::CatalogEntry, error::CatalogError, store::Catalog},
    connector::MarketParticipantsConnector,
    venue::definition::venues_supporting,
};
use market_participants_model::{
    identifier::VenueId,
    listing::route::{VenueListingKey, VenueRoute},
    routing::CapabilityId,
};

pub(super) fn market_venues() -> Vec<VenueId> {
    venues_supporting(CapabilityId::Tickers)
}

pub(super) trait WatchListDirectory {
    fn loaded_scopes(&self) -> Arc<[LoadedScope]>;

    fn any_market_loaded(&self) -> bool;

    fn is_loaded(&self, route: &VenueRoute) -> bool;

    fn serves_tickers(&self, route: &VenueRoute) -> bool;

    fn contains(&self, listing: &VenueListingKey) -> Result<bool, CatalogError>;

    fn listing(&self, listing: &VenueListingKey) -> Option<Arc<CatalogEntry>>;

    fn catalog(&self) -> Arc<Catalog>;
}

pub(super) struct ConnectorDirectory(MarketParticipantsConnector);

impl ConnectorDirectory {
    pub(super) const fn new(connector: MarketParticipantsConnector) -> Self {
        Self(connector)
    }
}

impl WatchListDirectory for ConnectorDirectory {
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

    fn serves_tickers(&self, route: &VenueRoute) -> bool {
        self.0.venues().scope(route).is_some_and(|scope| {
            scope
                .capabilities
                .iter()
                .any(|support| support.capability == CapabilityId::Tickers)
        })
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ListingSupport {
    Offered,
    Withdrawn,
    Unknown,
}

pub(super) fn listing_support(
    listing: &VenueListingKey,
    offered: Option<Result<bool, CatalogError>>,
    directory: &dyn WatchListDirectory,
) -> ListingSupport {
    match offered {
        None => match directory.is_loaded(&listing.route) {
            true if !directory.serves_tickers(&listing.route) => ListingSupport::Withdrawn,
            _ => ListingSupport::Unknown,
        },
        Some(Ok(true)) => match directory.listing(listing) {
            Some(entry) if entry.snapshot.status.is_terminal() => ListingSupport::Withdrawn,
            _ => ListingSupport::Offered,
        },
        Some(Ok(false)) => ListingSupport::Withdrawn,
        Some(Err(CatalogError::RouteNotRegistered { .. })) => ListingSupport::Withdrawn,
        Some(Err(error)) => {
            log::warn!(
                "asking whether `{}` is still published failed; it stays in the list and its \
                 stream reports what went wrong: {error}",
                listing.listing_id
            );
            ListingSupport::Unknown
        }
    }
}

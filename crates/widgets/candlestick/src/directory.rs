//! The loaded market directory one chart reads.
//!
//! Everything the pane decides *before* it asks for a stream is a directory answer: whether
//! a market that serves candles is in memory at all, whether the route of the stored listing
//! is one of the loaded venues' scopes, whether that scope offers candlesticks, what the
//! published catalogue says about the listing, and which display generation the readout's
//! units are written from. The production implementation wraps the injected
//! [`MarketParticipantsConnector`]; a test drives a fixture that answers the same questions.
//!
//! The directory is a seam rather than a direct connector call because the framework's
//! headless host cannot *load* a venue: `MarketParticipantsConnector` can be injected into
//! `WidgetTestApp`, but loading one needs a live transport through `load_venue`, and the
//! loaded-venue table's write entry (`LoadedVenues::replace`) is crate-private. The same
//! seam is what `market_trade` (W1b) and `order_book` (W4b) carry for the same reason.
//!
//! Nothing here loads the directory, triggers discovery or retries: the pane reads the
//! answers it is given and reloads when the injected
//! [`catalog_generation`](widget::services::WidgetServices::catalog_generation) signal moves
//! (AGENTS §8.3).

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

/// The venues whose loader can answer with candles.
///
/// This is the one static answer the pane may read: the capability's venue set off the
/// definition table, which is the same answer whether or not anything is loaded. It is what
/// the "load this market" notice names, and what says which markets *could* publish candles.
pub(super) fn market_venues() -> Vec<VenueId> {
    venues_supporting(CapabilityId::Candlesticks)
}

/// What the published catalogue proves about one listing.
pub(super) enum ListingAnswer {
    /// The catalogue publishes it and it trades.
    Listed,
    /// The catalogue publishes it, in a state no market call reads.
    Terminal,
    /// The catalogue provably does not publish it.
    Missing,
    /// The directory provably does not serve it (no registration for the route).
    Unserved,
    /// The lookup failed: it proves nothing, so the stored selection stays.
    Unknown(CatalogError),
}

/// The market directory one chart reads.
pub(super) trait MarketDirectory {
    /// Every scope the loaded venues answer, in the order they answer it.
    fn loaded_scopes(&self) -> Arc<[LoadedScope]>;

    /// Whether any market that can answer with candles is loaded at all.
    fn any_market_loaded(&self) -> bool;

    /// Whether `route` is one of the loaded venues' scopes.
    fn is_loaded(&self, route: &VenueRoute) -> bool;

    /// Whether the loaded scope of `route` offers candlesticks.
    fn offers_candlesticks(&self, route: &VenueRoute) -> bool;

    /// What the published catalogue says about `listing`.
    fn listing(&self, listing: &VenueListingKey) -> ListingAnswer;

    /// The display generation the readout's units are written from.
    fn display(&self) -> DisplaySnapshot;
}

/// The loaded directory of the running application.
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

/// What `catalog`'s published generation says about `listing`.
///
/// A failed lookup is split in two: the catalogue's own refusals (`RouteNotRegistered`,
/// `ListingNotFound`, `ListingRouteMismatch`) prove the listing is not there, while anything
/// else — a generation that never published the route, a read that could not be answered —
/// proves nothing and leaves the stored selection alone.
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

/// Whether a catalogue refusal proves that the listing is not chartable.
pub(super) fn proves_absent(error: &CatalogError) -> bool {
    matches!(
        error,
        CatalogError::RouteNotRegistered { .. }
            | CatalogError::ListingNotFound { .. }
            | CatalogError::ListingRouteMismatch { .. }
    )
}

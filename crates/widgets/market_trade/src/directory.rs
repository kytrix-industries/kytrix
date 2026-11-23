//! The loaded market directory one tape reads.
//!
//! A tape follows one listing on one route, but which routes exist, whether the
//! market behind one is in memory and what the published catalogue says about the
//! listing are all the directory's answers — the injected connector in the
//! application, a fixture in a test. The pane asks three questions of it:
//!
//! - which scopes the loaded venues offer (what its picker may present, and whether
//!   the route of the stored listing is still one it can ask);
//! - whether any market that could report trades is loaded at all (with none, there
//!   is nothing to pick a listing from, which is a wait rather than a failure);
//! - what the published catalogue currently says about the stored listing (does the
//!   route still publish it, and is it in a lifecycle state a stream can serve).
//!
//! The directory is a seam rather than a direct connector call because the
//! framework's headless host cannot build a [`MarketParticipantsConnector`] (its
//! constructor still needs the old toolkit's runtime): a test drives a fixture that
//! answers the same three questions, and the production implementation wraps the
//! connector. See `crates/widgets/AGENTS.md` §8 and the W1b test ledger's gap 1.
//!
//! Nothing here loads the directory, triggers discovery or retries: the pane reads
//! the answers it is given and reloads when the injected
//! [`catalog_generation`](widget::services::WidgetServices::catalog_generation)
//! signal moves (AGENTS §8.3).

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

/// The venues whose loader can answer with trades.
///
/// This is the one static answer a pane may read: this capability's venue set off
/// the definition table, which is the same answer whether or not anything is loaded
/// and whichever instrument is followed. It is what says which markets *could*
/// report trades, which is the only honest answer while nothing is loaded.
pub(super) fn market_venues() -> Vec<VenueId> {
    venues_supporting(CapabilityId::MarketTrade)
}

/// The loaded market directory one pane reads.
pub(super) trait MarketDirectory {
    /// Every scope the loaded venues answer, in the order they answer it.
    ///
    /// The picker is built from this snapshot, so it is re-read where the picker is
    /// built rather than remembered: a venue loaded after the tape started reading is
    /// one the user may select.
    fn loaded_scopes(&self) -> Arc<[LoadedScope]>;

    /// Whether any market that can report trades is loaded.
    ///
    /// This is the first half of the pane's precondition: a tape with nothing chosen
    /// draws its picker from the loaded venues, so with none of them loaded there is
    /// nothing to choose from at all.
    fn any_market_loaded(&self) -> bool;

    /// Whether one route's market is loaded: its catalogue has been published.
    fn is_loaded(&self, route: &VenueRoute) -> bool;

    /// Whether the published catalogue contains `listing`.
    ///
    /// `Err` does not prove the listing is gone (the route may simply not have
    /// answered); only `Ok(false)` and a terminal lifecycle state withdraw a stored
    /// selection.
    fn contains(&self, listing: &VenueListingKey) -> Result<bool, CatalogError>;

    /// The published catalogue entry for `listing`, when the generation holds it.
    fn listing(&self, listing: &VenueListingKey) -> Option<Arc<CatalogEntry>>;

    /// The published catalogue a stream resolves its listings against.
    fn catalog(&self) -> Arc<Catalog>;
}

/// The application directory: the injected market-participants connector.
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
}

/// How the stored listing stands against what its route still publishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ListingSupport {
    /// The route publishes the listing, and it still accepts followers.
    Offered,
    /// The listing is provably not followable: it left the catalogue, or its venue
    /// settled or delisted it.
    Withdrawn,
    /// The route could not be asked, which proves nothing about the listing.
    Unknown,
}

/// What the published catalogue answered about one listing.
///
/// Only two answers withdraw a stored listing: the route answered that it does not
/// publish it — including by reporting that it is not a registered route any more —
/// or the entry it publishes is in a terminal lifecycle state, which is a listing no
/// venue serves any more. Everything else — a route that did not answer, a catalogue
/// that could not be read — leaves the stored value alone, and the subscription that
/// follows reports what actually went wrong (I10).
pub(super) fn listing_support(
    listing: &VenueListingKey,
    offered: Result<bool, CatalogError>,
    directory: &dyn MarketDirectory,
) -> ListingSupport {
    match offered {
        Ok(true) => match directory.listing(listing) {
            Some(entry) if entry.snapshot.status.is_terminal() => ListingSupport::Withdrawn,
            _ => ListingSupport::Offered,
        },
        Ok(false) => ListingSupport::Withdrawn,
        // A route the registry has withdrawn since is a scope nothing can be followed
        // on, and the directory says so itself: the snapshot a widget holds was taken
        // when it was built, so relying on it alone would leave the stored listing in
        // place until the stream failed.
        Err(CatalogError::RouteNotRegistered { .. }) => ListingSupport::Withdrawn,
        Err(error) => {
            log::warn!(
                "asking whether `{}` is still published failed; the tape asks for the stream \
                 anyway: {error}",
                listing.listing_id
            );
            ListingSupport::Unknown
        }
    }
}

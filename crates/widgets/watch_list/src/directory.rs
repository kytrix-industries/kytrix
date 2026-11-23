//! The loaded market directory one watch list reads.
//!
//! A watch list follows listings of several venues at once, so which routes exist,
//! whether the market behind one is in memory and what the published catalogue says
//! about a listing are all the directory's answers — the injected connector in the
//! application, a fixture in a test. The pane asks it three questions: which scopes
//! the loaded venues offer (what its picker may present, and whether the route of a
//! followed listing is one it can read), whether any market that answers with
//! tickers is loaded (with none, there is nothing to pick a listing from, which is
//! a wait rather than a failure), and what the published catalogue says about a
//! listing (whether the route still publishes it, and what it is called).
//!
//! The directory is a seam rather than a direct connector call because the
//! framework's headless host cannot build a [`MarketParticipantsConnector`] (its
//! constructor still needs the old toolkit's runtime): a test drives a fixture that
//! answers the same questions, and the production implementation wraps the
//! connector. See `crates/widgets/AGENTS.md` §8 and the W1 test ledger's gap 1.
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

/// The venues whose loader can answer with tickers.
///
/// This is the one static answer a pane may read: this capability's venue set off
/// the definition table, which is the same answer whether or not anything is loaded
/// and whichever listings are followed. It is what says which markets *could*
/// report tickers, which is the only honest answer while nothing is loaded.
pub(super) fn market_venues() -> Vec<VenueId> {
    venues_supporting(CapabilityId::Tickers)
}

/// The loaded market directory one pane reads.
pub(super) trait WatchListDirectory {
    /// Every scope the loaded venues answer, in the order they answer it.
    fn loaded_scopes(&self) -> Arc<[LoadedScope]>;

    /// Whether any market that can report tickers is loaded.
    fn any_market_loaded(&self) -> bool;

    /// Whether one route's market is loaded: its catalogue has been published.
    fn is_loaded(&self, route: &VenueRoute) -> bool;

    /// Whether the loaded scope behind one route answers with tickers.
    ///
    /// A route that is loaded but carries no tickers is a scope nothing can be
    /// followed on, which is a proof that a stored listing left the list rather than
    /// a reason to keep waiting for it.
    fn serves_tickers(&self, route: &VenueRoute) -> bool;

    /// Whether the published catalogue contains `listing`.
    ///
    /// `Err` does not prove the listing is gone (the route may simply not have
    /// answered); only `Ok(false)` and a terminal lifecycle state withdraw a stored
    /// listing.
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

/// How a stored listing stands against what its route still publishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ListingSupport {
    /// The route publishes the listing, and it still accepts followers.
    Offered,
    /// The listing is provably not followable: it left the catalogue, its venue
    /// settled or delisted it, or the directory answered that its route is no longer
    /// registered.
    Withdrawn,
    /// The route could not be asked, which proves nothing about the listing —
    /// including the case of a reading that did not hold the route, because a load
    /// may have committed between that reading and this question.
    Unknown,
}

/// What the published catalogue answered about one followed listing.
///
/// Only two answers withdraw a followed listing: the route answered that it does not
/// publish it — including by reporting that it is not a registered route any more — or
/// the entry it publishes is in a terminal lifecycle state, which is a listing no venue
/// serves any more.
///
/// A question nobody asked proves nothing and leaves the stored value alone. The route
/// was not asked when the reading this settlement started with did not carry it, but
/// that reading is not the loaded venues: a load commits on the runtime's own thread, so
/// it may have landed between the reading and this answer. Reading "not in that reading"
/// as a withdrawal would delete a listing from the user's persisted entries over that
/// race.
///
/// What decides it is therefore the live answer, not that reading: a route that is
/// loaded *now* and still serves no tickers is a scope nothing can be followed on, and
/// its listings leave the list — while a route that is not loaded now is a question
/// nobody asked, and its listings stay for the request that follows to settle.
pub(super) fn listing_support(
    listing: &VenueListingKey,
    offered: Option<Result<bool, CatalogError>>,
    directory: &dyn WatchListDirectory,
) -> ListingSupport {
    match offered {
        // The reading did not hold the route, so there was nothing to ask. Whether
        // that is a withdrawal is a question for the loaded venues, which are read
        // here rather than remembered: a load may have committed since.
        None => match directory.is_loaded(&listing.route) {
            // The scope is there and answers no tickers: nothing can be followed on
            // it, so the listing leaves the list rather than being asked for again.
            true if !directory.serves_tickers(&listing.route) => ListingSupport::Withdrawn,
            // The scope is not in memory, so nobody can say what it carries. The
            // listing is the user's and stays: the row it gets says which market
            // participant has to be loaded, and the load that brings that market back
            // settles it.
            _ => ListingSupport::Unknown,
        },
        Some(Ok(true)) => match directory.listing(listing) {
            Some(entry) if entry.snapshot.status.is_terminal() => ListingSupport::Withdrawn,
            _ => ListingSupport::Offered,
        },
        Some(Ok(false)) => ListingSupport::Withdrawn,
        // A route the registry has withdrawn since is a scope nothing can be followed
        // on, and the directory says so itself: the snapshot a widget holds was taken
        // when it was built, so relying on it alone would leave the listing in the
        // list until its stream failed.
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

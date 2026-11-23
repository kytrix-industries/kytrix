//! The loaded market directory one prediction pane reads.
//!
//! A pane follows prediction events on the venues that publish them and may draw the
//! order book of one leg. Which routes are loaded, whether the event it follows is on
//! one of them, and what catalogue a selector dialog resolves against are the
//! directory's answers — the injected connector in the application, a fixture in a
//! test. The pane asks three questions of it:
//!
//! - whether any venue that publishes prediction events is loaded at all (with none,
//!   there is nothing to select an event from, which is a wait rather than a failure);
//! - whether the route of the event it follows is loaded (a route a shrinking commit
//!   withdrew cannot be read, and its chart is not vouched for);
//! - the published catalogue, which is what a selector dialog lists events from.
//!
//! The directory is a seam rather than a direct connector call because the
//! framework's headless host cannot build a
//! [`MarketParticipantsConnector`](market_participants_connector::connector::MarketParticipantsConnector)
//! (its constructor still needs the old toolkit's runtime): a test drives a fixture
//! that answers the same questions, and the production implementation wraps the
//! connector. See `crates/widgets/AGENTS.md` §8 and the W5b test ledger's gap 1.
//!
//! Nothing here loads the directory, triggers discovery or retries: the pane reads
//! the answers it is given and reloads when the injected
//! [`catalog_generation`](widget::services::WidgetServices::catalog_generation)
//! signal moves (AGENTS §8.3).

use std::sync::Arc;

use market_participants_connector::{
    catalog::store::Catalog, connector::MarketParticipantsConnector,
    venue::definition::venues_supporting,
};
use market_participants_model::{
    identifier::VenueId, listing::route::VenueRoute, routing::CapabilityId,
};

/// The venues whose loader can answer with prediction events.
///
/// This is the one static answer a pane may read: this capability's venue set off the
/// definition table, which is the same answer whether or not anything is loaded and
/// whichever event is followed. It is what the "market not loaded" sentence lists and
/// what the first half of the pane's precondition is asked of.
pub(super) fn event_venues() -> Vec<VenueId> {
    venues_supporting(CapabilityId::PredictionEvents)
}

/// The venues whose loader can answer a prediction order book.
///
/// A venue can publish prediction events without publishing their books: the chart
/// is drawable on either, but only a venue in this set answers a book request. Asking
/// an unsupported venue would turn every reading into a failure the user cannot
/// retry away.
pub(super) fn book_venues() -> Vec<VenueId> {
    venues_supporting(CapabilityId::PredictionOrderBook)
}

/// The loaded market directory one pane reads.
pub(super) trait MarketDirectory {
    /// Whether any venue that publishes prediction events is loaded, with a scope
    /// that actually declares the capability the event selector reads.
    ///
    /// A venue being loaded is not enough: a venue loads its core scopes even when its
    /// outcome metadata published no prediction scope, and a selector built over that
    /// venue would offer nothing.
    fn any_event_market_loaded(&self) -> bool;

    /// Whether one route's scope is loaded: its catalogue has been published.
    fn is_loaded(&self, route: &VenueRoute) -> bool;

    /// The published catalogue a selector dialog resolves events against.
    fn catalog(&self) -> Option<Arc<Catalog>>;
}

/// The application directory: the injected market-participants connector.
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

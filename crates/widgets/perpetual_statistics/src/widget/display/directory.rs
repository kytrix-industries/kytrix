//! The loaded market directory one statistics pane reads.
//!
//! A pane follows one listing on one route, but which routes exist, whether the market
//! behind one is in memory, what the published catalogue says about the listing and which
//! units its numbers are labelled in are all the directory's answers — the injected
//! connector in the application, a fixture in a test. The pane asks four questions of it:
//!
//! - which scopes the loaded venues offer (what its picker may present, and whether the
//!   route of the stored listing is still one it can ask);
//! - whether any market that could report statistics is loaded at all (with none, there
//!   is nothing to pick a listing from, which is a wait rather than a failure);
//! - whether the route still serves the stored listing, and what its rules are (the
//!   units the chart is labelled in come from those rules);
//! - what the published display generation names the assets in.
//!
//! The directory is a seam rather than a direct connector call because the framework's
//! headless host cannot build a [`MarketParticipantsConnector`] (its constructor still
//! needs the old toolkit's runtime): a test drives a fixture that answers the same
//! questions, and the production implementation wraps the connector. See
//! `crates/widgets/AGENTS.md` §8 and the W1b test ledger's gap 1.
//!
//! Nothing here loads the directory, triggers discovery or retries: the pane reads the
//! answers it is given and reloads when the injected
//! [`catalog_generation`](widget::services::WidgetServices::catalog_generation) signal
//! moves (AGENTS §8.3).

use std::sync::Arc;

use i18n::{t_smol, translate::Translate};
use instrument_selector_ui::instrument_selector::{LoadedScope, offers_route};
use market_participants_connector::{
    catalog::{display::catalog::DisplaySnapshot, error::CatalogError},
    connector::MarketParticipantsConnector,
    venue::definition::venues_supporting,
};
use market_participants_model::{
    asset::AssetId,
    identifier::VenueId,
    instrument::InstrumentKind,
    listing::{
        route::{VenueListingKey, VenueRoute},
        rules::TradingRules,
    },
    market::units::QuantityUnit,
    routing::CapabilityId,
};
use smol_str::SmolStr;

/// The venues whose loader can answer with one capability's series.
///
/// This is the one static answer a pane may read: the definition table is what says which
/// venues serve a capability, and that is the same answer whether or not anything is
/// loaded. It is asked per capability rather than once for the crate: the five widgets are
/// five different capabilities, and a venue added for one of them must not be counted as
/// one that can answer for the others.
pub(super) fn market_venues(capability: CapabilityId) -> Vec<VenueId> {
    venues_supporting(capability)
}

/// The units one chart's series are measured in, named the way the catalogue names them.
///
/// The size of a series is the listing's own traded quantity and its worth is a monetary
/// amount, so the two names come from the listing's quantity rule and from its price
/// rule: no field name is read as a unit, and a quantity the listing counts in contracts
/// is named as contracts rather than as an asset.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct ChartUnits {
    pub base: SmolStr,
    pub quote: SmolStr,
}

/// What the program knows about a listing this pane is configured to follow.
///
/// The three are kept apart because only one of them is an answer: a question that could
/// not be put proves nothing about the listing.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ListingSupport {
    /// The catalog holds the listing, the route still serves these statistics, and the
    /// listing is a perpetual — with the rules its units are read from.
    Supported(Box<TradingRules>),
    /// The program answered that it does not serve this listing: the route left the
    /// registry, the route publishes no perpetual statistics, the catalog does not hold
    /// the listing, or the listing is not a perpetual contract.
    Unsupported,
    /// The question could not be answered.
    Unknown,
}

/// The loaded market directory one pane reads.
pub(crate) trait StatisticsDirectory {
    /// Every scope the loaded venues answer, in the order they answer it.
    ///
    /// The picker is built from this snapshot, so it is re-read where the picker is built
    /// rather than remembered: a venue loaded after the pane started reading is one the
    /// user may select.
    fn loaded_scopes(&self) -> Arc<[LoadedScope]>;

    /// Whether any market that can report these statistics is loaded.
    ///
    /// This is the first half of the pane's precondition: a pane with nothing chosen draws
    /// its picker from the loaded venues, so with none of them loaded there is nothing to
    /// choose from at all.
    fn any_market_loaded(&self) -> bool;

    /// Whether one route's market is loaded: its catalogue has been published.
    fn is_loaded(&self, route: &VenueRoute) -> bool;

    /// Whether the route still publishes `listing`, and in which units.
    fn listing_support(&self, listing: &VenueListingKey) -> ListingSupport;

    /// The names one listing's chart is labelled in.
    fn units(&self, rules: &TradingRules, translate: &Translate) -> ChartUnits;
}

/// The application directory: the injected market-participants connector.
pub(super) struct ConnectorDirectory {
    connector: MarketParticipantsConnector,
    /// The capability the pane's series is served under.
    ///
    /// Which venues can answer at all, and whether a route still publishes the stored
    /// listing, are questions about the capability rather than about the pane's own metric:
    /// the five widgets share one directory shape, and the ones that differ are asked for
    /// the capability they plot.
    capability: CapabilityId,
}

impl ConnectorDirectory {
    pub(super) const fn new(
        connector: MarketParticipantsConnector,
        capability: CapabilityId,
    ) -> Self {
        Self {
            connector,
            capability,
        }
    }
}

impl StatisticsDirectory for ConnectorDirectory {
    fn loaded_scopes(&self) -> Arc<[LoadedScope]> {
        LoadedScope::all(self.connector.venues())
    }

    fn any_market_loaded(&self) -> bool {
        let loaded = self.connector.venues();
        market_venues(self.capability)
            .iter()
            .any(|venue| loaded.loaded(venue).is_some())
    }

    fn is_loaded(&self, route: &VenueRoute) -> bool {
        self.connector.venues().scope(route).is_some()
    }

    fn listing_support(&self, listing: &VenueListingKey) -> ListingSupport {
        if !offers_route(&self.loaded_scopes(), &listing.route, self.capability) {
            return ListingSupport::Unsupported;
        }
        match self.connector.catalog().resolve_listing(listing) {
            Ok(resolved)
                if resolved.instrument.kind() != InstrumentKind::Perpetual
                    || resolved.status.is_terminal() =>
            {
                ListingSupport::Unsupported
            }
            Ok(resolved) => ListingSupport::Supported(Box::new(resolved.rules)),
            Err(CatalogError::ListingNotFound { .. } | CatalogError::RouteNotRegistered { .. }) => {
                ListingSupport::Unsupported
            }
            Err(error) => {
                log::warn!(
                    "asking the catalog whether it still lists `{}` failed; the pane asks for \
                     the window anyway: {error}",
                    listing.listing_id.as_str()
                );
                ListingSupport::Unknown
            }
        }
    }

    fn units(&self, rules: &TradingRules, translate: &Translate) -> ChartUnits {
        // One capture names both axes, so a chart never mixes two publications even if the
        // catalog is republished while it is labelled.
        let generation = self.connector.catalog().cached_display();
        ChartUnits {
            base: quantity_unit_label(&rules.common.quantity.unit, &generation, translate),
            quote: asset_display(&generation, &rules.common.price.quote_asset, translate),
        }
    }
}

/// The display code of one asset, as the captured generation published it.
///
/// Only the display catalog knows what an asset is called: the canonical identity of a
/// token is a digest, and the code it publishes is the whole name of the asset. An asset
/// the captured generation does not name is the localized unknown rather than an identity
/// digest, because a reader cannot use a hash and a name invented here would disagree with
/// every other pane.
fn asset_display(generation: &DisplaySnapshot, asset: &AssetId, translate: &Translate) -> SmolStr {
    generation.resolver().asset_code(asset).map_or_else(
        |_| t_smol!(translate, "error.display.unknown_asset"),
        |code| SmolStr::new(code.as_str()),
    )
}

/// The label of what one quantity counts.
fn quantity_unit_label(
    unit: &QuantityUnit,
    generation: &DisplaySnapshot,
    translate: &Translate,
) -> SmolStr {
    match unit {
        QuantityUnit::AssetUnits { asset } | QuantityUnit::Notional { asset } => {
            asset_display(generation, asset, translate)
        }
        QuantityUnit::Contracts => t_smol!(translate, "widget.perpetual_statistics.contracts_unit"),
        QuantityUnit::OutcomeTokens { .. } => {
            t_smol!(translate, "widget.perpetual_statistics.outcome_tokens_unit")
        }
    }
}

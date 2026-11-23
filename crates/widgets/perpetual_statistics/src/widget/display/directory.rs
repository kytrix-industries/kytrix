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

pub(super) fn market_venues(capability: CapabilityId) -> Vec<VenueId> {
    venues_supporting(capability)
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct ChartUnits {
    pub base: SmolStr,
    pub quote: SmolStr,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ListingSupport {
    Supported(Box<TradingRules>),
    Unsupported,
    Unknown,
}

pub(crate) trait StatisticsDirectory {
    fn loaded_scopes(&self) -> Arc<[LoadedScope]>;

    fn any_market_loaded(&self) -> bool;

    fn is_loaded(&self, route: &VenueRoute) -> bool;

    fn listing_support(&self, listing: &VenueListingKey) -> ListingSupport;

    fn units(&self, rules: &TradingRules, translate: &Translate) -> ChartUnits;
}

pub(super) struct ConnectorDirectory {
    connector: MarketParticipantsConnector,
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
        let generation = self.connector.catalog().cached_display();
        ChartUnits {
            base: quantity_unit_label(&rules.common.quantity.unit, &generation, translate),
            quote: asset_display(&generation, &rules.common.price.quote_asset, translate),
        }
    }
}

fn asset_display(generation: &DisplaySnapshot, asset: &AssetId, translate: &Translate) -> SmolStr {
    generation.resolver().asset_code(asset).map_or_else(
        |_| t_smol!(translate, "error.display.unknown_asset"),
        |code| SmolStr::new(code.as_str()),
    )
}

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

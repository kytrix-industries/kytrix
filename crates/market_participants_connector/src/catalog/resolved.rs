use std::sync::Arc;

use market_participants_model::{
    identifier::NativeSymbol,
    instrument::Instrument,
    listing::{
        native::{ListingIdentifier, OutcomeEventIdentifier, OutcomeGroupIdentifier},
        route::{VenueListingKey, VenueRoute},
    },
    outcome::{OutcomeEventId, OutcomeGroupId},
};

use crate::catalog::{
    batch::CatalogEntry,
    error::CatalogError,
    generation::{self, CatalogGenerationId},
    store::Catalog,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedListing {
    pub key: VenueListingKey,
    pub generation: CatalogGenerationId,
    pub instrument: Instrument,
    pub native_symbol: Option<NativeSymbol>,
    pub identifiers: market_participants_model::listing::identifiers::VenueListingIdentifiers,
    pub rules: market_participants_model::listing::rules::TradingRules,
    pub status: market_participants_model::listing::listing_impl::ListingStatus,
    pub available_from: Option<market_participants_model::decimal::UnixMillis>,
    pub available_until: Option<market_participants_model::decimal::UnixMillis>,
}

impl ResolvedListing {
    pub(crate) fn from_entry(generation: CatalogGenerationId, entry: Arc<CatalogEntry>) -> Self {
        Self {
            key: entry.listing.key.clone(),
            generation,
            instrument: entry.instrument.clone(),
            native_symbol: entry.listing.native_symbol.clone(),
            identifiers: entry.listing.identifiers.clone(),
            rules: entry.snapshot.rules.clone(),
            status: entry.snapshot.status,
            available_from: entry.snapshot.available_from,
            available_until: entry.snapshot.available_until,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedListingRef {
    pub key: VenueListingKey,
    pub generation: CatalogGenerationId,
    pub native_symbol: Option<NativeSymbol>,
    pub identifiers: market_participants_model::listing::identifiers::VenueListingIdentifiers,
}

impl ResolvedListingRef {
    pub(crate) fn from_entry(generation: CatalogGenerationId, entry: Arc<CatalogEntry>) -> Self {
        Self {
            key: entry.listing.key.clone(),
            generation,
            native_symbol: entry.listing.native_symbol.clone(),
            identifiers: entry.listing.identifiers.clone(),
        }
    }
}

#[derive(Clone)]
pub struct EnsuredRoute {
    catalog: Arc<Catalog>,
    route: VenueRoute,
}

impl EnsuredRoute {
    pub(crate) fn new(catalog: Arc<Catalog>, route: VenueRoute) -> Self {
        Self { catalog, route }
    }

    pub fn route(&self) -> &VenueRoute {
        &self.route
    }

    pub fn listings(&self) -> Arc<[CatalogEntry]> {
        self.catalog
            .cached_listings(&self.route)
            .unwrap_or_else(|| Arc::from(Vec::new()))
    }

    pub fn resolve_listing_identifier(
        &self,
        identifier: &ListingIdentifier,
    ) -> Result<VenueListingKey, CatalogError> {
        let normalization = self
            .catalog
            .listing_identifier_normalization(&self.route, identifier)?;
        self.catalog.with_generation(|generation| {
            let snapshot = generation.route_snapshot(&self.route).ok_or_else(|| {
                CatalogError::RouteNotRegistered {
                    route: self.route.clone(),
                }
            })?;
            generation::resolve_native_listing(snapshot, normalization, identifier)
        })
    }

    pub fn resolve_native_symbol(
        &self,
        symbol: &NativeSymbol,
    ) -> Result<VenueListingKey, CatalogError> {
        let normalization = self.catalog.symbol_normalization(&self.route)?;
        self.catalog.with_generation(|generation| {
            let snapshot = generation.route_snapshot(&self.route).ok_or_else(|| {
                CatalogError::RouteNotRegistered {
                    route: self.route.clone(),
                }
            })?;
            generation::resolve_native_symbol(snapshot, normalization, symbol)
        })
    }

    pub fn resolve_group_identifier(
        &self,
        identifier: &OutcomeGroupIdentifier,
    ) -> Result<OutcomeGroupId, CatalogError> {
        self.catalog.with_generation(|generation| {
            let snapshot = generation.route_snapshot(&self.route).ok_or_else(|| {
                CatalogError::RouteNotRegistered {
                    route: self.route.clone(),
                }
            })?;
            generation::resolve_native_group(generation, snapshot, identifier)
        })
    }

    pub fn resolve_event_identifier(
        &self,
        identifier: &OutcomeEventIdentifier,
    ) -> Result<OutcomeEventId, CatalogError> {
        self.catalog.with_generation(|generation| {
            let snapshot = generation.route_snapshot(&self.route).ok_or_else(|| {
                CatalogError::RouteNotRegistered {
                    route: self.route.clone(),
                }
            })?;
            generation::resolve_native_event(generation, snapshot, identifier)
        })
    }

    pub fn native_symbol(&self, listing: &VenueListingKey) -> Option<NativeSymbol> {
        if listing.route != self.route {
            return None;
        }
        self.catalog.native_symbol(listing)
    }

    pub fn resolve_listing(
        &self,
        listing: &VenueListingKey,
    ) -> Result<ResolvedListing, CatalogError> {
        if listing.route != self.route {
            return Err(CatalogError::ListingRouteMismatch {
                expected: Box::new(self.route.clone()),
                found: Box::new(listing.clone()),
            });
        }
        self.catalog
            .with_generation(|generation| generation::resolve_listing(generation, listing))
    }
}

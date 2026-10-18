//! One listing resolved for a single use, and the proof that a route's
//! catalogue is present.
//!
//! A resolution is a value taken from one published generation: it names the
//! generation it came from, and it must not be reused after that generation is
//! replaced. An [`EnsuredRoute`] is the synchronous form of the same answer for a
//! caller that has already asserted the route is published — a per-message path
//! of a live stream, which cannot await a fetch inside an event loop poll.

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

/// One listing, resolved for a single use.
///
/// The resolution is only valid inside the generation that produced it: after a
/// route is revoked or a new generation is published it must not be reused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedListing {
    /// The listing key this resolution is about.
    pub key: VenueListingKey,
    /// The generation the resolution was taken from.
    pub generation: CatalogGenerationId,
    /// The canonical instrument of the listing.
    pub instrument: Instrument,
    /// The native symbol of the listing, when its venue has one.
    pub native_symbol: Option<NativeSymbol>,
    /// The typed native identifiers of the listing.
    pub identifiers: market_participants_model::listing::identifiers::VenueListingIdentifiers,
    /// The rules of the listing.
    pub rules: market_participants_model::listing::rules::TradingRules,
    /// The status of the listing.
    pub status: market_participants_model::listing::listing_impl::ListingStatus,
    /// The instant the listing becomes available, when it has one.
    pub available_from: Option<market_participants_model::decimal::UnixMillis>,
    /// The instant the listing stops being available, when it has one.
    pub available_until: Option<market_participants_model::decimal::UnixMillis>,
}

impl ResolvedListing {
    /// Builds the resolution of one published entry.
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

/// The protocol-facing projection of one listing.
///
/// It carries the typed native identity an adapter encodes a request with and
/// the generation that produced it; it is not a substitute for the full listing
/// when business rules have to be checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedListingRef {
    /// The listing key this reference is about.
    pub key: VenueListingKey,
    /// The generation the reference was taken from.
    pub generation: CatalogGenerationId,
    /// The native symbol of the listing, when its venue has one.
    pub native_symbol: Option<NativeSymbol>,
    /// The typed native identifiers of the listing.
    pub identifiers: market_participants_model::listing::identifiers::VenueListingIdentifiers,
}

impl ResolvedListingRef {
    /// Builds the projection of one published entry.
    pub(crate) fn from_entry(generation: CatalogGenerationId, entry: Arc<CatalogEntry>) -> Self {
        Self {
            key: entry.listing.key.clone(),
            generation,
            native_symbol: entry.listing.native_symbol.clone(),
            identifiers: entry.listing.identifiers.clone(),
        }
    }
}

/// A route whose catalogue is present, as the proof that a synchronous resolution
/// may answer from it.
///
/// Only [`Catalog::ensure`] and [`MarketCapability::ensure`] produce one, so
/// holding this value is proof that the route's catalogue has been published. The
/// resolvers on it are therefore synchronous, which is what the per-message paths
/// of a live stream need: an event loop poll cannot await a fetch.
///
/// [`MarketCapability::ensure`]: crate::market::provider::MarketCapability::ensure
#[derive(Clone)]
pub struct EnsuredRoute {
    catalog: Arc<Catalog>,
    route: VenueRoute,
}

impl EnsuredRoute {
    /// Builds the proof of one route the catalogue already publishes.
    pub(crate) fn new(catalog: Arc<Catalog>, route: VenueRoute) -> Self {
        Self { catalog, route }
    }

    /// Returns the route this proof is about.
    pub fn route(&self) -> &VenueRoute {
        &self.route
    }

    /// Returns the entries published for this route.
    pub fn listings(&self) -> Arc<[CatalogEntry]> {
        self.catalog
            .cached_listings(&self.route)
            .unwrap_or_else(|| Arc::from(Vec::new()))
    }

    /// Resolves a typed listing identifier from the published snapshot.
    ///
    /// The identifier must name a listing of *this* route: a scoped identifier —
    /// a Hyperliquid asset or outcome — only ever names a listing of the route's
    /// own scope, and a listing another route publishes is not reachable from this
    /// route's own index. It is folded with the route's own rule for its typed
    /// identifier and product before the lookup — the rule the route's index was
    /// built under — so a spelling the route folds names the same listing here. The
    /// answer is the one [`Catalog::resolve_listing_identifier`] gives: both call
    /// `generation::resolve_native_listing`.
    ///
    /// Nothing here loads a catalog, so a route whose snapshot was revoked serves
    /// nothing until the connection action loads it again.
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

    /// Resolves a native symbol from the published snapshot.
    ///
    /// The symbol is normalized with the route's own rule for bare symbols before
    /// the lookup — the rule the route's index was built under — and the answer is
    /// the one [`Catalog::resolve_native_symbol`] gives: both call
    /// `generation::resolve_native_symbol`.
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

    /// Resolves a native outcome group identifier from the published snapshot of
    /// this route.
    ///
    /// The group must be reachable from *this* route's own index — either through
    /// one of its native lookups or because one of its listings trades a leg of
    /// the group — and a scoped identifier only names a group of this route's own
    /// scope. A group that only another route publishes is refused, and the answer
    /// is the one [`Catalog::resolve_group_identifier`] gives: both call
    /// `generation::resolve_native_group`.
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

    /// Resolves a native outcome event identifier from the published snapshot of
    /// this route.
    ///
    /// The event must be reachable from *this* route's own index — either through
    /// one of its native event lookups or because the route publishes the event a
    /// lookup registered elsewhere names. An event that only another route
    /// publishes is refused, and the answer is the one
    /// [`Catalog::resolve_event_identifier`] gives: both call
    /// `generation::resolve_native_event`.
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

    /// Returns the native symbol of one listing of this route.
    pub fn native_symbol(&self, listing: &VenueListingKey) -> Option<NativeSymbol> {
        if listing.route != self.route {
            return None;
        }
        self.catalog.native_symbol(listing)
    }

    /// Returns the resolution of one listing of this route.
    ///
    /// A listing of another route is refused outright, however published it is:
    /// the proof of one route never answers with another route's listing. The
    /// answer for a listing of this route is the one [`Catalog::resolve_listing`]
    /// gives: both call `generation::resolve_listing`.
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

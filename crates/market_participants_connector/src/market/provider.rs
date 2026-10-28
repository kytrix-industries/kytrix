use std::sync::Arc;

use market_participants_model::{
    instrument::Instrument,
    listing::{
        listing_impl::ListingStatus,
        route::{VenueListingKey, VenueRoute},
    },
    routing::CapabilityId,
};

use super::{
    domain::{MarketSelector, require_listing_route},
    error::{MarketError, market_error_from_catalog},
    helpers::subscribe,
    streams::ForceOrderStream,
    timeframe::{Adjustment, TimeFrame},
};
use crate::{
    adapter::AdapterLimits,
    catalog::{
        error::CatalogError,
        resolved::{EnsuredRoute, ResolvedListing},
        store::Catalog,
    },
    connector::MarketParticipantsConnectorInner,
    market_participants::model::ForceOrderSubscriptionQuery,
};

/// One catalog row a route publishes, as a caller reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketListing {
    pub listing: VenueListingKey,
    pub status: ListingStatus,
    pub instrument: Instrument,
}

#[derive(Clone)]
pub struct MarketCapability {
    inner: Arc<MarketParticipantsConnectorInner>,
}

impl MarketCapability {
    pub(crate) fn new(inner: Arc<MarketParticipantsConnectorInner>) -> Self {
        Self { inner }
    }

    pub(crate) fn gateway(&self) -> &crate::client::MarketParticipantGateway {
        &self.inner.gateway
    }

    /// Returns the one routing view every call is admitted against.
    pub(crate) fn venues(&self) -> &crate::venue::loaded::LoadedVenues {
        &self.inner.venues
    }

    /// Returns the shared catalogue this capability resolves listings from.
    pub fn catalog(&self) -> &Catalog {
        self.inner.catalog()
    }

    /// Resolves one listing inside the generation the catalog is serving now.
    ///
    /// The resolution is the only way a request learns what its listing is: the
    /// rules that validate a quantity, the status that gates a command and the
    /// typed identifiers an adapter encodes with all come from this one value.
    pub fn resolve_listing(
        &self,
        listing: &VenueListingKey,
    ) -> Result<ResolvedListing, MarketError> {
        self.catalog()
            .resolve_listing(listing)
            .map_err(market_error_from_catalog)
    }

    /// Resolves one listing of a route, rejecting a listing of another route.
    pub(crate) fn resolve_listing_of_route(
        &self,
        route: &VenueRoute,
        listing: &VenueListingKey,
    ) -> Result<ResolvedListing, MarketError> {
        require_listing_route(route, listing)?;
        self.resolve_listing(listing)
    }

    /// Asserts that one route's catalog is already published, and answers the
    /// proof that it is present.
    ///
    /// The proof is what a per-message path needs: a stream resolves listing keys
    /// against the published snapshot without ever awaiting a fetch. Nothing here
    /// reaches the venue: a route no load has fetched is
    /// [`MarketError::Catalog`] carrying
    /// [`CatalogError::CatalogNotLoaded`].
    pub fn ensure(&self, route: &VenueRoute) -> Result<EnsuredRoute, MarketError> {
        self.catalog()
            .ensure(route)
            .map_err(market_error_from_catalog)?;
        Ok(EnsuredRoute::new(self.inner.catalog_arc(), route.clone()))
    }

    /// Returns the venue-specific bounds of one route.
    ///
    /// The bounds belong to the adapter that speaks the venue's wire, and the route
    /// they are looked up for is admitted by the one routing view: the loaded venues.
    /// A market nobody loaded is not a market this build does not serve — the two are
    /// the two answers — so an unloaded scope is refused as exactly that and a
    /// loaded scope whose entry lacks the capability is the one refusal this build's
    /// own scope set makes. Neither answer costs a request: both are decided against
    /// the loaded venues before anything is encoded.
    pub(crate) fn limits(
        &self,
        route: &VenueRoute,
        capability: CapabilityId,
    ) -> Result<AdapterLimits, MarketError> {
        self.gateway()
            .router
            .limits(self.venues(), route, capability)
            .map_err(MarketError::from)
    }

    /// The candlestick intervals one route publishes.
    ///
    /// The intervals are the venue's own answer for the route, read through the route's
    /// `Candlesticks` descriptor, so a caller validates an interval it persisted against
    /// what this build can actually ask for rather than against the intervals the model can
    /// express. A market nobody loaded is answered as not loaded — a caller waits for
    /// it — while a loaded route whose entry does not declare candlesticks is answered with
    /// the refusal that proves the interval cannot be asked for at all.
    pub fn supported_timeframes(&self, route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError> {
        self.limits(route, CapabilityId::Candlesticks)
            .map(|limits| limits.timeframes)
    }

    /// The price-adjustment conventions one route publishes.
    ///
    /// The conventions are the venue's own answer for the route, read through the
    /// route's `Candlesticks` descriptor, so a caller validates a convention it
    /// persisted against what this build can actually ask for rather than against
    /// what the model can express. A market nobody loaded is answered as not
    /// loaded — a caller waits for it — while a loaded route whose entry does not
    /// declare candlesticks is answered with the refusal that proves no convention
    /// can be asked for at all.
    pub fn supported_adjustments(
        &self,
        route: &VenueRoute,
    ) -> Result<Vec<Adjustment>, MarketError> {
        self.limits(route, CapabilityId::Candlesticks)
            .map(|limits| limits.adjustments)
    }

    /// The deepest book one route publishes per side.
    ///
    /// The depth is the venue's own bound for the route: a book deeper than this is
    /// not something the venue hands over, and a caller that draws the book uses it
    /// to offer only the rows the book can fill.
    pub fn order_book_depth(&self, route: &VenueRoute) -> Result<u16, MarketError> {
        self.limits(route, CapabilityId::OrderBook)
            .map(|limits| limits.order_book_depth_limit)
    }

    pub fn market(&self, route: VenueRoute) -> MarketSelector {
        MarketSelector::new(self.clone(), route)
    }

    /// Returns every listing of one route the published catalog holds.
    ///
    /// The read is answered from the published snapshot alone, so a route nothing
    /// has loaded is [`MarketError::Catalog`] carrying
    /// [`CatalogError::CatalogNotLoaded`] rather than an empty list:
    /// "this market has no instruments" and "this market was never loaded" are
    /// two different answers, and only the first is an empty list.
    pub fn listings(&self, route: &VenueRoute) -> Result<Vec<MarketListing>, MarketError> {
        self.ensure(route)?;
        let entries = self.catalog().cached_listings(route).ok_or_else(|| {
            market_error_from_catalog(CatalogError::CatalogNotLoaded {
                route: route.clone(),
            })
        })?;
        let mut listings: Vec<MarketListing> = entries
            .iter()
            .map(|entry| MarketListing {
                listing: entry.listing.key.clone(),
                status: entry.snapshot.status,
                instrument: entry.instrument.clone(),
            })
            .collect();
        listings.sort_by(|left, right| left.listing.cmp(&right.listing));
        Ok(listings)
    }

    pub async fn subscribe_force_order(
        &self,
        query: ForceOrderSubscriptionQuery,
    ) -> Result<ForceOrderStream, MarketError> {
        let route = query.route.clone();
        // The query's own validation is a typed model refusal, so it keeps the
        // typed error rather than being flattened into a sentence about a request.
        query.validate().map_err(MarketError::from)?;
        let ensured = self.ensure(&route)?;
        let handle = subscribe(
            self,
            route,
            crate::adapter::ProtocolRequest::SubscribeForceOrder(query),
        )
        .await?;
        Ok(ForceOrderStream::from_handle(handle, ensured))
    }

    /// Subscribes to the liquidation stream of one route.
    ///
    /// A liquidation subscription is route-scoped and carries no listing: which
    /// listings a route liquidates is what the events themselves say, and the
    /// stream resolves each of them against the published catalog. This is the
    /// entry a caller outside this crate uses, because the request type it builds
    /// the subscription from is this crate's own protocol boundary.
    pub async fn subscribe_force_order_on(
        &self,
        route: VenueRoute,
    ) -> Result<ForceOrderStream, MarketError> {
        self.subscribe_force_order(ForceOrderSubscriptionQuery { route })
            .await
    }
}

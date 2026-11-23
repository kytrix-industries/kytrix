use std::sync::Arc;

use market_participants_model::{
    instrument::Instrument,
    listing::{
        listing_impl::ListingStatus,
        route::{VenueListingKey, VenueRoute},
    },
    outcome::OutcomeLegRef,
    prediction::{PredictionEventRef, PredictionLeg, PredictionOrderBook},
    routing::CapabilityId,
};

use super::{
    domain::{MarketSelector, require_listing_route},
    error::{MarketError, market_error_from_catalog},
    helpers::{execute, protocol_response, subscribe},
    streams::{ForceOrderStream, PredictionOrderBookStream, PredictionSeriesStream},
    timeframe::{Adjustment, TimeFrame},
};
use crate::{
    adapter::{AdapterLimits, ProtocolRequest, ProtocolResponse},
    catalog::{
        error::CatalogError,
        resolved::{EnsuredRoute, ResolvedListing},
        store::Catalog,
    },
    connector::MarketParticipantsConnectorInner,
    market_participants::model::{
        ForceOrderSubscriptionQuery, PredictionOrderBookQuery, PredictionSeriesSubscriptionQuery,
    },
};

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

    pub(crate) fn venues(&self) -> &crate::venue::loaded::LoadedVenues {
        &self.inner.venues
    }

    pub fn catalog(&self) -> &Catalog {
        self.inner.catalog()
    }

    pub fn resolve_listing(
        &self,
        listing: &VenueListingKey,
    ) -> Result<ResolvedListing, MarketError> {
        self.catalog()
            .resolve_listing(listing)
            .map_err(market_error_from_catalog)
    }

    pub(crate) fn resolve_listing_of_route(
        &self,
        route: &VenueRoute,
        listing: &VenueListingKey,
    ) -> Result<ResolvedListing, MarketError> {
        require_listing_route(route, listing)?;
        self.resolve_listing(listing)
    }

    pub fn ensure(&self, route: &VenueRoute) -> Result<EnsuredRoute, MarketError> {
        self.catalog()
            .ensure(route)
            .map_err(market_error_from_catalog)?;
        Ok(EnsuredRoute::new(self.inner.catalog_arc(), route.clone()))
    }

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

    pub fn supported_timeframes(&self, route: &VenueRoute) -> Result<Vec<TimeFrame>, MarketError> {
        self.limits(route, CapabilityId::Candlesticks)
            .map(|limits| limits.timeframes)
    }

    pub fn supported_adjustments(
        &self,
        route: &VenueRoute,
    ) -> Result<Vec<Adjustment>, MarketError> {
        self.limits(route, CapabilityId::Candlesticks)
            .map(|limits| limits.adjustments)
    }

    pub fn order_book_depth(&self, route: &VenueRoute) -> Result<u16, MarketError> {
        self.limits(route, CapabilityId::OrderBook)
            .map(|limits| limits.order_book_depth_limit)
    }

    pub fn market(&self, route: VenueRoute) -> MarketSelector {
        MarketSelector::new(self.clone(), route)
    }

    pub fn prediction_events(
        &self,
        request: super::prediction::PredictionEventsRequest,
    ) -> super::prediction::PredictionEventsBuilder {
        super::prediction::PredictionEventsBuilder::new(self.clone(), request)
    }

    pub fn prediction_event(
        &self,
        event: market_participants_model::prediction::PredictionEventRef,
    ) -> super::prediction::PredictionEventBuilder {
        super::prediction::PredictionEventBuilder::new(self.clone(), event)
    }

    pub fn prediction_series(
        &self,
        event: market_participants_model::prediction::PredictionEventRef,
        legs: Vec<market_participants_model::prediction::PredictionLeg>,
        range: market_participants_model::prediction::PredictionRange,
    ) -> super::prediction::PredictionSeriesBuilder {
        super::prediction::PredictionSeriesBuilder::new(self.clone(), event, legs, range)
    }

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

    pub async fn subscribe_force_order_on(
        &self,
        route: VenueRoute,
    ) -> Result<ForceOrderStream, MarketError> {
        self.subscribe_force_order(ForceOrderSubscriptionQuery { route })
            .await
    }

    pub async fn subscribe_prediction_series(
        &self,
        event: PredictionEventRef,
        legs: Vec<PredictionLeg>,
    ) -> Result<PredictionSeriesStream, MarketError> {
        let route = event.route.clone();
        let handle = subscribe(
            self,
            route,
            ProtocolRequest::SubscribePredictionSeries(PredictionSeriesSubscriptionQuery {
                event,
                legs,
            }),
        )
        .await?;
        Ok(PredictionSeriesStream::from_handle(handle))
    }

    pub async fn fetch_prediction_order_book(
        &self,
        event: PredictionEventRef,
        legs: Vec<PredictionLeg>,
        leg: OutcomeLegRef,
    ) -> Result<PredictionOrderBook, MarketError> {
        let query = PredictionOrderBookQuery { event, legs, leg };
        let route = query.route().clone();
        let request = ProtocolRequest::FetchPredictionOrderBook(query);
        match execute(self, route, request).await? {
            ProtocolResponse::PredictionOrderBook(book) => Ok(book),
            _ => Err(protocol_response("fetch a prediction order book")),
        }
    }

    pub async fn subscribe_prediction_order_book(
        &self,
        event: PredictionEventRef,
        legs: Vec<PredictionLeg>,
        leg: OutcomeLegRef,
    ) -> Result<PredictionOrderBookStream, MarketError> {
        let query = PredictionOrderBookQuery { event, legs, leg };
        let route = query.route().clone();
        let handle = subscribe(
            self,
            route,
            ProtocolRequest::SubscribePredictionOrderBook(query),
        )
        .await?;
        Ok(PredictionOrderBookStream::from_handle(handle))
    }
}

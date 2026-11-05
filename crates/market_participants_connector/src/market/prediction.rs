//! Event-scoped prediction-market reads.
//!
//! A prediction venue publishes events rather than a stable instrument catalog,
//! so these reads are addressed by an event identity and never resolve a listing
//! from the catalog. The builder is the one place that knows a venue may publish
//! its events across several scopes: Hyperliquid serves one scope per outcome
//! deployer, so the facade asks every loaded scope and merges the answers into one
//! page. A venue with one event scope passes the server's cursor through.

use std::collections::BTreeMap;

use market_participants_model::{
    identifier::{VenueId, VenueScopeId},
    prediction::{
        PredictionEventCard, PredictionEventRef, PredictionEventView, PredictionEventsPage,
        PredictionLeg, PredictionRange, PredictionSeries,
    },
    routing::CapabilityId,
};
use smol_str::{SmolStr, format_smolstr};

use super::{error::MarketError, helpers::execute, provider::MarketCapability};
use crate::{
    adapter::{ProtocolRequest, ProtocolResponse},
    market_participants::model::{
        PredictionEventQuery, PredictionEventsQuery, PredictionSeriesQuery,
    },
};

/// The page size a caller gets when it does not choose one.
pub const DEFAULT_PREDICTION_PAGE_SIZE: u16 = 8;

/// The largest page the facade may ask one route for while merging scopes.
pub const MAX_PREDICTION_PAGE_SIZE: u16 = 500;

/// One page of events a caller asks a venue for.
///
/// It names a venue rather than a route: the facade resolves the routes that
/// publish prediction events and merges them. A caller outside the connector
/// never builds a route-scoped query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PredictionEventsRequest {
    pub venue: VenueId,
    pub category: Option<SmolStr>,
    pub search: Option<SmolStr>,
    pub cursor: Option<SmolStr>,
    pub page_size: u16,
    /// The caller's opaque content locale, forwarded to venues that localize.
    pub locale: Option<SmolStr>,
}

impl PredictionEventsRequest {
    /// Builds the first page of one venue's events.
    pub fn first(venue: VenueId) -> Self {
        Self {
            venue,
            category: None,
            search: None,
            cursor: None,
            page_size: DEFAULT_PREDICTION_PAGE_SIZE,
            locale: None,
        }
    }

    /// Sets the category filter.
    pub fn category(mut self, category: Option<SmolStr>) -> Self {
        self.category = category;
        self
    }

    /// Sets the search query.
    pub fn search(mut self, search: Option<SmolStr>) -> Self {
        self.search = search;
        self
    }

    /// Sets the cursor.
    pub fn cursor(mut self, cursor: Option<SmolStr>) -> Self {
        self.cursor = cursor;
        self
    }

    /// Sets the page size, clamped to what a selector may ask for.
    pub fn page_size(mut self, page_size: u16) -> Self {
        self.page_size = page_size.clamp(1, MAX_PREDICTION_PAGE_SIZE);
        self
    }

    /// Sets the opaque content locale forwarded to venues that localize.
    pub fn locale(mut self, locale: Option<SmolStr>) -> Self {
        self.locale = locale;
        self
    }
}

/// Reads one page of prediction events.
pub struct PredictionEventsBuilder {
    provider: MarketCapability,
    request: PredictionEventsRequest,
}

impl PredictionEventsBuilder {
    pub(super) fn new(provider: MarketCapability, request: PredictionEventsRequest) -> Self {
        Self { provider, request }
    }

    /// Fetches the page.
    pub async fn fetch(self) -> Result<PredictionEventsPage, MarketError> {
        let routes = self.routes()?;
        if routes.len() == 1 {
            return self.fetch_one(routes[0].clone()).await;
        }
        self.fetch_merged(routes).await
    }

    fn routes(
        &self,
    ) -> Result<Vec<market_participants_model::listing::route::VenueRoute>, MarketError> {
        let routes: Vec<_> = self
            .provider
            .venues()
            .entries()
            .iter()
            .filter(|(route, entry)| {
                route.venue == self.request.venue
                    && entry
                        .capabilities
                        .iter()
                        .any(|support| support.capability == CapabilityId::PredictionEvents)
            })
            .map(|(route, _)| route.clone())
            .collect();
        if !routes.is_empty() {
            return Ok(routes);
        }
        Err(MarketError::UnsupportedCapability {
            route: market_participants_model::listing::route::VenueRoute::new(
                self.request.venue,
                VenueScopeId::new("predict").expect("the prediction scope name is canonical"),
            ),
            capability: CapabilityId::PredictionEvents,
        })
    }

    async fn fetch_one(
        &self,
        route: market_participants_model::listing::route::VenueRoute,
    ) -> Result<PredictionEventsPage, MarketError> {
        let query = PredictionEventsQuery {
            route: route.clone(),
            category: self.request.category.clone(),
            search: self.request.search.clone(),
            cursor: self.request.cursor.clone(),
            page_size: self.request.page_size,
            locale: self.request.locale.clone(),
        };
        match execute(
            &self.provider,
            route,
            ProtocolRequest::FetchPredictionEvents(query),
        )
        .await?
        {
            ProtocolResponse::PredictionEvents(page) => Ok(page),
            _ => Err(super::helpers::protocol_response("fetch prediction events")),
        }
    }

    async fn fetch_merged(
        &self,
        routes: Vec<market_participants_model::listing::route::VenueRoute>,
    ) -> Result<PredictionEventsPage, MarketError> {
        let mut events: BTreeMap<PredictionEventRef, PredictionEventCard> = BTreeMap::new();
        for route in routes {
            let mut cursor = None;
            loop {
                let query = PredictionEventsQuery {
                    route: route.clone(),
                    category: None,
                    search: None,
                    cursor: cursor.clone(),
                    page_size: MAX_PREDICTION_PAGE_SIZE,
                    locale: self.request.locale.clone(),
                };
                let page = match execute(
                    &self.provider,
                    route.clone(),
                    ProtocolRequest::FetchPredictionEvents(query),
                )
                .await?
                {
                    ProtocolResponse::PredictionEvents(page) => page,
                    _ => {
                        return Err(super::helpers::protocol_response("fetch prediction events"));
                    }
                };
                for event in page.events {
                    events.entry(event.event.clone()).or_insert(event);
                }
                let Some(next) = page.next_cursor else {
                    break;
                };
                if cursor.as_ref() == Some(&next) {
                    break;
                }
                cursor = Some(next);
            }
        }
        let mut events: Vec<PredictionEventCard> = events
            .into_values()
            .filter(|event| {
                self.request
                    .category
                    .as_ref()
                    .is_none_or(|category| &event.category.key == category)
            })
            .filter(|event| self.matches_search(event))
            .collect();
        events.sort_by(|left, right| {
            right
                .volume
                .cmp(&left.volume)
                .then_with(|| left.title.cmp(&right.title))
        });
        let categories = categories_of(&events);
        let total = u32::try_from(events.len()).unwrap_or(u32::MAX);
        let offset = self
            .request
            .cursor
            .as_deref()
            .and_then(|cursor| cursor.parse::<usize>().ok())
            .unwrap_or(0)
            .min(events.len());
        let page_size = usize::from(self.request.page_size.clamp(1, MAX_PREDICTION_PAGE_SIZE));
        let end = offset.saturating_add(page_size).min(events.len());
        let next_cursor = (end < events.len()).then(|| SmolStr::new(format_smolstr!("{end}")));
        Ok(PredictionEventsPage {
            events: events[offset..end].to_vec(),
            categories,
            next_cursor,
            total: Some(total),
        })
    }

    fn matches_search(&self, event: &PredictionEventCard) -> bool {
        let Some(search) = self.request.search.as_deref() else {
            return true;
        };
        let needle = search.trim().to_lowercase();
        if needle.is_empty() {
            return true;
        }
        event.title.to_lowercase().contains(&needle)
            || event
                .options
                .iter()
                .any(|option| option.label.to_lowercase().contains(&needle))
    }
}

fn categories_of(
    events: &[PredictionEventCard],
) -> Vec<market_participants_model::prediction::PredictionCategory> {
    let mut categories = BTreeMap::new();
    for event in events {
        categories
            .entry(event.category.key.clone())
            .or_insert_with(|| event.category.clone());
    }
    categories.into_values().collect()
}

/// Reads one prediction event.
pub struct PredictionEventBuilder {
    provider: MarketCapability,
    event: PredictionEventRef,
    locale: Option<SmolStr>,
}

impl PredictionEventBuilder {
    pub(super) fn new(provider: MarketCapability, event: PredictionEventRef) -> Self {
        Self {
            provider,
            event,
            locale: None,
        }
    }

    /// Sets the opaque content locale forwarded to venues that localize.
    pub fn locale(mut self, locale: Option<SmolStr>) -> Self {
        self.locale = locale;
        self
    }

    /// Fetches the event and every leg it publishes.
    pub async fn fetch(self) -> Result<PredictionEventView, MarketError> {
        let route = self.event.route.clone();
        match execute(
            &self.provider,
            route,
            ProtocolRequest::FetchPredictionEvent(PredictionEventQuery {
                event: self.event,
                locale: self.locale,
            }),
        )
        .await?
        {
            ProtocolResponse::PredictionEvent(view) => Ok(view),
            _ => Err(super::helpers::protocol_response(
                "fetch a prediction event",
            )),
        }
    }
}

/// Reads the probability series of one prediction event.
pub struct PredictionSeriesBuilder {
    provider: MarketCapability,
    event: PredictionEventRef,
    legs: Vec<PredictionLeg>,
    range: PredictionRange,
}

impl PredictionSeriesBuilder {
    pub(super) fn new(
        provider: MarketCapability,
        event: PredictionEventRef,
        legs: Vec<PredictionLeg>,
        range: PredictionRange,
    ) -> Self {
        Self {
            provider,
            event,
            legs,
            range,
        }
    }

    /// Fetches the series.
    pub async fn fetch(self) -> Result<Vec<PredictionSeries>, MarketError> {
        let route = self.event.route.clone();
        match execute(
            &self.provider,
            route,
            ProtocolRequest::FetchPredictionSeries(PredictionSeriesQuery {
                event: self.event,
                legs: self.legs,
                range: self.range,
            }),
        )
        .await?
        {
            ProtocolResponse::PredictionSeries(series) => Ok(series),
            _ => Err(super::helpers::protocol_response("fetch prediction series")),
        }
    }
}

#[cfg(test)]
mod tests {
    use market_participants_model::identifier::VenueId;

    use super::{DEFAULT_PREDICTION_PAGE_SIZE, PredictionEventsRequest};

    #[test]
    fn a_request_defaults_to_the_first_page_of_a_venue() {
        let request = PredictionEventsRequest::first(VenueId::Polymarket);

        assert_eq!(request.venue, VenueId::Polymarket);
        assert_eq!(request.page_size, DEFAULT_PREDICTION_PAGE_SIZE);
        assert_eq!(DEFAULT_PREDICTION_PAGE_SIZE, 8);
        assert!(request.category.is_none());
        assert!(request.search.is_none());
        assert!(request.cursor.is_none());
        assert!(request.locale.is_none());
    }

    #[test]
    fn a_request_carries_the_callers_locale() {
        let request = PredictionEventsRequest::first(VenueId::Polymarket).locale(Some("zh".into()));

        assert_eq!(request.locale.as_deref(), Some("zh"));
    }

    #[test]
    fn a_page_size_is_clamped_to_what_a_selector_may_ask_for() {
        assert_eq!(
            PredictionEventsRequest::first(VenueId::Polymarket)
                .page_size(0)
                .page_size,
            1
        );
        assert_eq!(
            PredictionEventsRequest::first(VenueId::Polymarket)
                .page_size(u16::MAX)
                .page_size,
            super::MAX_PREDICTION_PAGE_SIZE
        );
    }
}

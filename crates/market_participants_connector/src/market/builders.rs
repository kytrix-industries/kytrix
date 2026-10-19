//! Listing-scoped market builders.
//!
//! Every builder is about one exact listing. It resolves that listing inside one
//! catalog generation, validates what it is about to ask against the listing's
//! own rules and status, and only then encodes a request: the quantity unit, the
//! order-book depth and the candlestick interval are all checked against the
//! route's own descriptor and the listing's own rule set.

use std::marker::PhantomData;

use market_participants_model::{
    listing::{
        listing_impl::ListingStatus,
        route::{VenueListingKey, VenueRoute},
    },
    market::{
        book::{
            ListingCandlestick, ListingLongShortRatio, ListingOpenInterest, ListingOrderBook,
            ListingTakerVolume, ListingTrade,
        },
        selection::ListingTicker,
    },
    routing::CapabilityId,
};

use super::{
    error::{MarketError, MarketRequestRejection},
    helpers::{execute, protocol_response, subscribe},
    provider::MarketCapability,
    streams::{CandlestickStream, OrderBookStream, TickerStream, TradeStream},
    units::{ListingUnits, candlestick, order_book, ticker, trade},
};
use crate::{
    adapter::{NativeListing, ProtocolRequest, ProtocolResponse},
    catalog::resolved::ResolvedListing,
    market_participants::model::{
        Adjustment, CandlestickFetchQuery, CandlestickSubscriptionQuery, MarketTradeFetchQuery,
        MarketTradeSubscriptionQuery, OrderBookQuery, PerpetualStatisticsQuery,
        StatisticsTimeFrame, TickerQuery, TickersQuery, TimeFrame,
    },
};

/// The route and the listing one builder is about.
struct ListingScope {
    provider: MarketCapability,
    route: VenueRoute,
    listing: VenueListingKey,
}

impl ListingScope {
    /// Resolves the listing, rejecting one that is not published on the route.
    fn resolve(&self) -> Result<ResolvedListing, MarketError> {
        self.provider
            .resolve_listing_of_route(&self.route, &self.listing)
    }

    /// Resolves the listing and rejects a status that no longer reads.
    async fn resolve_readable(&self) -> Result<ResolvedListing, MarketError> {
        let resolved = self.resolve()?;
        if matches!(
            resolved.status,
            ListingStatus::Settled | ListingStatus::Delisted
        ) {
            return Err(MarketError::InvalidRequest {
                operation: "read listing market data".into(),
                reason: MarketRequestRejection::ListingNotReadable {
                    status: resolved.status,
                },
            });
        }
        Ok(resolved)
    }

    /// Validates one candlestick interval against the route's own descriptor.
    fn require_timeframe(&self, timeframe: TimeFrame) -> Result<(), MarketError> {
        let limits = self
            .provider
            .limits(&self.route, CapabilityId::Candlesticks)?;
        if limits.timeframes.contains(&timeframe) {
            return Ok(());
        }
        Err(crate::error::MarketParticipantError::UnsupportedTimeFrame {
            route: self.route.clone(),
            timeframe: smol_str::format_smolstr!("{timeframe:?}"),
        }
        .into())
    }

    /// Rejects an adjustment convention this route does not publish.
    fn require_adjustment(&self, adjustment: Adjustment) -> Result<(), MarketError> {
        let limits = self
            .provider
            .limits(&self.route, CapabilityId::Candlesticks)?;
        if limits.adjustments.contains(&adjustment) {
            return Ok(());
        }
        Err(
            crate::error::MarketParticipantError::UnsupportedAdjustment {
                route: self.route.clone(),
                adjustment: smol_str::SmolStr::new(adjustment.label()),
            }
            .into(),
        )
    }

    /// Caps one order-book depth at what the route's venue accepts.
    fn capped_depth(&self, depth: u16) -> Result<u16, MarketError> {
        let limits = self.provider.limits(&self.route, CapabilityId::OrderBook)?;
        Ok(depth.min(limits.order_book_depth_limit))
    }
}

pub struct CandlestickBuilder {
    scope: ListingScope,
    timeframe: TimeFrame,
    adjustment: Adjustment,
    limit: u16,
    start_timestamp: Option<i64>,
    end_timestamp: Option<i64>,
}

impl CandlestickBuilder {
    pub(super) fn new(
        provider: MarketCapability,
        route: VenueRoute,
        listing: VenueListingKey,
        timeframe: TimeFrame,
    ) -> Self {
        Self {
            scope: ListingScope {
                provider,
                route,
                listing,
            },
            timeframe,
            adjustment: Adjustment::None,
            limit: 500,
            start_timestamp: None,
            end_timestamp: None,
        }
    }

    /// Sets the price-adjustment convention this request asks for.
    pub fn adjustment(mut self, adjustment: Adjustment) -> Self {
        self.adjustment = adjustment;
        self
    }

    pub fn limit(mut self, limit: u16) -> Self {
        self.limit = limit;
        self
    }

    pub fn from(mut self, timestamp: i64) -> Self {
        self.start_timestamp = Some(timestamp);
        self
    }

    pub fn to(mut self, timestamp: i64) -> Self {
        self.end_timestamp = Some(timestamp);
        self
    }

    pub async fn fetch(self) -> Result<Vec<ListingCandlestick>, MarketError> {
        let resolved = self.scope.resolve_readable().await?;
        self.scope.require_timeframe(self.timeframe)?;
        self.scope.require_adjustment(self.adjustment)?;
        let query = CandlestickFetchQuery {
            listing: resolved.key.clone(),
            timeframe: self.timeframe,
            adjustment: self.adjustment,
            start_timestamp: self.start_timestamp,
            end_timestamp: self.end_timestamp,
            limit: self.limit,
        };
        query
            .validate()
            .map_err(|error| MarketError::Configuration(smol_str::format_smolstr!("{error}")))?;
        let units = ListingUnits::of(&resolved.rules);
        match execute(
            &self.scope.provider,
            resolved.key.route.clone(),
            ProtocolRequest::FetchCandlesticks(query),
        )
        .await?
        {
            ProtocolResponse::Candlesticks(values) => values
                .iter()
                .map(|value| candlestick(&value.listing, &value.data, &units))
                .collect::<Result<Vec<_>, _>>()
                .map_err(MarketError::from),
            _ => Err(protocol_response("fetch candlesticks")),
        }
    }

    pub async fn subscribe(self) -> Result<CandlestickStream, MarketError> {
        let resolved = self.scope.resolve_readable().await?;
        self.scope.require_timeframe(self.timeframe)?;
        self.scope.require_adjustment(self.adjustment)?;
        let query = CandlestickSubscriptionQuery {
            listing: resolved.key.clone(),
            timeframe: self.timeframe,
            adjustment: self.adjustment,
        };
        query
            .validate()
            .map_err(|error| MarketError::Configuration(smol_str::format_smolstr!("{error}")))?;
        let route = resolved.key.route.clone();
        let handle = subscribe(
            &self.scope.provider,
            route,
            ProtocolRequest::SubscribeCandlesticks(query),
        )
        .await?;
        Ok(CandlestickStream::from_handle(
            handle,
            ListingUnits::of(&resolved.rules),
        ))
    }

    pub async fn subscribe_with_history(
        self,
        history_limit: u16,
    ) -> Result<CandlestickStreamWithHistory, MarketError> {
        let mut history_builder = self.clone();
        history_builder.limit = history_limit;
        let history = history_builder.fetch().await?;
        let stream = self.subscribe().await?;
        Ok(CandlestickStreamWithHistory { history, stream })
    }
}

impl Clone for CandlestickBuilder {
    fn clone(&self) -> Self {
        Self {
            scope: ListingScope {
                provider: self.scope.provider.clone(),
                route: self.scope.route.clone(),
                listing: self.scope.listing.clone(),
            },
            timeframe: self.timeframe,
            adjustment: self.adjustment,
            limit: self.limit,
            start_timestamp: self.start_timestamp,
            end_timestamp: self.end_timestamp,
        }
    }
}

pub struct CandlestickStreamWithHistory {
    pub history: Vec<ListingCandlestick>,
    pub stream: CandlestickStream,
}

pub struct TickerBuilder {
    scope: ListingScope,
}

impl TickerBuilder {
    pub(super) fn new(
        provider: MarketCapability,
        route: VenueRoute,
        listing: VenueListingKey,
    ) -> Self {
        Self {
            scope: ListingScope {
                provider,
                route,
                listing,
            },
        }
    }

    pub async fn fetch(self) -> Result<ListingTicker, MarketError> {
        let resolved = self.scope.resolve()?;
        let query = TickerQuery {
            listing: resolved.key.clone(),
        };
        query
            .validate()
            .map_err(|error| MarketError::Configuration(smol_str::format_smolstr!("{error}")))?;
        let units = ListingUnits::of(&resolved.rules);
        match execute(
            &self.scope.provider,
            resolved.key.route.clone(),
            ProtocolRequest::FetchTicker(query),
        )
        .await?
        {
            ProtocolResponse::Ticker(value) => {
                ticker(&value.listing, &value.data, &units).map_err(MarketError::from)
            }
            _ => Err(protocol_response("fetch ticker")),
        }
    }

    pub async fn subscribe(self) -> Result<TickerStream, MarketError> {
        let resolved = self.scope.resolve()?;
        let query = TickerQuery {
            listing: resolved.key.clone(),
        };
        let route = resolved.key.route.clone();
        let handle = subscribe(
            &self.scope.provider,
            route,
            ProtocolRequest::SubscribeTicker(query),
        )
        .await?;
        Ok(TickerStream::from_handle(
            handle,
            ListingUnits::of(&resolved.rules),
        ))
    }
}

pub struct AllTickersBuilder {
    provider: MarketCapability,
    route: VenueRoute,
}

impl AllTickersBuilder {
    pub(super) fn new(provider: MarketCapability, route: VenueRoute) -> Self {
        Self { provider, route }
    }

    /// Fetches every ticker the route publishes.
    ///
    /// A venue keeps reporting rolling statistics for listings its catalog no
    /// longer publishes. Such a ticker names no listing the published snapshot
    /// holds, so it is left out rather than failing the whole answer.
    pub async fn fetch(self) -> Result<Vec<ListingTicker>, MarketError> {
        let ensured = self.provider.ensure(&self.route)?;
        let response = execute(
            &self.provider,
            self.route.clone(),
            ProtocolRequest::FetchAllTickers(TickersQuery {
                route: self.route.clone(),
            }),
        )
        .await?;
        let values = match response {
            ProtocolResponse::Tickers(values) => values,
            _ => return Err(protocol_response("fetch all tickers")),
        };
        let mut tickers = Vec::with_capacity(values.len());
        for value in values {
            let Ok(resolved) = ensured.resolve_listing(&value.listing) else {
                continue;
            };
            let units = ListingUnits::of(&resolved.rules);
            tickers.push(ticker(&value.listing, &value.data, &units).map_err(MarketError::from)?);
        }
        Ok(tickers)
    }
}

pub struct OrderBookBuilder {
    scope: ListingScope,
    depth: u16,
}

impl OrderBookBuilder {
    pub(super) fn new(
        provider: MarketCapability,
        route: VenueRoute,
        listing: VenueListingKey,
    ) -> Self {
        Self {
            scope: ListingScope {
                provider,
                route,
                listing,
            },
            depth: 20,
        }
    }

    pub fn depth(mut self, depth: u16) -> Self {
        self.depth = depth;
        self
    }

    pub async fn fetch(self) -> Result<ListingOrderBook, MarketError> {
        let resolved = self.scope.resolve()?;
        let units = ListingUnits::of(&resolved.rules);
        let query = OrderBookQuery {
            listing: resolved.key.clone(),
            depth: self.scope.capped_depth(self.depth)?,
        };
        query
            .validate()
            .map_err(|error| MarketError::Configuration(smol_str::format_smolstr!("{error}")))?;
        match execute(
            &self.scope.provider,
            resolved.key.route.clone(),
            ProtocolRequest::FetchOrderBook(query),
        )
        .await?
        {
            ProtocolResponse::OrderBook(value) => {
                order_book(&value.listing, &value.data, &units).map_err(MarketError::from)
            }
            _ => Err(protocol_response("fetch order book")),
        }
    }

    pub async fn subscribe(self) -> Result<OrderBookStream, MarketError> {
        let resolved = self.scope.resolve()?;
        let query = OrderBookQuery {
            listing: resolved.key.clone(),
            depth: self.scope.capped_depth(self.depth)?,
        };
        let route = resolved.key.route.clone();
        let handle = subscribe(
            &self.scope.provider,
            route,
            ProtocolRequest::SubscribeOrderBook(query),
        )
        .await?;
        Ok(OrderBookStream::from_handle(
            handle,
            ListingUnits::of(&resolved.rules),
        ))
    }
}

pub struct TradeBuilder {
    scope: ListingScope,
    limit: u16,
}

impl TradeBuilder {
    pub(super) fn new(
        provider: MarketCapability,
        route: VenueRoute,
        listing: VenueListingKey,
    ) -> Self {
        Self {
            scope: ListingScope {
                provider,
                route,
                listing,
            },
            limit: 500,
        }
    }

    pub fn limit(mut self, limit: u16) -> Self {
        self.limit = limit;
        self
    }

    pub async fn fetch(self) -> Result<Vec<ListingTrade>, MarketError> {
        let resolved = self.scope.resolve()?;
        let query = MarketTradeFetchQuery {
            listing: resolved.key.clone(),
            start_timestamp: None,
            end_timestamp: None,
            limit: self.limit,
        };
        query
            .validate()
            .map_err(|error| MarketError::Configuration(smol_str::format_smolstr!("{error}")))?;
        let units = ListingUnits::of(&resolved.rules);
        match execute(
            &self.scope.provider,
            resolved.key.route.clone(),
            ProtocolRequest::FetchTrades(query),
        )
        .await?
        {
            ProtocolResponse::Trades(values) => values
                .iter()
                .map(|value| trade(&value.listing, &value.data, &units))
                .collect::<Result<Vec<_>, _>>()
                .map_err(MarketError::from),
            _ => Err(protocol_response("fetch trades")),
        }
    }

    pub async fn subscribe(self) -> Result<TradeStream, MarketError> {
        let resolved = self.scope.resolve()?;
        let query = MarketTradeSubscriptionQuery {
            listing: resolved.key.clone(),
        };
        let route = resolved.key.route.clone();
        let handle = subscribe(
            &self.scope.provider,
            route,
            ProtocolRequest::SubscribeTrades(query),
        )
        .await?;
        Ok(TradeStream::from_handle(
            handle,
            ListingUnits::of(&resolved.rules),
        ))
    }
}

pub struct AnalyticsSelector {
    provider: MarketCapability,
    route: VenueRoute,
}

impl AnalyticsSelector {
    pub(super) fn new(provider: MarketCapability, route: VenueRoute) -> Self {
        Self { provider, route }
    }

    pub fn long_short_ratio(
        &self,
        listing: VenueListingKey,
    ) -> AnalyticsBuilder<LongShortRatioKind> {
        AnalyticsBuilder::<LongShortRatioKind>::new(
            self.provider.clone(),
            self.route.clone(),
            listing,
        )
    }

    pub fn top_trader_account_ratio(
        &self,
        listing: VenueListingKey,
    ) -> AnalyticsBuilder<TopTraderAccountRatioKind> {
        AnalyticsBuilder::<TopTraderAccountRatioKind>::new(
            self.provider.clone(),
            self.route.clone(),
            listing,
        )
    }

    pub fn top_trader_position_ratio(
        &self,
        listing: VenueListingKey,
    ) -> AnalyticsBuilder<TopTraderPositionRatioKind> {
        AnalyticsBuilder::<TopTraderPositionRatioKind>::new(
            self.provider.clone(),
            self.route.clone(),
            listing,
        )
    }

    pub fn taker_volume(&self, listing: VenueListingKey) -> AnalyticsBuilder<TakerVolumeKind> {
        AnalyticsBuilder::<TakerVolumeKind>::new(self.provider.clone(), self.route.clone(), listing)
    }

    pub fn open_interest(&self, listing: VenueListingKey) -> AnalyticsBuilder<OpenInterestKind> {
        AnalyticsBuilder::<OpenInterestKind>::new(
            self.provider.clone(),
            self.route.clone(),
            listing,
        )
    }
}

pub struct LongShortRatioKind;
pub struct TopTraderAccountRatioKind;
pub struct TopTraderPositionRatioKind;
pub struct TakerVolumeKind;
pub struct OpenInterestKind;

pub struct AnalyticsBuilder<K> {
    scope: ListingScope,
    timeframe: StatisticsTimeFrame,
    limit: u16,
    marker: PhantomData<K>,
}

impl<K> AnalyticsBuilder<K> {
    fn new(provider: MarketCapability, route: VenueRoute, listing: VenueListingKey) -> Self {
        Self {
            scope: ListingScope {
                provider,
                route,
                listing,
            },
            timeframe: StatisticsTimeFrame::Minute5,
            limit: 100,
            marker: PhantomData,
        }
    }

    pub fn timeframe(mut self, timeframe: impl Into<StatisticsTimeFrame>) -> Self {
        self.timeframe = timeframe.into();
        self
    }

    pub fn limit(mut self, limit: u16) -> Self {
        self.limit = limit;
        self
    }

    async fn query(&self) -> Result<(ResolvedListing, PerpetualStatisticsQuery), MarketError> {
        let resolved = self.scope.resolve()?;
        let query = PerpetualStatisticsQuery {
            listing: resolved.key.clone(),
            timeframe: self.timeframe,
            start_timestamp: None,
            end_timestamp: None,
            limit: self.limit,
        };
        // The product family, the window and the limit are validated as one typed
        // answer: a listing whose product this operation does not serve is a
        // `ModelError::UnsupportedInstrumentKind`, not a paraphrased string, so a
        // caller can tell a product rejection from a malformed request.
        query
            .validate_for(resolved.instrument.kind())
            .map_err(MarketError::from)?;
        Ok((resolved, query))
    }
}

/// Maps one ratio answer, whose subjects the adapter already resolved.
fn ratios(
    values: Vec<NativeListing<crate::adapter::NativeLongShortRatio>>,
) -> Vec<ListingLongShortRatio> {
    values
        .into_iter()
        .map(|value| ListingLongShortRatio {
            listing: value.listing,
            ratio: value.data.ratio,
            timestamp: value.data.timestamp,
        })
        .collect()
}

impl AnalyticsBuilder<LongShortRatioKind> {
    pub async fn fetch(self) -> Result<Vec<ListingLongShortRatio>, MarketError> {
        let (resolved, query) = self.query().await?;
        match execute(
            &self.scope.provider,
            resolved.key.route.clone(),
            ProtocolRequest::FetchLongShortRatio(query),
        )
        .await?
        {
            ProtocolResponse::LongShortRatios(values) => Ok(ratios(values)),
            _ => Err(protocol_response("fetch long-short ratio")),
        }
    }
}

impl AnalyticsBuilder<TopTraderAccountRatioKind> {
    pub async fn fetch(self) -> Result<Vec<ListingLongShortRatio>, MarketError> {
        let (resolved, query) = self.query().await?;
        match execute(
            &self.scope.provider,
            resolved.key.route.clone(),
            ProtocolRequest::FetchTopTraderAccountRatio(query),
        )
        .await?
        {
            ProtocolResponse::LongShortRatios(values) => Ok(ratios(values)),
            _ => Err(protocol_response("fetch top-trader account ratio")),
        }
    }
}

impl AnalyticsBuilder<TopTraderPositionRatioKind> {
    pub async fn fetch(self) -> Result<Vec<ListingLongShortRatio>, MarketError> {
        let (resolved, query) = self.query().await?;
        match execute(
            &self.scope.provider,
            resolved.key.route.clone(),
            ProtocolRequest::FetchTopTraderPositionRatio(query),
        )
        .await?
        {
            ProtocolResponse::LongShortRatios(values) => Ok(ratios(values)),
            _ => Err(protocol_response("fetch top-trader position ratio")),
        }
    }
}

impl AnalyticsBuilder<TakerVolumeKind> {
    pub async fn fetch(self) -> Result<Vec<ListingTakerVolume>, MarketError> {
        let (resolved, query) = self.query().await?;
        let units = ListingUnits::of(&resolved.rules);
        match execute(
            &self.scope.provider,
            resolved.key.route.clone(),
            ProtocolRequest::FetchTakerVolume(query),
        )
        .await?
        {
            ProtocolResponse::TakerVolumes(values) => values
                .iter()
                .map(|value| super::units::taker_volume(&value.listing, &value.data, &units))
                .collect::<Result<Vec<_>, _>>()
                .map_err(MarketError::from),
            _ => Err(protocol_response("fetch taker volume")),
        }
    }
}

impl AnalyticsBuilder<OpenInterestKind> {
    pub async fn fetch(self) -> Result<Vec<ListingOpenInterest>, MarketError> {
        let (resolved, query) = self.query().await?;
        let units = ListingUnits::of(&resolved.rules);
        match execute(
            &self.scope.provider,
            resolved.key.route.clone(),
            ProtocolRequest::FetchOpenInterest(query),
        )
        .await?
        {
            ProtocolResponse::OpenInterest(values) => values
                .iter()
                .map(|value| {
                    Ok(ListingOpenInterest {
                        listing: value.listing.clone(),
                        quantity: units.traded(value.data.quantity).map_err(|error| {
                            crate::error::MarketParticipantError::MarketModel(Box::new(error))
                        })?,
                        notional: units
                            .optional_amount(value.data.notional)
                            .map_err(|error| {
                                crate::error::MarketParticipantError::MarketModel(Box::new(error))
                            })?,
                        timestamp: value.data.timestamp,
                    })
                })
                .collect::<Result<Vec<_>, crate::error::MarketParticipantError>>()
                .map_err(MarketError::from),
            _ => Err(protocol_response("fetch open interest")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use app_config_provider::notification::ConfigNotification;
    use market_participants_model::{
        error::ModelError, instrument::InstrumentKind, routing::CapabilityId,
    };
    use network::transport::hub::{TransportHub, TransportPolicy};
    use widget_tokio_runtime::runtime::Runtime;

    use super::MarketError;
    use crate::{
        catalog::{
            source::{
                CatalogRouteRegistration, CatalogRouteUpdate, CatalogSourceBinding,
                CatalogSourceId, RouteInputRevision,
            },
            test_support,
        },
        connector::{MarketParticipantsConnector, MarketParticipantsConnectorInput},
        error::MarketParticipantError,
        market::{provider::MarketCapability, timeframe::Adjustment},
        market_participants::model::TimeFrame,
        runtime::MarketParticipantNetworkSnapshot,
    };

    /// A market capability whose spot route publishes one fixture listing.
    async fn capability() -> MarketCapability {
        let snapshot = Arc::new(MarketParticipantNetworkSnapshot::default());
        let notification = ConfigNotification::new(snapshot);
        let catalog = Arc::new(crate::catalog::store::Catalog::new());
        let connector = MarketParticipantsConnector::new_with_catalog(
            MarketParticipantsConnectorInput::new(
                Arc::new(TransportHub::new(TransportPolicy::default())),
                notification,
            )
            .with_runtime(Runtime::for_test()),
            Arc::clone(&catalog),
        )
        .expect("the root builds from typed runtime inputs");
        connector
            .load_fixture(
                &crate::test_support::definitions(&[test_support::spot_route().venue]),
                &[],
                &[],
            )
            .expect("the Binance routes register");
        // The route's catalog is rebound to a fixture source, so the listing
        // resolves through the real connector while the venue behind it is the
        // fixture.
        let route = test_support::spot_route();
        let source_id =
            CatalogSourceId::new("builder-fixture").expect("the fixture source id is canonical");
        let rules = test_support::case_insensitive_symbols();
        catalog
            .commit_fixture_routes(
                CatalogRouteUpdate::new(
                    [CatalogSourceBinding::new(
                        source_id.clone(),
                        test_support::listing(vec![test_support::entry(
                            &route,
                            test_support::spot_token("binance", "BTC", "USDT"),
                            "BTCUSDT",
                        )]),
                    )],
                    [CatalogRouteRegistration::catalog(
                        route.clone(),
                        source_id.clone(),
                        rules.clone(),
                        vec![CapabilityId::MarketCatalog],
                        RouteInputRevision::of_declaration(&route, Some(&source_id), &rules),
                    )],
                ),
                &test_support::requirements(std::slice::from_ref(&route)),
            )
            .expect("the fixture catalog route publishes");
        // The fixture loads the route the way a load does: the registration is
        // followed by a load of the route's catalog.
        catalog
            .refresh_for_test(&route)
            .await
            .expect("the fixture catalog route loads");
        connector.market()
    }

    /// A perpetual-statistics request about a spot listing is a product
    /// rejection, and it stays the typed model error it is.
    #[tokio::test]
    async fn a_spot_listing_is_refused_by_the_perpetual_statistics_builder() {
        let market = capability().await;
        let route = test_support::spot_route();
        let listing = test_support::listing_key(&route, "BTCUSDT");
        let error = market
            .market(route)
            .analytics()
            .open_interest(listing)
            .fetch()
            .await
            .expect_err("a spot listing serves no perpetual statistics");

        let MarketError::Model(boxed) = error else {
            panic!("an unsupported product must stay a typed model error: {error:?}");
        };
        assert_eq!(
            *boxed,
            ModelError::UnsupportedInstrumentKind {
                kind: InstrumentKind::Spot
            }
        );
    }

    /// The adjustment survives the clone `subscribe_with_history` makes.
    ///
    /// A history page is fetched through a clone of the builder, so a hand-written
    /// `Clone` that forgets the field would silently revert the request to the model's
    /// default: the clone is read directly, and the request it goes on to make is the
    /// proof that the value reached the route's own check.
    #[tokio::test]
    async fn a_cloned_builder_should_keep_the_adjustment_it_was_asked_for() {
        let market = capability().await;
        let route = test_support::spot_route();
        let listing = test_support::listing_key(&route, "BTCUSDT");
        let builder = market
            .market(route.clone())
            .candlesticks(listing, TimeFrame::Minute1)
            .adjustment(Adjustment::Forward);

        assert_eq!(
            builder.clone().adjustment,
            Adjustment::Forward,
            "the clone a history page is fetched through carries the adjustment"
        );

        let error = match builder.subscribe_with_history(10).await {
            Ok(_) => panic!("the fixture route publishes raw candles only"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            MarketError::from(MarketParticipantError::UnsupportedAdjustment {
                route,
                adjustment: smol_str::SmolStr::new_static("forward"),
            }),
            "the history fetch asked for the adjustment the builder was given"
        );
    }
}

use crate::{
    market_participants::model::{
        AuthenticatedQuery, CancelOrderCommand, CandlestickFetchQuery,
        CandlestickSubscriptionQuery, ForceOrderSubscriptionQuery, HyperliquidUserFillsByTimeQuery,
        HyperliquidWalletQuery, MarketCatalogQuery, MarketTradeFetchQuery,
        MarketTradeSubscriptionQuery, OrderBookQuery, PerpetualStatisticsQuery, PlaceOrderCommand,
        PredictionEventQuery, PredictionEventsQuery, PredictionOrderBookQuery,
        PredictionSeriesQuery, PredictionSeriesSubscriptionQuery, SetLeverageCommand,
        SetMarginModeCommand, SetPositionModeCommand, TickerQuery, TickersQuery,
        TradingSettingsQuery,
    },
    protocol::{AuthenticatedTarget, WalletTarget},
};

#[derive(Clone, Debug)]
pub enum ProtocolRequest {
    FetchMarkets(MarketCatalogQuery),
    FetchAllTickers(TickersQuery),
    FetchPredictionEvents(PredictionEventsQuery),
    FetchPredictionEvent(PredictionEventQuery),
    FetchPredictionSeries(PredictionSeriesQuery),
    SubscribePredictionSeries(PredictionSeriesSubscriptionQuery),
    FetchPredictionOrderBook(PredictionOrderBookQuery),
    SubscribePredictionOrderBook(PredictionOrderBookQuery),
    SubscribeForceOrder(ForceOrderSubscriptionQuery),
    FetchHyperliquidSpotPairs(MarketCatalogQuery),
    SubscribeHyperliquidFastContexts(HyperliquidWalletQuery),

    FetchCandlesticks(CandlestickFetchQuery),
    SubscribeCandlesticks(CandlestickSubscriptionQuery),
    FetchTicker(TickerQuery),
    SubscribeTicker(TickerQuery),
    FetchOrderBook(OrderBookQuery),
    SubscribeOrderBook(OrderBookQuery),
    FetchTrades(MarketTradeFetchQuery),
    SubscribeTrades(MarketTradeSubscriptionQuery),
    FetchOpenInterest(PerpetualStatisticsQuery),
    FetchLongShortRatio(PerpetualStatisticsQuery),
    FetchTopTraderAccountRatio(PerpetualStatisticsQuery),
    FetchTopTraderPositionRatio(PerpetualStatisticsQuery),
    FetchTakerVolume(PerpetualStatisticsQuery),

    FetchTradingSettings(TradingSettingsQuery),
    FetchBalances(AuthenticatedQuery),
    FetchActiveOrders(AuthenticatedQuery),
    FetchOrderHistory(AuthenticatedQuery),
    FetchPositions(AuthenticatedQuery),
    FetchAccountTrades(AuthenticatedQuery),
    FetchPositionHistory(AuthenticatedQuery),
    SubscribeOrders(AuthenticatedQuery),
    SubscribePositions(AuthenticatedQuery),
    PlaceOrder(PlaceOrderCommand),
    CancelOrder(CancelOrderCommand),
    SetPositionMode(SetPositionModeCommand),
    SetMarginMode(SetMarginModeCommand),
    SetLeverage(SetLeverageCommand),

    FetchHyperliquidSpotState(HyperliquidWalletQuery),
    FetchHyperliquidPerpetualState(HyperliquidWalletQuery),
    FetchHyperliquidOpenOrders(HyperliquidWalletQuery),
    FetchHyperliquidHistoricalOrders(HyperliquidWalletQuery),
    FetchHyperliquidUserFills {
        query: HyperliquidWalletQuery,
        aggregate_by_time: bool,
    },
    FetchHyperliquidUserFillsByTime(HyperliquidUserFillsByTimeQuery),
    FetchHyperliquidUserLedger {
        query: HyperliquidWalletQuery,
        start_time: market_participants_model::decimal::UnixMillis,
    },
    SubscribeHyperliquidUserFills(HyperliquidWalletQuery),
    SubscribeHyperliquidUserLedger(HyperliquidWalletQuery),
}

impl ProtocolRequest {
    pub fn target(&self) -> Option<&AuthenticatedTarget> {
        match self {
            Self::FetchTradingSettings(query) => Some(&query.target),
            Self::PlaceOrder(command) => Some(&command.target),
            Self::CancelOrder(command) => Some(&command.target),
            Self::SetPositionMode(command) => Some(&command.target),
            Self::SetMarginMode(command) => Some(&command.target),
            Self::SetLeverage(command) => Some(&command.target),
            Self::FetchBalances(query)
            | Self::FetchActiveOrders(query)
            | Self::FetchOrderHistory(query)
            | Self::FetchPositions(query)
            | Self::FetchAccountTrades(query)
            | Self::FetchPositionHistory(query)
            | Self::SubscribeOrders(query)
            | Self::SubscribePositions(query) => Some(&query.target),
            Self::FetchMarkets(_)
            | Self::FetchAllTickers(_)
            | Self::FetchPredictionEvents(_)
            | Self::FetchPredictionEvent(_)
            | Self::FetchPredictionSeries(_)
            | Self::SubscribePredictionSeries(_)
            | Self::FetchPredictionOrderBook(_)
            | Self::SubscribePredictionOrderBook(_)
            | Self::SubscribeForceOrder(_)
            | Self::FetchCandlesticks(_)
            | Self::SubscribeCandlesticks(_)
            | Self::FetchTicker(_)
            | Self::SubscribeTicker(_)
            | Self::FetchOrderBook(_)
            | Self::SubscribeOrderBook(_)
            | Self::FetchTrades(_)
            | Self::SubscribeTrades(_)
            | Self::FetchOpenInterest(_)
            | Self::FetchLongShortRatio(_)
            | Self::FetchTopTraderAccountRatio(_)
            | Self::FetchTopTraderPositionRatio(_)
            | Self::FetchTakerVolume(_)
            | Self::FetchHyperliquidSpotPairs(_)
            | Self::SubscribeHyperliquidFastContexts(_)
            | Self::FetchHyperliquidSpotState(_)
            | Self::FetchHyperliquidPerpetualState(_)
            | Self::FetchHyperliquidOpenOrders(_)
            | Self::FetchHyperliquidHistoricalOrders(_)
            | Self::FetchHyperliquidUserFills { .. }
            | Self::FetchHyperliquidUserFillsByTime(_)
            | Self::FetchHyperliquidUserLedger { .. }
            | Self::SubscribeHyperliquidUserFills(_)
            | Self::SubscribeHyperliquidUserLedger(_) => None,
        }
    }

    pub fn wallet_target(&self) -> Option<&WalletTarget> {
        match self {
            Self::FetchHyperliquidSpotState(query)
            | Self::FetchHyperliquidPerpetualState(query)
            | Self::FetchHyperliquidOpenOrders(query)
            | Self::FetchHyperliquidHistoricalOrders(query)
            | Self::FetchHyperliquidUserFills { query, .. }
            | Self::FetchHyperliquidUserLedger { query, .. }
            | Self::SubscribeHyperliquidUserFills(query)
            | Self::SubscribeHyperliquidUserLedger(query) => Some(&query.target),
            Self::FetchHyperliquidUserFillsByTime(query) => Some(&query.target),
            Self::SubscribeHyperliquidFastContexts(query) => Some(&query.target),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PreparedRequest {
    request: ProtocolRequest,
    listing: Option<crate::catalog::resolved::ResolvedListingRef>,
}

impl PreparedRequest {
    pub(crate) fn new(
        request: ProtocolRequest,
        listing: Option<crate::catalog::resolved::ResolvedListingRef>,
    ) -> Self {
        Self { request, listing }
    }

    pub const fn request(&self) -> &ProtocolRequest {
        &self.request
    }

    pub const fn listing(&self) -> Option<&crate::catalog::resolved::ResolvedListingRef> {
        self.listing.as_ref()
    }

    pub fn into_parts(
        self,
    ) -> (
        ProtocolRequest,
        Option<crate::catalog::resolved::ResolvedListingRef>,
    ) {
        (self.request, self.listing)
    }
}

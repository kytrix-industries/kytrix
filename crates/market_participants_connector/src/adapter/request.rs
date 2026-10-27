//! The protocol request vocabulary of the adapter boundary.
//!
//! Every variant names the identity it addresses and nothing more: a
//! listing-scoped request carries the exact `VenueListingKey` it is about, a
//! route-scoped request carries its `VenueRoute`, and an authenticated request
//! carries its `AuthenticatedTarget`. The gateway resolves the listing into the
//! [`PreparedRequest`] an adapter encodes with, so an adapter never reads a
//! catalog and never keeps a second directory.

use crate::{
    market_participants::model::{
        AuthenticatedQuery, CancelOrderCommand, CandlestickFetchQuery,
        CandlestickSubscriptionQuery, ForceOrderSubscriptionQuery, HyperliquidUserFillsByTimeQuery,
        HyperliquidWalletQuery, MarketCatalogQuery, MarketTradeFetchQuery,
        MarketTradeSubscriptionQuery, OrderBookQuery, PerpetualStatisticsQuery, PlaceOrderCommand,
        SetLeverageCommand, SetMarginModeCommand, SetPositionModeCommand, TickerQuery,
        TickersQuery, TradingSettingsQuery,
    },
    protocol::{AuthenticatedTarget, WalletTarget},
};

/// One protocol request.
#[derive(Clone, Debug)]
pub enum ProtocolRequest {
    // ---- route-scoped -------------------------------------------------
    FetchMarkets(MarketCatalogQuery),
    FetchAllTickers(TickersQuery),
    SubscribeForceOrders(ForceOrderSubscriptionQuery),
    FetchHyperliquidSpotPairs(MarketCatalogQuery),
    SubscribeHyperliquidFastContexts(HyperliquidWalletQuery),

    // ---- listing-scoped -----------------------------------------------
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

    // ---- authenticated account ----------------------------------------
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

    // ---- Hyperliquid wallet monitor -----------------------------------
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
    /// Returns the authenticated target this request acts for, when it acts for one.
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
            | Self::SubscribeForceOrders(_)
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
            // Every wallet-monitor request is a public address read or a route-level
            // stream, so none of them authenticates.
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

    /// Returns the wallet-monitor target this request reads, when it reads one.
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

/// One protocol request together with the listing the gateway resolved for it.
///
/// This is what an adapter receives: the request names the listing key, and the
/// reference is that key resolved inside one catalog generation. An adapter that
/// needs the venue's own identity reads it from the reference and never invents
/// one, and a reference is `None` exactly when the request addresses no listing
/// at all.
#[derive(Clone, Debug)]
pub struct PreparedRequest {
    request: ProtocolRequest,
    listing: Option<crate::catalog::resolved::ResolvedListingRef>,
}

impl PreparedRequest {
    /// Pairs one request with the listing the gateway resolved for it.
    pub(crate) fn new(
        request: ProtocolRequest,
        listing: Option<crate::catalog::resolved::ResolvedListingRef>,
    ) -> Self {
        Self { request, listing }
    }

    /// Returns the request being made.
    pub const fn request(&self) -> &ProtocolRequest {
        &self.request
    }

    /// Returns the resolved listing of this request, when it is about one.
    pub const fn listing(&self) -> Option<&crate::catalog::resolved::ResolvedListingRef> {
        self.listing.as_ref()
    }

    /// Consumes this prepared request and returns its parts.
    pub fn into_parts(
        self,
    ) -> (
        ProtocolRequest,
        Option<crate::catalog::resolved::ResolvedListingRef>,
    ) {
        (self.request, self.listing)
    }
}

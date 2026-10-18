//! The Hyperliquid `MarketParticipantAdapter` implementation.
//!
//! `venue` is the static attribution this adapter is registered under, and
//! `read_venue` is the venue-level conversation a load opens before anything of the
//! venue is registered. Which scopes exist and what each of them serves is the
//! venue's loader's answer, not this adapter's.

use market_participants_model::{identifier::VenueId, listing::route::VenueRoute};

use super::implementation::{
    BoxFuture, ExecutionContext, HyperliquidAdapter, MarketParticipantAdapter,
    MarketParticipantError, ORDER_BOOK_DEPTH_LIMIT, PreparedRequest, ProtocolRequest,
    ProtocolResponse, SubscriptionHandle, TIMEFRAMES, VenueExecutionContext, WireListing,
    listing_key, model_error, unsupported, wallet_dex, wallet_user,
};
use crate::market::timeframe::Adjustment;

impl MarketParticipantAdapter for HyperliquidAdapter {
    fn venue(&self) -> &VenueId {
        self.venue_id()
    }

    /// Reads what the venue publishes, through the venue's own configuration.
    ///
    /// A load of this venue asks what it deploys before anything of it is
    /// registered, so the read addresses the venue rather than a scope of it: it is
    /// dialed on the origin the venue's own settings resolve and carries no request
    /// identity.
    fn read_venue(
        &self,
        context: VenueExecutionContext,
    ) -> BoxFuture<'static, Result<crate::venue::metadata::VenueReading, MarketParticipantError>>
    {
        let adapter = self.clone();
        Box::pin(async move { HyperliquidAdapter::read_venue(&adapter, &context).await })
    }

    /// Answers the venue's own bounds for one route.
    ///
    /// Hyperliquid publishes a twenty-level order book and its own candle set for
    /// every scope it serves — one it publishes raw prices on — so every scope is
    /// answered from the venue's values rather than from a venue-wide guess.
    fn limits(&self, route: &VenueRoute) -> crate::adapter::AdapterLimits {
        let _ = route;
        crate::adapter::AdapterLimits {
            order_book_depth_limit: ORDER_BOOK_DEPTH_LIMIT,
            timeframes: TIMEFRAMES.to_vec(),
            adjustments: vec![Adjustment::None],
        }
    }

    fn execute(
        &self,
        context: ExecutionContext,
        prepared: PreparedRequest,
    ) -> BoxFuture<'static, Result<ProtocolResponse, MarketParticipantError>> {
        let adapter = self.clone();
        Box::pin(async move {
            let request = prepared.request().clone();
            match &request {
                ProtocolRequest::FetchMarkets(query) => Ok(ProtocolResponse::Markets(
                    adapter.fetch_markets(&context, &query.route)?,
                )),
                ProtocolRequest::FetchHyperliquidSpotPairs(query) => {
                    Ok(ProtocolResponse::HyperliquidSpotPairs(
                        adapter.spot_pairs(&context, &query.route)?,
                    ))
                }
                ProtocolRequest::FetchCandlesticks(query) => {
                    query.validate().map_err(model_error)?;
                    let listing = listing_key(&prepared)?;
                    let wire = WireListing::of(&prepared, &context)?;
                    let candles = adapter
                        .candles(
                            &context,
                            &request,
                            wire.coin(),
                            query.route(),
                            query.timeframe,
                            query.start_timestamp,
                            query.end_timestamp,
                            query.limit,
                        )
                        .await?;
                    Ok(ProtocolResponse::Candlesticks(
                        candles
                            .into_iter()
                            .map(|data| crate::adapter::NativeListing::new(listing.clone(), data))
                            .collect(),
                    ))
                }
                ProtocolRequest::FetchTicker(query) => {
                    query.validate().map_err(model_error)?;
                    let listing = listing_key(&prepared)?;
                    let wire = WireListing::of(&prepared, &context)?;
                    let ticker = adapter.ticker(&context, query.route(), &wire.coin())?;
                    Ok(ProtocolResponse::Ticker(
                        crate::adapter::NativeListing::new(listing, ticker),
                    ))
                }
                ProtocolRequest::FetchAllTickers(query) => {
                    let tickers = adapter.all_tickers(&context, &query.route)?;
                    Ok(ProtocolResponse::Tickers(
                        tickers
                            .into_iter()
                            .map(|(listing, data)| {
                                crate::adapter::NativeListing::new(listing, data)
                            })
                            .collect(),
                    ))
                }
                ProtocolRequest::FetchOrderBook(query) => {
                    query.validate().map_err(model_error)?;
                    if query.depth > ORDER_BOOK_DEPTH_LIMIT {
                        return Err(MarketParticipantError::InvalidLimit {
                            operation: "order book".into(),
                            requested: query.depth,
                            maximum: ORDER_BOOK_DEPTH_LIMIT,
                        });
                    }
                    let listing = listing_key(&prepared)?;
                    let wire = WireListing::of(&prepared, &context)?;
                    let book = adapter
                        .order_book(&context, &request, wire.coin(), query.depth)
                        .await?;
                    Ok(ProtocolResponse::OrderBook(
                        crate::adapter::NativeListing::new(listing, book),
                    ))
                }
                ProtocolRequest::FetchTrades(query) => {
                    query.validate().map_err(model_error)?;
                    let listing = listing_key(&prepared)?;
                    let wire = WireListing::of(&prepared, &context)?;
                    let trades = adapter
                        .trades(&context, &request, wire.coin(), query.limit)
                        .await?;
                    Ok(ProtocolResponse::Trades(
                        trades
                            .into_iter()
                            .map(|data| crate::adapter::NativeListing::new(listing.clone(), data))
                            .collect(),
                    ))
                }
                ProtocolRequest::PlaceOrder(command) => Ok(ProtocolResponse::OrderPlaced(
                    adapter
                        .place_order(&context, &prepared, &request, command.clone())
                        .await?,
                )),
                ProtocolRequest::CancelOrder(command) => Ok(ProtocolResponse::OrderCancelled(
                    adapter
                        .cancel_order(&context, &prepared, &request, command.clone())
                        .await?,
                )),
                ProtocolRequest::FetchBalances(query) => {
                    query.validate().map_err(model_error)?;
                    Ok(ProtocolResponse::Balances(
                        adapter.balances(&context, &request, query).await?,
                    ))
                }
                ProtocolRequest::FetchPositions(query) => {
                    query.validate().map_err(model_error)?;
                    Ok(ProtocolResponse::Positions(
                        adapter.positions(&context, &request, query).await?,
                    ))
                }
                ProtocolRequest::FetchActiveOrders(query) => {
                    query.validate().map_err(model_error)?;
                    Ok(ProtocolResponse::Orders(
                        adapter.orders(&context, &request, query, false).await?,
                    ))
                }
                ProtocolRequest::FetchOrderHistory(query) => {
                    query.validate().map_err(model_error)?;
                    Ok(ProtocolResponse::Orders(
                        adapter.orders(&context, &request, query, true).await?,
                    ))
                }
                ProtocolRequest::FetchAccountTrades(query) => {
                    query.validate().map_err(model_error)?;
                    Ok(ProtocolResponse::AccountTrades(
                        adapter.account_trades(&context, &request, query).await?,
                    ))
                }
                ProtocolRequest::FetchHyperliquidSpotState(query) => {
                    let user = wallet_user(query.wallet_target(), &context)?;
                    Ok(ProtocolResponse::HyperliquidSpotState(
                        adapter.wallet_spot_state(&context, &request, user).await?,
                    ))
                }
                ProtocolRequest::FetchHyperliquidPerpetualState(query) => {
                    let user = wallet_user(query.wallet_target(), &context)?;
                    let dex = wallet_dex(query.wallet_target(), query.route())?;
                    Ok(ProtocolResponse::HyperliquidPerpetualState(
                        adapter
                            .wallet_perpetual_state(&context, &request, user, query.route(), dex)
                            .await?,
                    ))
                }
                ProtocolRequest::FetchHyperliquidOpenOrders(query) => {
                    adapter
                        .wallet_orders(&context, &request, query, false)
                        .await
                }
                ProtocolRequest::FetchHyperliquidHistoricalOrders(query) => {
                    adapter.wallet_orders(&context, &request, query, true).await
                }
                ProtocolRequest::FetchHyperliquidUserFills {
                    query,
                    aggregate_by_time,
                } => Ok(ProtocolResponse::HyperliquidUserFills(
                    adapter
                        .wallet_fills(
                            &context,
                            &request,
                            query.wallet_target(),
                            query.decoding_routes(),
                            *aggregate_by_time,
                            None,
                        )
                        .await?,
                )),
                ProtocolRequest::FetchHyperliquidUserFillsByTime(query) => {
                    query.validate().map_err(model_error)?;
                    Ok(ProtocolResponse::HyperliquidUserFills(
                        adapter
                            .wallet_fills(
                                &context,
                                &request,
                                query.wallet_target(),
                                query.decoding_routes(),
                                true,
                                Some((query.start_timestamp, query.end_timestamp)),
                            )
                            .await?,
                    ))
                }
                ProtocolRequest::FetchHyperliquidUserLedger { query, start_time } => {
                    Ok(ProtocolResponse::HyperliquidLedger(
                        adapter
                            .wallet_ledger(
                                &context,
                                &request,
                                query.wallet_target(),
                                query.decoding_routes(),
                                *start_time,
                            )
                            .await?,
                    ))
                }
                _ => Err(unsupported(&context)),
            }
        })
    }

    fn subscribe(
        &self,
        context: ExecutionContext,
        prepared: PreparedRequest,
    ) -> BoxFuture<'static, Result<SubscriptionHandle, MarketParticipantError>> {
        let adapter = self.clone();
        Box::pin(async move { adapter.subscribe_request(context, prepared).await })
    }
}

#[cfg(test)]
mod tests {
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
    };

    use super::{HyperliquidAdapter, MarketParticipantAdapter};
    use crate::{
        market_participants::model::TimeFrame,
        routing::{CORE_PERP_SCOPE, CORE_SPOT_SCOPE},
    };

    fn route(venue: &str, scope: &str) -> VenueRoute {
        VenueRoute::new(
            VenueId::from_name(venue).expect("the fixture venue is canonical"),
            VenueScopeId::new(scope).expect("the fixture scope is canonical"),
        )
    }

    /// Both scopes answer the venue's own bounds: a twenty-level book and the
    /// candle intervals Hyperliquid publishes, which are the model's intervals
    /// without six hours and without two days.
    #[test]
    fn limits_answer_the_venues_own_bounds_per_scope() {
        let adapter = HyperliquidAdapter::new();
        for scope in [CORE_SPOT_SCOPE, CORE_PERP_SCOPE, "perp-dex/xyz"] {
            let limits = adapter.limits(&route("hyperliquid", scope));

            assert_eq!(limits.order_book_depth_limit, 20);
            assert_eq!(limits.timeframes.len(), 14);
            assert!(limits.timeframes.contains(&TimeFrame::Hour8));
            assert!(limits.timeframes.contains(&TimeFrame::Day3));
            assert!(!limits.timeframes.contains(&TimeFrame::Hour6));
            assert!(!limits.timeframes.contains(&TimeFrame::Day2));
            assert!(!limits.timeframes.contains(&TimeFrame::Second1));
        }
    }

    /// The venue attribution is static and never decides routing.
    #[test]
    fn the_venue_is_the_static_attribution() {
        assert_eq!(HyperliquidAdapter::new().venue().as_str(), "hyperliquid");
    }
}

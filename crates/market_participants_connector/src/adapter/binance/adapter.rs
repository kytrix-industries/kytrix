use std::{collections::BTreeMap, sync::Arc, time::Duration};

use app_config::market_participants::{
    credentials::BinanceCredentialMarket, origins::BinanceMarket,
};
use bytes::Bytes;
use futures::{StreamExt, future::BoxFuture, stream};
use market_participants_model::{
    asset::{
        AssetId, CryptoTokenOrigin, CryptoTokenTerms, DerivativeUnderlying, TokenReference,
        UnderlyingRef,
    },
    decimal::{Fraction, Price, Quantity, Ratio, Tick},
    identifier::{
        IdentifierNamespace, IdentifierValue, NativeSymbol, ReferenceSourceId,
        ReferenceTermsVersion, TokenIssuerId, VenueId, VenueScopeId,
    },
    instrument::{Instrument, InstrumentTerms, PerpetualTerms, SpotTerms},
    listing::{
        identifiers::{BinanceListingIdentifiers, VenueListingIdentifiers},
        listing_impl::ListingStatus,
        route::{VenueListingKey, VenueRoute},
        rules::{
            CommonTradingRules, FundingInterval, FundingSchedule, LeverageTier, LotPolicy,
            MarginMode, MarginProfile, NotionalRule, PerpetualMarginTerms, PerpetualRisk,
            PerpetualTradingRules, PriceRule, ProductTradingRules, QuantityRule, ShortSalePolicy,
            SpotTradingRules, TradingRules, VenueTradingPolicy,
        },
        venue::{BinanceProduct, PriceBasis},
    },
    market::{
        book::MarketSequence,
        selection::OrderQuantity,
        units::{MarketTradeId, PositionSide, QuantityUnit, Side},
    },
    reference::{
        PriceReferenceTerms, ReferenceAggregation, ReferenceObservation, ReferenceObservationKind,
        ReferenceSessionConvention, ReferenceSettlementConvention,
    },
    routing::CapabilityId,
    settlement::{ContractSize, ContractUnit, DerivativePnlModel},
};
use smol_str::{SmolStr, StrExt, format_smolstr};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{
    BinanceCodec, BinanceKlineRow, BinanceLevelRow, BinanceOrderBookWire, BinanceOrderResult,
    BinanceRecentTradeWire, BinanceTickerWire, Ed25519Signer, ListenKeyManager,
    account::{
        PrivateSubscriptionKind, RECV_WINDOW_MS, decode_account_trades, decode_balances,
        decode_orders, decode_positions, decode_private_events, decode_trading_settings,
        history_parameters, is_account_update, spawn_listen_key_refresh, validate_history,
    },
};
use crate::{
    account::shared::{OrderId, OrderKind, OrderRoute, TimeInForce},
    adapter::{
        AdapterLimits, Authentication, ExecutionContext, MarketParticipantAdapter,
        NativeCandlestick, NativeCatalogEntry, NativeForceOrder, NativeListing,
        NativeLongShortRatio, NativeOpenInterest, NativeOrderBook, NativeOrderBookUpdate,
        NativeTakerVolume, NativeTicker, NativeTrade, PreparedRequest, ProtocolEvent,
        ProtocolRequest, ProtocolResponse, SubscriptionHandle,
    },
    catalog::{registry::asset::AssetTerms, resolved::ResolvedListingRef},
    connection::{
        ConnectionKey, ConnectionState, ReconnectPolicy, SubscriptionTopic,
        physical::ConnectionProtocol,
    },
    error::{AuthError, MarketParticipantError},
    market::timeframe::Adjustment,
    market_participants::model::{
        AuthenticatedQuery, CancelOrderCommand, CandlestickFetchQuery, MarketCatalogQuery,
        MarketTradeFetchQuery, NativeOrderBookLevel, OrderBookQuery, PerpetualStatisticsQuery,
        PlaceOrderCommand, StatisticsTimeFrame, TickerQuery, TickersQuery, TradingSettingsQuery,
    },
    runtime::{CredentialState, MarketParticipantNetworkSnapshot},
};

#[derive(Clone, Copy)]
struct BinanceRest {
    market_participant_info: &'static str,
    klines: &'static str,
    depth: &'static str,
    trades: &'static str,
    ticker: &'static str,
    order: &'static str,
    open_orders: &'static str,
    all_orders: &'static str,
    open_algo_orders: &'static str,
    all_algo_orders: &'static str,
    positions: &'static str,
    account: &'static str,
    my_trades: &'static str,
    listen_key: &'static str,
}

const SPOT_REST: BinanceRest = BinanceRest {
    market_participant_info: "api/v3/exchangeInfo",
    klines: "api/v3/klines",
    depth: "api/v3/depth",
    trades: "api/v3/trades",
    ticker: "api/v3/ticker/24hr",
    order: "api/v3/order",
    open_orders: "api/v3/openOrders",
    all_orders: "api/v3/allOrders",
    open_algo_orders: "",
    all_algo_orders: "",
    positions: "",
    account: "api/v3/account",
    my_trades: "api/v3/myTrades",
    listen_key: "api/v3/userDataStream",
};

const PERPETUAL_REST: BinanceRest = BinanceRest {
    market_participant_info: "fapi/v1/exchangeInfo",
    klines: "fapi/v1/klines",
    depth: "fapi/v1/depth",
    trades: "fapi/v1/trades",
    ticker: "fapi/v1/ticker/24hr",
    order: "fapi/v1/order",
    open_orders: "fapi/v1/openOrders",
    all_orders: "fapi/v1/allOrders",
    open_algo_orders: "fapi/v1/openAlgoOrders",
    all_algo_orders: "fapi/v1/allAlgoOrders",
    positions: "fapi/v3/positionRisk",
    account: "fapi/v3/balance",
    my_trades: "fapi/v1/userTrades",
    listen_key: "fapi/v1/listenKey",
};

const BINANCE_ISSUER: &str = "binance";

#[derive(Clone)]
struct BinanceMarketSigner {
    api_key: SmolStr,
    signer: Ed25519Signer,
}

#[derive(Clone)]
pub struct BinanceAdapter {
    venue: VenueId,
    pub codec: BinanceCodec,
    credentials: BTreeMap<BinanceCredentialMarket, BinanceMarketSigner>,
}

impl BinanceAdapter {
    #[cfg(test)]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            venue: VenueId::Binance,
            codec: BinanceCodec,
            credentials: BTreeMap::new(),
        })
    }

    pub fn from_config(
        config: &MarketParticipantNetworkSnapshot,
    ) -> Result<Arc<Self>, MarketParticipantError> {
        let venue = VenueId::Binance;
        let mut credentials: BTreeMap<BinanceCredentialMarket, BinanceMarketSigner> =
            BTreeMap::new();
        for market in BinanceMarket::ALL {
            let scope = VenueScopeId::new(market.scope())
                .expect("a Binance market's scope name is canonical");
            match config.resolve_binance_credentials(&venue, &scope) {
                CredentialState::Configured(values) => {
                    credentials.insert(
                        market.credential_market(),
                        BinanceMarketSigner {
                            api_key: values.api_key.clone(),
                            signer: Ed25519Signer::from_private_key(
                                values.private_key.as_str(),
                                values.passphrase.as_deref(),
                            )?,
                        },
                    );
                }
                CredentialState::Unconfigured => {}
                CredentialState::Invalid(_) => {
                    return Err(MarketParticipantError::IncompleteCredentials {
                        venue,
                        scope: Some(SmolStr::new_static(market.scope())),
                    });
                }
            }
        }
        Ok(Arc::new(Self {
            venue,
            codec: BinanceCodec,
            credentials,
        }))
    }

    fn product(context: &ExecutionContext) -> Result<BinanceProduct, MarketParticipantError> {
        BinanceCodec::product(&context.route()).ok_or_else(|| {
            MarketParticipantError::UnsupportedCapability {
                route: context.route(),
                capability: context.capability(),
            }
        })
    }

    fn rest(product: BinanceProduct) -> Result<&'static BinanceRest, MarketParticipantError> {
        match product {
            BinanceProduct::Spot => Ok(&SPOT_REST),
            BinanceProduct::Perpetual => Ok(&PERPETUAL_REST),
            BinanceProduct::Future | BinanceProduct::Option => {
                Err(MarketParticipantError::Configuration(
                    "the Binance adapter serves spot and USD-M perpetual only".into(),
                ))
            }
        }
    }

    fn http_url(context: &ExecutionContext, path: &str) -> Result<Url, MarketParticipantError> {
        context.http_origin().join(path).map_err(|_| {
            MarketParticipantError::Configuration(format_smolstr!(
                "invalid Binance HTTP path {path} at {}",
                context.http_origin()
            ))
        })
    }

    fn websocket_url(
        context: &ExecutionContext,
        path: &str,
    ) -> Result<Url, MarketParticipantError> {
        context.websocket_origin().join(path).map_err(|_| {
            MarketParticipantError::Configuration(format_smolstr!(
                "invalid Binance WebSocket path {path} at {}",
                context.websocket_origin()
            ))
        })
    }

    async fn connection(
        &self,
        context: &ExecutionContext,
        authenticated: bool,
        endpoint: Url,
    ) -> Result<Arc<crate::connection::Connection>, MarketParticipantError> {
        let auth = context
            .auth
            .as_ref()
            .map(|value| SmolStr::new(value.account_id()));
        let protocol = ConnectionProtocol::new(BinanceCodec::extract_response_id)
            .with_liveness_timeout(liveness_timeout(&context.route()));
        let key = crate::adapter::bind_execution_identity(
            ConnectionKey::for_route(
                endpoint,
                context.proxy.clone(),
                auth,
                context.route(),
                context.network_route_generation(),
                &protocol,
            ),
            context,
        )?;
        let pool = Arc::clone(&context.connection_pool);
        pool.connect_or_create(
            key,
            if authenticated {
                ConnectionState::stateful()
            } else {
                ConnectionState::stateless()
            },
            protocol,
            pool.cancellation(),
            ReconnectPolicy::default(),
        )
        .await
    }

    async fn get_bytes(
        &self,
        context: &ExecutionContext,
        url: Url,
    ) -> Result<Bytes, MarketParticipantError> {
        let response = context
            .http_client
            .get(url.clone())
            .send()
            .await
            .map_err(|error| MarketParticipantError::ConnectionFailed {
                route: context.route(),
                endpoint: format_smolstr!("{}", url),
                source: network::error::NetworkError::Io {
                    message: format_smolstr!("{}", error),
                },
            })?;
        let status = response.status();
        let body =
            response
                .bytes()
                .await
                .map_err(|error| MarketParticipantError::DecodeFailed {
                    operation: "read Binance response".into(),
                    raw_data: Bytes::from(error.to_string()),
                })?;
        if !status.is_success() {
            return Err(self
                .codec
                .response_error(&context.route(), url.as_str(), status, body));
        }
        Ok(body)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        context: &ExecutionContext,
        url: Url,
    ) -> Result<T, MarketParticipantError> {
        let body = self.get_bytes(context, url).await?;
        self.codec.decode(&body)
    }

    fn credentials_for_context(
        &self,
        context: &ExecutionContext,
    ) -> Result<(Option<SmolStr>, Option<Ed25519Signer>), MarketParticipantError> {
        let account = context
            .auth
            .as_ref()
            .map(|auth| SmolStr::new(auth.account_id()));
        match context.auth.as_ref() {
            Some(Authentication::ApiKey {
                api_key, secret, ..
            }) => {
                let signer =
                    Ed25519Signer::from_private_key(secret.as_str(), None).map_err(|_| {
                        MarketParticipantError::AuthenticationFailed {
                            route: context.route(),
                            account: account.clone(),
                            source: AuthError::UnusableCredentials {
                                venue: VenueId::Binance,
                            },
                        }
                    })?;
                Ok((Some(api_key.clone()), Some(signer)))
            }
            Some(
                Authentication::Wallet { .. }
                | Authentication::PassphraseApiKey { .. }
                | Authentication::LongbridgeApiKey { .. },
            ) => Err(MarketParticipantError::AuthenticationFailed {
                route: context.route(),
                account,
                source: AuthError::InvalidCredentials,
            }),
            None => {
                Ok(self
                    .configured_signer(&context.route())
                    .map_or((None, None), |credentials| {
                        (
                            Some(credentials.api_key.clone()),
                            Some(credentials.signer.clone()),
                        )
                    }))
            }
        }
    }

    fn configured_signer(&self, route: &VenueRoute) -> Option<&BinanceMarketSigner> {
        let market = match BinanceCodec::product(route)? {
            BinanceProduct::Spot => BinanceCredentialMarket::Spot,
            BinanceProduct::Perpetual => BinanceCredentialMarket::UsdMPerp,
            BinanceProduct::Future | BinanceProduct::Option => return None,
        };
        self.credentials.get(&market)
    }

    pub(crate) async fn signed_request(
        &self,
        context: &ExecutionContext,
        method: reqwest::Method,
        path: &str,
        mut parameters: Vec<(String, String)>,
    ) -> Result<Bytes, MarketParticipantError> {
        let Some(account_id) = context
            .auth
            .as_ref()
            .map(|auth| SmolStr::new(auth.account_id()))
        else {
            return Err(MarketParticipantError::AuthenticationFailed {
                route: context.route(),
                account: None,
                source: AuthError::AccountIdentityMissing,
            });
        };
        let (api_key, signer) = self.credentials_for_context(context)?;
        let api_key = api_key.ok_or_else(|| MarketParticipantError::AuthenticationFailed {
            route: context.route(),
            account: Some(account_id.clone()),
            source: AuthError::ApiKeyNotFound,
        })?;
        let signer = signer.ok_or_else(|| MarketParticipantError::AuthenticationFailed {
            route: context.route(),
            account: Some(account_id.clone()),
            source: AuthError::InvalidCredentials,
        })?;
        parameters.push(("recvWindow".into(), RECV_WINDOW_MS.to_string()));
        parameters.push(("timestamp".into(), current_timestamp_ms().to_string()));
        let unsigned = serialize_parameters(&parameters);
        let signature = signer.sign(unsigned.as_bytes());
        let url = Self::http_url(context, path)?;
        let safe_endpoint: SmolStr = format_smolstr!("{}", url);
        let mut request_url = url;
        request_url.set_query(Some(&format!("{unsigned}&signature={signature}")));
        let response = context
            .http_client
            .request(method, request_url)
            .header("X-MBX-APIKEY", api_key.as_str())
            .send()
            .await
            .map_err(|error| MarketParticipantError::ConnectionFailed {
                route: context.route(),
                endpoint: safe_endpoint.clone(),
                source: network::error::NetworkError::Io {
                    message: format_smolstr!("{}", error),
                },
            })?;
        let status = response.status();
        let body =
            response
                .bytes()
                .await
                .map_err(|error| MarketParticipantError::DecodeFailed {
                    operation: "read Binance account response".into(),
                    raw_data: Bytes::from(error.to_string()),
                })?;
        if !status.is_success() {
            return Err(self.codec.response_error(
                &context.route(),
                safe_endpoint.as_str(),
                status,
                body,
            ));
        }
        Ok(body)
    }

    pub(crate) async fn signed_get(
        &self,
        context: &ExecutionContext,
        path: &str,
        parameters: Vec<(String, String)>,
    ) -> Result<Bytes, MarketParticipantError> {
        self.signed_request(context, reqwest::Method::GET, path, parameters)
            .await
    }

    async fn fetch_candlesticks(
        &self,
        context: &ExecutionContext,
        query: &CandlestickFetchQuery,
        listing: Option<&ResolvedListingRef>,
    ) -> Result<ProtocolResponse, MarketParticipantError> {
        query
            .validate()
            .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{}", error)))?;
        let (key, symbol) = listing_identity(listing, "fetch candlesticks")?;
        let product = Self::product(context)?;
        let interval = self.codec.encode_timeframe(&key.route, query.timeframe)?;
        let mut url = Self::http_url(context, Self::rest(product)?.klines)?;
        {
            let mut parameters = url.query_pairs_mut();
            parameters
                .append_pair("symbol", symbol.as_str())
                .append_pair("interval", interval)
                .append_pair("limit", &query.limit.to_string());
            if let Some(start_time) = query.start_timestamp {
                parameters.append_pair("startTime", &start_time.to_string());
            }
            if let Some(end_time) = query.end_timestamp {
                parameters.append_pair("endTime", &end_time.to_string());
            }
        }
        let body = self.get_bytes(context, url).await?;
        let rows: Vec<BinanceKlineRow> = self.codec.decode(&body)?;
        let candlesticks = rows
            .into_iter()
            .map(|row| {
                Ok(NativeListing::new(
                    key.clone(),
                    NativeCandlestick {
                        timestamp: row.open_time,
                        open: parse_price(&row.open, "decode Binance kline")?,
                        high: parse_price(&row.high, "decode Binance kline")?,
                        low: parse_price(&row.low, "decode Binance kline")?,
                        close: parse_price(&row.close, "decode Binance kline")?,
                        volume: parse_quantity(&row.volume, "decode Binance kline")?,
                    },
                ))
            })
            .collect::<Result<Vec<_>, MarketParticipantError>>()?;
        Ok(ProtocolResponse::Candlesticks(candlesticks))
    }

    async fn fetch_markets(
        &self,
        context: &ExecutionContext,
        query: &MarketCatalogQuery,
    ) -> Result<ProtocolResponse, MarketParticipantError> {
        let product = Self::product(context)?;
        let url = Self::http_url(context, Self::rest(product)?.market_participant_info)?;
        let decoded: BinanceMarketParticipantInfo = self.get_json(context, url).await?;
        let mut markets = Vec::with_capacity(decoded.symbols.len());
        for symbol in decoded.symbols {
            if product == BinanceProduct::Perpetual
                && !matches!(
                    symbol.contract_type.as_deref(),
                    Some("PERPETUAL" | "TRADIFI_PERPETUAL")
                )
            {
                continue;
            }
            markets.push(decode_market_entry(&query.route, product, symbol)?);
        }
        markets.sort_unstable_by(|left, right| left.native_symbol.cmp(&right.native_symbol));
        Ok(ProtocolResponse::Markets(markets))
    }

    async fn fetch_ticker(
        &self,
        context: &ExecutionContext,
        _query: &TickerQuery,
        listing: Option<&ResolvedListingRef>,
    ) -> Result<ProtocolResponse, MarketParticipantError> {
        let (key, symbol) = listing_identity(listing, "fetch ticker")?;
        let product = Self::product(context)?;
        let mut url = Self::http_url(context, Self::rest(product)?.ticker)?;
        url.query_pairs_mut().append_pair("symbol", symbol.as_str());
        let wire: BinanceTickerWire = self.get_json(context, url).await?;
        Ok(ProtocolResponse::Ticker(NativeListing::new(
            key.clone(),
            decode_ticker(&wire, "decode Binance ticker")?,
        )))
    }

    async fn fetch_all_tickers(
        &self,
        context: &ExecutionContext,
        _query: &TickersQuery,
    ) -> Result<ProtocolResponse, MarketParticipantError> {
        let product = Self::product(context)?;
        let route = context.route();
        let url = Self::http_url(context, Self::rest(product)?.ticker)?;
        let rows: Vec<BinanceTickerWire> = self.get_json(context, url).await?;
        let mut result = Vec::with_capacity(rows.len());
        for row in rows {
            let key = BinanceCodec::listing_key(&route, product, row.symbol.as_str())?;
            result.push(NativeListing::new(
                key,
                decode_ticker(&row, "decode Binance ticker")?,
            ));
        }
        Ok(ProtocolResponse::Tickers(result))
    }

    async fn fetch_order_book(
        &self,
        context: &ExecutionContext,
        query: &OrderBookQuery,
        listing: Option<&ResolvedListingRef>,
    ) -> Result<ProtocolResponse, MarketParticipantError> {
        query
            .validate()
            .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{}", error)))?;
        let (key, symbol) = listing_identity(listing, "fetch order book")?;
        let product = Self::product(context)?;
        let mut url = Self::http_url(context, Self::rest(product)?.depth)?;
        url.query_pairs_mut()
            .append_pair("symbol", symbol.as_str())
            .append_pair("limit", &query.depth.to_string());
        let wire: BinanceOrderBookWire = self.get_json(context, url).await?;
        let sequence = wire.last_update_id.ok_or_else(|| {
            protocol_error(
                "decode Binance order book",
                "the venue published no update id",
            )
        })?;
        Ok(ProtocolResponse::OrderBook(NativeListing::new(
            key.clone(),
            NativeOrderBook {
                sequence: MarketSequence::Numeric(sequence),
                timestamp: None,
                bids: decode_levels(&wire.bids, "decode Binance order book")?,
                asks: decode_levels(&wire.asks, "decode Binance order book")?,
            },
        )))
    }

    async fn fetch_trades(
        &self,
        context: &ExecutionContext,
        query: &MarketTradeFetchQuery,
        listing: Option<&ResolvedListingRef>,
    ) -> Result<ProtocolResponse, MarketParticipantError> {
        query
            .validate()
            .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{}", error)))?;
        let (key, symbol) = listing_identity(listing, "fetch trades")?;
        let product = Self::product(context)?;
        let mut url = Self::http_url(context, Self::rest(product)?.trades)?;
        url.query_pairs_mut()
            .append_pair("symbol", symbol.as_str())
            .append_pair("limit", &query.limit.to_string());
        let rows: Vec<BinanceRecentTradeWire> = self.get_json(context, url).await?;
        let mut trades = rows
            .into_iter()
            .map(|trade| {
                Ok(NativeListing::new(
                    key.clone(),
                    NativeTrade {
                        id: MarketTradeId::Numeric(trade.id),
                        price: parse_price(&trade.price, "decode Binance trade")?,
                        quantity: parse_quantity(&trade.qty, "decode Binance trade")?,
                        side: if trade.buyer_is_maker {
                            Side::Sell
                        } else {
                            Side::Buy
                        },
                        timestamp: trade.time,
                    },
                ))
            })
            .collect::<Result<Vec<_>, MarketParticipantError>>()?;
        trades.sort_unstable_by(|left, right| {
            (left.data.timestamp, &left.data.id).cmp(&(right.data.timestamp, &right.data.id))
        });
        Ok(ProtocolResponse::Trades(trades))
    }

    async fn fetch_statistics(
        &self,
        context: &ExecutionContext,
        query: &PerpetualStatisticsQuery,
        listing: Option<&ResolvedListingRef>,
        kind: StatisticsKind,
    ) -> Result<ProtocolResponse, MarketParticipantError> {
        query
            .validate()
            .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{}", error)))?;
        let (key, symbol) = listing_identity(listing, "fetch perpetual statistics")?;
        let path = match kind {
            StatisticsKind::OpenInterest => "futures/data/openInterestHist",
            StatisticsKind::LongShort => "futures/data/globalLongShortAccountRatio",
            StatisticsKind::TopTraderAccount => "futures/data/topLongShortAccountRatio",
            StatisticsKind::TopTraderPosition => "futures/data/topLongShortPositionRatio",
            StatisticsKind::Taker => "futures/data/takerlongshortRatio",
        };
        let mut url = Self::http_url(context, path)?;
        url.query_pairs_mut()
            .append_pair("symbol", symbol.as_str())
            .append_pair("period", statistics_period(query.timeframe))
            .append_pair("limit", &query.limit.to_string());
        let body = self.get_bytes(context, url).await?;
        match kind {
            StatisticsKind::OpenInterest => {
                let rows: Vec<BinanceOpenInterestRow> =
                    decode_vec_or_single(&body, "decode Binance open interest")?;
                let values = rows
                    .into_iter()
                    .map(|row| {
                        Ok(NativeListing::new(
                            key.clone(),
                            NativeOpenInterest {
                                quantity: parse_quantity(
                                    &row.sum_open_interest,
                                    "decode Binance open interest",
                                )?,
                                notional: Some(parse_quantity(
                                    &row.sum_open_interest_value,
                                    "decode Binance open interest",
                                )?),
                                timestamp: row.timestamp,
                            },
                        ))
                    })
                    .collect::<Result<Vec<_>, MarketParticipantError>>()?;
                Ok(ProtocolResponse::OpenInterest(values))
            }
            StatisticsKind::LongShort
            | StatisticsKind::TopTraderAccount
            | StatisticsKind::TopTraderPosition => {
                let rows: Vec<BinanceRatioRow> =
                    decode_vec_or_single(&body, "decode Binance long-short ratio")?;
                let values = rows
                    .into_iter()
                    .map(|row| {
                        let ratio = row.long_short_ratio.parse::<Ratio>().map_err(|error| {
                            protocol_error(
                                "decode Binance long-short ratio",
                                format_smolstr!("{}", error),
                            )
                        })?;
                        Ok(NativeListing::new(
                            key.clone(),
                            NativeLongShortRatio {
                                ratio,
                                timestamp: row.timestamp,
                            },
                        ))
                    })
                    .collect::<Result<Vec<_>, MarketParticipantError>>()?;
                Ok(ProtocolResponse::LongShortRatios(values))
            }
            StatisticsKind::Taker => {
                let rows: Vec<BinanceTakerRow> =
                    decode_vec_or_single(&body, "decode Binance taker volume")?;
                let values = rows
                    .into_iter()
                    .map(|row| {
                        Ok(NativeListing::new(
                            key.clone(),
                            NativeTakerVolume {
                                buy: parse_quantity(
                                    &row.buy_volume,
                                    "decode Binance taker volume",
                                )?,
                                sell: parse_quantity(
                                    &row.sell_volume,
                                    "decode Binance taker volume",
                                )?,
                                timestamp: row.timestamp,
                            },
                        ))
                    })
                    .collect::<Result<Vec<_>, MarketParticipantError>>()?;
                Ok(ProtocolResponse::TakerVolumes(values))
            }
        }
    }

    async fn place_order(
        &self,
        context: &ExecutionContext,
        command: &PlaceOrderCommand,
        listing: Option<&ResolvedListingRef>,
    ) -> Result<ProtocolResponse, MarketParticipantError> {
        command
            .validate()
            .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{}", error)))?;
        let (_, symbol) = listing_identity(listing, "place order")?;
        let product = Self::product(context)?;
        let quantity = match &command.quantity {
            OrderQuantity::AssetUnits { value, .. } | OrderQuantity::Contracts(value) => *value,
            OrderQuantity::Notional { .. } | OrderQuantity::OutcomeTokens { .. } => {
                return Err(MarketParticipantError::Configuration(
                    "the Binance adapter places base-asset and contract quantities only".into(),
                ));
            }
        };
        let (order_type, price, time_in_force) = match &command.kind {
            OrderKind::Market => ("MARKET", None, None),
            OrderKind::Limit {
                price,
                time_in_force,
            } => ("LIMIT", Some(*price), Some(*time_in_force)),
            _ => {
                return Err(MarketParticipantError::Configuration(
                    "the Binance adapter places market and limit orders only".into(),
                ));
            }
        };
        let mut parameters = vec![
            ("symbol".to_owned(), symbol.as_str().to_owned()),
            ("side".to_owned(), side_text(command.side).to_owned()),
            ("type".to_owned(), order_type.to_owned()),
            ("quantity".to_owned(), quantity.to_string()),
        ];
        if let Some(price) = price {
            parameters.push(("price".to_owned(), price.to_string()));
        }
        if let Some(time_in_force) = time_in_force {
            parameters.push((
                "timeInForce".to_owned(),
                time_in_force_text(time_in_force).to_owned(),
            ));
        }
        if let Some(position_side) = command.position_side {
            parameters.push((
                "positionSide".to_owned(),
                position_side_text(position_side).to_owned(),
            ));
        }
        if command.reduce_only {
            parameters.push(("reduceOnly".to_owned(), "true".to_owned()));
        }
        let body = self
            .signed_request(
                context,
                reqwest::Method::POST,
                Self::rest(product)?.order,
                parameters,
            )
            .await?;
        let result: BinanceOrderResult = self.codec.decode(&body)?;
        let order_id = result
            .order_id
            .ok_or_else(|| protocol_error("decode Binance order response", "missing orderId"))?;
        let id = OrderId::normal(order_id.to_string()).map_err(|error| {
            protocol_error("create Binance order ID", format_smolstr!("{}", error))
        })?;
        Ok(ProtocolResponse::OrderPlaced(id))
    }

    async fn cancel_order(
        &self,
        context: &ExecutionContext,
        command: &CancelOrderCommand,
        listing: Option<&ResolvedListingRef>,
    ) -> Result<ProtocolResponse, MarketParticipantError> {
        let (_, symbol) = listing_identity(listing, "cancel order")?;
        let product = Self::product(context)?;
        let body = self
            .signed_request(
                context,
                reqwest::Method::DELETE,
                Self::rest(product)?.order,
                vec![
                    ("symbol".to_owned(), symbol.as_str().to_owned()),
                    ("orderId".to_owned(), command.order_id.as_str().to_owned()),
                ],
            )
            .await?;
        let result: BinanceOrderResult = self.codec.decode(&body)?;
        let value = result
            .order_id
            .map_or_else(|| command.order_id.as_str().to_owned(), |id| id.to_string());
        let id = OrderId::new(value, command.order_id.route()).map_err(|error| {
            protocol_error(
                "create Binance cancellation ID",
                format_smolstr!("{}", error),
            )
        })?;
        Ok(ProtocolResponse::OrderCancelled(id))
    }

    async fn fetch_balances(
        &self,
        context: &ExecutionContext,
        query: &AuthenticatedQuery,
    ) -> Result<ProtocolResponse, MarketParticipantError> {
        query
            .validate()
            .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{}", error)))?;
        let product = Self::product(context)?;
        let body = self
            .signed_get(context, Self::rest(product)?.account, Vec::new())
            .await?;
        Ok(ProtocolResponse::Balances(decode_balances(product, &body)?))
    }

    async fn fetch_active_orders(
        &self,
        context: &ExecutionContext,
        query: &AuthenticatedQuery,
        listing: Option<&ResolvedListingRef>,
    ) -> Result<ProtocolResponse, MarketParticipantError> {
        query
            .validate()
            .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{}", error)))?;
        let product = Self::product(context)?;
        let route = context.route();
        let rest = Self::rest(product)?;
        let parameters = narrowing(listing, "fetch active orders")?
            .map_or_else(Vec::new, |symbol| {
                vec![("symbol".to_owned(), symbol.as_str().to_owned())]
            });
        let body = self
            .signed_get(context, rest.open_orders, parameters.clone())
            .await?;
        let mut orders = decode_orders(&route, product, OrderRoute::Normal, &body)?;
        if product == BinanceProduct::Perpetual {
            let algo = self
                .signed_get(context, rest.open_algo_orders, parameters)
                .await?;
            orders.append(&mut decode_orders(
                &route,
                product,
                OrderRoute::Algo,
                &algo,
            )?);
        }
        Ok(ProtocolResponse::Orders(orders))
    }

    async fn fetch_order_history(
        &self,
        context: &ExecutionContext,
        query: &AuthenticatedQuery,
        listing: Option<&ResolvedListingRef>,
    ) -> Result<ProtocolResponse, MarketParticipantError> {
        validate_history(query, "fetch order history")?;
        let (_, symbol) = listing_identity(listing, "fetch order history")?;
        let product = Self::product(context)?;
        let route = context.route();
        let rest = Self::rest(product)?;
        let order_routes = usize::from(product == BinanceProduct::Perpetual) + 1;
        let mut orders = Vec::with_capacity(usize::from(query.limit) * order_routes);
        let body = self
            .signed_get(context, rest.all_orders, history_parameters(symbol, query))
            .await?;
        orders.append(&mut decode_orders(
            &route,
            product,
            OrderRoute::Normal,
            &body,
        )?);
        if product == BinanceProduct::Perpetual {
            let algo = self
                .signed_get(
                    context,
                    rest.all_algo_orders,
                    history_parameters(symbol, query),
                )
                .await?;
            orders.append(&mut decode_orders(
                &route,
                product,
                OrderRoute::Algo,
                &algo,
            )?);
        }
        orders.retain(|order| {
            query
                .start_timestamp
                .is_none_or(|start| order.create_timestamp >= start)
                && query
                    .end_timestamp
                    .is_none_or(|end| order.create_timestamp <= end)
        });
        orders.sort_unstable_by(|left, right| {
            right
                .create_timestamp
                .cmp(&left.create_timestamp)
                .then_with(|| right.id.cmp(&left.id))
        });
        orders.truncate(usize::from(query.limit));
        Ok(ProtocolResponse::Orders(orders))
    }

    async fn fetch_positions(
        &self,
        context: &ExecutionContext,
        query: &AuthenticatedQuery,
        listing: Option<&ResolvedListingRef>,
    ) -> Result<ProtocolResponse, MarketParticipantError> {
        query
            .validate()
            .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{}", error)))?;
        let product = Self::product(context)?;
        if product != BinanceProduct::Perpetual {
            return Err(MarketParticipantError::UnsupportedCapability {
                route: context.route(),
                capability: CapabilityId::AccountPositions,
            });
        }
        let parameters = narrowing(listing, "fetch positions")?.map_or_else(Vec::new, |symbol| {
            vec![("symbol".to_owned(), symbol.as_str().to_owned())]
        });
        let body = self
            .signed_get(context, Self::rest(product)?.positions, parameters)
            .await?;
        Ok(ProtocolResponse::Positions(decode_positions(
            &context.route(),
            product,
            &body,
        )?))
    }

    async fn fetch_account_trades(
        &self,
        context: &ExecutionContext,
        query: &AuthenticatedQuery,
        listing: Option<&ResolvedListingRef>,
    ) -> Result<ProtocolResponse, MarketParticipantError> {
        validate_history(query, "fetch account trades")?;
        let (_, symbol) = listing_identity(listing, "fetch account trades")?;
        let product = Self::product(context)?;
        let body = self
            .signed_get(
                context,
                Self::rest(product)?.my_trades,
                history_parameters(symbol, query),
            )
            .await?;
        let mut trades = decode_account_trades(&context.route(), product, &body)?;
        trades.retain(|trade| {
            query
                .start_timestamp
                .is_none_or(|start| trade.timestamp >= start)
                && query.end_timestamp.is_none_or(|end| trade.timestamp <= end)
        });
        trades.sort_unstable_by_key(|trade| trade.timestamp);
        trades.truncate(usize::from(query.limit));
        Ok(ProtocolResponse::AccountTrades(trades))
    }

    async fn fetch_trading_settings(
        &self,
        context: &ExecutionContext,
        _query: &TradingSettingsQuery,
        listing: Option<&ResolvedListingRef>,
    ) -> Result<ProtocolResponse, MarketParticipantError> {
        if Self::product(context)? != BinanceProduct::Perpetual {
            return Err(MarketParticipantError::UnsupportedCapability {
                route: context.route(),
                capability: CapabilityId::AuthenticatedTrading,
            });
        }
        let (_, symbol) = listing_identity(listing, "fetch trading settings")?;
        let mode = self
            .signed_get(context, "fapi/v1/positionSide/dual", Vec::new())
            .await?;
        let positions = self
            .signed_get(
                context,
                "fapi/v2/positionRisk",
                vec![("symbol".to_owned(), symbol.as_str().to_owned())],
            )
            .await?;
        Ok(ProtocolResponse::TradingSettings(decode_trading_settings(
            &mode, &positions,
        )?))
    }

    async fn reject_position_history(
        &self,
        context: &ExecutionContext,
    ) -> Result<ProtocolResponse, MarketParticipantError> {
        Err(MarketParticipantError::UnsupportedCapability {
            route: context.route(),
            capability: CapabilityId::AccountHistory,
        })
    }

    async fn subscribe_request(
        &self,
        context: ExecutionContext,
        request: PreparedRequest,
    ) -> Result<SubscriptionHandle, MarketParticipantError> {
        let product = Self::product(&context)?;
        let (stream_name, kind, stream_path) = match request.request() {
            ProtocolRequest::SubscribeCandlesticks(query) => {
                query.validate().map_err(|error| {
                    MarketParticipantError::Configuration(format_smolstr!("{}", error))
                })?;
                let (key, symbol) = listing_identity(request.listing(), "subscribe candlesticks")?;
                let interval =
                    SmolStr::new_static(self.codec.encode_timeframe(&key.route, query.timeframe)?);
                (
                    format_smolstr!(
                        "{}@kline_{}",
                        symbol.as_str().to_ascii_lowercase_smolstr(),
                        interval
                    ),
                    SubscriptionKind::Candlestick {
                        listing: key.clone(),
                        symbol: symbol.clone(),
                        interval,
                    },
                    StreamPath::Market,
                )
            }
            ProtocolRequest::SubscribeTicker(_) => {
                let (key, symbol) = listing_identity(request.listing(), "subscribe ticker")?;
                (
                    format_smolstr!("{}@ticker", symbol.as_str().to_ascii_lowercase_smolstr()),
                    SubscriptionKind::Ticker {
                        listing: key.clone(),
                        symbol: symbol.clone(),
                    },
                    StreamPath::Market,
                )
            }
            ProtocolRequest::SubscribeTrades(_) => {
                let (key, symbol) = listing_identity(request.listing(), "subscribe trades")?;
                (
                    format_smolstr!("{}@aggTrade", symbol.as_str().to_ascii_lowercase_smolstr()),
                    SubscriptionKind::Trade {
                        listing: key.clone(),
                        symbol: symbol.clone(),
                    },
                    StreamPath::Market,
                )
            }
            ProtocolRequest::SubscribeOrderBook(query) => {
                query.validate().map_err(|error| {
                    MarketParticipantError::Configuration(format_smolstr!("{}", error))
                })?;
                let (key, symbol) = listing_identity(request.listing(), "subscribe order book")?;
                (
                    format_smolstr!(
                        "{}@depth@100ms",
                        symbol.as_str().to_ascii_lowercase_smolstr()
                    ),
                    SubscriptionKind::OrderBook {
                        listing: key.clone(),
                        symbol: symbol.clone(),
                    },
                    StreamPath::Public,
                )
            }
            ProtocolRequest::SubscribeForceOrder(query) => {
                query.validate().map_err(|error| {
                    MarketParticipantError::Configuration(format_smolstr!("{}", error))
                })?;
                (
                    SmolStr::new_static("!forceOrder@arr"),
                    SubscriptionKind::ForceOrder {
                        route: query.route.clone(),
                        product,
                    },
                    StreamPath::Market,
                )
            }
            _ => {
                return Err(MarketParticipantError::UnsupportedCapability {
                    route: context.route(),
                    capability: context.capability(),
                });
            }
        };
        let endpoint = Self::websocket_url(&context, stream_path.path(product))?;
        let connection = self.connection(&context, false, endpoint).await?;
        let request_id = connection.allocate_request_id();
        let topic = SubscriptionTopic::from(Bytes::copy_from_slice(stream_name.as_bytes()));
        let request = self
            .codec
            .encode_subscription(std::slice::from_ref(&stream_name), request_id);
        let unsubscribe_id = connection.allocate_request_id();
        let mut unsubscribe = request.clone();
        unsubscribe.id = unsubscribe_id;
        unsubscribe.method = SmolStr::new_static("UNSUBSCRIBE");
        let payload = self.codec.encode(&request)?;
        let unsubscribe_payload = self.codec.encode(&unsubscribe)?;
        let connection_subscription =
            connection.subscribe_with_id(request_id, topic, payload, unsubscribe_payload)?;
        let cancellation = CancellationToken::new();
        let source = Box::pin(connection.event_stream());
        let stream = stream::unfold(
            (source, cancellation.clone(), kind),
            |(mut source, cancellation, kind)| async move {
                loop {
                    let next_payload = source.next();
                    let payload = tokio::select! { () = cancellation.cancelled() => return None, payload = next_payload => payload };
                    let payload = match payload {
                        Some(Ok(payload)) => payload,
                        Some(Err(error)) => {
                            return Some((Err(error), (source, cancellation, kind)));
                        }
                        None => return None,
                    };
                    match decode_subscription_event(payload.as_ref(), &kind) {
                        Ok(Some(event)) => {
                            return Some((Ok(event), (source, cancellation, kind)));
                        }
                        Ok(None) => continue,
                        Err(error) => {
                            return Some((Err(error), (source, cancellation, kind)));
                        }
                    }
                }
            },
        );
        Ok(SubscriptionHandle::with_connection_subscription(
            Box::pin(stream),
            cancellation,
            connection_subscription,
        ))
    }

    async fn private_connection(
        &self,
        context: &ExecutionContext,
    ) -> Result<(Arc<crate::connection::Connection>, CancellationToken), MarketParticipantError>
    {
        let Some(account_id): Option<SmolStr> =
            context.auth.as_ref().map(|auth| auth.account_id().into())
        else {
            return Err(MarketParticipantError::AuthenticationFailed {
                route: context.route(),
                account: None,
                source: AuthError::AccountIdentityMissing,
            });
        };
        let (api_key, _) = self.credentials_for_context(context)?;
        let api_key = api_key.ok_or_else(|| MarketParticipantError::AuthenticationFailed {
            route: context.route(),
            account: Some(account_id.clone()),
            source: AuthError::ApiKeyNotFound,
        })?;
        let product = Self::product(context)?;
        let listen_endpoint = Self::http_url(context, Self::rest(product)?.listen_key)?;
        let manager = ListenKeyManager::new(listen_endpoint.as_str(), api_key);
        let endpoint = Self::websocket_url(context, "ws")?;
        let auth = context
            .auth
            .as_ref()
            .map(|value| SmolStr::new(value.account_id()));
        let protocol = ConnectionProtocol::new(BinanceCodec::extract_response_id)
            .with_liveness_timeout(liveness_timeout(&context.route()));
        let key = crate::adapter::bind_execution_identity(
            ConnectionKey::for_route(
                endpoint.clone(),
                context.proxy.clone(),
                auth,
                context.route(),
                context.network_route_generation(),
                &protocol,
            ),
            context,
        )?;
        let pool = Arc::clone(&context.connection_pool);
        if let Some(connection) = pool.get(&key).await.filter(|value| value.is_healthy()) {
            let lifecycle = connection.cancellation();
            return Ok((connection, lifecycle));
        }
        let listen_key = manager
            .acquire(&context.http_client, &context.route())
            .await?;
        let mut stream_endpoint = endpoint.clone();
        let path = stream_endpoint.path().trim_end_matches('/');
        stream_endpoint.set_path(&format!("{path}/{listen_key}"));
        let (connection, created) = match pool
            .connect_or_create_with_endpoint(
                key,
                stream_endpoint,
                ConnectionState::stateful(),
                protocol,
                pool.cancellation(),
                ReconnectPolicy::default(),
            )
            .await
        {
            Ok(connection) => connection,
            Err(error) => {
                let _ = manager
                    .delete(&context.http_client, &listen_key, &context.route())
                    .await;
                return Err(error);
            }
        };
        let lifecycle = connection.cancellation();
        if !created {
            let _ = manager
                .delete(&context.http_client, &listen_key, &context.route())
                .await;
            return Ok((connection, lifecycle));
        }
        let refresh_client = context.http_client.clone();
        spawn_listen_key_refresh(
            manager,
            refresh_client,
            listen_key,
            context.route(),
            Arc::clone(&connection),
            Duration::from_secs(25 * 60),
        );
        Ok((connection, lifecycle))
    }

    async fn subscribe_private(
        &self,
        context: ExecutionContext,
        request: PreparedRequest,
    ) -> Result<SubscriptionHandle, MarketParticipantError> {
        let product = Self::product(&context)?;
        let Some(target) = request.request().target().cloned() else {
            return Err(MarketParticipantError::UnsupportedCapability {
                route: context.route(),
                capability: context.capability(),
            });
        };
        let (kind, listing) = match request.request() {
            ProtocolRequest::SubscribeOrders(_) => {
                (PrivateSubscriptionKind::Orders, request.listing().cloned())
            }
            ProtocolRequest::SubscribePositions(_) => {
                if product != BinanceProduct::Perpetual {
                    return Err(MarketParticipantError::UnsupportedCapability {
                        route: context.route(),
                        capability: CapabilityId::AccountPositions,
                    });
                }
                (
                    PrivateSubscriptionKind::Positions,
                    request.listing().cloned(),
                )
            }
            _ => {
                return Err(MarketParticipantError::UnsupportedCapability {
                    route: context.route(),
                    capability: context.capability(),
                });
            }
        };
        let (connection, cancellation) = self.private_connection(&context).await?;
        let source = Box::pin(connection.event_stream());
        let route = context.route();
        let adapter = self.clone();
        let refresh_context = context.clone();
        let stream = stream::unfold(
            PrivateStreamState {
                source,
                cancellation: cancellation.clone(),
                kind,
                listing,
                target,
                adapter,
                refresh_context,
                route,
                product,
            },
            |mut state| async move {
                loop {
                    let payload = tokio::select! {
                        () = state.cancellation.cancelled() => return None,
                        payload = state.source.next() => payload,
                    };
                    let payload = match payload {
                        Some(Ok(payload)) => payload,
                        Some(Err(error)) => return Some((Err(error), state)),
                        None => return None,
                    };
                    if matches!(state.kind, PrivateSubscriptionKind::Positions)
                        && is_account_update(payload.as_ref())
                    {
                        let query = AuthenticatedQuery::account_wide(state.target.clone());
                        let refreshed = state
                            .adapter
                            .fetch_positions(&state.refresh_context, &query, state.listing.as_ref())
                            .await;
                        match refreshed {
                            Ok(ProtocolResponse::Positions(mut values)) => {
                                if let Some(value) = values.pop() {
                                    return Some((Ok(ProtocolEvent::Position(value)), state));
                                }
                                continue;
                            }
                            Ok(_) => continue,
                            Err(error) => return Some((Err(error), state)),
                        }
                    }
                    match decode_private_events(
                        payload.as_ref(),
                        &state.route,
                        state.product,
                        state.kind,
                        state.listing.as_ref().map(|listing| &listing.key),
                    ) {
                        Ok(mut events) if !events.is_empty() => {
                            let first = events.remove(0);
                            return Some((Ok(first), state));
                        }
                        Ok(_) => continue,
                        Err(error) => return Some((Err(error), state)),
                    }
                }
            },
        );
        Ok(SubscriptionHandle::new(Box::pin(stream), cancellation))
    }
}

impl MarketParticipantAdapter for BinanceAdapter {
    fn venue(&self) -> &VenueId {
        &self.venue
    }

    fn limits(&self, route: &VenueRoute) -> AdapterLimits {
        match route.scope.as_str() {
            crate::routing::SPOT_SCOPE => AdapterLimits {
                order_book_depth_limit: 5_000,
                timeframes: BinanceCodec::published_timeframes(route),
                adjustments: vec![Adjustment::None],
            },
            crate::routing::PERPETUAL_SCOPE => AdapterLimits {
                order_book_depth_limit: 1_000,
                timeframes: BinanceCodec::published_timeframes(route),
                adjustments: vec![Adjustment::None],
            },
            _ => AdapterLimits {
                order_book_depth_limit: 0,
                timeframes: Vec::new(),
                adjustments: Vec::new(),
            },
        }
    }

    fn execute(
        &self,
        context: ExecutionContext,
        request: PreparedRequest,
    ) -> BoxFuture<'static, Result<ProtocolResponse, MarketParticipantError>> {
        let adapter = self.clone();
        Box::pin(async move {
            match request.request() {
                ProtocolRequest::FetchMarkets(query) => {
                    adapter.fetch_markets(&context, query).await
                }
                ProtocolRequest::FetchCandlesticks(query) => {
                    adapter
                        .fetch_candlesticks(&context, query, request.listing())
                        .await
                }
                ProtocolRequest::FetchTicker(query) => {
                    adapter
                        .fetch_ticker(&context, query, request.listing())
                        .await
                }
                ProtocolRequest::FetchAllTickers(query) => {
                    adapter.fetch_all_tickers(&context, query).await
                }
                ProtocolRequest::FetchOrderBook(query) => {
                    adapter
                        .fetch_order_book(&context, query, request.listing())
                        .await
                }
                ProtocolRequest::FetchTrades(query) => {
                    adapter
                        .fetch_trades(&context, query, request.listing())
                        .await
                }
                ProtocolRequest::PlaceOrder(command) => {
                    adapter
                        .place_order(&context, command, request.listing())
                        .await
                }
                ProtocolRequest::CancelOrder(command) => {
                    adapter
                        .cancel_order(&context, command, request.listing())
                        .await
                }
                ProtocolRequest::FetchOpenInterest(query) => {
                    adapter
                        .fetch_statistics(
                            &context,
                            query,
                            request.listing(),
                            StatisticsKind::OpenInterest,
                        )
                        .await
                }
                ProtocolRequest::FetchLongShortRatio(query) => {
                    adapter
                        .fetch_statistics(
                            &context,
                            query,
                            request.listing(),
                            StatisticsKind::LongShort,
                        )
                        .await
                }
                ProtocolRequest::FetchTopTraderAccountRatio(query) => {
                    adapter
                        .fetch_statistics(
                            &context,
                            query,
                            request.listing(),
                            StatisticsKind::TopTraderAccount,
                        )
                        .await
                }
                ProtocolRequest::FetchTopTraderPositionRatio(query) => {
                    adapter
                        .fetch_statistics(
                            &context,
                            query,
                            request.listing(),
                            StatisticsKind::TopTraderPosition,
                        )
                        .await
                }
                ProtocolRequest::FetchTakerVolume(query) => {
                    adapter
                        .fetch_statistics(&context, query, request.listing(), StatisticsKind::Taker)
                        .await
                }
                ProtocolRequest::FetchBalances(query) => {
                    adapter.fetch_balances(&context, query).await
                }
                ProtocolRequest::FetchActiveOrders(query) => {
                    adapter
                        .fetch_active_orders(&context, query, request.listing())
                        .await
                }
                ProtocolRequest::FetchOrderHistory(query) => {
                    adapter
                        .fetch_order_history(&context, query, request.listing())
                        .await
                }
                ProtocolRequest::FetchPositions(query) => {
                    adapter
                        .fetch_positions(&context, query, request.listing())
                        .await
                }
                ProtocolRequest::FetchAccountTrades(query) => {
                    adapter
                        .fetch_account_trades(&context, query, request.listing())
                        .await
                }
                ProtocolRequest::FetchPositionHistory(_) => {
                    adapter.reject_position_history(&context).await
                }
                ProtocolRequest::FetchTradingSettings(query) => {
                    adapter
                        .fetch_trading_settings(&context, query, request.listing())
                        .await
                }
                _ => Err(MarketParticipantError::UnsupportedCapability {
                    route: context.route(),
                    capability: context.capability(),
                }),
            }
        })
    }

    fn subscribe(
        &self,
        context: ExecutionContext,
        request: PreparedRequest,
    ) -> BoxFuture<'static, Result<SubscriptionHandle, MarketParticipantError>> {
        let adapter = self.clone();
        Box::pin(async move {
            match request.request() {
                ProtocolRequest::SubscribeOrders(_) | ProtocolRequest::SubscribePositions(_) => {
                    adapter.subscribe_private(context, request).await
                }
                _ => adapter.subscribe_request(context, request).await,
            }
        })
    }
}

type ConnectionFrames = std::pin::Pin<
    Box<dyn futures::Stream<Item = Result<Bytes, MarketParticipantError>> + Send + 'static>,
>;

struct PrivateStreamState {
    source: ConnectionFrames,
    cancellation: CancellationToken,
    kind: PrivateSubscriptionKind,
    listing: Option<ResolvedListingRef>,
    target: crate::protocol::AuthenticatedTarget,
    adapter: BinanceAdapter,
    refresh_context: ExecutionContext,
    route: VenueRoute,
    product: BinanceProduct,
}

#[derive(Clone, Copy)]
enum StreamPath {
    Market,
    Public,
}

impl StreamPath {
    const fn path(self, product: BinanceProduct) -> &'static str {
        match (self, product) {
            (Self::Public, BinanceProduct::Perpetual) => "public/stream",
            (_, BinanceProduct::Perpetual) => "market/stream",
            (_, BinanceProduct::Spot | BinanceProduct::Future | BinanceProduct::Option) => "ws",
        }
    }
}

fn liveness_timeout(route: &VenueRoute) -> Duration {
    match route.scope.as_str() {
        crate::routing::PERPETUAL_SCOPE => Duration::from_secs(540),
        _ => Duration::from_secs(60),
    }
}

fn listing_identity<'a>(
    listing: Option<&'a ResolvedListingRef>,
    operation: &'static str,
) -> Result<(&'a VenueListingKey, &'a NativeSymbol), MarketParticipantError> {
    let listing = listing.ok_or_else(|| {
        protocol_error(
            operation,
            "the request carries no resolved listing reference",
        )
    })?;
    let symbol = listing.native_symbol.as_ref().ok_or_else(|| {
        protocol_error(operation, "the resolved listing publishes no native symbol")
    })?;
    Ok((&listing.key, symbol))
}

fn narrowing<'a>(
    listing: Option<&'a ResolvedListingRef>,
    operation: &'static str,
) -> Result<Option<&'a NativeSymbol>, MarketParticipantError> {
    match listing {
        None => Ok(None),
        Some(listing) => listing.native_symbol.as_ref().map(Some).ok_or_else(|| {
            protocol_error(operation, "the resolved listing publishes no native symbol")
        }),
    }
}

#[derive(Clone)]
enum SubscriptionKind {
    Candlestick {
        listing: VenueListingKey,
        symbol: NativeSymbol,
        interval: SmolStr,
    },
    Ticker {
        listing: VenueListingKey,
        symbol: NativeSymbol,
    },
    Trade {
        listing: VenueListingKey,
        symbol: NativeSymbol,
    },
    OrderBook {
        listing: VenueListingKey,
        symbol: NativeSymbol,
    },
    ForceOrder {
        route: VenueRoute,
        product: BinanceProduct,
    },
}

#[derive(serde::Deserialize)]
struct BinanceMarketParticipantInfo {
    #[serde(default)]
    symbols: Vec<BinanceMarketParticipantSymbol>,
}

#[derive(serde::Deserialize)]
struct BinanceMarketParticipantSymbol {
    symbol: SmolStr,
    #[serde(rename = "baseAsset")]
    base_asset: SmolStr,
    #[serde(rename = "quoteAsset")]
    quote_asset: SmolStr,
    #[serde(rename = "marginAsset")]
    margin_asset: Option<SmolStr>,
    #[serde(rename = "contractType")]
    contract_type: Option<SmolStr>,
    status: SmolStr,
    #[serde(rename = "onboardDate")]
    onboard_date: Option<i64>,
    #[serde(rename = "requiredMarginPercent")]
    required_margin_percent: Option<SmolStr>,
    #[serde(rename = "maintMarginPercent")]
    maintenance_margin_percent: Option<SmolStr>,
    #[serde(default)]
    filters: Vec<BinanceMarketParticipantFilter>,
}

#[derive(serde::Deserialize)]
struct BinanceMarketParticipantFilter {
    #[serde(rename = "filterType")]
    filter_type: SmolStr,
    #[serde(rename = "tickSize")]
    tick_size: Option<SmolStr>,
    #[serde(rename = "stepSize")]
    step_size: Option<SmolStr>,
    #[serde(rename = "minQty")]
    min_qty: Option<SmolStr>,
    #[serde(rename = "maxQty")]
    max_qty: Option<SmolStr>,
    #[serde(rename = "minNotional")]
    min_notional: Option<SmolStr>,
    #[serde(rename = "notional")]
    notional: Option<SmolStr>,
}

#[derive(serde::Deserialize)]
struct BinanceKlineEvent {
    #[serde(rename = "e")]
    event: SmolStr,
    #[serde(rename = "k")]
    kline: BinanceKlineEventData,
}

#[derive(serde::Deserialize)]
struct BinanceKlineEventData {
    #[serde(rename = "s")]
    symbol: SmolStr,
    #[serde(rename = "t")]
    open_time: i64,
    #[serde(rename = "i")]
    interval: SmolStr,
    #[serde(rename = "o")]
    open: SmolStr,
    #[serde(rename = "h")]
    high: SmolStr,
    #[serde(rename = "l")]
    low: SmolStr,
    #[serde(rename = "c")]
    close: SmolStr,
    #[serde(rename = "v")]
    volume: SmolStr,
}

#[derive(serde::Deserialize)]
struct BinanceTickerEvent {
    #[serde(rename = "e")]
    event: SmolStr,
    #[serde(rename = "s")]
    symbol: SmolStr,
    #[serde(rename = "o")]
    open: SmolStr,
    #[serde(rename = "h")]
    high: SmolStr,
    #[serde(rename = "l")]
    low: SmolStr,
    #[serde(rename = "c")]
    close: SmolStr,
    #[serde(rename = "v")]
    volume: SmolStr,
    #[serde(rename = "q")]
    quote_volume: SmolStr,
}

#[derive(serde::Deserialize)]
struct BinanceTradeEvent {
    #[serde(rename = "e")]
    event: SmolStr,
    #[serde(rename = "a")]
    id: u64,
    #[serde(rename = "s")]
    symbol: SmolStr,
    #[serde(rename = "p")]
    price: SmolStr,
    #[serde(rename = "q")]
    quantity: SmolStr,
    #[serde(rename = "T")]
    timestamp: i64,
    #[serde(rename = "m")]
    buyer_is_maker: bool,
}

#[derive(serde::Deserialize)]
struct BinanceDepthEvent {
    #[serde(rename = "e")]
    event: SmolStr,
    #[serde(rename = "E")]
    timestamp: i64,
    #[serde(rename = "s")]
    symbol: SmolStr,
    #[serde(rename = "U")]
    first_update_id: u64,
    #[serde(rename = "u")]
    last_update_id: u64,
    #[serde(rename = "pu")]
    previous_update_id: Option<u64>,
    #[serde(rename = "b")]
    bids: Vec<BinanceLevelRow>,
    #[serde(rename = "a")]
    asks: Vec<BinanceLevelRow>,
}

#[derive(serde::Deserialize)]
struct BinanceForceOrderEvent {
    #[serde(rename = "e")]
    event: SmolStr,
    #[serde(rename = "E")]
    timestamp: i64,
    #[serde(rename = "o")]
    order: BinanceForceOrderData,
}

#[derive(serde::Deserialize)]
struct BinanceForceOrderData {
    #[serde(rename = "s")]
    symbol: SmolStr,
    #[serde(rename = "S")]
    side: SmolStr,
    #[serde(rename = "q")]
    quantity: SmolStr,
    #[serde(rename = "p")]
    price: SmolStr,
    #[serde(rename = "T")]
    order_timestamp: i64,
}

#[derive(serde::Deserialize)]
struct BinanceCombinedStreamEvent<T> {
    data: T,
}

#[derive(serde::Deserialize)]
struct BinanceOpenInterestRow {
    #[serde(rename = "timestamp")]
    timestamp: i64,
    #[serde(rename = "sumOpenInterest")]
    sum_open_interest: SmolStr,
    #[serde(rename = "sumOpenInterestValue")]
    sum_open_interest_value: SmolStr,
}

#[derive(serde::Deserialize)]
struct BinanceRatioRow {
    #[serde(rename = "timestamp")]
    timestamp: i64,
    #[serde(rename = "longShortRatio")]
    long_short_ratio: SmolStr,
}

#[derive(serde::Deserialize)]
struct BinanceTakerRow {
    #[serde(rename = "timestamp")]
    timestamp: i64,
    #[serde(rename = "sellVol")]
    sell_volume: SmolStr,
    #[serde(rename = "buyVol")]
    buy_volume: SmolStr,
}

#[derive(Clone, Copy)]
enum StatisticsKind {
    OpenInterest,
    LongShort,
    TopTraderAccount,
    TopTraderPosition,
    Taker,
}

struct VenueRules {
    product: BinanceProduct,
    instrument: Instrument,
    quote: AssetId,
    base: AssetId,
    tick: Tick,
    step: Quantity,
    minimum_quantity: Option<Quantity>,
    maximum_quantity: Option<Quantity>,
    minimum_notional: Option<Quantity>,
    required_margin_percent: Option<SmolStr>,
    maintenance_margin_percent: Option<SmolStr>,
}

fn decode_subscription_event(
    payload: &[u8],
    kind: &SubscriptionKind,
) -> Result<Option<ProtocolEvent>, MarketParticipantError> {
    match kind {
        SubscriptionKind::Candlestick {
            listing,
            symbol,
            interval,
        } => {
            let event = match decode_stream_event::<BinanceKlineEvent>(payload) {
                Some(event) => event,
                None => return Ok(None),
            };
            if event.event != "kline"
                || event.kline.interval != *interval
                || !event.kline.symbol.eq_ignore_ascii_case(symbol.as_str())
            {
                return Ok(None);
            }
            let data = NativeCandlestick {
                timestamp: event.kline.open_time,
                open: parse_price(&event.kline.open, "decode Binance kline event")?,
                high: parse_price(&event.kline.high, "decode Binance kline event")?,
                low: parse_price(&event.kline.low, "decode Binance kline event")?,
                close: parse_price(&event.kline.close, "decode Binance kline event")?,
                volume: parse_quantity(&event.kline.volume, "decode Binance kline event")?,
            };
            Ok(Some(ProtocolEvent::Candlestick(NativeListing::new(
                listing.clone(),
                data,
            ))))
        }
        SubscriptionKind::Ticker { listing, symbol } => {
            let event = match decode_stream_event::<BinanceTickerEvent>(payload) {
                Some(event) if event.event == "24hrTicker" => event,
                _ => return Ok(None),
            };
            if !event.symbol.eq_ignore_ascii_case(symbol.as_str()) {
                return Ok(None);
            }
            let data = NativeTicker {
                monotonic_id: None,
                open: parse_price(&event.open, "decode Binance ticker event")?,
                high: Some(parse_price(&event.high, "decode Binance ticker event")?),
                low: Some(parse_price(&event.low, "decode Binance ticker event")?),
                close: parse_price(&event.close, "decode Binance ticker event")?,
                traded_quantity: Some(parse_quantity(
                    &event.volume,
                    "decode Binance ticker event",
                )?),
                traded_notional: Some(parse_quantity(
                    &event.quote_volume,
                    "decode Binance ticker event",
                )?),
            };
            Ok(Some(ProtocolEvent::Ticker(NativeListing::new(
                listing.clone(),
                data,
            ))))
        }
        SubscriptionKind::Trade { listing, symbol } => {
            let event = match decode_stream_event::<BinanceTradeEvent>(payload) {
                Some(event) if event.event == "aggTrade" => event,
                _ => return Ok(None),
            };
            if !event.symbol.eq_ignore_ascii_case(symbol.as_str()) {
                return Ok(None);
            }
            let data = NativeTrade {
                id: MarketTradeId::Numeric(event.id),
                price: parse_price(&event.price, "decode Binance trade event")?,
                quantity: parse_quantity(&event.quantity, "decode Binance trade event")?,
                side: if event.buyer_is_maker {
                    Side::Sell
                } else {
                    Side::Buy
                },
                timestamp: event.timestamp,
            };
            Ok(Some(ProtocolEvent::Trade(NativeListing::new(
                listing.clone(),
                data,
            ))))
        }
        SubscriptionKind::OrderBook { listing, symbol } => {
            let event = match decode_stream_event::<BinanceDepthEvent>(payload) {
                Some(event) if event.event == "depthUpdate" => event,
                _ => return Ok(None),
            };
            if !event.symbol.eq_ignore_ascii_case(symbol.as_str()) {
                return Ok(None);
            }
            let data = NativeOrderBookUpdate {
                snapshot: false,
                first_sequence: MarketSequence::Numeric(event.first_update_id),
                last_sequence: MarketSequence::Numeric(event.last_update_id),
                previous_sequence: event.previous_update_id.map(MarketSequence::Numeric),
                timestamp: Some(event.timestamp),
                bids: decode_levels(&event.bids, "decode Binance order book update")?,
                asks: decode_levels(&event.asks, "decode Binance order book update")?,
            };
            Ok(Some(ProtocolEvent::OrderBook(NativeListing::new(
                listing.clone(),
                data,
            ))))
        }
        SubscriptionKind::ForceOrder { route, product } => {
            let event = match decode_stream_event::<BinanceForceOrderEvent>(payload) {
                Some(event) if event.event == "forceOrder" => event,
                _ => return Ok(None),
            };
            let listing = BinanceCodec::listing_key(route, *product, event.order.symbol.as_str())?;
            let position_side = if event.order.side.eq_ignore_ascii_case("SELL") {
                PositionSide::Long
            } else {
                PositionSide::Short
            };
            let data = NativeForceOrder {
                position_side,
                price: parse_price(&event.order.price, "decode Binance force order price")?,
                quantity: parse_quantity(
                    &event.order.quantity,
                    "decode Binance force order quantity",
                )?,
                timestamp: if event.order.order_timestamp > 0 {
                    event.order.order_timestamp
                } else {
                    event.timestamp
                },
            };
            Ok(Some(ProtocolEvent::ForceOrder(NativeListing::new(
                listing, data,
            ))))
        }
    }
}

fn decode_stream_event<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Option<T> {
    serde_json::from_slice(payload).ok().or_else(|| {
        serde_json::from_slice::<BinanceCombinedStreamEvent<T>>(payload)
            .ok()
            .map(|event| event.data)
    })
}

fn decode_ticker(
    wire: &BinanceTickerWire,
    operation: &'static str,
) -> Result<NativeTicker, MarketParticipantError> {
    Ok(NativeTicker {
        monotonic_id: None,
        open: parse_price(&wire.open_price, operation)?,
        high: wire
            .high_price
            .as_deref()
            .map(|value| parse_price(value, operation))
            .transpose()?,
        low: wire
            .low_price
            .as_deref()
            .map(|value| parse_price(value, operation))
            .transpose()?,
        close: parse_price(&wire.last_price, operation)?,
        traded_quantity: Some(parse_quantity(&wire.volume, operation)?),
        traded_notional: Some(parse_quantity(&wire.quote_volume, operation)?),
    })
}

fn decode_levels(
    levels: &[BinanceLevelRow],
    operation: &'static str,
) -> Result<Vec<NativeOrderBookLevel>, MarketParticipantError> {
    levels
        .iter()
        .map(|level| {
            Ok(NativeOrderBookLevel {
                price: parse_price(&level.price, operation)?,
                quantity: parse_quantity(&level.quantity, operation)?,
            })
        })
        .collect()
}

fn decode_market_entry(
    route: &VenueRoute,
    product: BinanceProduct,
    symbol: BinanceMarketParticipantSymbol,
) -> Result<NativeCatalogEntry, MarketParticipantError> {
    let native_symbol = NativeSymbol::new(symbol.symbol.as_str()).map_err(|error| {
        protocol_error("decode Binance native symbol", format_smolstr!("{}", error))
    })?;
    let listing = BinanceCodec::listing_key(route, product, symbol.symbol.as_str())?;
    let base_terms = venue_asset(&symbol.base_asset)?;
    let quote_terms = venue_asset(&symbol.quote_asset)?;
    let base = base_terms.asset_id();
    let quote = quote_terms.asset_id();
    let tick = parse::<Tick>(
        symbol
            .filters
            .iter()
            .find(|filter| filter.filter_type == "PRICE_FILTER")
            .and_then(|filter| filter.tick_size.as_deref())
            .ok_or_else(|| {
                protocol_error("decode Binance market filters", "price tick is missing")
            })?,
        "decode Binance price tick",
    )?;
    let lot = symbol
        .filters
        .iter()
        .find(|filter| filter.filter_type == "LOT_SIZE")
        .ok_or_else(|| protocol_error("decode Binance market filters", "lot size is missing"))?;
    let step = parse::<Quantity>(
        lot.step_size.as_deref().ok_or_else(|| {
            protocol_error("decode Binance quantity tick", "step size is missing")
        })?,
        "decode Binance quantity tick",
    )?;
    let minimum_quantity = parse_quantity_bound(lot.min_qty.as_deref())?;
    let maximum_quantity = parse_quantity_bound(lot.max_qty.as_deref())?;
    let minimum_notional = symbol
        .filters
        .iter()
        .find_map(|filter| match filter.filter_type.as_str() {
            "NOTIONAL" => filter.min_notional.as_deref(),
            "MIN_NOTIONAL" => filter.notional.as_deref(),
            _ => None,
        })
        .map(|value| parse::<Quantity>(value, "decode Binance minimum notional"))
        .transpose()?;
    let (instrument, mut asset_terms, reference_terms) =
        market_instrument(product, &symbol, &base, &quote)?;
    asset_terms.push(base_terms);
    asset_terms.push(quote_terms);
    let rules = trading_rules(VenueRules {
        product,
        instrument: instrument.clone(),
        quote,
        base,
        tick,
        step,
        minimum_quantity,
        maximum_quantity,
        minimum_notional,
        required_margin_percent: symbol.required_margin_percent.clone(),
        maintenance_margin_percent: symbol.maintenance_margin_percent.clone(),
    })?;
    Ok(NativeCatalogEntry {
        listing,
        native_symbol,
        aliases: Vec::new(),
        identifiers: VenueListingIdentifiers::Binance(BinanceListingIdentifiers {
            product,
            future_series: None,
            option_series: None,
        }),
        rules,
        status: listing_status(&symbol.status),
        available_from: symbol.onboard_date.filter(|value| *value > 0),
        available_until: None,
        instrument,
        asset_terms,
        reference_terms,
        asset_codes: Vec::new(),
    })
}

fn market_instrument(
    product: BinanceProduct,
    symbol: &BinanceMarketParticipantSymbol,
    base: &AssetId,
    quote: &AssetId,
) -> Result<(Instrument, Vec<AssetTerms>, Vec<PriceReferenceTerms>), MarketParticipantError> {
    let (terms, asset_terms, reference_terms) = match product {
        BinanceProduct::Spot => (
            InstrumentTerms::Spot(SpotTerms::new(base.clone(), quote.clone()).map_err(
                |error| {
                    protocol_error(
                        "decode Binance spot instrument",
                        format_smolstr!("{}", error),
                    )
                },
            )?),
            Vec::new(),
            Vec::new(),
        ),
        BinanceProduct::Perpetual => {
            let settlement_terms =
                venue_asset(symbol.margin_asset.as_deref().ok_or_else(|| {
                    protocol_error(
                        "decode Binance perpetual instrument",
                        "margin asset is missing",
                    )
                })?)?;
            let mark_terms = mark_reference(base, quote)?;
            let perpetual = PerpetualTerms::new(
                DerivativeUnderlying {
                    reference: UnderlyingRef::Asset(base.clone()),
                    price_reference: mark_terms.reference_id().map_err(|error| {
                        protocol_error(
                            "decode Binance price reference",
                            format_smolstr!("{}", error),
                        )
                    })?,
                },
                quote.clone(),
                settlement_terms.asset_id(),
                ContractSize::new(
                    ContractUnit::asset_quantity(Quantity::from(1_u8), base.clone()).map_err(
                        |error| {
                            protocol_error(
                                "decode Binance contract size",
                                format_smolstr!("{}", error),
                            )
                        },
                    )?,
                )
                .map_err(|error| {
                    protocol_error("decode Binance contract size", format_smolstr!("{}", error))
                })?,
                DerivativePnlModel::Linear,
                None,
            )
            .map_err(|error| {
                protocol_error(
                    "decode Binance perpetual instrument",
                    format_smolstr!("{}", error),
                )
            })?;
            (
                InstrumentTerms::Perpetual(perpetual),
                vec![settlement_terms],
                vec![mark_terms],
            )
        }
        BinanceProduct::Future | BinanceProduct::Option => {
            return Err(protocol_error(
                "decode Binance market instrument",
                "the Binance adapter serves spot and USD-M perpetual only",
            ));
        }
    };
    let instrument = Instrument::new(terms).map_err(|error| {
        protocol_error(
            "decode Binance market instrument",
            format_smolstr!("{}", error),
        )
    })?;
    Ok((instrument, asset_terms, reference_terms))
}

fn trading_rules(venue: VenueRules) -> Result<TradingRules, MarketParticipantError> {
    let common = CommonTradingRules {
        price: PriceRule {
            quote_asset: venue.quote.clone(),
            basis: PriceBasis::PerAssetUnit,
            tick: venue.tick,
            minimum: None,
            maximum: None,
        },
        quantity: QuantityRule {
            unit: QuantityUnit::AssetUnits {
                asset: venue.base.clone(),
            },
            step: venue.step,
            minimum: venue.minimum_quantity,
            maximum: venue.maximum_quantity,
        },
        notional: venue.minimum_notional.map(|minimum| NotionalRule {
            asset: venue.quote.clone(),
            minimum: Some(minimum),
            maximum: None,
        }),
        venue_policy: Arc::new(VenueTradingPolicy::continuous()),
    };
    let product = match venue.product {
        BinanceProduct::Spot => ProductTradingRules::Spot(
            SpotTradingRules::new(
                LotPolicy::FractionalAllowed {
                    lot_size: venue.step,
                    minimum_fraction: None,
                },
                ShortSalePolicy::NotApplicable,
            )
            .map_err(|error| {
                protocol_error("decode Binance spot rules", format_smolstr!("{}", error))
            })?,
        ),
        BinanceProduct::Perpetual => {
            let initial_margin = margin_fraction(
                venue.required_margin_percent.as_deref(),
                "decode Binance initial margin",
            )?;
            let maintenance_margin = margin_fraction(
                venue.maintenance_margin_percent.as_deref(),
                "decode Binance maintenance margin",
            )?;
            let settlement = match venue.instrument.terms() {
                InstrumentTerms::Perpetual(terms) => terms.settlement_asset().clone(),
                _ => {
                    return Err(protocol_error(
                        "decode Binance perpetual rules",
                        "the instrument carries no perpetual terms",
                    ));
                }
            };
            ProductTradingRules::Perpetual(PerpetualTradingRules {
                risk: PerpetualRisk::Published(
                    PerpetualMarginTerms::new(
                        MarginProfile::new(
                            MarginMode::Cross,
                            initial_margin,
                            maintenance_margin,
                            settlement.clone(),
                        )
                        .map_err(|error| {
                            protocol_error(
                                "decode Binance margin profile",
                                format_smolstr!("{}", error),
                            )
                        })?,
                        vec![leverage_tier(
                            initial_margin,
                            maintenance_margin,
                            venue.maximum_quantity,
                        )?],
                    )
                    .map_err(|error| {
                        protocol_error("decode Binance margin terms", format_smolstr!("{}", error))
                    })?,
                ),
                funding: FundingSchedule {
                    interval: FundingInterval::Adaptive,
                    funding_rate_cap: None,
                    funding_rate_floor: None,
                    asset: settlement,
                },
                open_interest_cap: None,
            })
        }
        BinanceProduct::Future | BinanceProduct::Option => {
            return Err(protocol_error(
                "decode Binance market rules",
                "the Binance adapter serves spot and USD-M perpetual only",
            ));
        }
    };
    let rules = TradingRules {
        common,
        product: Arc::new(product),
    };
    rules.validate_for(&venue.instrument).map_err(|error| {
        protocol_error("decode Binance trading rules", format_smolstr!("{}", error))
    })?;
    Ok(rules)
}

fn leverage_tier(
    initial_margin: Fraction,
    maintenance_margin: Fraction,
    maximum_quantity: Option<Quantity>,
) -> Result<LeverageTier, MarketParticipantError> {
    let notional_cap = maximum_quantity.ok_or_else(|| {
        protocol_error(
            "decode Binance leverage",
            "the venue published no maximum order size for the contract",
        )
    })?;
    let scale = Quantity::from(1_u8).inner();
    let max_leverage = u32::try_from(scale / initial_margin.ratio().inner())
        .map_err(|_| protocol_error("decode Binance leverage", "leverage is out of range"))?;
    LeverageTier::new(notional_cap, max_leverage, maintenance_margin)
        .map_err(|error| protocol_error("decode Binance leverage", format_smolstr!("{}", error)))
}

pub(super) fn venue_asset(code: &str) -> Result<AssetTerms, MarketParticipantError> {
    let issuer = TokenIssuerId::new(BINANCE_ISSUER).map_err(|error| {
        protocol_error("decode Binance asset issuer", format_smolstr!("{}", error))
    })?;
    let namespace = IdentifierNamespace::new(BINANCE_ISSUER).map_err(|error| {
        protocol_error(
            "decode Binance asset namespace",
            format_smolstr!("{}", error),
        )
    })?;
    let token = IdentifierValue::new(code).map_err(|error| {
        protocol_error("decode Binance asset code", format_smolstr!("{}", error))
    })?;
    let terms = CryptoTokenTerms {
        origin: CryptoTokenOrigin::VenueIssued {
            issuer,
            namespace,
            token: TokenReference::Native { code: token },
        },
    };
    terms
        .validate()
        .map_err(|error| protocol_error("decode Binance asset", format_smolstr!("{}", error)))?;
    Ok(AssetTerms::CryptoToken(terms))
}

fn mark_reference(
    base: &AssetId,
    quote: &AssetId,
) -> Result<PriceReferenceTerms, MarketParticipantError> {
    let source = ReferenceSourceId::new(crate::routing::BINANCE_VENUE).map_err(|error| {
        protocol_error(
            "decode Binance price reference",
            format_smolstr!("{}", error),
        )
    })?;
    let schema_version = ReferenceTermsVersion::new("v1").map_err(|error| {
        protocol_error(
            "decode Binance price reference",
            format_smolstr!("{}", error),
        )
    })?;
    let terms = PriceReferenceTerms {
        underlying: UnderlyingRef::Asset(base.clone()),
        quote_asset: quote.clone(),
        basis: PriceBasis::PerAssetUnit,
        observation: ReferenceObservation {
            source,
            kind: ReferenceObservationKind::Mark,
        },
        aggregation: ReferenceAggregation::Direct,
        session: ReferenceSessionConvention::Continuous,
        settlement: ReferenceSettlementConvention::Continuous,
        schema_version,
    };
    terms.reference_id().map_err(|error| {
        protocol_error(
            "decode Binance price reference",
            format_smolstr!("{}", error),
        )
    })?;
    Ok(terms)
}

fn listing_status(status: &str) -> ListingStatus {
    match status {
        "TRADING" => ListingStatus::Trading,
        "PRE_TRADING" | "PENDING_TRADING" => ListingStatus::Scheduled,
        "DELIVERING" | "DELIVERED" | "CLOSE" => ListingStatus::Expired,
        "PRE_SETTLE" | "SETTLING" => ListingStatus::Settled,
        _ => ListingStatus::Halted,
    }
}

fn margin_fraction(
    value: Option<&str>,
    operation: &'static str,
) -> Result<Fraction, MarketParticipantError> {
    let percent = value
        .ok_or_else(|| protocol_error(operation, "the venue published no margin percent"))?
        .parse::<Quantity>()
        .map_err(|error| protocol_error(operation, format_smolstr!("{}", error)))?;
    Fraction::new(Ratio::from_inner(percent.inner() / 100))
        .map_err(|error| protocol_error(operation, format_smolstr!("{}", error)))
}

fn parse<T: std::str::FromStr>(
    value: &str,
    operation: &'static str,
) -> Result<T, MarketParticipantError> {
    value
        .parse()
        .map_err(|_| protocol_error(operation, "invalid numeric value"))
}

fn parse_quantity_bound(value: Option<&str>) -> Result<Option<Quantity>, MarketParticipantError> {
    value
        .filter(|value| !value.is_empty())
        .map(|value| parse::<Quantity>(value, "decode Binance quantity bound"))
        .transpose()
}

fn parse_price(value: &str, operation: &'static str) -> Result<Price, MarketParticipantError> {
    value
        .parse()
        .map_err(|error| protocol_error(operation, format_smolstr!("{}", error)))
}

fn parse_quantity(
    value: &str,
    operation: &'static str,
) -> Result<Quantity, MarketParticipantError> {
    value
        .parse()
        .map_err(|error| protocol_error(operation, format_smolstr!("{}", error)))
}

fn protocol_error(operation: &'static str, reason: impl Into<SmolStr>) -> MarketParticipantError {
    MarketParticipantError::ProtocolError {
        operation: operation.into(),
        reason: reason.into(),
        raw_data: None,
    }
}

fn serialize_parameters(parameters: &[(String, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.extend_pairs(
        parameters
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    );
    serializer.finish()
}

fn side_text(side: Side) -> &'static str {
    match side {
        Side::Buy => "BUY",
        Side::Sell => "SELL",
    }
}

fn position_side_text(side: PositionSide) -> &'static str {
    match side {
        PositionSide::Net => "BOTH",
        PositionSide::Long => "LONG",
        PositionSide::Short => "SHORT",
    }
}

fn time_in_force_text(value: TimeInForce) -> &'static str {
    match value {
        TimeInForce::GoodTillCancelled => "GTC",
        TimeInForce::ImmediateOrCancel => "IOC",
        TimeInForce::FillOrKill => "FOK",
        TimeInForce::PostOnly => "GTX",
        TimeInForce::GoodTillDate(_) => "GTD",
    }
}

fn decode_vec_or_single<T: serde::de::DeserializeOwned>(
    body: &[u8],
    operation: &'static str,
) -> Result<Vec<T>, MarketParticipantError> {
    if let Ok(rows) = serde_json::from_slice::<Vec<T>>(body) {
        return Ok(rows);
    } else if let Ok(row) = serde_json::from_slice::<T>(body) {
        return Ok(vec![row]);
    }
    Err(MarketParticipantError::DecodeFailed {
        operation: operation.into(),
        raw_data: Bytes::copy_from_slice(body),
    })
}

fn statistics_period(timeframe: StatisticsTimeFrame) -> &'static str {
    match timeframe {
        StatisticsTimeFrame::Minute5 => "5m",
        StatisticsTimeFrame::Minute15 => "15m",
        StatisticsTimeFrame::Minute30 => "30m",
        StatisticsTimeFrame::Hour1 => "1h",
        StatisticsTimeFrame::Hour2 => "2h",
        StatisticsTimeFrame::Hour4 => "4h",
        StatisticsTimeFrame::Hour6 => "6h",
        StatisticsTimeFrame::Hour12 => "12h",
        StatisticsTimeFrame::Day1 => "1d",
    }
}

pub(crate) fn current_timestamp_ms() -> u64 {
    (time::OffsetDateTime::now_utc()
        .unix_timestamp_nanos()
        .max(0) as u128
        / 1_000_000) as u64
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use market_participants_model::{
        decimal::Quantity,
        identifier::{NativeSymbol, VenueId, VenueScopeId},
        instrument::InstrumentKind,
        listing::{
            identifiers::{BinanceListingIdentifiers, VenueListingIdentifiers},
            listing_impl::ListingStatus,
            route::{VenueListingKey, VenueRoute},
            rules::{PerpetualRisk, ProductTradingRules},
            venue::BinanceProduct,
        },
        market::{book::MarketSequence, units::PositionSide},
        routing::CapabilityId,
    };
    use network::transport::hub::{TransportHub, TransportPolicy};

    use super::{
        BinanceAdapter, BinanceCodec, BinanceMarketParticipantFilter,
        BinanceMarketParticipantSymbol, BinanceTickerWire, SubscriptionKind, decode_market_entry,
        decode_subscription_event, decode_ticker, listing_identity,
    };
    use crate::{
        adapter::{ExecutionContext, MarketParticipantAdapter, ProtocolEvent},
        connection::ConnectionPool,
        error::MarketParticipantError,
        market_participants::model::TimeFrame,
        protocol::RequestIdentity,
    };

    fn route(scope: &str) -> VenueRoute {
        VenueRoute::new(
            VenueId::Binance,
            VenueScopeId::new(scope).expect("the scope should be canonical"),
        )
    }

    fn product(scope: &str) -> BinanceProduct {
        BinanceCodec::product(&route(scope)).expect("the scope has a product family")
    }

    fn listing(scope: &str, symbol: &str) -> VenueListingKey {
        BinanceCodec::listing_key(&route(scope), product(scope), symbol)
            .expect("the symbol names a listing")
    }

    fn symbol(value: &str) -> NativeSymbol {
        NativeSymbol::new(value).expect("the fixture symbol is valid")
    }

    fn context(scope_name: &str) -> ExecutionContext {
        let transport = Arc::new(TransportHub::new(TransportPolicy::default()));
        let pool = Arc::new(ConnectionPool::with_transport(transport));
        let scene = route(scope_name);
        let origins = app_config::settings::Config::default()
            .venues
            .get(&scene.venue)
            .expect("a built-in venue is configured")
            .origins(scene.scope.as_str())
            .expect("the fixture scope is one the venue publishes");
        let identity = RequestIdentity::new(scene, CapabilityId::MarketCatalog);
        ExecutionContext::new(identity, origins, reqwest::Client::new(), pool)
    }

    fn filter(filter_type: &str, tick: &str, step: &str) -> BinanceMarketParticipantFilter {
        BinanceMarketParticipantFilter {
            filter_type: filter_type.into(),
            tick_size: (!tick.is_empty()).then(|| tick.into()),
            step_size: (!step.is_empty()).then(|| step.into()),
            min_qty: None,
            max_qty: None,
            min_notional: None,
            notional: None,
        }
    }

    fn lot(step: &str, minimum: &str, maximum: &str) -> BinanceMarketParticipantFilter {
        BinanceMarketParticipantFilter {
            filter_type: "LOT_SIZE".into(),
            tick_size: None,
            step_size: Some(step.into()),
            min_qty: Some(minimum.into()),
            max_qty: Some(maximum.into()),
            min_notional: None,
            notional: None,
        }
    }

    #[test]
    fn limits_follow_the_venue_book_depth_and_kline_set() {
        let adapter = BinanceAdapter::new();
        let spot = adapter.limits(&route(crate::routing::SPOT_SCOPE));
        let perpetual = adapter.limits(&route(crate::routing::PERPETUAL_SCOPE));

        assert_eq!(spot.order_book_depth_limit, 5_000);
        assert_eq!(perpetual.order_book_depth_limit, 1_000);
        assert!(spot.timeframes.contains(&TimeFrame::Second1));
        assert!(!perpetual.timeframes.contains(&TimeFrame::Second1));
        assert_eq!(spot.timeframes.len(), 16);
        assert_eq!(perpetual.timeframes.len(), 15);
        assert!(!spot.timeframes.contains(&TimeFrame::Day2));
        assert!(!perpetual.timeframes.contains(&TimeFrame::Day2));
        assert!(spot.timeframes.contains(&TimeFrame::Month1));
        assert_eq!(
            adapter.limits(&route("swap")).order_book_depth_limit,
            0,
            "a scope the adapter does not trade publishes no depth"
        );
    }

    #[test]
    fn the_second_candle_is_encoded_for_spot_only() {
        assert_eq!(
            BinanceCodec
                .encode_timeframe(&route(crate::routing::SPOT_SCOPE), TimeFrame::Second1)
                .expect("spot publishes the second candle"),
            "1s"
        );
        assert!(matches!(
            BinanceCodec
                .encode_timeframe(&route(crate::routing::PERPETUAL_SCOPE), TimeFrame::Second1),
            Err(MarketParticipantError::UnsupportedTimeFrame { timeframe, .. }) if timeframe == "1s"
        ));
        assert!(matches!(
            BinanceCodec
                .encode_timeframe(&route(crate::routing::SPOT_SCOPE), TimeFrame::Day2),
            Err(MarketParticipantError::UnsupportedTimeFrame { timeframe, .. }) if timeframe == "2d"
        ));
    }

    #[test]
    fn a_spot_catalog_entry_carries_its_instrument_identifiers_and_rules() {
        let route = route(crate::routing::SPOT_SCOPE);
        let mut notional = filter("NOTIONAL", "", "");
        notional.min_notional = Some("5".into());
        let entry = decode_market_entry(
            &route,
            BinanceProduct::Spot,
            BinanceMarketParticipantSymbol {
                symbol: "BTCUSDT".into(),
                base_asset: "BTC".into(),
                quote_asset: "USDT".into(),
                margin_asset: None,
                contract_type: None,
                status: "TRADING".into(),
                onboard_date: None,
                required_margin_percent: None,
                maintenance_margin_percent: None,
                filters: vec![
                    filter("PRICE_FILTER", "0.01", ""),
                    lot("0.001", "0.001", "100"),
                    notional,
                ],
            },
        )
        .expect("the fixture symbol should decode");

        assert_eq!(entry.native_symbol, symbol("BTCUSDT"));
        assert_eq!(entry.instrument.kind(), InstrumentKind::Spot);
        assert_eq!(
            entry.listing,
            listing(crate::routing::SPOT_SCOPE, "BTCUSDT")
        );
        assert_eq!(
            entry.identifiers,
            VenueListingIdentifiers::Binance(BinanceListingIdentifiers {
                product: BinanceProduct::Spot,
                future_series: None,
                option_series: None,
            })
        );
        assert_eq!(entry.status, ListingStatus::Trading);
        assert_eq!(entry.available_from, None);
        assert_eq!(entry.rules.common.price.tick.to_string(), "0.01");
        assert_eq!(entry.rules.common.quantity.step.to_string(), "0.001");
        assert_eq!(
            entry.rules.common.quantity.minimum,
            Some("0.001".parse::<Quantity>().expect("quantity"))
        );
        assert_eq!(
            entry.rules.common.quantity.maximum,
            Some("100".parse::<Quantity>().expect("quantity"))
        );
        assert_eq!(
            entry
                .rules
                .common
                .notional
                .as_ref()
                .and_then(|rule| rule.minimum)
                .map(|value| value.to_string()),
            Some("5".to_owned())
        );
        entry
            .rules
            .validate_for(&entry.instrument)
            .expect("the decoded rules must hold for the decoded instrument");
    }

    #[test]
    fn a_perpetual_catalog_entry_states_the_venue_margin_and_contract_size() {
        let route = route(crate::routing::PERPETUAL_SCOPE);
        let mut notional = filter("MIN_NOTIONAL", "", "");
        notional.notional = Some("5".into());
        let entry = decode_market_entry(
            &route,
            BinanceProduct::Perpetual,
            BinanceMarketParticipantSymbol {
                symbol: "BTCUSDT".into(),
                base_asset: "BTC".into(),
                quote_asset: "USDT".into(),
                margin_asset: Some("USDT".into()),
                contract_type: Some("PERPETUAL".into()),
                status: "TRADING".into(),
                onboard_date: Some(1_600_000_000_000),
                required_margin_percent: Some("5.0000".into()),
                maintenance_margin_percent: Some("2.5000".into()),
                filters: vec![
                    filter("PRICE_FILTER", "0.10", ""),
                    lot("0.001", "0.001", "1000"),
                    notional,
                ],
            },
        )
        .expect("the fixture symbol should decode");

        assert_eq!(entry.instrument.kind(), InstrumentKind::Perpetual);
        assert_eq!(entry.available_from, Some(1_600_000_000_000));
        let market_participants_model::instrument::InstrumentTerms::Perpetual(terms) =
            entry.instrument.terms()
        else {
            panic!("the entry must carry a perpetual instrument");
        };
        assert_eq!(terms.contract_size().quantity().to_string(), "1");
        assert_eq!(terms.price_quote(), terms.settlement_asset());
        let ProductTradingRules::Perpetual(rules) = entry.rules.product.as_ref() else {
            panic!("the entry must carry perpetual rules");
        };
        let PerpetualRisk::Published(margin_terms) = &rules.risk else {
            panic!("the venue publishes these terms with the contract itself");
        };
        assert_eq!(margin_terms.leverage.len(), 1);
        assert_eq!(margin_terms.leverage[0].max_leverage, 20);
        assert_eq!(margin_terms.leverage[0].notional_cap.to_string(), "1000");
        assert_eq!(margin_terms.margin.initial_margin.to_string(), "0.05");
        assert_eq!(margin_terms.margin.maintenance_margin.to_string(), "0.025");
        assert_eq!(rules.funding.asset, *terms.price_quote());
        entry
            .rules
            .validate_for(&entry.instrument)
            .expect("the decoded rules must hold for the decoded instrument");
    }

    #[test]
    fn an_unknown_listing_status_is_halted() {
        assert_eq!(super::listing_status("TRADING"), ListingStatus::Trading);
        assert_eq!(
            super::listing_status("PENDING_TRADING"),
            ListingStatus::Scheduled
        );
        assert_eq!(
            super::listing_status("SOMETHING_NEW"),
            ListingStatus::Halted
        );
    }

    #[test]
    fn a_request_without_a_native_symbol_is_rejected() {
        let error = listing_identity(None, "fetch ticker")
            .expect_err("a missing listing reference cannot be encoded");
        assert!(matches!(
            error,
            MarketParticipantError::ProtocolError { operation, .. } if operation == "fetch ticker"
        ));
    }

    #[test]
    fn a_request_url_is_the_venues_own_origin_with_its_path() {
        let spot = context(crate::routing::SPOT_SCOPE);
        let perpetual = context(crate::routing::PERPETUAL_SCOPE);

        assert_eq!(
            BinanceAdapter::http_url(&spot, "api/v3/klines")
                .expect("the spot origin joins its path")
                .as_str(),
            "https://api.binance.com/api/v3/klines",
            "the HTTP request dials the origin the spot scope is configured with"
        );
        assert_eq!(
            BinanceAdapter::websocket_url(&spot, "ws")
                .expect("the spot origin joins its path")
                .as_str(),
            "wss://stream.binance.com:9443/ws",
            "and the socket dials the WebSocket origin of that same scope"
        );
        assert_eq!(
            BinanceAdapter::http_url(&perpetual, "fapi/v1/klines")
                .expect("the perpetual origin joins its path")
                .as_str(),
            "https://fapi.binance.com/fapi/v1/klines",
            "another scope of the venue answers its own origin rather than the first one"
        );
    }

    #[test]
    fn decodes_spot_raw_stream_kline_event() {
        let key = listing(crate::routing::SPOT_SCOPE, "BTCUSDT");
        let event = decode_subscription_event(
            br#"{"e":"kline","k":{"s":"BTCUSDT","t":1700000000000,"i":"1m","o":"100","h":"110","l":"90","c":"105","v":"12.5"}}"#,
            &SubscriptionKind::Candlestick {
                listing: key.clone(),
                symbol: symbol("BTCUSDT"),
                interval: "1m".into(),
            },
        )
        .expect("event should decode")
        .expect("event should match");

        let ProtocolEvent::Candlestick(candlestick) = event else {
            panic!("expected candlestick event");
        };
        assert_eq!(candlestick.listing, key);
        assert_eq!(candlestick.data.timestamp, 1_700_000_000_000);
        assert_eq!(candlestick.data.close.to_string(), "105");
        assert_eq!(candlestick.data.volume.to_string(), "12.5");
    }

    #[test]
    fn decodes_futures_combined_market_stream_event() {
        let key = listing(crate::routing::PERPETUAL_SCOPE, "BTCUSDT");
        let event = decode_subscription_event(
            br#"{"stream":"btcusdt@kline_1m","data":{"e":"kline","k":{"s":"BTCUSDT","t":1700000000000,"i":"1m","o":"100","h":"110","l":"90","c":"105","v":"12.5"}}}"#,
            &SubscriptionKind::Candlestick {
                listing: key.clone(),
                symbol: symbol("BTCUSDT"),
                interval: "1m".into(),
            },
        )
        .expect("event should decode")
        .expect("event should match");

        let ProtocolEvent::Candlestick(candlestick) = event else {
            panic!("expected candlestick event");
        };
        assert_eq!(candlestick.listing, key);
        assert_eq!(candlestick.data.timestamp, 1_700_000_000_000);
    }

    #[test]
    fn a_candle_of_another_symbol_is_not_published() {
        let event = decode_subscription_event(
            br#"{"e":"kline","k":{"s":"ETHUSDT","t":1700000000000,"i":"1m","o":"100","h":"110","l":"90","c":"105","v":"12.5"}}"#,
            &SubscriptionKind::Candlestick {
                listing: listing(crate::routing::SPOT_SCOPE, "BTCUSDT"),
                symbol: symbol("BTCUSDT"),
                interval: "1m".into(),
            },
        )
        .expect("a foreign payload is not an error");

        assert!(event.is_none());
    }

    #[test]
    fn decodes_binance_depth_sequence_range_and_predecessor() {
        let key = listing(crate::routing::SPOT_SCOPE, "BTCUSDT");
        let event = decode_subscription_event(
            br#"{"e":"depthUpdate","E":1700000000000,"s":"BTCUSDT","U":101,"u":105,"pu":100,"b":[["100","2"]],"a":[["101","3"]]}"#,
            &SubscriptionKind::OrderBook {
                listing: key.clone(),
                symbol: symbol("BTCUSDT"),
            },
        )
        .expect("depth event should decode")
        .expect("depth event should match");

        let ProtocolEvent::OrderBook(update) = event else {
            panic!("expected order-book event");
        };
        assert_eq!(update.listing, key);
        assert!(!update.data.snapshot);
        assert_eq!(update.data.first_sequence, MarketSequence::Numeric(101));
        assert_eq!(update.data.last_sequence, MarketSequence::Numeric(105));
        assert_eq!(
            update.data.previous_sequence,
            Some(MarketSequence::Numeric(100))
        );
        assert_eq!(update.data.bids[0].quantity.to_string(), "2");
        assert_eq!(update.data.asks[0].price.to_string(), "101");
    }

    #[test]
    fn a_force_order_names_the_position_side_it_closes() {
        let event = decode_subscription_event(
            br#"{"e":"forceOrder","E":1568014460893,"o":{"s":"BTCUSDT","S":"SELL","q":"0.014","p":"9910","T":1568014460893}}"#,
            &SubscriptionKind::ForceOrder {
                route: route(crate::routing::PERPETUAL_SCOPE),
                product: BinanceProduct::Perpetual,
            },
        )
        .expect("force order should decode")
        .expect("force order should match");

        let ProtocolEvent::ForceOrder(order) = event else {
            panic!("expected force-order event");
        };
        assert_eq!(order.data.position_side, PositionSide::Long);
        assert_eq!(order.data.price.to_string(), "9910");
        assert_eq!(order.data.quantity.to_string(), "0.014");
        assert_eq!(
            order.listing,
            listing(crate::routing::PERPETUAL_SCOPE, "BTCUSDT")
        );
    }

    #[test]
    fn a_ticker_carries_the_venue_numbers() {
        let ticker = decode_ticker(
            &BinanceTickerWire {
                symbol: "BTCUSDT".into(),
                open_price: "100".into(),
                high_price: Some("110".into()),
                low_price: Some("90".into()),
                last_price: "105".into(),
                volume: "12.5".into(),
                quote_volume: "1300".into(),
            },
            "decode Binance ticker",
        )
        .expect("the fixture ticker should decode");

        assert_eq!(ticker.monotonic_id, None);
        assert_eq!(ticker.open.to_string(), "100");
        assert_eq!(ticker.close.to_string(), "105");
        assert_eq!(
            ticker.traded_quantity.map(|value| value.to_string()),
            Some("12.5".to_owned())
        );
        assert_eq!(
            ticker.traded_notional.map(|value| value.to_string()),
            Some("1300".to_owned())
        );
    }

    #[test]
    fn a_depth_update_level_with_zero_quantity_is_decoded() {
        let event = decode_subscription_event(
            br#"{"e":"depthUpdate","E":1700000000000,"s":"BTCUSDT","U":101,"u":105,"b":[["100","0"]],"a":[["101","3"]]}"#,
            &SubscriptionKind::OrderBook {
                listing: listing(crate::routing::SPOT_SCOPE, "BTCUSDT"),
                symbol: symbol("BTCUSDT"),
            },
        )
        .expect("a removal level is not an error")
        .expect("depth event should match");

        let ProtocolEvent::OrderBook(update) = event else {
            panic!("expected order-book event");
        };
        assert_eq!(update.data.bids[0].price.to_string(), "100");
        assert!(update.data.bids[0].quantity.is_zero());
    }

    #[test]
    fn the_two_scopes_address_their_own_paths() {
        assert_eq!(
            super::SPOT_REST.market_participant_info,
            "api/v3/exchangeInfo"
        );
        assert_eq!(
            super::PERPETUAL_REST.market_participant_info,
            "fapi/v1/exchangeInfo"
        );
        assert_eq!(super::SPOT_REST.depth, "api/v3/depth");
        assert_eq!(super::PERPETUAL_REST.depth, "fapi/v1/depth");
        assert_eq!(super::SPOT_REST.order, "api/v3/order");
        assert_eq!(super::PERPETUAL_REST.order, "fapi/v1/order");

        assert_eq!(super::StreamPath::Market.path(BinanceProduct::Spot), "ws");
        assert_eq!(
            super::StreamPath::Market.path(BinanceProduct::Perpetual),
            "market/stream"
        );
        assert_eq!(
            super::StreamPath::Public.path(BinanceProduct::Perpetual),
            "public/stream"
        );
    }
}

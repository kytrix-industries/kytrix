use std::sync::Arc;

use ahash::AHashMap;
use futures::future::BoxFuture;
use market_participants_model::{identifier::VenueId, listing::route::VenueRoute};
use parking_lot::Mutex;
use smol_str::SmolStr;

use super::{
    auth::LongbridgeSession,
    candles, catalog,
    socket::{self, ConnectionPlan},
    tickers,
};
use crate::{
    adapter::{
        AdapterLimits, ExecutionContext, MarketParticipantAdapter, PreparedRequest,
        ProtocolRequest, ProtocolResponse, SubscriptionHandle,
    },
    error::{AuthError, MarketParticipantError},
    market::timeframe::{Adjustment, TimeFrame},
    routing::LONGBRIDGE_US_SCOPE,
    runtime::{CredentialState, LongbridgeCredentialValues},
};

const LONGBRIDGE_TIMEFRAMES: &[TimeFrame] = &[
    TimeFrame::Minute1,
    TimeFrame::Minute2,
    TimeFrame::Minute3,
    TimeFrame::Minute5,
    TimeFrame::Minute10,
    TimeFrame::Minute15,
    TimeFrame::Minute20,
    TimeFrame::Minute30,
    TimeFrame::Minute45,
    TimeFrame::Hour1,
    TimeFrame::Hour2,
    TimeFrame::Hour3,
    TimeFrame::Hour4,
    TimeFrame::Day1,
    TimeFrame::Week1,
    TimeFrame::Month1,
    TimeFrame::Quarter1,
    TimeFrame::Year1,
];

struct Sessions {
    established: Mutex<AHashMap<VenueRoute, Arc<LongbridgeSession>>>,
    establishing: tokio::sync::Mutex<()>,
}

impl Sessions {
    fn new() -> Self {
        Self {
            established: Mutex::new(AHashMap::with_capacity(1)),
            establishing: tokio::sync::Mutex::new(()),
        }
    }

    async fn session(
        &self,
        context: &ExecutionContext,
    ) -> Result<Arc<LongbridgeSession>, MarketParticipantError> {
        let credentials = credentials(context)?;
        let plan = ConnectionPlan::of(context, &credentials)?;
        let route = context.route();
        if let Some(session) = self.established.lock().get(&route)
            && session.belongs_to(&plan.key)
        {
            return Ok(Arc::clone(session));
        }
        let _establishing = self.establishing.lock().await;
        if let Some(session) = self.established.lock().get(&route)
            && session.belongs_to(&plan.key)
        {
            return Ok(Arc::clone(session));
        }
        let session = socket::establish(context, &credentials, &plan).await?;
        self.established.lock().insert(route, Arc::clone(&session));
        Ok(session)
    }
}

pub struct LongbridgeAdapter {
    venue: VenueId,
    sessions: Arc<Sessions>,
}

impl LongbridgeAdapter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            venue: VenueId::Longbridge,
            sessions: Arc::new(Sessions::new()),
        })
    }
}

impl MarketParticipantAdapter for LongbridgeAdapter {
    fn venue(&self) -> &VenueId {
        &self.venue
    }

    fn limits(&self, route: &VenueRoute) -> AdapterLimits {
        let served = route.venue == self.venue && route.scope.as_str() == LONGBRIDGE_US_SCOPE;
        AdapterLimits {
            order_book_depth_limit: 0,
            timeframes: if served {
                LONGBRIDGE_TIMEFRAMES.to_vec()
            } else {
                Vec::new()
            },
            adjustments: if served {
                vec![Adjustment::None, Adjustment::Forward]
            } else {
                Vec::new()
            },
        }
    }

    fn execute(
        &self,
        context: ExecutionContext,
        request: PreparedRequest,
    ) -> BoxFuture<'static, Result<ProtocolResponse, MarketParticipantError>> {
        let sessions = Arc::clone(&self.sessions);
        Box::pin(async move {
            match request.request() {
                ProtocolRequest::FetchMarkets(query) => {
                    catalog::fetch_markets(&context, query).await
                }
                ProtocolRequest::FetchCandlesticks(query) => {
                    let listing = listing_of(&request)?;
                    let session = sessions.session(&context).await?;
                    candles::fetch_candlesticks(&context, &session, listing, query).await
                }
                ProtocolRequest::FetchTicker(_) => {
                    let listing = listing_of(&request)?;
                    let session = sessions.session(&context).await?;
                    tickers::fetch_ticker(&context, &session, listing).await
                }
                _ => Err(unsupported(&context)),
            }
        })
    }

    fn subscribe(
        &self,
        context: ExecutionContext,
        request: PreparedRequest,
    ) -> BoxFuture<'static, Result<SubscriptionHandle, MarketParticipantError>> {
        let sessions = Arc::clone(&self.sessions);
        Box::pin(async move {
            match request.request() {
                ProtocolRequest::SubscribeCandlesticks(query) => {
                    let listing = listing_of(&request)?;
                    let session = sessions.session(&context).await?;
                    candles::subscribe_candlesticks(&context, &session, listing, query).await
                }
                ProtocolRequest::SubscribeTicker(_) => {
                    let listing = listing_of(&request)?;
                    let session = sessions.session(&context).await?;
                    tickers::subscribe_ticker(&context, &session, listing).await
                }
                _ => Err(unsupported(&context)),
            }
        })
    }
}

fn listing_of(
    request: &PreparedRequest,
) -> Result<&crate::catalog::resolved::ResolvedListingRef, MarketParticipantError> {
    request
        .listing()
        .ok_or_else(|| MarketParticipantError::ProtocolError {
            operation: SmolStr::new_static("encode Longbridge request"),
            reason: SmolStr::new_static("the request carries no resolved listing"),
            raw_data: None,
        })
}

fn credentials(
    context: &ExecutionContext,
) -> Result<LongbridgeCredentialValues, MarketParticipantError> {
    let route = context.route();
    let resolved = context
        .current_runtime()
        .map(|snapshot| snapshot.resolve_longbridge_credentials(&route.venue, &route.scope));
    match resolved {
        Some(CredentialState::Configured(credentials)) => Ok(credentials),
        Some(CredentialState::Unconfigured | CredentialState::Invalid(_)) | None => {
            Err(MarketParticipantError::AuthenticationFailed {
                route,
                account: None,
                source: AuthError::ApiKeyNotFound,
            })
        }
    }
}

fn unsupported(context: &ExecutionContext) -> MarketParticipantError {
    MarketParticipantError::UnsupportedCapability {
        route: context.route(),
        capability: context.capability(),
    }
}

#[cfg(test)]
mod tests {
    use app_config::market_participants::credentials::LongbridgeCredentials;
    use market_participants_model::{
        identifier::{Mic, VenueId, VenueScopeId},
        listing::route::{VenueListingKey, VenueRoute},
        routing::CapabilityId,
    };
    use smol_str::SmolStr;

    use super::{LONGBRIDGE_TIMEFRAMES, LongbridgeAdapter};
    use crate::{
        adapter::{
            MarketParticipantAdapter, PreparedRequest, ProtocolRequest,
            longbridge::{
                SymbolRow,
                catalog::catalog_entry,
                codec,
                symbols::{NASDAQ_LISTED_FILE, OTHER_LISTED_FILE},
                test_support::{
                    AuthAnswer, CandleAnswer, FixtureCandle, LongbridgeFixture, ProfileAnswer,
                    QuoteAnswer, TokenAnswer, credentials, execution, resolved_listing, route,
                },
            },
        },
        error::{AuthError, MarketParticipantError},
        market::timeframe::{Adjustment, TimeFrame},
        market_participants::model::{
            CandlestickFetchQuery, CandlestickSubscriptionQuery, MarketCatalogQuery, TickersQuery,
        },
        test_support::{
            DirectoryAnswer, NASDAQ_LISTED_SAMPLE, OTHER_LISTED_SAMPLE, SymbolDirectoryFixture,
        },
    };

    async fn candle_request(request: ProtocolRequest) -> PreparedRequest {
        PreparedRequest::new(request, Some(resolved_listing().await))
    }

    fn unlisted(request: ProtocolRequest) -> PreparedRequest {
        PreparedRequest::new(request, None)
    }

    fn listing() -> VenueListingKey {
        catalog_entry(
            &route(),
            SymbolRow {
                code: SmolStr::new_static("AAPL"),
                name: SmolStr::new_static("Apple Inc. Common Stock"),
                mic: Mic::new("XNAS").expect("the fixture market is canonical"),
            },
        )
        .expect("the fixture row is publishable")
        .listing
    }

    fn candlestick_query() -> CandlestickFetchQuery {
        CandlestickFetchQuery {
            listing: listing(),
            timeframe: TimeFrame::Day1,
            adjustment: Adjustment::None,
            start_timestamp: None,
            end_timestamp: None,
            limit: 10,
        }
    }

    fn candlestick_subscription() -> CandlestickSubscriptionQuery {
        CandlestickSubscriptionQuery {
            listing: listing(),
            timeframe: TimeFrame::Day1,
            adjustment: Adjustment::None,
        }
    }

    #[test]
    fn the_advertised_intervals_are_exactly_the_encodable_ones() {
        let advertised = LONGBRIDGE_TIMEFRAMES;
        let encodable: Vec<TimeFrame> = advertised
            .iter()
            .copied()
            .filter(|timeframe| codec::LongbridgePeriod::of(*timeframe).is_some())
            .collect();

        assert_eq!(
            encodable.len(),
            advertised.len(),
            "every advertised interval has a period: {:?}",
            advertised
                .iter()
                .copied()
                .filter(|timeframe| codec::LongbridgePeriod::of(*timeframe).is_none())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            advertised.len(),
            codec::LongbridgePeriod::ALL.len(),
            "and every period this build encodes is advertised"
        );
        for (index, period) in codec::LongbridgePeriod::ALL.into_iter().enumerate() {
            assert_eq!(
                codec::LongbridgePeriod::of(advertised[index]),
                Some(period),
                "the advertised list and the period table are in one order"
            );
        }
    }

    #[test]
    fn the_adapter_answers_the_venues_own_bounds() {
        let adapter = LongbridgeAdapter::new();
        assert_eq!(adapter.venue(), &VenueId::Longbridge);

        let served = adapter.limits(&route());
        assert_eq!(
            served.order_book_depth_limit, 0,
            "this build serves no book"
        );
        assert_eq!(served.timeframes.len(), 18);
        assert!(served.timeframes.contains(&TimeFrame::Minute45));
        assert!(served.timeframes.contains(&TimeFrame::Year1));
        assert!(!served.timeframes.contains(&TimeFrame::Second1));
        assert_eq!(
            served.adjustments,
            vec![Adjustment::None, Adjustment::Forward],
            "the venue publishes unadjusted and forward-adjusted candles"
        );

        let elsewhere = adapter.limits(&VenueRoute::new(
            VenueId::Longbridge,
            VenueScopeId::new("other").expect("the fixture scope is canonical"),
        ));
        assert!(elsewhere.timeframes.is_empty());
        assert!(elsewhere.adjustments.is_empty());
    }

    #[tokio::test]
    async fn a_candlestick_request_establishes_the_session_and_then_asks() {
        let fixture = LongbridgeFixture::install(
            TokenAnswer::granted("fixture-otp"),
            AuthAnswer::accepted(),
            ProfileAnswer::granted(),
        )
        .await;
        let adapter = LongbridgeAdapter::new();
        let context = execution(&fixture, Some(credentials()), CapabilityId::Candlesticks);

        adapter
            .execute(
                context.clone(),
                candle_request(ProtocolRequest::FetchCandlesticks(candlestick_query())).await,
            )
            .await
            .expect("the fixture venue answers the history request");

        assert_eq!(
            fixture.commands(),
            vec![
                codec::COMMAND_CODE_AUTH,
                codec::QUERY_USER_QUOTE_PROFILE,
                codec::GET_SECURITY_HISTORY_CANDLESTICKS
            ],
            "the handshake ran, and then the history request was sent"
        );

        let before = fixture.frames().len();
        adapter
            .execute(
                context,
                candle_request(ProtocolRequest::FetchCandlesticks(candlestick_query())).await,
            )
            .await
            .expect("the pooled socket answers the same request again");
        assert_eq!(
            fixture.frames().len(),
            before + 1,
            "the second request sent its own frame and no handshake"
        );
    }

    #[tokio::test]
    async fn a_candlestick_subscription_asks_for_its_screen_before_it_registers() {
        let fixture = LongbridgeFixture::install(
            TokenAnswer::granted("fixture-otp"),
            AuthAnswer::accepted(),
            ProfileAnswer::granted(),
        )
        .await;
        let adapter = LongbridgeAdapter::new();
        let context = execution(&fixture, Some(credentials()), CapabilityId::Candlesticks);

        let handle = adapter
            .subscribe(
                context,
                candle_request(ProtocolRequest::SubscribeCandlesticks(
                    candlestick_subscription(),
                ))
                .await,
            )
            .await
            .expect("the fixture venue answers the screen and then registers");

        fixture.await_command(codec::SUBSCRIBE).await;
        assert_eq!(
            fixture.commands(),
            vec![
                codec::COMMAND_CODE_AUTH,
                codec::QUERY_USER_QUOTE_PROFILE,
                codec::GET_SECURITY_CANDLESTICKS,
                codec::SUBSCRIBE
            ],
            "the screen was awaited before the subscribe frame was registered"
        );
        drop(handle);
    }

    #[tokio::test]
    async fn a_candlestick_request_without_permission_is_refused() {
        let fixture = LongbridgeFixture::install(
            TokenAnswer::granted("fixture-otp"),
            AuthAnswer::accepted(),
            ProfileAnswer::without_us_market_data(),
        )
        .await;
        let adapter = LongbridgeAdapter::new();
        let context = execution(&fixture, Some(credentials()), CapabilityId::Candlesticks);

        let error = adapter
            .execute(
                context,
                candle_request(ProtocolRequest::FetchCandlesticks(candlestick_query())).await,
            )
            .await
            .expect_err("the fixture account may not read US market data");

        assert!(
            matches!(
                error,
                MarketParticipantError::AuthenticationFailed {
                    source: AuthError::NoMarketDataPermission,
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(
            fixture.commands(),
            vec![codec::COMMAND_CODE_AUTH, codec::QUERY_USER_QUOTE_PROFILE],
            "the gate is what refused it, not the venue"
        );
    }

    #[tokio::test]
    async fn the_route_level_bulk_ticker_read_is_refused_without_a_connection() {
        let fixture = LongbridgeFixture::install(
            TokenAnswer::granted("fixture-otp"),
            AuthAnswer::accepted(),
            ProfileAnswer::granted(),
        )
        .await;
        let adapter = LongbridgeAdapter::new();
        let context = execution(&fixture, Some(credentials()), CapabilityId::AllTickers);

        let error = adapter
            .execute(
                context,
                unlisted(ProtocolRequest::FetchAllTickers(TickersQuery {
                    route: route(),
                })),
            )
            .await
            .expect_err("this adapter serves no route-level bulk ticker read");

        assert_eq!(
            error,
            MarketParticipantError::UnsupportedCapability {
                route: route(),
                capability: CapabilityId::AllTickers,
            }
        );
        assert!(fixture.token_header_blocks().is_empty());
    }

    #[tokio::test]
    async fn a_venue_without_credentials_is_refused_without_a_connection() {
        let fixture = LongbridgeFixture::install(
            TokenAnswer::granted("fixture-otp"),
            AuthAnswer::accepted(),
            ProfileAnswer::granted(),
        )
        .await;
        let adapter = LongbridgeAdapter::new();
        let context = execution(&fixture, None, CapabilityId::Candlesticks);

        let error = adapter
            .execute(
                context,
                candle_request(ProtocolRequest::FetchCandlesticks(candlestick_query())).await,
            )
            .await
            .expect_err("a venue with nothing stored cannot authenticate");

        assert_eq!(
            error,
            MarketParticipantError::AuthenticationFailed {
                route: route(),
                account: None,
                source: AuthError::ApiKeyNotFound,
            }
        );
        assert!(fixture.token_header_blocks().is_empty());
        assert!(fixture.commands().is_empty());
    }

    #[tokio::test]
    async fn a_market_request_is_answered_with_the_venues_directory() {
        let _directory = SymbolDirectoryFixture::install(vec![
            (
                NASDAQ_LISTED_FILE,
                DirectoryAnswer::File(NASDAQ_LISTED_SAMPLE),
            ),
            (
                OTHER_LISTED_FILE,
                DirectoryAnswer::File(OTHER_LISTED_SAMPLE),
            ),
        ])
        .await;
        let fixture = LongbridgeFixture::install(
            TokenAnswer::granted("fixture-otp"),
            AuthAnswer::accepted(),
            ProfileAnswer::granted(),
        )
        .await;
        let adapter = LongbridgeAdapter::new();
        let context = execution(&fixture, Some(credentials()), CapabilityId::MarketCatalog);

        let response = adapter
            .execute(
                context,
                unlisted(ProtocolRequest::FetchMarkets(MarketCatalogQuery {
                    route: route(),
                })),
            )
            .await
            .expect("the fixture directory reads");

        let crate::adapter::ProtocolResponse::Markets(entries) = response else {
            panic!("a catalog fetch answers with the route's catalog");
        };
        assert_eq!(entries.len(), 3, "both files, without their ETF rows");
        assert_eq!(entries[0].native_symbol.as_str(), "A.US");
        assert!(
            entries.iter().all(|entry| entry.listing.route == route()),
            "every entry belongs to the route that asked"
        );
        assert!(
            fixture.commands().is_empty(),
            "the directory is public: no socket was opened for it"
        );
    }

    #[tokio::test]
    async fn a_changed_region_is_read_as_a_new_session() {
        let fixture = LongbridgeFixture::install_with(
            TokenAnswer::granted("fixture-otp"),
            AuthAnswer::accepted(),
            ProfileAnswer::granted(),
            CandleAnswer::of(vec![FixtureCandle::flat(10_000, 1_781_097_600)]),
            QuoteAnswer::default(),
        )
        .await;
        let adapter = LongbridgeAdapter::new();
        let asia = execution(&fixture, Some(credentials()), CapabilityId::Candlesticks);
        let united_states = execution(
            &fixture,
            Some(LongbridgeCredentials {
                app_key: SmolStr::new_static("us_app-key"),
                ..credentials()
            }),
            CapabilityId::Candlesticks,
        );

        adapter
            .execute(
                asia,
                candle_request(ProtocolRequest::FetchCandlesticks(candlestick_query())).await,
            )
            .await
            .expect("the fixture venue answers the history request");
        adapter
            .execute(
                united_states,
                candle_request(ProtocolRequest::FetchCandlesticks(candlestick_query())).await,
            )
            .await
            .expect("the fixture venue answers the history request");

        let headers = fixture.token_header_blocks();
        assert_eq!(
            headers.len(),
            2,
            "each data centre was authenticated on its own connection, and each asked for \
             one password"
        );
        for (index, region) in [(0, "ap"), (1, "us")] {
            assert!(
                headers[index]
                    .iter()
                    .any(|(name, value)| name == "x-dc-region" && value == region),
                "request {index} names {region}: {:?}",
                headers[index]
            );
        }
        assert_eq!(
            fixture
                .upgrade_header_blocks()
                .iter()
                .filter(|headers| headers
                    .iter()
                    .any(|(name, value)| name == "x-dc-region" && value == "us"))
                .count(),
            1,
            "and the US socket was dialled with the US header on its own upgrade"
        );
    }
}

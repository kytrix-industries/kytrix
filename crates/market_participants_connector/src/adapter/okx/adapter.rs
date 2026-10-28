//! The OKX adapter.
//!
//! The adapter speaks OKX's v5 protocol for the two scopes it is registered on.
//! It owns no origin of its own: the HTTP and WebSocket origins come from the
//! execution context, which the gateway built from the venue configuration of the
//! route the request was resolved against — the credentials come from the
//! authentication the gateway resolved for the request's account, and the only
//! identity it derives is the venue's own `instId` — a listing key, never a second
//! directory.
//!
//! A route-level answer (the catalog, every ticker, a liquidation push) names its
//! subjects with the venue's identifiers, and every row is resolved to the
//! listing key that identifier derives before it leaves this module.

use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use futures::{StreamExt, future::BoxFuture, stream};
use market_participants_model::{
    asset::{
        AssetId, CryptoTokenOrigin, CryptoTokenTerms, CurrencyTerms, DerivativeUnderlying,
        Iso4217Code, TokenReference, UnderlyingRef,
    },
    decimal::{Price, Quantity, Tick, UnixMillis},
    identifier::{
        IdentifierNamespace, IdentifierValue, NativeSymbol, ReferenceSourceId,
        ReferenceTermsVersion, TokenIssuerId, VenueId,
    },
    instrument::{Instrument, InstrumentTerms, PerpetualTerms, SpotTerms},
    listing::{
        identifiers::{OkxListingIdentifiers, VenueListingIdentifiers},
        listing_impl::ListingStatus,
        route::{VenueListingKey, VenueRoute},
        rules::{
            CommonTradingRules, FundingInterval, FundingSchedule, LotPolicy, PerpetualRisk,
            PerpetualTradingRules, PriceRule, ProductTradingRules, QuantityRule, ShortSalePolicy,
            SpotTradingRules, TradingRules, VenueTradingPolicy,
        },
        venue::{OkxInstrumentType, PriceBasis},
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
use serde::de::DeserializeOwned;
use smol_str::{SmolStr, format_smolstr};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{
    OkxArg, OkxCodec, OkxSigner,
    account::{
        ConfigRow, HttpResponse, OkxBalanceAccount, OkxHistoricalPosition, OkxLeverage, OkxOrder,
        OkxPosition, OkxTrade, PAGE_LIMIT, decode_account_trades, decode_balances, decode_leverage,
        decode_order_frame, decode_orders, decode_position_frame, decode_position_history,
        decode_position_mode, decode_positions, private_frame_channel, validate_history,
    },
    market_data::{
        decode_candlesticks, decode_order_book, decode_public, decode_ticker, decode_tickers,
        decode_trades, public_get, ticker_of,
    },
};
use crate::{
    account::shared::{MarginMode, OrderId, OrderKind, OrderRoute, TradingSettings},
    adapter::{
        AdapterLimits, Authentication, ExecutionContext, MarketParticipantAdapter,
        NativeCandlestick, NativeForceOrder, NativeListing, NativeOrderBookUpdate, NativeTrade,
        PreparedRequest, ProtocolEvent, ProtocolRequest, ProtocolResponse, SubscriptionHandle,
        response::{
            NativeCatalogEntry, NativeLongShortRatio, NativeOpenInterest, NativeTakerVolume,
        },
    },
    catalog::{registry::asset::AssetTerms, resolved::ResolvedListingRef},
    connection::{
        Connection, ConnectionKey, ConnectionState, Reauthentication, ReauthenticationRequest,
        ReconnectPolicy, SubscriptionTopic, physical::ConnectionProtocol,
    },
    error::{AuthError, MarketParticipantError},
    market::timeframe::Adjustment,
    market_participants::model::{
        AuthenticatedQuery, CancelOrderCommand, CandlestickFetchQuery, MarketCatalogQuery,
        MarketTradeFetchQuery, NativeOrderBookLevel, OrderBookQuery, PerpetualStatisticsQuery,
        PlaceOrderCommand, TickerQuery, TickersQuery, TimeFrame, TradingSettingsQuery,
    },
    routing::{SPOT_SCOPE, SWAP_SCOPE},
};

/// How long one request sent over a socket may take before it is given up on.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// The deepest book OKX hands over in one request.
const ORDER_BOOK_DEPTH_LIMIT: u16 = 400;
/// The namespace OKX's own tokens and price references are issued under.
const OKX_NAMESPACE: &str = "okx";
/// The RFC 3339 version of the reference terms this adapter declares.
const REFERENCE_TERMS_VERSION: &str = "v1";
/// The candlestick intervals OKX publishes on the scopes this adapter serves.
const TIMEFRAMES: &[TimeFrame] = &[
    TimeFrame::Minute1,
    TimeFrame::Minute3,
    TimeFrame::Minute5,
    TimeFrame::Minute15,
    TimeFrame::Minute30,
    TimeFrame::Hour1,
    TimeFrame::Hour2,
    TimeFrame::Hour4,
    TimeFrame::Hour6,
    TimeFrame::Hour12,
    TimeFrame::Day1,
    TimeFrame::Day2,
    TimeFrame::Day3,
    TimeFrame::Week1,
    TimeFrame::Month1,
    TimeFrame::Month3,
];

/// The credential set one signed OKX request needs.
struct OkxCredentials {
    api_key: SmolStr,
    passphrase: SmolStr,
    signer: OkxSigner,
}

pub struct OkxAdapter {
    venue: VenueId,
}

impl OkxAdapter {
    /// Builds the OKX adapter.
    ///
    /// The hosts a request dials are its **venue's** configuration, so the adapter
    /// holds no region: two installations whose OKX accounts belong to different
    /// service regions differ in `venues.okx.region`, and one adapter serves both.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            venue: VenueId::Okx,
        })
    }
}

impl MarketParticipantAdapter for OkxAdapter {
    fn venue(&self) -> &VenueId {
        &self.venue
    }

    /// Returns OKX's own bounds for one scope.
    ///
    /// Both scopes ask for a book of up to four hundred levels and publish the
    /// same sixteen candlestick intervals, none of them restated. A scope this
    /// adapter does not serve has no answer at all rather than another scope's
    /// numbers.
    fn limits(&self, route: &VenueRoute) -> AdapterLimits {
        let served =
            route.venue == self.venue && matches!(route.scope.as_str(), SPOT_SCOPE | SWAP_SCOPE);
        AdapterLimits {
            order_book_depth_limit: ORDER_BOOK_DEPTH_LIMIT,
            timeframes: if served {
                TIMEFRAMES.to_vec()
            } else {
                Vec::new()
            },
            adjustments: if served {
                vec![Adjustment::None]
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
        Box::pin(async move {
            let (request, listing) = request.into_parts();
            let listing = listing.as_ref();
            match request {
                ProtocolRequest::FetchMarkets(query) => fetch_markets(&context, query).await,
                ProtocolRequest::FetchCandlesticks(query) => {
                    fetch_candlesticks(&context, listing, query).await
                }
                ProtocolRequest::FetchTicker(query) => fetch_ticker(&context, listing, query).await,
                ProtocolRequest::FetchAllTickers(query) => fetch_all_tickers(&context, query).await,
                ProtocolRequest::FetchOrderBook(query) => {
                    fetch_order_book(&context, listing, query).await
                }
                ProtocolRequest::FetchTrades(query) => fetch_trades(&context, listing, query).await,
                ProtocolRequest::FetchOpenInterest(query) => {
                    fetch_open_interest(&context, listing, query).await
                }
                ProtocolRequest::FetchLongShortRatio(query) => {
                    fetch_ratio(&context, listing, query, RatioKind::LongShortAccount).await
                }
                ProtocolRequest::FetchTopTraderAccountRatio(query) => {
                    fetch_ratio(&context, listing, query, RatioKind::TopTraderAccount).await
                }
                ProtocolRequest::FetchTopTraderPositionRatio(query) => {
                    fetch_ratio(&context, listing, query, RatioKind::TopTraderPosition).await
                }
                ProtocolRequest::FetchTakerVolume(query) => {
                    fetch_taker_volume(&context, listing, query).await
                }
                ProtocolRequest::PlaceOrder(command) => {
                    place_order(&context, listing, command).await
                }
                ProtocolRequest::CancelOrder(command) => {
                    cancel_order(&context, listing, command).await
                }
                ProtocolRequest::FetchBalances(query) => fetch_balances(&context, query).await,
                ProtocolRequest::FetchActiveOrders(query) => {
                    fetch_active_orders(&context, listing, query).await
                }
                ProtocolRequest::FetchOrderHistory(query) => {
                    fetch_order_history(&context, listing, query).await
                }
                ProtocolRequest::FetchPositions(query) => {
                    fetch_positions(&context, listing, query).await
                }
                ProtocolRequest::FetchAccountTrades(query) => {
                    fetch_account_trades(&context, listing, query).await
                }
                ProtocolRequest::FetchPositionHistory(query) => {
                    fetch_position_history(&context, listing, query).await
                }
                ProtocolRequest::FetchTradingSettings(query) => {
                    fetch_trading_settings(&context, listing, query).await
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
        Box::pin(async move {
            let (request, listing) = request.into_parts();
            let listing = listing.as_ref();
            let (args, kind, socket, authenticated) = subscription(&context, listing, request)?;
            let connection = if authenticated {
                authenticated_connection(&context).await?
            } else {
                connection(&context, socket).await?
            };
            open_subscription(&connection, args, kind)
        })
    }
}

/// One listing as the OKX wire addresses it.
struct OkxListing {
    key: VenueListingKey,
    instrument_id: SmolStr,
    instrument_type: OkxInstrumentType,
}

/// Reads the venue identity of one resolved listing.
fn resolved_listing(
    listing: Option<&ResolvedListingRef>,
) -> Result<OkxListing, MarketParticipantError> {
    let Some(listing) = listing else {
        return Err(protocol_error(
            "encode OKX request",
            "the request names no resolved listing",
        ));
    };
    let VenueListingIdentifiers::Okx(identifiers) = &listing.identifiers else {
        return Err(protocol_error(
            "encode OKX request",
            "the resolved listing is not an OKX listing",
        ));
    };
    let Some(instrument_id) = listing.native_symbol.as_ref().map(SmolStr::from) else {
        return Err(protocol_error(
            "encode OKX request",
            "the listing has no native OKX instrument id",
        ));
    };
    Ok(OkxListing {
        key: listing.key.clone(),
        instrument_id,
        instrument_type: identifiers.instrument_type,
    })
}

/// Reads the venue identity of one optional listing.
fn optional_listing(
    listing: Option<&ResolvedListingRef>,
) -> Result<Option<OkxListing>, MarketParticipantError> {
    listing
        .map(|listing| resolved_listing(Some(listing)))
        .transpose()
}

/// The instrument type of the scope this request was validated against.
fn scope_instrument_type(
    context: &ExecutionContext,
) -> Result<OkxInstrumentType, MarketParticipantError> {
    OkxCodec
        .scope_instrument_type(&context.route().scope)
        .ok_or_else(|| MarketParticipantError::UnsupportedCapability {
            route: context.route(),
            capability: context.capability(),
        })
}

/// Builds one protocol failure that names the operation it was raised in.
fn protocol_error(operation: &'static str, reason: impl Into<SmolStr>) -> MarketParticipantError {
    MarketParticipantError::ProtocolError {
        operation: operation.into(),
        reason: reason.into(),
        raw_data: None,
    }
}

/// Wraps one model rejection as the protocol failure of an operation.
fn model_error(operation: &'static str, error: impl std::fmt::Display) -> MarketParticipantError {
    protocol_error(operation, format_smolstr!("{error}"))
}

/// Builds one OKX REST URL from the origin of this request's venue.
pub(super) fn http_url(
    context: &ExecutionContext,
    path: &str,
    query: &[(&str, String)],
) -> Result<Url, MarketParticipantError> {
    let mut url = context
        .http_origin()
        .join(path.trim_start_matches('/'))
        .map_err(|_| MarketParticipantError::Configuration("invalid OKX endpoint path".into()))?;
    if !query.is_empty() {
        url.query_pairs_mut()
            .extend_pairs(query.iter().map(|(key, value)| (*key, value.as_str())));
    }
    Ok(url)
}

/// The WebSocket endpoint of one OKX channel family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OkxSocket {
    /// The public market-data channels.
    Public,
    /// The business channels, which carry the candlestick channels.
    Business,
    /// The authenticated account channels.
    Private,
}

impl OkxSocket {
    /// The path this channel family lives on.
    const fn path(self) -> &'static str {
        match self {
            Self::Public => "/ws/v5/public",
            Self::Business => "/ws/v5/business",
            Self::Private => "/ws/v5/private",
        }
    }

    const fn is_private(self) -> bool {
        matches!(self, Self::Private)
    }

    /// The WebSocket URL of this channel family on this request's venue.
    fn url(self, context: &ExecutionContext) -> Result<Url, MarketParticipantError> {
        context
            .websocket_origin()
            .join(self.path())
            .map_err(|_| MarketParticipantError::Configuration("invalid OKX WebSocket path".into()))
    }
}

/// Opens or reuses one physical connection of one channel family.
async fn connection(
    context: &ExecutionContext,
    socket: OkxSocket,
) -> Result<Arc<Connection>, MarketParticipantError> {
    let endpoint = socket.url(context)?;
    let account = context
        .auth
        .as_ref()
        .map(|auth| SmolStr::new(auth.account_id()));
    // The protocol is built before the key because the key is built from it: the upgrade
    // headers a connection declares are one value, and the key that decides whether a
    // socket may be reused reads them off the protocol that dials.
    let protocol = ConnectionProtocol::new(OkxCodec::extract_response_id)
        .with_keepalive(super::heartbeat::KEEPALIVE)
        .with_liveness_timeout(super::heartbeat::LIVENESS_TIMEOUT);
    let key = crate::adapter::bind_execution_identity(
        ConnectionKey::for_route(
            endpoint,
            context.proxy.clone(),
            account,
            context.route(),
            context.network_route_generation(),
            &protocol,
        ),
        context,
    )?;
    let pool = Arc::clone(&context.connection_pool);
    pool.connect_or_create(
        key,
        if socket.is_private() {
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

/// The credentials one signed OKX request signs with.
///
/// Every part of an OKX credential comes from the authentication the gateway
/// resolved for this request's account: the API key, the secret it signs with and
/// the passphrase the venue verifies next to the signature are one credential, so
/// an execution authenticated any other way fails closed instead of being signed
/// with a secret or a passphrase read from somewhere else.
fn credentials_for_context(
    context: &ExecutionContext,
) -> Result<OkxCredentials, MarketParticipantError> {
    let Some(auth) = context.auth.as_ref() else {
        return Err(MarketParticipantError::AuthenticationFailed {
            route: context.route(),
            account: None,
            source: AuthError::AccountIdentityMissing,
        });
    };
    let account_id: SmolStr = SmolStr::new(auth.account_id());
    let Authentication::PassphraseApiKey {
        api_key,
        secret,
        passphrase,
        ..
    } = auth
    else {
        return Err(MarketParticipantError::AuthenticationFailed {
            route: context.route(),
            account: Some(account_id),
            source: AuthError::InvalidCredentials,
        });
    };
    Ok(OkxCredentials {
        api_key: api_key.clone(),
        passphrase: passphrase.clone(),
        signer: OkxSigner::new(secret.clone()),
    })
}

/// The account this request authenticates as.
fn account_id(context: &ExecutionContext) -> Result<SmolStr, MarketParticipantError> {
    context
        .auth
        .as_ref()
        .map(|auth| SmolStr::new(auth.account_id()))
        .ok_or_else(|| MarketParticipantError::AuthenticationFailed {
            route: context.route(),
            account: None,
            source: AuthError::AccountIdentityMissing,
        })
}

/// Opens an authenticated connection, logging in when it is not authenticated yet.
async fn authenticated_connection(
    context: &ExecutionContext,
) -> Result<Arc<Connection>, MarketParticipantError> {
    let credentials = credentials_for_context(context)?;
    let account_id = account_id(context)?;
    let connection = connection(context, OkxSocket::Private).await?;
    if connection.state().is_authenticated() {
        return Ok(connection);
    }
    connection.set_reauthentication(reauthentication(context, &credentials));
    login(&connection, context, &credentials, account_id).await?;
    Ok(connection)
}

/// The login one reconnected OKX socket replays before its subscriptions resume.
fn reauthentication(context: &ExecutionContext, credentials: &OkxCredentials) -> Reauthentication {
    let api_key = credentials.api_key.clone();
    let passphrase = credentials.passphrase.clone();
    let signer = credentials.signer.clone();
    let route = context.route();
    Arc::new(move || {
        let api_key = api_key.clone();
        let passphrase = passphrase.clone();
        let signer = signer.clone();
        let route = route.clone();
        Box::pin(async move {
            let timestamp = timestamp()?;
            let login = signer.login_message(api_key, passphrase, timestamp.clone());
            let payload = OkxCodec.encode(&login)?;
            let acknowledgement_signer = signer.clone();
            Ok(ReauthenticationRequest {
                payload,
                session_token: timestamp,
                acknowledged: Arc::new(move |payload| {
                    let Ok(ack) = OkxCodec.decode::<super::OkxLoginAck>(payload) else {
                        return Ok(None);
                    };
                    let Some(event) = ack.event.as_deref() else {
                        return Ok(None);
                    };
                    if event != "login" && event != "error" {
                        return Ok(None);
                    }
                    acknowledgement_signer
                        .login_acknowledged(&ack, &route)
                        .map(|()| Some(()))
                }),
            })
        })
    })
}

/// Logs one fresh connection in and waits for the venue's acknowledgement.
async fn login(
    connection: &Arc<Connection>,
    context: &ExecutionContext,
    credentials: &OkxCredentials,
    account_id: SmolStr,
) -> Result<(), MarketParticipantError> {
    let timestamp = timestamp()?;
    let login = credentials.signer.login_message(
        credentials.api_key.clone(),
        credentials.passphrase.clone(),
        timestamp.clone(),
    );
    connection.send(OkxCodec.encode(&login)?).await?;
    let mut events = Box::pin(connection.event_stream());
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        while let Some(payload) = events.next().await {
            // A connection that stopped publishing ends this handshake as well.
            let Ok(payload) = payload else { break };
            let Ok(ack) = OkxCodec.decode::<super::OkxLoginAck>(&payload) else {
                continue;
            };
            let Some(event) = ack.event.as_deref() else {
                continue;
            };
            if event != "login" && event != "error" {
                continue;
            }
            credentials
                .signer
                .login_acknowledged(&ack, &context.route())?;
            return Ok::<(), MarketParticipantError>(());
        }
        Err(MarketParticipantError::AuthenticationFailed {
            route: context.route(),
            account: Some(account_id.clone()),
            source: AuthError::ServiceClosed,
        })
    })
    .await
    .map_err(|_| MarketParticipantError::AuthenticationFailed {
        route: context.route(),
        account: Some(account_id.clone()),
        source: AuthError::HandshakeTimeout,
    })??;
    connection.authenticate(account_id, timestamp);
    Ok(())
}

/// The RFC 3339 millisecond timestamp OKX signs with.
fn timestamp() -> Result<SmolStr, MarketParticipantError> {
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|_| MarketParticipantError::Configuration("invalid OKX timestamp".into()))?;
    Ok(SmolStr::new(now))
}

/// Signs and sends one authenticated GET, answering its rows.
pub(super) async fn signed_get<T: DeserializeOwned>(
    context: &ExecutionContext,
    path: &str,
    parameters: Vec<(String, String)>,
) -> Result<Vec<T>, MarketParticipantError> {
    let credentials = credentials_for_context(context)?;
    let query: Vec<(&str, String)> = parameters
        .iter()
        .map(|(key, value)| (key.as_str(), value.clone()))
        .collect();
    let url = http_url(context, path, &query)?;
    let timestamp = timestamp()?;
    let path_and_query = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    };
    let signature = credentials
        .signer
        .request_signature(&timestamp, "GET", &path_and_query);
    let response = context
        .http_client
        .get(url.clone())
        .header("OK-ACCESS-KEY", credentials.api_key.as_str())
        .header("OK-ACCESS-PASSPHRASE", credentials.passphrase.as_str())
        .header("OK-ACCESS-TIMESTAMP", timestamp.as_str())
        .header("OK-ACCESS-SIGN", signature.as_str())
        .send()
        .await
        .map_err(|error| MarketParticipantError::ConnectionFailed {
            route: context.route(),
            endpoint: format_smolstr!("{}", url.origin().ascii_serialization()),
            source: network::error::NetworkError::Io {
                message: format_smolstr!("{error}"),
            },
        })?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| MarketParticipantError::DecodeFailed {
            operation: "read OKX account response".into(),
            raw_data: Bytes::from(error.to_string()),
        })?;
    if !status.is_success() {
        return Err(OkxCodec.response_error(
            &context.route(),
            url.origin().ascii_serialization().as_str(),
            status,
            body,
        ));
    }
    let decoded: HttpResponse<T> =
        serde_json::from_slice(&body).map_err(|_| MarketParticipantError::DecodeFailed {
            operation: "decode OKX account response".into(),
            raw_data: body.clone(),
        })?;
    if decoded.code != "0" {
        return Err(OkxCodec.map_error(
            decoded.code.as_str(),
            decoded.msg.unwrap_or_default(),
            Some(body),
        ));
    }
    Ok(decoded.data)
}

// ---------------------------------------------------------------------------
// Market catalog
// ---------------------------------------------------------------------------

/// Fetches the catalog of one route.
async fn fetch_markets(
    context: &ExecutionContext,
    query: MarketCatalogQuery,
) -> Result<ProtocolResponse, MarketParticipantError> {
    let route = query.route().clone();
    let instrument_type = OkxCodec
        .scope_instrument_type(&route.scope)
        .ok_or_else(|| MarketParticipantError::UnsupportedCapability {
            route: route.clone(),
            capability: CapabilityId::MarketCatalog,
        })?;
    let body = public_get(
        context,
        "/api/v5/public/instruments",
        &[(
            "instType",
            OkxCodec.instrument_type_param(instrument_type).to_owned(),
        )],
    )
    .await?;
    // A perpetual's margin requirements are not part of its instrument record:
    // OKX publishes them as the contract's own position tiers, which this build
    // never reads, so every perpetual entry below carries its risk unloaded.
    let rows = decode_public::<OkxInstrumentWire>(&body, "decode OKX market catalog response")?;
    let mut markets = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(entry) = catalog_entry(&route, instrument_type, row)? {
            markets.push(entry);
        }
    }
    markets.sort_unstable_by(|left, right| left.native_symbol.cmp(&right.native_symbol));
    Ok(ProtocolResponse::Markets(markets))
}

/// Turns one OKX instrument row into a complete catalog entry.
///
/// # Two deliberate kinds of refusal
///
/// A row whose terms this model cannot express is left out rather than published
/// as a different product, and one whose venue fields are missing or inconsistent
/// is a protocol failure: the catalog is the identity source of its route and
/// never a partial answer.
///
/// The two are told apart by whose decision the refusal is, and the distinction is
/// deliberate rather than incidental:
///
/// - **A product this build cannot express is a listing to leave out, with a
///   warning.** An unknown `ctType`, a contract multiplier the model cannot fold
///   into a contract value, a listing state it cannot name and a row the venue has
///   not described yet are all statements that OKX trades something this build has
///   no terms for. The rest of the route is still a complete answer for every
///   product it does serve, so failing the route would hide those listings behind
///   one the user cannot trade anyway.
/// - **A venue response that contradicts itself is a protocol failure.** A swap
///   that states a contract value currency its own shape disagrees with, or that
///   omits a field every described swap carries, is not a product decision: it is a
///   row this model cannot read, and publishing anything from it would invent terms
///   the venue did not state.
///
/// Skipping never weakens the identity rules either way: every row that *is*
/// published still goes through the validated constructors, so a combination the
/// model forbids is refused rather than skipped into a different shape.
fn catalog_entry(
    route: &VenueRoute,
    instrument_type: OkxInstrumentType,
    row: OkxInstrumentWire,
) -> Result<Option<NativeCatalogEntry>, MarketParticipantError> {
    let instrument = match OkxInstrumentRow::read(route, instrument_type, row)? {
        // A contract the venue has announced but not described has no terms to
        // publish and no family to read its risk by: it is not a listing yet, and
        // nothing that reads a listing can reach it.
        Some(OkxInstrumentRow::Announced { instrument_id }) => {
            log::warn!(
                "OKX instrument {instrument_id} of {route} publishes no trading terms yet and is not published"
            );
            return Ok(None);
        }
        Some(OkxInstrumentRow::Listed(instrument)) => instrument,
        None => return Ok(None),
    };
    let canonical = match instrument.terms()? {
        // A product this build cannot express is a listing to leave out, not a route
        // to fail: the venue trades a contract whose terms this model has no shape
        // for. `terms` reports it with a warning and the row is dropped here.
        Some(terms) => terms,
        None => return Ok(None),
    };
    canonical
        .rules
        .validate_for(&canonical.instrument)
        .map_err(|error| model_error("validate OKX trading rules", error))?;
    let key = OkxCodec.listing_key(route, instrument_type, instrument.instrument_id.as_str())?;
    Ok(Some(NativeCatalogEntry {
        listing: key,
        native_symbol: NativeSymbol::new(instrument.instrument_id.as_str())
            .map_err(|error| model_error("decode OKX native symbol", error))?,
        aliases: Vec::new(),
        identifiers: VenueListingIdentifiers::Okx(OkxListingIdentifiers {
            instrument_type,
            instrument_family: instrument.instrument_family,
            future_series: None,
            option_series: None,
        }),
        rules: canonical.rules,
        status: instrument.status,
        available_from: instrument.available_from,
        available_until: None,
        instrument: canonical.instrument,
        asset_terms: canonical.asset_terms,
        reference_terms: canonical.reference_terms,
        // Every asset these listings publish states its own display code.
        asset_codes: Vec::new(),
    }))
}

/// The contract shape one OKX swap states.
///
/// The venue states this in `ctType` and nowhere else, and it decides what the
/// contract's value and settlement mean: a linear contract is an amount of the
/// asset it tracks, an inverse one is a notional of its price quote that settles
/// in that asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OkxSwapContractType {
    Linear,
    Inverse,
}

impl OkxSwapContractType {
    /// Reads the contract shape of one swap.
    ///
    /// `Ok(None)` is a shape this build cannot express — the venue trades a
    /// contract whose value and settlement this model has no terms for — so the
    /// listing is left out of the catalog instead of being published as one of the
    /// shapes the venue did not state. A row that omits the field entirely breaks
    /// the protocol instead: `ctType` is part of the terms every published swap is
    /// described by, so its absence is not a product decision.
    fn read(value: Option<&SmolStr>) -> Result<Option<Self>, MarketParticipantError> {
        match non_empty(value) {
            Some("linear") => Ok(Some(Self::Linear)),
            Some("inverse") => Ok(Some(Self::Inverse)),
            Some(_) => Ok(None),
            None => Err(protocol_error(
                "decode OKX contract type",
                "a swap omits its contract type",
            )),
        }
    }

    /// Returns the currency the venue counts the contract value in.
    ///
    /// This is the shape's own statement of what `ctVal` means, and it is the
    /// half of the contract terms a swap cannot be read without.
    fn contract_value_currency(self, base: &AssetTerms, quote: &AssetTerms) -> AssetTerms {
        match self {
            Self::Linear => base.clone(),
            Self::Inverse => quote.clone(),
        }
    }
}

/// What one published OKX listing is.
///
/// The instrument family decides what terms a row can have at all, so the shape a
/// row was read as carries the family rather than sitting beside it: a spot listing
/// has a base and a quote currency and no contract value or settlement model, and a
/// swap always has the contract shape the venue stated. The two can therefore not be
/// confused with each other anywhere the shape is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OkxInstrumentShape {
    /// A spot market: two currencies exchanged immediately.
    Spot,
    /// A swap stating one of the contract shapes this build can express.
    Swap(OkxSwapContractType),
}

/// One row of the instrument response, classified by what the venue published.
///
/// OKX answers a contract it has not described yet with its name alone — the
/// rows of a `preopen` swap carry no family, no contract value and no tick or lot
/// size — and those rows are not listings: publishing one would publish a
/// contract with invented terms.
#[derive(Clone, Debug)]
enum OkxInstrumentRow {
    /// The venue published the name only.
    Announced { instrument_id: SmolStr },
    /// The venue published every term the contract is described by.
    Listed(OkxListedInstrument),
}

impl OkxInstrumentRow {
    /// Classifies one instrument row of one route.
    ///
    /// `None` is a row this route does not publish at all: another product
    /// family, or a row the venue answered with nothing.
    fn read(
        route: &VenueRoute,
        instrument_type: OkxInstrumentType,
        row: OkxInstrumentWire,
    ) -> Result<Option<Self>, MarketParticipantError> {
        // The fields the venue describes each product with. A row that carries
        // none of them announces a contract whose terms do not exist yet; a row
        // that carries only some of them contradicts itself, and publishing it
        // would be publishing a contract the venue does not trade.
        //
        // `ctValCcy` and `ctMult` are deliberately not part of this count. They are
        // read when the venue states them — see `check_stated_contract_terms` — but
        // they are stated *beside* the terms a contract is described by, and a
        // response that omits them still describes its product completely.
        let described: &[&Option<SmolStr>] = match instrument_type {
            OkxInstrumentType::Spot => &[&row.base_ccy, &row.quote_ccy, &row.tick_sz, &row.lot_sz],
            OkxInstrumentType::Perpetual => &[
                &row.uly,
                &row.instrument_family,
                &row.ct_type,
                &row.ct_val,
                &row.settle_ccy,
                &row.tick_sz,
                &row.lot_sz,
            ],
            OkxInstrumentType::Future | OkxInstrumentType::Option => {
                log::warn!(
                    "OKX instrument {} of {route} is not a product this route serves and is not published",
                    row.inst_id
                );
                return Ok(None);
            }
        };
        let published = described
            .iter()
            .filter(|field| non_empty(field.as_ref()).is_some())
            .count();
        if published == 0 {
            return Ok(Some(Self::Announced {
                instrument_id: row.inst_id,
            }));
        }
        if published < described.len() {
            return Err(protocol_error(
                "decode OKX market instrument",
                format_smolstr!(
                    "{} publishes only part of the terms it is described by",
                    row.inst_id
                ),
            ));
        }
        Ok(OkxListedInstrument::read(route, instrument_type, row)?.map(Self::Listed))
    }
}

/// The venue fields of one fully published catalog instrument, parsed and
/// validated.
///
/// Every asset is kept as the canonical terms the venue's own currency code was
/// translated into, because a canonical identity is a digest of those terms: the
/// instrument is built from the identities and the entry that publishes it carries
/// the terms the display contract renders.
#[derive(Clone, Debug)]
struct OkxListedInstrument {
    instrument_id: SmolStr,
    instrument_type: OkxInstrumentType,
    /// What this listing is, which is the contract shape of a swap and the plain
    /// two-currency shape of a spot market.
    shape: OkxInstrumentShape,
    /// The venue's own instrument family, which a perpetual's risk is read by.
    instrument_family: Option<NativeSymbol>,
    base: AssetTerms,
    quote: AssetTerms,
    settle: AssetTerms,
    tick: Tick,
    lot_size: Quantity,
    minimum: Option<Quantity>,
    maximum: Option<Quantity>,
    contract_value: Option<Quantity>,
    /// The currency the venue states for the contract value, when it states one.
    contract_value_currency: Option<AssetTerms>,
    /// The venue's contract multiplier, when it states one.
    contract_multiplier: Option<Quantity>,
    status: ListingStatus,
    available_from: Option<UnixMillis>,
}

/// One OKX row as the canonical catalog values it describes.
///
/// The instrument and its rules are what the venue's own row states; the terms are
/// the ones their construction went through. A canonical asset or reference
/// identity is a digest of its terms, so the entry that publishes the identities
/// carries the terms beside them, and nothing downstream has to reconstruct what
/// the venue already stated.
struct OkxInstrumentTerms {
    instrument: Instrument,
    rules: TradingRules,
    asset_terms: Vec<AssetTerms>,
    reference_terms: Vec<PriceReferenceTerms>,
}

impl OkxListedInstrument {
    /// Reads one described instrument record, answering `None` for a product or
    /// a market state this model does not publish.
    fn read(
        route: &VenueRoute,
        instrument_type: OkxInstrumentType,
        row: OkxInstrumentWire,
    ) -> Result<Option<Self>, MarketParticipantError> {
        let instrument_id = row.inst_id.clone();
        let Some(status) = listing_status(&row) else {
            log::warn!(
                "OKX instrument {instrument_id} of {route} reports a listing state this model cannot express and is not published"
            );
            return Ok(None);
        };
        // An OKX swap publishes which of the two contract shapes it is, and that
        // field is the only place either shape is stated: every contract below is
        // expressed from it rather than from the instrument's own name. A swap
        // stating a shape this build cannot express is a product it cannot publish,
        // which is a listing to leave out and not a route to fail.
        let shape = match instrument_type {
            OkxInstrumentType::Perpetual => {
                let Some(contract_type) = OkxSwapContractType::read(row.ct_type.as_ref())? else {
                    log::warn!(
                        "OKX instrument {instrument_id} of {route} states a contract type this model cannot express and is not published"
                    );
                    return Ok(None);
                };
                OkxInstrumentShape::Swap(contract_type)
            }
            OkxInstrumentType::Spot => OkxInstrumentShape::Spot,
            // The two remaining products are refused before a row is parsed;
            // this route serves spot and perpetual only.
            OkxInstrumentType::Future | OkxInstrumentType::Option => return Ok(None),
        };
        let (base, quote) = match instrument_type {
            OkxInstrumentType::Spot => {
                let Some(base) = non_empty(row.base_ccy.as_ref()) else {
                    return Err(protocol_error(
                        "decode OKX market instrument",
                        format_smolstr!("{instrument_id} omits its base currency"),
                    ));
                };
                let Some(quote) = non_empty(row.quote_ccy.as_ref()) else {
                    return Err(protocol_error(
                        "decode OKX market instrument",
                        format_smolstr!("{instrument_id} omits its quote currency"),
                    ));
                };
                (base, quote)
            }
            OkxInstrumentType::Perpetual => {
                let Some(underlying) = non_empty(row.uly.as_ref()) else {
                    return Err(protocol_error(
                        "decode OKX market instrument",
                        format_smolstr!("{instrument_id} omits its underlying"),
                    ));
                };
                let mut fields = underlying.split('-');
                let (Some(base), Some(quote), None) = (fields.next(), fields.next(), fields.next())
                else {
                    return Err(protocol_error(
                        "decode OKX market instrument",
                        format_smolstr!("{instrument_id} has an unreadable underlying"),
                    ));
                };
                if base.is_empty() || quote.is_empty() {
                    return Err(protocol_error(
                        "decode OKX market instrument",
                        format_smolstr!("{instrument_id} has an unreadable underlying"),
                    ));
                }
                (base, quote)
            }
            // The two remaining products are refused before a row is parsed;
            // this route serves spot and perpetual only.
            OkxInstrumentType::Future | OkxInstrumentType::Option => return Ok(None),
        };
        let quote_asset = asset(quote);
        let base_asset = asset(base);
        let list_time = non_empty(row.list_time.as_ref())
            .and_then(|value| value.parse::<i64>().ok())
            .filter(|list_time| *list_time > 0);
        Ok(Some(Self {
            instrument_id,
            instrument_type,
            shape,
            instrument_family: non_empty(row.instrument_family.as_ref())
                .map(|family| {
                    NativeSymbol::new(family)
                        .map_err(|error| model_error("decode OKX instrument family", error))
                })
                .transpose()?,
            base: base_asset,
            settle: non_empty(row.settle_ccy.as_ref())
                .map(asset)
                .unwrap_or_else(|| quote_asset.clone()),
            quote: quote_asset,
            tick: required_tick(&row)?,
            lot_size: required_lot_size(&row)?,
            minimum: optional_quantity(row.min_sz.as_ref(), "decode OKX minimum size")?,
            maximum: optional_quantity(row.max_lmt_sz.as_ref(), "decode OKX maximum size")?,
            contract_value: optional_quantity(row.ct_val.as_ref(), "decode OKX contract value")?,
            contract_value_currency: non_empty(row.ct_val_ccy.as_ref()).map(asset),
            contract_multiplier: optional_quantity(row.ct_mult.as_ref(), "decode OKX multiplier")?,
            status,
            // A scheduled listing is available from the instant the venue
            // published; a live one has no lower bound to report.
            available_from: (status == ListingStatus::Scheduled)
                .then_some(list_time)
                .flatten(),
        }))
    }

    /// Builds the canonical catalog values of this listing.
    ///
    /// `None` is a listing this build cannot express at all: the venue states a
    /// contract multiplier the model has no way to fold into a contract value, which
    /// is a product decision and not a protocol contradiction. A row whose stated
    /// terms contradict the shape it also states is a protocol failure instead and is
    /// reported as one.
    fn terms(&self) -> Result<Option<OkxInstrumentTerms>, MarketParticipantError> {
        match self.shape {
            OkxInstrumentShape::Spot => {
                let terms = SpotTerms::new(self.base.asset_id(), self.quote.asset_id())
                    .map_err(|error| model_error("decode OKX market instrument", error))?;
                let instrument = Instrument::new(InstrumentTerms::Spot(terms))
                    .map_err(|error| model_error("decode OKX market instrument", error))?;
                Ok(Some(OkxInstrumentTerms {
                    instrument,
                    rules: self.spot_rules()?,
                    // A spot pair tracks no reference and its contract value is not
                    // a quote notional, so the two assets it exchanges are the whole
                    // of what it names.
                    asset_terms: vec![self.base.clone(), self.quote.clone()],
                    reference_terms: Vec::new(),
                }))
            }
            OkxInstrumentShape::Swap(contract_type) => {
                let contract_value = self.contract_value.ok_or_else(|| {
                    protocol_error(
                        "decode OKX contract value",
                        format_smolstr!("{} omits its contract value", self.instrument_id),
                    )
                })?;
                let mark = mark_reference(&self.base.asset_id(), &self.quote.asset_id())?;
                let underlying = DerivativeUnderlying::new(
                    UnderlyingRef::Asset(self.base.asset_id()),
                    mark.reference_id()
                        .map_err(|error| model_error("build OKX price reference", error))?,
                );
                // `ctVal` counts the contract value in the currency the venue
                // states for the contract shape, so the two shapes read the same
                // field into different units rather than into different numbers.
                let denomination = contract_type.contract_value_currency(&self.base, &self.quote);
                if !self.check_stated_contract_terms(&denomination)? {
                    return Ok(None);
                }
                let contract_size = ContractSize::new(
                    ContractUnit::asset_quantity(contract_value, denomination.asset_id())
                        .map_err(|error| model_error("decode OKX contract value", error))?,
                )
                .map_err(|error| model_error("decode OKX contract value", error))?;
                // An inverse contract values a position of `ctVal` quote units in
                // the underlying, so it settles in the very asset it tracks and
                // the venue's mark of that asset is the rate its settlement
                // converts through.
                let pnl_model = match contract_type {
                    OkxSwapContractType::Linear => DerivativePnlModel::Linear,
                    OkxSwapContractType::Inverse => {
                        if self.settle.asset_id() != self.base.asset_id() {
                            return Err(protocol_error(
                                "decode OKX contract type",
                                format_smolstr!(
                                    "{} is inverse but does not settle in the asset it tracks",
                                    self.instrument_id
                                ),
                            ));
                        }
                        DerivativePnlModel::Inverse
                    }
                };
                let conversion = matches!(pnl_model, DerivativePnlModel::Inverse)
                    .then(|| conversion_reference(&self.base.asset_id(), &self.quote.asset_id()))
                    .transpose()?;
                let terms = PerpetualTerms::new(
                    underlying,
                    self.quote.asset_id(),
                    self.settle.asset_id(),
                    contract_size,
                    pnl_model,
                    conversion.as_ref(),
                )
                .map_err(|error| model_error("decode OKX market instrument", error))?;
                let instrument = Instrument::new(InstrumentTerms::Perpetual(terms))
                    .map_err(|error| model_error("decode OKX market instrument", error))?;
                // A conversion states the direction `S -> Q`, so an inverse contract
                // names a second reference role whenever its shape requires one; on
                // this venue that observation is field for field the mark above, so
                // the registry carries one entry for the two roles.
                let reference_terms = std::iter::once(mark)
                    .chain(conversion)
                    .collect::<Vec<PriceReferenceTerms>>();
                Ok(Some(OkxInstrumentTerms {
                    instrument,
                    rules: self.perpetual_rules()?,
                    asset_terms: vec![
                        self.base.clone(),
                        self.quote.clone(),
                        self.settle.clone(),
                        denomination,
                    ],
                    reference_terms,
                }))
            }
        }
    }

    /// Cross-checks the contract terms the venue states against the ones its own
    /// contract shape implies.
    ///
    /// The shape is derived from `ctType`, and a real OKX instrument response also
    /// publishes `ctValCcy` and `ctMult` beside it. When the venue states them they
    /// are the venue's own account of what its contract value means, so a row whose
    /// stated term disagrees with the derived one contradicts itself: publishing it
    /// either way would publish a contract the venue does not trade.
    ///
    /// A response that omits them carries no such statement, so the derivation from
    /// `ctType` is all there is to publish from and the contract stands; the fields
    /// are optional in the wire shape for exactly that reason, because this build has
    /// no verified sample of every product's response to pin their presence to.
    ///
    /// A multiplier the model cannot fold into the contract value is a product this
    /// build has no terms for rather than a contradiction, so it is reported like any
    /// other unsupported contract: `Ok(false)` leaves the listing out of the catalog
    /// with a warning instead of failing the route.
    fn check_stated_contract_terms(
        &self,
        denomination: &AssetTerms,
    ) -> Result<bool, MarketParticipantError> {
        if let Some(stated) = &self.contract_value_currency
            && stated.asset_id() != denomination.asset_id()
        {
            return Err(protocol_error(
                "decode OKX contract value currency",
                format_smolstr!(
                    "{} states ctValCcy {} but values its contract in {}",
                    self.instrument_id,
                    stated.asset_id(),
                    denomination.asset_id()
                ),
            ));
        }
        if let Some(multiplier) = self.contract_multiplier
            && multiplier != Quantity::from(1_u8)
        {
            log::warn!(
                "OKX instrument {} of {} states a contract multiplier this model cannot express and is not published",
                self.instrument_id,
                self.instrument_type.label()
            );
            return Ok(false);
        }
        Ok(true)
    }

    /// The rules of one OKX spot listing.
    fn spot_rules(&self) -> Result<TradingRules, MarketParticipantError> {
        Ok(TradingRules {
            common: CommonTradingRules {
                price: PriceRule {
                    quote_asset: self.quote.asset_id(),
                    basis: PriceBasis::PerAssetUnit,
                    tick: self.tick,
                    minimum: None,
                    maximum: None,
                },
                quantity: QuantityRule {
                    unit: QuantityUnit::AssetUnits {
                        asset: self.base.asset_id(),
                    },
                    step: self.lot_size,
                    minimum: self.minimum,
                    maximum: self.maximum,
                },
                notional: None,
                venue_policy: Arc::new(VenueTradingPolicy::continuous()),
            },
            product: Arc::new(ProductTradingRules::Spot(
                SpotTradingRules::new(
                    LotPolicy::FractionalAllowed {
                        lot_size: self.lot_size,
                        minimum_fraction: None,
                    },
                    ShortSalePolicy::NotApplicable,
                )
                .map_err(|error| model_error("build OKX spot rules", error))?,
            )),
        })
    }

    /// The rules of one OKX perpetual.
    ///
    /// OKX publishes a perpetual's margin requirements and leverage ladder as the
    /// contract's own position tiers, outside the instrument record this row came
    /// from, and this build has no reader for them: the listing is published with
    /// its risk unloaded, and nothing loads the terms for the contract's family
    /// afterwards.
    fn perpetual_rules(&self) -> Result<TradingRules, MarketParticipantError> {
        Ok(TradingRules {
            common: CommonTradingRules {
                price: PriceRule {
                    quote_asset: self.quote.asset_id(),
                    basis: PriceBasis::PerAssetUnit,
                    tick: self.tick,
                    minimum: None,
                    maximum: None,
                },
                quantity: QuantityRule {
                    unit: QuantityUnit::Contracts,
                    step: self.lot_size,
                    minimum: self.minimum,
                    maximum: self.maximum,
                },
                notional: None,
                venue_policy: Arc::new(VenueTradingPolicy::continuous()),
            },
            product: Arc::new(ProductTradingRules::Perpetual(PerpetualTradingRules {
                risk: PerpetualRisk::Unloaded,
                funding: FundingSchedule::new(
                    FundingInterval::Adaptive,
                    None,
                    None,
                    self.settle.asset_id(),
                )
                .map_err(|error| model_error("build OKX funding schedule", error))?,
                open_interest_cap: None,
            })),
        })
    }
}

/// The mark-price reference of one derivative underlying.
///
/// OKX publishes the mark price of an instrument against its own index, which is
/// what this reference identity describes: the venue's direct, continuously
/// observed mark of that underlying quoted in that asset. The terms are the
/// identity, so they are built here and handed on with the instrument that names
/// them.
fn mark_reference(
    base: &AssetId,
    quote: &AssetId,
) -> Result<PriceReferenceTerms, MarketParticipantError> {
    let source = ReferenceSourceId::new(OKX_NAMESPACE)
        .map_err(|error| model_error("build OKX price reference", error))?;
    let version = ReferenceTermsVersion::new(REFERENCE_TERMS_VERSION)
        .map_err(|error| model_error("build OKX price reference", error))?;
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
        schema_version: version,
    };
    terms
        .reference_id()
        .map_err(|error| model_error("build OKX price reference", error))?;
    Ok(terms)
}

/// The registry terms of the conversion rate between one settlement asset and one
/// price quote.
///
/// On OKX that rate is the venue's own continuously observed mark of the
/// settlement asset quoted in the price quote — the same observation an asset
/// underlying is priced by, because a contract that settles in the asset it
/// tracks converts its settlement through exactly that price. The terms are the
/// registry entry a contract's conversion reference names, and the model
/// validates the direction they have to carry; nothing here is derived from a
/// native symbol or a hash.
fn conversion_reference(
    settlement_asset: &AssetId,
    price_quote: &AssetId,
) -> Result<PriceReferenceTerms, MarketParticipantError> {
    Ok(PriceReferenceTerms {
        underlying: UnderlyingRef::Asset(settlement_asset.clone()),
        quote_asset: price_quote.clone(),
        basis: PriceBasis::PerAssetUnit,
        observation: ReferenceObservation {
            source: ReferenceSourceId::new(OKX_NAMESPACE)
                .map_err(|error| model_error("build OKX conversion reference", error))?,
            kind: ReferenceObservationKind::Mark,
        },
        aggregation: ReferenceAggregation::Direct,
        session: ReferenceSessionConvention::Continuous,
        settlement: ReferenceSettlementConvention::Continuous,
        schema_version: ReferenceTermsVersion::new(REFERENCE_TERMS_VERSION)
            .map_err(|error| model_error("build OKX conversion reference", error))?,
    })
}

/// The canonical asset of one OKX currency code.
///
/// A real ISO 4217 code is a currency and every other code is a token OKX issues
/// itself: the chain identity of a listed asset cannot be verified from the
/// venue's own catalog, so claiming one would be an invention. One code always
/// names one asset, so an instrument and a balance that name the same code are
/// denominated in the same canonical asset.
///
/// The terms are built here, where the venue's own code is still authoritative,
/// and the identity is derived from them: a digest cannot be read back into the
/// terms the display contract renders, so a caller that needs the identity takes
/// it from [`AssetTerms::asset_id`] rather than the other way round.
pub(super) fn asset(code: &str) -> AssetTerms {
    if let Ok(iso) = Iso4217Code::new(code) {
        return AssetTerms::Currency(CurrencyTerms { code: iso });
    }
    let terms = CryptoTokenTerms {
        origin: CryptoTokenOrigin::VenueIssued {
            issuer: TokenIssuerId::new(OKX_NAMESPACE).expect("the OKX issuer is a canonical name"),
            namespace: IdentifierNamespace::new(OKX_NAMESPACE)
                .expect("the OKX namespace is canonical"),
            token: TokenReference::Native {
                code: IdentifierValue::new(code)
                    .expect("a venue currency code is a valid identifier"),
            },
        },
    };
    terms
        .validate()
        .expect("a venue-issued native token is a canonical token identity");
    AssetTerms::CryptoToken(terms)
}

/// The listing state one OKX instrument reports, when it is a state this model has.
fn listing_status(instrument: &OkxInstrumentWire) -> Option<ListingStatus> {
    match instrument.state.as_deref() {
        Some("live") => Some(ListingStatus::Trading),
        Some("suspend") => Some(ListingStatus::Halted),
        // A scheduled instrument is only expressible with the instant it becomes
        // available, which the venue publishes as `listTime`.
        Some("preopen") => non_empty(instrument.list_time.as_ref())
            .and_then(|value| value.parse::<i64>().ok())
            .filter(|list_time| *list_time > 0)
            .map(|_| ListingStatus::Scheduled),
        _ => None,
    }
}

fn required_tick(instrument: &OkxInstrumentWire) -> Result<Tick, MarketParticipantError> {
    let value = non_empty(instrument.tick_sz.as_ref()).ok_or_else(|| {
        protocol_error(
            "decode OKX price tick",
            format_smolstr!("{} omits tickSz", instrument.inst_id),
        )
    })?;
    parse_text(value, "decode OKX price tick")
}

fn required_lot_size(instrument: &OkxInstrumentWire) -> Result<Quantity, MarketParticipantError> {
    let value = non_empty(instrument.lot_sz.as_ref()).ok_or_else(|| {
        protocol_error(
            "decode OKX quantity step",
            format_smolstr!("{} omits lotSz", instrument.inst_id),
        )
    })?;
    parse_text(value, "decode OKX quantity step")
}

fn optional_quantity(
    value: Option<&SmolStr>,
    operation: &'static str,
) -> Result<Option<Quantity>, MarketParticipantError> {
    non_empty(value)
        .map(|value| parse_text::<Quantity>(value, operation))
        .transpose()
}

fn parse_text<T: std::str::FromStr>(
    value: &str,
    operation: &'static str,
) -> Result<T, MarketParticipantError> {
    value
        .parse()
        .map_err(|_| protocol_error(operation, "invalid numeric value"))
}

fn non_empty(value: Option<&SmolStr>) -> Option<&str> {
    value
        .map(SmolStr::as_str)
        .filter(|value| !value.trim().is_empty())
}

/// One row of the OKX instrument response, as the venue publishes it.
///
/// The optional fields are the ones the venue states beside the terms a contract is
/// described by: a response that omits one still describes its product completely, so
/// they are read when present and derived when absent. The derivation is
/// [`OkxSwapContractType`], which is the shape the venue states in `ctType`.
#[derive(Clone, Debug, serde::Deserialize)]
struct OkxInstrumentWire {
    #[serde(rename = "instId")]
    inst_id: SmolStr,
    #[serde(rename = "baseCcy")]
    base_ccy: Option<SmolStr>,
    #[serde(rename = "quoteCcy")]
    quote_ccy: Option<SmolStr>,
    #[serde(rename = "settleCcy")]
    settle_ccy: Option<SmolStr>,
    #[serde(rename = "tickSz")]
    tick_sz: Option<SmolStr>,
    #[serde(rename = "lotSz")]
    lot_sz: Option<SmolStr>,
    #[serde(rename = "minSz")]
    min_sz: Option<SmolStr>,
    #[serde(rename = "maxLmtSz")]
    max_lmt_sz: Option<SmolStr>,
    #[serde(rename = "ctVal")]
    ct_val: Option<SmolStr>,
    /// The currency the contract value is counted in.
    ///
    /// OKX publishes it beside `ctVal`, and it is the venue's own statement of what
    /// the contract value means: a linear swap counts `ctVal` in the asset it tracks
    /// and an inverse one counts it in the price quote.
    #[serde(rename = "ctValCcy")]
    ct_val_ccy: Option<SmolStr>,
    /// The contract's multiplier.
    ///
    /// The model expresses one contract at a time, so a venue multiplier this build
    /// cannot fold into the contract value is a contract it cannot publish.
    #[serde(rename = "ctMult")]
    ct_mult: Option<SmolStr>,
    #[serde(rename = "ctType")]
    ct_type: Option<SmolStr>,
    #[serde(rename = "listTime")]
    list_time: Option<SmolStr>,
    /// The venue's own instrument family, which its position tiers are keyed by.
    #[serde(rename = "instFamily")]
    instrument_family: Option<SmolStr>,
    uly: Option<SmolStr>,
    state: Option<SmolStr>,
}

// ---------------------------------------------------------------------------
// Route-scoped market data
// ---------------------------------------------------------------------------

/// Fetches every ticker of one route.
async fn fetch_all_tickers(
    context: &ExecutionContext,
    query: TickersQuery,
) -> Result<ProtocolResponse, MarketParticipantError> {
    let route = query.route().clone();
    let instrument_type = OkxCodec
        .scope_instrument_type(&route.scope)
        .ok_or_else(|| MarketParticipantError::UnsupportedCapability {
            route: route.clone(),
            capability: CapabilityId::Tickers,
        })?;
    let body = public_get(
        context,
        "/api/v5/market/tickers",
        &[(
            "instType",
            OkxCodec.instrument_type_param(instrument_type).to_owned(),
        )],
    )
    .await?;
    Ok(ProtocolResponse::Tickers(decode_tickers(
        &route,
        instrument_type,
        &body,
    )?))
}

// ---------------------------------------------------------------------------
// Listing-scoped market data
// ---------------------------------------------------------------------------

/// Fetches candlesticks of one listing.
async fn fetch_candlesticks(
    context: &ExecutionContext,
    listing: Option<&ResolvedListingRef>,
    query: CandlestickFetchQuery,
) -> Result<ProtocolResponse, MarketParticipantError> {
    query
        .validate()
        .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{error}")))?;
    let listing = resolved_listing(listing)?;
    let interval = OkxCodec.encode_timeframe(&context.route(), query.timeframe)?;
    let params = candle_params(&listing.instrument_id, interval, &query);
    let body = public_get(context, "/api/v5/market/candles", &params).await?;
    let candles = decode_candlesticks(&body)?;
    Ok(ProtocolResponse::Candlesticks(
        candles
            .into_iter()
            .map(|candle| NativeListing::new(listing.key.clone(), candle))
            .collect(),
    ))
}

/// Fetches the ticker of one listing.
async fn fetch_ticker(
    context: &ExecutionContext,
    listing: Option<&ResolvedListingRef>,
    query: TickerQuery,
) -> Result<ProtocolResponse, MarketParticipantError> {
    query
        .validate()
        .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{error}")))?;
    let listing = resolved_listing(listing)?;
    let body = public_get(
        context,
        "/api/v5/market/ticker",
        &[("instId", listing.instrument_id.to_string())],
    )
    .await?;
    Ok(ProtocolResponse::Ticker(decode_ticker(
        listing.instrument_type,
        &listing.key,
        listing.instrument_id.as_str(),
        &body,
    )?))
}

/// Fetches one order-book snapshot of one listing.
async fn fetch_order_book(
    context: &ExecutionContext,
    listing: Option<&ResolvedListingRef>,
    query: OrderBookQuery,
) -> Result<ProtocolResponse, MarketParticipantError> {
    query
        .validate()
        .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{error}")))?;
    if query.depth > ORDER_BOOK_DEPTH_LIMIT {
        return Err(MarketParticipantError::InvalidLimit {
            operation: "OKX order book".into(),
            requested: query.depth,
            maximum: ORDER_BOOK_DEPTH_LIMIT,
        });
    }
    let listing = resolved_listing(listing)?;
    let body = public_get(
        context,
        "/api/v5/market/books",
        &[
            ("instId", listing.instrument_id.to_string()),
            ("sz", query.depth.to_string()),
        ],
    )
    .await?;
    Ok(ProtocolResponse::OrderBook(decode_order_book(
        &listing.key,
        &body,
    )?))
}

/// Fetches the recent market trades of one listing.
async fn fetch_trades(
    context: &ExecutionContext,
    listing: Option<&ResolvedListingRef>,
    query: MarketTradeFetchQuery,
) -> Result<ProtocolResponse, MarketParticipantError> {
    query
        .validate()
        .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{error}")))?;
    let listing = resolved_listing(listing)?;
    let mut params = vec![
        ("instId", listing.instrument_id.to_string()),
        ("limit", query.limit.to_string()),
    ];
    if let Some(start_timestamp) = query.start_timestamp {
        params.push(("after", start_timestamp.to_string()));
    }
    if let Some(end_timestamp) = query.end_timestamp {
        params.push(("before", end_timestamp.to_string()));
    }
    let body = public_get(context, "/api/v5/market/trades", &params).await?;
    Ok(ProtocolResponse::Trades(decode_trades(
        &listing.key,
        listing.instrument_id.as_str(),
        &body,
    )?))
}

/// The query parameters of one candlestick request.
///
/// OKX names its pagination from the returned records' point of view: `after`
/// selects the records *earlier* than the timestamp and `before` selects the
/// records *newer* than it, which is the opposite of what the names suggest. A
/// range therefore has to be sent as `before` for its start and `after` for its
/// end, each shifted by one millisecond because both bounds are exclusive.
fn candle_params(
    instrument_id: &str,
    interval: &str,
    query: &CandlestickFetchQuery,
) -> Vec<(&'static str, String)> {
    let mut params = vec![
        ("instId", instrument_id.to_owned()),
        ("bar", interval.to_owned()),
        ("limit", query.limit.to_string()),
    ];
    if let Some(start_timestamp) = query.start_timestamp {
        params.push(("before", start_timestamp.saturating_sub(1).to_string()));
    }
    if let Some(end_timestamp) = query.end_timestamp {
        params.push(("after", end_timestamp.saturating_add(1).to_string()));
    }
    params
}

// ---------------------------------------------------------------------------
// Perpetual statistics
// ---------------------------------------------------------------------------

async fn fetch_open_interest(
    context: &ExecutionContext,
    listing: Option<&ResolvedListingRef>,
    query: PerpetualStatisticsQuery,
) -> Result<ProtocolResponse, MarketParticipantError> {
    query
        .validate()
        .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{error}")))?;
    let listing = resolved_listing(listing)?;
    let body = public_get(
        context,
        "/api/v5/rubik/stat/contracts/open-interest-history",
        &[
            ("instId", listing.instrument_id.to_string()),
            (
                "period",
                OkxCodec
                    .encode_statistics_timeframe(query.timeframe)
                    .to_owned(),
            ),
            ("limit", query.limit.to_string()),
        ],
    )
    .await?;
    let rows: Vec<OkxOpenInterestHistoryRow> =
        decode_public(&body, "decode OKX open-interest history response")?;
    let values = rows
        .into_iter()
        .map(
            |OkxOpenInterestHistoryRow([timestamp, _, quantity, notional])| {
                Ok(NativeListing::new(
                    listing.key.clone(),
                    NativeOpenInterest {
                        quantity: parse_text(&quantity, "decode OKX open interest quantity")?,
                        notional: Some(parse_text(&notional, "decode OKX open interest notional")?),
                        timestamp: parse_text(&timestamp, "decode OKX open interest timestamp")?,
                    },
                ))
            },
        )
        .collect::<Result<Vec<_>, MarketParticipantError>>()?;
    Ok(ProtocolResponse::OpenInterest(values))
}

async fn fetch_ratio(
    context: &ExecutionContext,
    listing: Option<&ResolvedListingRef>,
    query: PerpetualStatisticsQuery,
    kind: RatioKind,
) -> Result<ProtocolResponse, MarketParticipantError> {
    let path = match kind {
        RatioKind::LongShortAccount => {
            "/api/v5/rubik/stat/contracts/long-short-account-ratio-contract"
        }
        RatioKind::TopTraderAccount => {
            "/api/v5/rubik/stat/contracts/long-short-account-ratio-contract-top-trader"
        }
        RatioKind::TopTraderPosition => {
            "/api/v5/rubik/stat/contracts/long-short-position-ratio-contract-top-trader"
        }
    };
    let body = rubik(context, listing, &query, path, false).await?;
    let rows: Vec<OkxRatioRow> = decode_public(&body, "decode OKX ratio response")?;
    let listing = resolved_listing(listing)?.key;
    let values = rows
        .into_iter()
        .map(|OkxRatioRow([timestamp, ratio])| {
            Ok(NativeListing::new(
                listing.clone(),
                NativeLongShortRatio {
                    ratio: parse_text(&ratio, "decode OKX long-short ratio")?,
                    timestamp: parse_text(&timestamp, "decode OKX ratio timestamp")?,
                },
            ))
        })
        .collect::<Result<Vec<_>, MarketParticipantError>>()?;
    Ok(ProtocolResponse::LongShortRatios(values))
}

async fn fetch_taker_volume(
    context: &ExecutionContext,
    listing: Option<&ResolvedListingRef>,
    query: PerpetualStatisticsQuery,
) -> Result<ProtocolResponse, MarketParticipantError> {
    let body = rubik(
        context,
        listing,
        &query,
        "/api/v5/rubik/stat/taker-volume-contract",
        true,
    )
    .await?;
    let rows: Vec<OkxTakerVolumeRow> = decode_public(&body, "decode OKX taker-volume response")?;
    let listing = resolved_listing(listing)?.key;
    let values = rows
        .into_iter()
        .map(|OkxTakerVolumeRow([timestamp, sell, buy])| {
            Ok(NativeListing::new(
                listing.clone(),
                NativeTakerVolume {
                    buy: parse_text(&buy, "decode OKX taker buy volume")?,
                    sell: parse_text(&sell, "decode OKX taker sell volume")?,
                    timestamp: parse_text(&timestamp, "decode OKX taker volume timestamp")?,
                },
            ))
        })
        .collect::<Result<Vec<_>, MarketParticipantError>>()?;
    Ok(ProtocolResponse::TakerVolumes(values))
}

/// Reads one rubik statistics endpoint for one listing.
///
/// `contracts_unit` asks the endpoint for its contract-denominated answer. Only
/// the taker-volume endpoint takes the parameter, and it is what makes the two
/// volumes it returns the unit the listing's own quantity rule declares: a
/// contract count is what a `Contracts` listing trades in.
async fn rubik(
    context: &ExecutionContext,
    listing: Option<&ResolvedListingRef>,
    query: &PerpetualStatisticsQuery,
    path: &str,
    contracts_unit: bool,
) -> Result<Bytes, MarketParticipantError> {
    query
        .validate()
        .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{error}")))?;
    let listing = resolved_listing(listing)?;
    let mut params = vec![
        ("instId", listing.instrument_id.to_string()),
        (
            "period",
            OkxCodec
                .encode_statistics_timeframe(query.timeframe)
                .to_owned(),
        ),
        ("limit", query.limit.to_string()),
    ];
    if contracts_unit {
        params.push(("unit", "0".to_owned()));
    }
    public_get(context, path, &params).await
}

/// One rubik statistics response.
#[derive(serde::Deserialize)]
struct OkxOpenInterestHistoryRow([SmolStr; 4]);
#[derive(serde::Deserialize)]
struct OkxRatioRow([SmolStr; 2]);
#[derive(serde::Deserialize)]
struct OkxTakerVolumeRow([SmolStr; 3]);

#[derive(Clone, Copy)]
enum RatioKind {
    LongShortAccount,
    TopTraderAccount,
    TopTraderPosition,
}

// ---------------------------------------------------------------------------
// Authenticated trading
// ---------------------------------------------------------------------------

/// Places one order over the authenticated socket.
async fn place_order(
    context: &ExecutionContext,
    listing: Option<&ResolvedListingRef>,
    command: PlaceOrderCommand,
) -> Result<ProtocolResponse, MarketParticipantError> {
    command
        .validate()
        .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{error}")))?;
    let listing = resolved_listing(listing)?;
    let route = context.route();
    let td_mode = OkxCodec.encode_trade_mode(&route, command.margin_mode)?;
    let (ord_type, price) = match &command.kind {
        OrderKind::Market => ("market", None),
        OrderKind::Limit { price, .. } => ("limit", Some(format_smolstr!("{price}"))),
        _ => {
            return Err(MarketParticipantError::Configuration(
                "OKX orders are placed as market or limit orders".into(),
            ));
        }
    };
    let size = match &command.quantity {
        OrderQuantity::AssetUnits { value, .. } | OrderQuantity::Contracts(value) => {
            format_smolstr!("{value}")
        }
        OrderQuantity::Notional { .. } => {
            return Err(MarketParticipantError::Configuration(
                "OKX orders are sized in the listing's own traded unit, not as an amount".into(),
            ));
        }
        OrderQuantity::OutcomeTokens { .. } => {
            return Err(MarketParticipantError::UnsupportedCapability {
                route,
                capability: CapabilityId::AuthenticatedTrading,
            });
        }
    };
    let order = OkxCodec.encode_order_request(
        listing.instrument_id.clone(),
        td_mode,
        match command.side {
            Side::Buy => "buy",
            Side::Sell => "sell",
        },
        ord_type,
        size,
        price,
    );
    let connection = authenticated_connection(context).await?;
    let request_id = connection.allocate_request_id();
    let payload = OkxCodec.encode(&OkxCommand {
        id: request_id,
        op: "order",
        args: vec![order],
    })?;
    let response_payload = connection
        .send_and_wait_with_id(request_id, payload, REQUEST_TIMEOUT)
        .await?;
    let response: OkxOrderResponse = OkxCodec.decode(&response_payload)?;
    if response.code != "0" {
        return Err(OkxCodec.map_error(
            response.code.as_str(),
            response.msg.unwrap_or_default(),
            Some(response_payload),
        ));
    }
    let item = response.data.into_iter().next().ok_or_else(|| {
        protocol_error("decode OKX order response", "order response omitted data")
    })?;
    if item.s_code.as_deref().is_some_and(|code| code != "0") {
        return Err(OkxCodec.map_error(
            item.s_code.as_deref().unwrap_or_default(),
            item.s_msg.unwrap_or_default(),
            Some(response_payload),
        ));
    }
    let order_id = item.ord_id.ok_or_else(|| {
        protocol_error("decode OKX order response", "order response omitted ordId")
    })?;
    Ok(ProtocolResponse::OrderPlaced(
        OrderId::normal(order_id).map_err(|error| model_error("create OKX order ID", error))?,
    ))
}

/// Cancels one order over the authenticated socket.
async fn cancel_order(
    context: &ExecutionContext,
    listing: Option<&ResolvedListingRef>,
    command: CancelOrderCommand,
) -> Result<ProtocolResponse, MarketParticipantError> {
    let listing = resolved_listing(listing)?;
    let connection = authenticated_connection(context).await?;
    let request_id = connection.allocate_request_id();
    let payload = OkxCodec.encode(&OkxCommand {
        id: request_id,
        op: "cancel-order",
        args: vec![super::OkxCancelOrderRequest {
            inst_id: listing.instrument_id.clone(),
            ord_id: format_smolstr!("{}", command.order_id),
        }],
    })?;
    let response_payload = connection
        .send_and_wait_with_id(request_id, payload, REQUEST_TIMEOUT)
        .await?;
    let response: OkxOrderResponse = OkxCodec.decode(&response_payload)?;
    if response.code != "0" {
        return Err(OkxCodec.map_error(
            response.code.as_str(),
            response.msg.unwrap_or_default(),
            Some(response_payload),
        ));
    }
    Ok(ProtocolResponse::OrderCancelled(command.order_id))
}

// ---------------------------------------------------------------------------
// Authenticated account reads
// ---------------------------------------------------------------------------

async fn fetch_balances(
    context: &ExecutionContext,
    query: AuthenticatedQuery,
) -> Result<ProtocolResponse, MarketParticipantError> {
    query
        .validate()
        .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{error}")))?;
    let rows: Vec<OkxBalanceAccount> =
        signed_get(context, "/api/v5/account/balance", Vec::new()).await?;
    Ok(ProtocolResponse::Balances(decode_balances(&rows)?))
}

async fn fetch_active_orders(
    context: &ExecutionContext,
    listing: Option<&ResolvedListingRef>,
    query: AuthenticatedQuery,
) -> Result<ProtocolResponse, MarketParticipantError> {
    query
        .validate()
        .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{error}")))?;
    let listing = optional_listing(listing)?;
    let instrument_type = scope_instrument_type(context)?;
    let parameters = order_parameters(instrument_type, listing.as_ref(), PAGE_LIMIT);
    let normal: Vec<OkxOrder> =
        signed_get(context, "/api/v5/trade/orders-pending", parameters.clone()).await?;
    let mut orders = decode_orders(
        &context.route(),
        instrument_type,
        OrderRoute::Normal,
        normal,
    )?;
    for order_type in ["conditional", "trigger"] {
        let mut params = parameters.clone();
        params.push(("ordType".to_owned(), order_type.to_owned()));
        let rows: Vec<OkxOrder> =
            signed_get(context, "/api/v5/trade/orders-algo-pending", params).await?;
        orders.extend(decode_orders(
            &context.route(),
            instrument_type,
            OrderRoute::Algo,
            rows,
        )?);
    }
    Ok(ProtocolResponse::Orders(orders))
}

async fn fetch_order_history(
    context: &ExecutionContext,
    listing: Option<&ResolvedListingRef>,
    query: AuthenticatedQuery,
) -> Result<ProtocolResponse, MarketParticipantError> {
    validate_history(&query, "fetch order history")?;
    let listing = optional_listing(listing)?;
    let instrument_type = scope_instrument_type(context)?;
    let mut params = order_parameters(instrument_type, listing.as_ref(), PAGE_LIMIT);
    if let Some(value) = query.start_timestamp {
        params.push(("begin".to_owned(), value.to_string()));
    }
    if let Some(value) = query.end_timestamp {
        params.push(("end".to_owned(), value.to_string()));
    }
    let normal: Vec<OkxOrder> =
        signed_get(context, "/api/v5/trade/orders-history", params.clone()).await?;
    let mut orders = decode_orders(
        &context.route(),
        instrument_type,
        OrderRoute::Normal,
        normal,
    )?;
    for order_type in ["conditional", "trigger"] {
        let mut algo_params = params.clone();
        algo_params.push(("ordType".to_owned(), order_type.to_owned()));
        let rows: Vec<OkxOrder> =
            signed_get(context, "/api/v5/trade/orders-algo-history", algo_params).await?;
        orders.extend(decode_orders(
            &context.route(),
            instrument_type,
            OrderRoute::Algo,
            rows,
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
    context: &ExecutionContext,
    listing: Option<&ResolvedListingRef>,
    query: AuthenticatedQuery,
) -> Result<ProtocolResponse, MarketParticipantError> {
    query
        .validate()
        .map_err(|error| MarketParticipantError::Configuration(format_smolstr!("{error}")))?;
    let listing = optional_listing(listing)?;
    let instrument_type = scope_instrument_type(context)?;
    let rows: Vec<OkxPosition> = signed_get(
        context,
        "/api/v5/account/positions",
        order_parameters(instrument_type, listing.as_ref(), PAGE_LIMIT),
    )
    .await?;
    let mut positions = decode_positions(&context.route(), instrument_type, rows)?;
    // An account read lists the positions it holds; a position the venue reports
    // with no quantity is one it no longer holds.
    positions.retain(|position| !position.quantity.is_zero());
    Ok(ProtocolResponse::Positions(positions))
}

async fn fetch_account_trades(
    context: &ExecutionContext,
    listing: Option<&ResolvedListingRef>,
    query: AuthenticatedQuery,
) -> Result<ProtocolResponse, MarketParticipantError> {
    validate_history(&query, "fetch account trades")?;
    let listing = optional_listing(listing)?;
    let instrument_type = scope_instrument_type(context)?;
    let mut params = order_parameters(instrument_type, listing.as_ref(), PAGE_LIMIT);
    if let Some(value) = query.start_timestamp {
        params.push(("begin".to_owned(), value.to_string()));
    }
    if let Some(value) = query.end_timestamp {
        params.push(("end".to_owned(), value.to_string()));
    }
    let rows: Vec<OkxTrade> = signed_get(context, "/api/v5/trade/fills-history", params).await?;
    let mut trades = decode_account_trades(&context.route(), instrument_type, rows)?;
    trades.truncate(usize::from(query.limit));
    Ok(ProtocolResponse::AccountTrades(trades))
}

async fn fetch_position_history(
    context: &ExecutionContext,
    listing: Option<&ResolvedListingRef>,
    query: AuthenticatedQuery,
) -> Result<ProtocolResponse, MarketParticipantError> {
    validate_history(&query, "fetch position history")?;
    let listing = optional_listing(listing)?;
    let instrument_type = scope_instrument_type(context)?;
    let rows: Vec<OkxHistoricalPosition> = signed_get(
        context,
        "/api/v5/account/positions-history",
        order_parameters(instrument_type, listing.as_ref(), PAGE_LIMIT),
    )
    .await?;
    let mut history = decode_position_history(&context.route(), instrument_type, rows)?;
    history.retain(|position| {
        query
            .start_timestamp
            .is_none_or(|start| position.closed_timestamp >= start)
            && query
                .end_timestamp
                .is_none_or(|end| position.closed_timestamp <= end)
    });
    history.truncate(usize::from(query.limit));
    Ok(ProtocolResponse::HistoricalPositions(history))
}

async fn fetch_trading_settings(
    context: &ExecutionContext,
    listing: Option<&ResolvedListingRef>,
    query: TradingSettingsQuery,
) -> Result<ProtocolResponse, MarketParticipantError> {
    let listing = resolved_listing(listing)?;
    let config: Vec<ConfigRow> = signed_get(context, "/api/v5/account/config", Vec::new()).await?;
    let position_mode = decode_position_mode(&config)?;
    let instrument_type = scope_instrument_type(context)?;
    let leverage: Vec<OkxLeverage> = signed_get(
        context,
        "/api/v5/account/leverage-info",
        vec![
            (
                "instType".to_owned(),
                OkxCodec.instrument_type_param(instrument_type).to_owned(),
            ),
            ("instId".to_owned(), listing.instrument_id.to_string()),
            (
                "mgnMode".to_owned(),
                match query.margin_mode {
                    MarginMode::Cross => "cross",
                    MarginMode::Isolated => "isolated",
                }
                .to_owned(),
            ),
        ],
    )
    .await?;
    let (margin_mode, leverage) = decode_leverage(&leverage)?;
    Ok(ProtocolResponse::TradingSettings(TradingSettings {
        position_mode,
        margin_mode,
        leverage,
    }))
}

/// The parameters every account read is narrowed by.
fn order_parameters(
    instrument_type: OkxInstrumentType,
    listing: Option<&OkxListing>,
    limit: u16,
) -> Vec<(String, String)> {
    let mut params = vec![
        (
            "instType".to_owned(),
            OkxCodec.instrument_type_param(instrument_type).to_owned(),
        ),
        ("limit".to_owned(), limit.to_string()),
    ];
    if let Some(listing) = listing {
        params.push(("instId".to_owned(), listing.instrument_id.to_string()));
    }
    params
}

// ---------------------------------------------------------------------------
// Subscriptions
// ---------------------------------------------------------------------------

/// One subscription as this adapter holds it.
///
/// The kind carries the listing every event of the subscription belongs to, so a
/// frame is decoded into the exact key the catalog published rather than into the
/// venue's identifier again.
#[derive(Clone, Debug)]
enum SubscriptionKind {
    Candlestick {
        listing: VenueListingKey,
        instrument_id: SmolStr,
        channel: SmolStr,
    },
    Ticker {
        listing: VenueListingKey,
        instrument_id: SmolStr,
        instrument_type: OkxInstrumentType,
    },
    OrderBook {
        listing: VenueListingKey,
        instrument_id: SmolStr,
    },
    Trade {
        listing: VenueListingKey,
        instrument_id: SmolStr,
    },
    ForceOrder {
        route: VenueRoute,
        instrument_type: OkxInstrumentType,
    },
    Orders {
        route: VenueRoute,
        instrument_type: OkxInstrumentType,
        listing: Option<VenueListingKey>,
    },
    Positions {
        route: VenueRoute,
        instrument_type: OkxInstrumentType,
        listing: Option<VenueListingKey>,
    },
}

impl SubscriptionKind {
    /// Whether one OKX frame is addressed to this subscription.
    ///
    /// A frame that names this subscription's channel can still belong to
    /// another one: the liquidation channel is shared by every instrument type
    /// subscribed to it, and all of them name the same channel in `arg`.
    fn matches_event(&self, arg: &OkxEventArg) -> bool {
        match self {
            Self::Candlestick {
                channel: subscribed,
                ..
            } => arg.channel == *subscribed,
            Self::Ticker { .. } => arg.channel == "tickers",
            Self::OrderBook { .. } => arg.channel.starts_with("books"),
            Self::Trade { .. } => arg.channel == "trades",
            Self::ForceOrder {
                instrument_type, ..
            } => {
                arg.channel == "liquidation-orders"
                    && arg.inst_type.as_deref()
                        == Some(OkxCodec.instrument_type_param(*instrument_type))
            }
            Self::Orders { .. } => arg.channel == "orders" || arg.channel == "orders-algo",
            Self::Positions { .. } => arg.channel == "positions",
        }
    }
}

/// The args, frame kind, socket and authentication of one subscription.
fn subscription(
    context: &ExecutionContext,
    listing: Option<&ResolvedListingRef>,
    request: ProtocolRequest,
) -> Result<(Vec<OkxArg>, SubscriptionKind, OkxSocket, bool), MarketParticipantError> {
    let configuration = |error: market_participants_model::error::ModelError| {
        MarketParticipantError::Configuration(format_smolstr!("{error}"))
    };
    Ok(match request {
        ProtocolRequest::SubscribeCandlesticks(query) => {
            query.validate().map_err(configuration)?;
            let listing = resolved_listing(listing)?;
            let interval = OkxCodec.encode_timeframe(&context.route(), query.timeframe)?;
            let channel = format_smolstr!("candle{interval}");
            (
                vec![OkxArg {
                    channel: channel.clone(),
                    inst_id: Some(listing.instrument_id.clone()),
                    inst_type: None,
                }],
                SubscriptionKind::Candlestick {
                    listing: listing.key,
                    instrument_id: listing.instrument_id,
                    channel,
                },
                OkxSocket::Business,
                false,
            )
        }
        ProtocolRequest::SubscribeTicker(query) => {
            query.validate().map_err(configuration)?;
            let listing = resolved_listing(listing)?;
            (
                vec![OkxArg {
                    channel: SmolStr::new_static("tickers"),
                    inst_id: Some(listing.instrument_id.clone()),
                    inst_type: None,
                }],
                SubscriptionKind::Ticker {
                    listing: listing.key,
                    instrument_id: listing.instrument_id,
                    instrument_type: listing.instrument_type,
                },
                OkxSocket::Public,
                false,
            )
        }
        ProtocolRequest::SubscribeOrderBook(query) => {
            query.validate().map_err(configuration)?;
            let listing = resolved_listing(listing)?;
            // `books5` publishes a whole five-level book on every push, `books`
            // publishes the four-hundred-level book as snapshots and updates.
            let channel = if query.depth <= 5 { "books5" } else { "books" };
            (
                vec![OkxArg {
                    channel: SmolStr::new_static(channel),
                    inst_id: Some(listing.instrument_id.clone()),
                    inst_type: None,
                }],
                SubscriptionKind::OrderBook {
                    listing: listing.key,
                    instrument_id: listing.instrument_id,
                },
                OkxSocket::Public,
                false,
            )
        }
        ProtocolRequest::SubscribeTrades(_query) => {
            let listing = resolved_listing(listing)?;
            (
                vec![OkxArg {
                    channel: SmolStr::new_static("trades"),
                    inst_id: Some(listing.instrument_id.clone()),
                    inst_type: None,
                }],
                SubscriptionKind::Trade {
                    listing: listing.key,
                    instrument_id: listing.instrument_id,
                },
                OkxSocket::Public,
                false,
            )
        }
        ProtocolRequest::SubscribeForceOrder(query) => {
            query.validate().map_err(configuration)?;
            // The liquidation channel is selected by instrument type rather than
            // by instrument, and OKX refuses the subscription when the type is
            // missing from the args. The type is the scope's own, so a scope is
            // never asked for another scope's liquidations; a product family
            // without liquidations has no instrument type to select here.
            let instrument_type = OkxCodec
                .scope_instrument_type(&query.route().scope)
                .filter(|instrument_type| instrument_type != &OkxInstrumentType::Spot)
                .ok_or_else(|| MarketParticipantError::UnsupportedCapability {
                    route: query.route().clone(),
                    capability: CapabilityId::ForceOrder,
                })?;
            (
                vec![OkxArg {
                    channel: SmolStr::new_static("liquidation-orders"),
                    inst_id: None,
                    inst_type: Some(SmolStr::new_static(
                        OkxCodec.instrument_type_param(instrument_type),
                    )),
                }],
                SubscriptionKind::ForceOrder {
                    route: query.route().clone(),
                    instrument_type,
                },
                OkxSocket::Public,
                false,
            )
        }
        ProtocolRequest::SubscribeOrders(query) => {
            let listing = optional_listing(listing)?;
            let instrument_type = scope_instrument_type(context)?;
            let args = ["orders", "orders-algo"]
                .into_iter()
                .map(|channel| OkxArg {
                    channel: SmolStr::new_static(channel),
                    inst_id: listing
                        .as_ref()
                        .map(|listing| listing.instrument_id.clone()),
                    inst_type: None,
                })
                .collect();
            (
                args,
                SubscriptionKind::Orders {
                    route: query.route().clone(),
                    instrument_type,
                    listing: listing.map(|listing| listing.key),
                },
                OkxSocket::Private,
                true,
            )
        }
        ProtocolRequest::SubscribePositions(query) => {
            let listing = optional_listing(listing)?;
            let instrument_type = scope_instrument_type(context)?;
            (
                vec![OkxArg {
                    channel: SmolStr::new_static("positions"),
                    inst_id: listing
                        .as_ref()
                        .map(|listing| listing.instrument_id.clone()),
                    inst_type: None,
                }],
                SubscriptionKind::Positions {
                    route: query.route().clone(),
                    instrument_type,
                    listing: listing.map(|listing| listing.key),
                },
                OkxSocket::Private,
                true,
            )
        }
        _ => {
            return Err(MarketParticipantError::UnsupportedCapability {
                route: context.route(),
                capability: context.capability(),
            });
        }
    })
}

/// Opens one subscription on one connection and streams its events.
fn open_subscription(
    connection: &Arc<Connection>,
    args: Vec<OkxArg>,
    kind: SubscriptionKind,
) -> Result<SubscriptionHandle, MarketParticipantError> {
    let request_id = connection.allocate_request_id();
    let topic = SubscriptionTopic::from(OkxCodec.encode(&args)?);
    let payload = OkxCodec.encode(&OkxCommand {
        id: request_id,
        op: "subscribe",
        args: args.clone(),
    })?;
    let unsubscribe_payload = OkxCodec.encode(&OkxCommand {
        id: connection.allocate_request_id(),
        op: "unsubscribe",
        args,
    })?;
    let connection_subscription =
        connection.subscribe_with_id(request_id, topic, payload, unsubscribe_payload)?;
    let cancellation = CancellationToken::new();
    let source = Box::pin(connection.event_stream());
    // `unfold`'s state is the stream itself, so what is left of the frame it last
    // decoded lives in front of the source it drains next.
    let state = (Vec::new(), source, cancellation.clone(), kind, request_id);
    let stream = stream::unfold(
        state,
        |(pending, source, cancellation, kind, request_id)| async move {
            let mut source = source;
            let mut pending = pending.into_iter();
            loop {
                if let Some(event) = pending.next() {
                    return Some((
                        Ok(event),
                        (pending.collect(), source, cancellation, kind, request_id),
                    ));
                }
                let next_payload = source.next();
                let payload = tokio::select! { () = cancellation.cancelled() => return None, payload = next_payload => payload };
                let payload = match payload {
                    Some(Ok(payload)) => payload,
                    // A connection that stopped publishing ends its streams with why.
                    Some(Err(error)) => {
                        return Some((
                            Err(error),
                            (Vec::new(), source, cancellation, kind, request_id),
                        ));
                    }
                    None => return None,
                };
                match decode_frame(&payload, &kind, request_id) {
                    Ok(events) => pending = events.into_iter(),
                    Err(error) => {
                        return Some((
                            Err(error),
                            (Vec::new(), source, cancellation, kind, request_id),
                        ));
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

/// One frame of the OKX WebSocket protocol.
#[derive(serde::Deserialize)]
pub(super) struct OkxEventArg {
    channel: SmolStr,
    #[serde(rename = "instId")]
    inst_id: Option<SmolStr>,
    #[serde(rename = "instType")]
    inst_type: Option<SmolStr>,
}
#[derive(serde::Deserialize)]
struct OkxFrameEnvelope {
    /// The `event` of a control frame. A subscribe acknowledgement names this
    /// subscription's channel and is still not data this subscription failed to
    /// decode.
    event: Option<SmolStr>,
    arg: Option<OkxEventArg>,
}
#[derive(serde::Deserialize)]
struct OkxErrorEvent {
    event: SmolStr,
    code: SmolStr,
    msg: SmolStr,
}
#[derive(serde::Deserialize)]
struct OkxCandleEvent {
    arg: OkxEventArg,
    data: Vec<Vec<SmolStr>>,
}
#[derive(serde::Deserialize)]
struct OkxTickerEvent {
    arg: OkxEventArg,
    data: Vec<super::market_data::OkxTickerData>,
}
#[derive(serde::Deserialize)]
struct OkxBookEvent {
    arg: OkxEventArg,
    /// The `books` channel labels every push `snapshot` or `update`. The `books5`
    /// channel publishes whole books and carries no `action` at all.
    action: Option<SmolStr>,
    data: Vec<OkxBookData>,
}
#[derive(serde::Deserialize)]
struct OkxBookData {
    asks: Vec<Vec<SmolStr>>,
    bids: Vec<Vec<SmolStr>>,
    ts: SmolStr,
    #[serde(rename = "seqId")]
    sequence: Option<u64>,
    #[serde(rename = "prevSeqId")]
    previous_sequence: Option<i64>,
}
#[derive(serde::Deserialize)]
struct OkxTradeEvent {
    arg: OkxEventArg,
    data: Vec<OkxTradeData>,
}
#[derive(serde::Deserialize)]
struct OkxTradeData {
    #[serde(rename = "instId")]
    inst_id: SmolStr,
    #[serde(rename = "tradeId")]
    trade_id: SmolStr,
    px: SmolStr,
    sz: SmolStr,
    side: SmolStr,
    ts: SmolStr,
}
#[derive(serde::Deserialize)]
struct OkxForceOrderEvent {
    arg: OkxEventArg,
    data: Vec<OkxForceOrderData>,
}
#[derive(serde::Deserialize)]
struct OkxForceOrderData {
    #[serde(rename = "instId")]
    inst_id: SmolStr,
    /// One push settles every liquidation OKX has for the instrument it names, so
    /// the liquidation's own fields live in here rather than beside it.
    details: Vec<OkxForceOrderDetail>,
}
#[derive(serde::Deserialize)]
struct OkxForceOrderDetail {
    #[serde(rename = "posSide")]
    position_side: SmolStr,
    #[serde(rename = "bkPx")]
    bankruptcy_price: SmolStr,
    sz: SmolStr,
    ts: SmolStr,
}

#[derive(serde::Serialize)]
struct OkxCommand<T> {
    id: u64,
    op: &'static str,
    args: Vec<T>,
}
#[derive(serde::Deserialize)]
struct OkxOrderResponse {
    code: SmolStr,
    msg: Option<SmolStr>,
    data: Vec<OkxOrderResult>,
}
#[derive(serde::Deserialize)]
struct OkxOrderResult {
    #[serde(rename = "ordId")]
    ord_id: Option<SmolStr>,
    #[serde(rename = "sCode")]
    s_code: Option<SmolStr>,
    #[serde(rename = "sMsg")]
    s_msg: Option<SmolStr>,
}

/// The failure OKX reports for one subscription, if this frame is that report.
///
/// Every subscription reads the same connection's stream, so a frame is this
/// subscription's only when OKX echoes the request id it was opened with. An
/// error frame that names no id cannot be attributed to one subscription on a
/// shared connection.
fn subscription_error(payload: &[u8], request_id: u64) -> Option<MarketParticipantError> {
    let event: OkxErrorEvent = serde_json::from_slice(payload).ok()?;
    if event.event != "error" || OkxCodec::extract_response_id(payload) != Some(request_id) {
        return None;
    }
    Some(MarketParticipantError::Rejected {
        code: event.code,
        message: event.msg,
        retryable: false,
        raw_response: Some(Bytes::copy_from_slice(payload)),
    })
}

/// Whether one frame is data addressed to this subscription that it could not decode.
///
/// Data for another subscription on the same connection is not this one's to
/// report, and neither is a control frame: an acknowledgement names this
/// subscription's channel without carrying any of its data.
fn frame_is_unparsable_data(payload: &[u8], kind: &SubscriptionKind) -> bool {
    let Ok(envelope) = serde_json::from_slice::<OkxFrameEnvelope>(payload) else {
        return false;
    };
    if envelope.event.is_some() {
        return false;
    }
    envelope.arg.is_some_and(|arg| kind.matches_event(&arg))
}

/// The failure a frame addressed to this subscription reports when its shape is
/// not the one the subscribed channel publishes.
fn malformed_event(payload: &[u8], operation: &'static str) -> MarketParticipantError {
    MarketParticipantError::ProtocolError {
        operation: operation.into(),
        reason: "the frame does not match the shape of its channel".into(),
        raw_data: Some(Bytes::copy_from_slice(payload)),
    }
}

/// Decodes the events one frame carries.
///
/// A frame is one push of a channel, which is not the same unit as the events
/// this adapter reports: the liquidation channel settles several orders at once
/// and delivers them in a single frame. Returning all of them keeps a frame's
/// data whole; a frame of another channel is an empty batch and not a failure.
fn decode_frame(
    payload: &[u8],
    kind: &SubscriptionKind,
    request_id: u64,
) -> Result<Vec<ProtocolEvent>, MarketParticipantError> {
    if let Some(error) = subscription_error(payload, request_id) {
        return Err(error);
    }
    match kind {
        SubscriptionKind::Candlestick {
            listing,
            instrument_id,
            ..
        } => decode_candle_frame(payload, kind, listing, instrument_id),
        SubscriptionKind::Ticker {
            listing,
            instrument_id,
            instrument_type,
        } => decode_ticker_frame(payload, kind, listing, instrument_id, *instrument_type),
        SubscriptionKind::OrderBook {
            listing,
            instrument_id,
        } => decode_book_frame(payload, kind, listing, instrument_id),
        SubscriptionKind::Trade {
            listing,
            instrument_id,
        } => decode_trade_frame(payload, kind, listing, instrument_id),
        SubscriptionKind::ForceOrder {
            route,
            instrument_type,
        } => decode_force_order_frame(payload, kind, route, *instrument_type),
        SubscriptionKind::Orders {
            route,
            instrument_type,
            listing,
        } => {
            let Some(channel) = private_frame_channel(payload) else {
                // A control frame carries none of this channel's data.
                return Ok(Vec::new());
            };
            let order_route = match channel.as_str() {
                "orders" => OrderRoute::Normal,
                "orders-algo" => OrderRoute::Algo,
                _ => return Ok(Vec::new()),
            };
            decode_order_frame(
                payload,
                route,
                *instrument_type,
                order_route,
                listing.as_ref(),
            )
        }
        SubscriptionKind::Positions {
            route,
            instrument_type,
            listing,
        } => {
            let Some(channel) = private_frame_channel(payload) else {
                return Ok(Vec::new());
            };
            if channel != "positions" {
                return Ok(Vec::new());
            }
            decode_position_frame(payload, route, *instrument_type, listing.as_ref())
        }
    }
}

fn decode_candle_frame(
    payload: &[u8],
    kind: &SubscriptionKind,
    listing: &VenueListingKey,
    instrument_id: &str,
) -> Result<Vec<ProtocolEvent>, MarketParticipantError> {
    let event: OkxCandleEvent = match serde_json::from_slice::<OkxCandleEvent>(payload) {
        Ok(event) if kind.matches_event(&event.arg) => event,
        Err(_) if frame_is_unparsable_data(payload, kind) => {
            return Err(malformed_event(payload, "decode OKX candle event"));
        }
        _ => return Ok(Vec::new()),
    };
    if event.arg.inst_id.as_deref() != Some(instrument_id) {
        return Ok(Vec::new());
    }
    let values = event
        .data
        .first()
        .ok_or_else(|| protocol_error("decode OKX candle event", "candle event omitted data"))?;
    if values.len() < 6 {
        return Err(protocol_error(
            "decode OKX candle event",
            "candle event has too few fields",
        ));
    }
    let price = |index: usize, operation: &'static str| -> Result<Price, MarketParticipantError> {
        parse_text(&values[index], operation)
    };
    let candle = NativeCandlestick {
        timestamp: parse_text(&values[0], "decode OKX candle timestamp")?,
        open: price(1, "decode OKX candle open")?,
        high: price(2, "decode OKX candle high")?,
        low: price(3, "decode OKX candle low")?,
        close: price(4, "decode OKX candle close")?,
        volume: parse_text(&values[5], "decode OKX candle volume")?,
    };
    Ok(vec![ProtocolEvent::Candlestick(NativeListing::new(
        listing.clone(),
        candle,
    ))])
}

fn decode_ticker_frame(
    payload: &[u8],
    kind: &SubscriptionKind,
    listing: &VenueListingKey,
    instrument_id: &str,
    instrument_type: OkxInstrumentType,
) -> Result<Vec<ProtocolEvent>, MarketParticipantError> {
    let event: OkxTickerEvent = match serde_json::from_slice::<OkxTickerEvent>(payload) {
        Ok(event) if kind.matches_event(&event.arg) => event,
        Err(_) if frame_is_unparsable_data(payload, kind) => {
            return Err(malformed_event(payload, "decode OKX ticker event"));
        }
        _ => return Ok(Vec::new()),
    };
    let value = event
        .data
        .first()
        .ok_or_else(|| protocol_error("decode OKX ticker event", "ticker event omitted data"))?;
    if value.inst_id().as_str() != instrument_id {
        return Ok(Vec::new());
    }
    Ok(vec![ProtocolEvent::Ticker(NativeListing::new(
        listing.clone(),
        ticker_of(instrument_type, value, payload)?,
    ))])
}

fn decode_book_frame(
    payload: &[u8],
    kind: &SubscriptionKind,
    listing: &VenueListingKey,
    instrument_id: &str,
) -> Result<Vec<ProtocolEvent>, MarketParticipantError> {
    let event: OkxBookEvent = match serde_json::from_slice::<OkxBookEvent>(payload) {
        Ok(event) if kind.matches_event(&event.arg) => event,
        Err(_) if frame_is_unparsable_data(payload, kind) => {
            return Err(malformed_event(payload, "decode OKX order book event"));
        }
        _ => return Ok(Vec::new()),
    };
    let value = event.data.first().ok_or_else(|| {
        protocol_error(
            "decode OKX order book event",
            "order book event omitted data",
        )
    })?;
    // The `books5` channel repeats the instrument inside `data` and the `books`
    // channel names it in `arg` alone, so the subscription is identified by `arg`.
    if event.arg.inst_id.as_deref() != Some(instrument_id) {
        return Ok(Vec::new());
    }
    // OKX numbers its pushes; a push without a sequence cannot be compared with
    // the one before it, so its absence is a protocol failure rather than an
    // invented sequence.
    let last_sequence = value
        .sequence
        .ok_or_else(|| protocol_error("decode OKX order book sequence", "push omitted seqId"))?;
    let previous_sequence = value
        .previous_sequence
        .filter(|sequence| *sequence >= 0)
        .and_then(|sequence| u64::try_from(sequence).ok());
    // `books5` publishes a whole book on every push, so each of its frames is a
    // snapshot even though it carries no `action` to say so.
    let snapshot =
        event.action.as_deref() == Some("snapshot") || event.arg.channel.starts_with("books5");
    let first_sequence = if snapshot {
        last_sequence
    } else {
        previous_sequence
            .and_then(|previous| previous.checked_add(1))
            .unwrap_or(last_sequence)
    };
    let update = NativeOrderBookUpdate {
        snapshot,
        first_sequence: MarketSequence::Numeric(first_sequence),
        last_sequence: MarketSequence::Numeric(last_sequence),
        previous_sequence: previous_sequence.map(MarketSequence::Numeric),
        timestamp: Some(parse_text(&value.ts, "decode OKX order book timestamp")?),
        bids: decode_levels(&value.bids)?,
        asks: decode_levels(&value.asks)?,
    };
    Ok(vec![ProtocolEvent::OrderBook(NativeListing::new(
        listing.clone(),
        update,
    ))])
}

fn decode_levels(
    rows: &[Vec<SmolStr>],
) -> Result<Vec<NativeOrderBookLevel>, MarketParticipantError> {
    rows.iter()
        .map(|row| {
            let price = row.first().ok_or_else(|| {
                protocol_error("decode OKX order book level", "level omitted price")
            })?;
            let quantity = row.get(1).ok_or_else(|| {
                protocol_error("decode OKX order book level", "level omitted quantity")
            })?;
            Ok(NativeOrderBookLevel {
                price: parse_text(price, "decode OKX order book price")?,
                quantity: parse_text(quantity, "decode OKX order book quantity")?,
            })
        })
        .collect()
}

fn decode_trade_frame(
    payload: &[u8],
    kind: &SubscriptionKind,
    listing: &VenueListingKey,
    instrument_id: &str,
) -> Result<Vec<ProtocolEvent>, MarketParticipantError> {
    let event: OkxTradeEvent = match serde_json::from_slice::<OkxTradeEvent>(payload) {
        Ok(event) if kind.matches_event(&event.arg) => event,
        Err(_) if frame_is_unparsable_data(payload, kind) => {
            return Err(malformed_event(payload, "decode OKX trade event"));
        }
        _ => return Ok(Vec::new()),
    };
    let value = event
        .data
        .first()
        .ok_or_else(|| protocol_error("decode OKX trade event", "trade event omitted data"))?;
    if value.inst_id != *instrument_id {
        return Ok(Vec::new());
    }
    let side = match value.side.as_str() {
        "buy" => Side::Buy,
        "sell" => Side::Sell,
        _ => {
            return Err(protocol_error(
                "decode OKX trade side",
                "trade side must be buy or sell",
            ));
        }
    };
    let trade = NativeTrade {
        id: MarketTradeId::Numeric(parse_text(&value.trade_id, "decode OKX trade ID")?),
        timestamp: parse_text(&value.ts, "decode OKX trade timestamp")?,
        price: parse_text(&value.px, "decode OKX trade price")?,
        quantity: parse_text(&value.sz, "decode OKX trade quantity")?,
        side,
    };
    Ok(vec![ProtocolEvent::Trade(NativeListing::new(
        listing.clone(),
        trade,
    ))])
}

fn decode_force_order_frame(
    payload: &[u8],
    kind: &SubscriptionKind,
    route: &VenueRoute,
    instrument_type: OkxInstrumentType,
) -> Result<Vec<ProtocolEvent>, MarketParticipantError> {
    let event: OkxForceOrderEvent = match serde_json::from_slice::<OkxForceOrderEvent>(payload) {
        Ok(event) if kind.matches_event(&event.arg) => event,
        Err(_) if frame_is_unparsable_data(payload, kind) => {
            return Err(malformed_event(payload, "decode OKX liquidation event"));
        }
        _ => return Ok(Vec::new()),
    };
    let Some(value) = event.data.first() else {
        return Err(protocol_error(
            "decode OKX liquidation event",
            "liquidation event omitted data",
        ));
    };
    let listing = OkxCodec.listing_key(route, instrument_type, value.inst_id.as_str())?;
    value
        .details
        .iter()
        .map(|detail| {
            let position_side = match detail.position_side.to_ascii_lowercase().as_str() {
                "long" => PositionSide::Long,
                "short" => PositionSide::Short,
                _ => {
                    return Err(protocol_error(
                        "decode OKX liquidation position side",
                        "unsupported position side",
                    ));
                }
            };
            Ok(ProtocolEvent::ForceOrder(NativeListing::new(
                listing.clone(),
                NativeForceOrder {
                    position_side,
                    price: parse_text(&detail.bankruptcy_price, "decode OKX liquidation price")?,
                    quantity: parse_text(&detail.sz, "decode OKX liquidation quantity")?,
                    timestamp: parse_text(&detail.ts, "decode OKX liquidation timestamp")?,
                },
            )))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use market_participants_model::{
        asset::{AssetId, UnderlyingRef},
        decimal::{Price, Quantity},
        identifier::{VenueId, VenueScopeId},
        instrument::{InstrumentKind, InstrumentTerms},
        listing::{
            identifiers::{OkxListingIdentifiers, VenueListingIdentifiers},
            listing_impl::ListingStatus,
            route::VenueRoute,
            rules::{PerpetualRisk, ProductTradingRules},
            venue::OkxInstrumentType,
        },
        market::{
            book::MarketSequence,
            units::{PositionSide, QuantityUnit},
        },
        routing::CapabilityId,
        settlement::DerivativePnlModel,
    };
    use network::transport::hub::{TransportHub, TransportPolicy};
    use smol_str::SmolStr;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    use url::Url;

    use super::{
        OkxAdapter, OkxInstrumentWire, OkxSwapContractType, candle_params, catalog_entry,
        conversion_reference, decode_frame, fetch_markets, fetch_ticker, subscription,
    };
    use crate::{
        adapter::{
            Authentication, ExecutionContext, MarketParticipantAdapter, NativeCatalogEntry,
            ProtocolEvent, ProtocolRequest, ProtocolResponse,
        },
        connection::ConnectionPool,
        error::MarketParticipantError,
        market::timeframe::Adjustment,
        market_participants::model::{
            CandlestickFetchQuery, MarketCatalogQuery, TickerQuery, TimeFrame,
        },
        protocol::RequestIdentity,
        routing::{SPOT_SCOPE, SWAP_SCOPE},
    };

    fn route(scope: &str) -> VenueRoute {
        VenueRoute::new(
            VenueId::Okx,
            VenueScopeId::new(scope).expect("the scope id is canonical"),
        )
    }

    /// A context whose origins are `origin` on both roles.
    ///
    /// The pair is built here rather than read from a venue's settings, because what
    /// these tests drive is the adapter's own encoding: the context is the one place an
    /// origin enters it, so a fixture socket is named directly.
    fn context(scope: &str, capability: CapabilityId, origin: &Url) -> ExecutionContext {
        let scope_id = VenueScopeId::new(scope).expect("the scope id is canonical");
        let venue = VenueId::Okx;
        let origins = app_config::market_participants::origins::OriginPair {
            http: origin.clone(),
            websocket: origin.clone(),
        };
        let identity = RequestIdentity::new(VenueRoute::new(venue, scope_id), capability);
        let transport = Arc::new(TransportHub::new(TransportPolicy::default()));
        let pool = Arc::new(ConnectionPool::with_transport(transport));
        ExecutionContext::new(identity, origins, reqwest::Client::new(), pool)
    }

    /// The fixture origin of a test that never dials.
    fn fixture_origin() -> Url {
        Url::parse("https://openapi.okx.com").expect("the fixture origin parses")
    }

    /// A fixture server answering one HTTP request per body, in order.
    async fn response_server(bodies: Vec<String>) -> (Url, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let address = listener.local_addr().expect("fixture server address");
        let task = tokio::spawn(async move {
            for body in bodies {
                let (mut socket, _) = listener.accept().await.expect("fixture request");
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0_u8; 512];
                    let count = socket.read(&mut chunk).await.expect("read fixture request");
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write fixture response");
            }
        });
        let origin = Url::parse(&format!("http://{address}")).expect("fixture URL");
        (origin, task)
    }

    /// One instrument row of the venue's own response shape.
    fn instrument(json: &str) -> OkxInstrumentWire {
        serde_json::from_str(json).expect("the OKX instrument fixture decodes")
    }

    /// A fixture server that answers one request and reports what it received.
    async fn signing_server(
        body: &str,
    ) -> (
        Url,
        tokio::sync::oneshot::Receiver<String>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let address = listener.local_addr().expect("fixture server address");
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let body = body.to_owned();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("fixture request");
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 512];
                let count = socket.read(&mut chunk).await.expect("read fixture request");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = sender.send(String::from_utf8_lossy(&request).into_owned());
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write fixture response");
        });
        let origin = Url::parse(&format!("http://{address}")).expect("fixture URL");
        (origin, receiver, task)
    }

    /// An OKX credential is one credential: the request it signs carries the API
    /// key, the passphrase and a signature computed with the secret the gateway
    /// resolved, and none of the three is read from anywhere else.
    #[tokio::test]
    async fn a_signed_request_carries_the_resolved_passphrase_and_its_signature() {
        let (origin, received, server) = signing_server(r#"{"code":"0","msg":"","data":[]}"#).await;
        let context = context(SPOT_SCOPE, CapabilityId::AccountBalances, &origin)
            .with_authentication(Authentication::PassphraseApiKey {
                account_id: "primary".into(),
                api_key: "api-key".into(),
                secret: "signing-secret".into(),
                passphrase: "passphrase".into(),
            });

        super::signed_get::<OkxInstrumentWire>(&context, "/api/v5/account/balance", Vec::new())
            .await
            .expect("the fixture endpoint answers the signed request");

        let request = received
            .await
            .expect("the fixture server reports its request");
        /// Returns one header value of a captured request, by name.
        fn header(request: &str, name: &str) -> Option<String> {
            request.lines().find_map(|line| {
                let (field, value) = line.split_once(':')?;
                field
                    .eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_owned())
            })
        }
        assert_eq!(
            header(&request, "ok-access-key").as_deref(),
            Some("api-key"),
            "{request}"
        );
        assert_eq!(
            header(&request, "ok-access-passphrase").as_deref(),
            Some("passphrase"),
            "{request}"
        );
        let timestamp =
            header(&request, "ok-access-timestamp").expect("the request carries its timestamp");
        let signature =
            header(&request, "ok-access-sign").expect("the request carries its signature");
        assert_eq!(
            signature,
            super::OkxSigner::new("signing-secret")
                .request_signature(&timestamp, "GET", "/api/v5/account/balance")
                .as_str(),
            "the signature covers the timestamp, the method and the path"
        );
        server.abort();
    }

    /// A credential that is not an OKX one is refused before anything is signed.
    #[test]
    fn a_credential_without_a_passphrase_is_refused() {
        let context = context(SPOT_SCOPE, CapabilityId::AccountBalances, &fixture_origin())
            .with_authentication(Authentication::ApiKey {
                account_id: "primary".into(),
                api_key: "api-key".into(),
                secret: "signing-secret".into(),
            });

        let error = match super::credentials_for_context(&context) {
            Err(error) => error,
            Ok(_) => panic!("an OKX request cannot be signed without a passphrase"),
        };

        assert!(matches!(
            error,
            MarketParticipantError::AuthenticationFailed {
                source: crate::error::AuthError::InvalidCredentials,
                ..
            }
        ));
    }

    fn param<'a>(params: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
        params
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The bounds of both served scopes are OKX's own, and a scope this adapter
    /// does not serve has no bounds at all.
    #[test]
    fn limits_report_okx_depth_and_intervals_per_scope() {
        let adapter = OkxAdapter::new();

        for scope in [SPOT_SCOPE, SWAP_SCOPE] {
            let limits = adapter.limits(&route(scope));
            assert_eq!(limits.order_book_depth_limit, 400);
            assert_eq!(limits.timeframes.len(), 16);
            assert!(limits.timeframes.contains(&TimeFrame::Hour4));
            assert!(limits.timeframes.contains(&TimeFrame::Day2));
            assert!(!limits.timeframes.contains(&TimeFrame::Hour8));
            assert!(!limits.timeframes.contains(&TimeFrame::Second1));
        }

        let futures = adapter.limits(&route("futures"));
        assert!(futures.timeframes.is_empty());
    }

    /// OKX's pagination names read backwards, and a range that is sent the way
    /// the names suggest comes back as the window the caller is already showing.
    #[test]
    fn a_time_range_should_be_sent_as_the_opposite_of_its_okx_pagination_names() {
        let query = CandlestickFetchQuery {
            listing: super::OkxCodec
                .listing_key(&route(SPOT_SCOPE), OkxInstrumentType::Spot, "BTC-USDT")
                .expect("the fixture instrument id is a stable identity"),
            timeframe: TimeFrame::Minute5,
            adjustment: Adjustment::None,
            start_timestamp: Some(1_000),
            end_timestamp: Some(2_000),
            limit: 300,
        };
        let params = candle_params("BTC-USDT", "5m", &query);

        assert_eq!(param(&params, "before"), Some("999"));
        assert_eq!(param(&params, "after"), Some("2001"));
        assert_eq!(param(&params, "instId"), Some("BTC-USDT"));
        assert_eq!(param(&params, "limit"), Some("300"));
    }

    #[test]
    fn an_open_ended_range_should_only_send_the_bound_it_has() {
        let query = |start_timestamp, end_timestamp| CandlestickFetchQuery {
            listing: super::OkxCodec
                .listing_key(&route(SPOT_SCOPE), OkxInstrumentType::Spot, "BTC-USDT")
                .expect("the fixture instrument id is a stable identity"),
            timeframe: TimeFrame::Minute5,
            adjustment: Adjustment::None,
            start_timestamp,
            end_timestamp,
            limit: 300,
        };

        let newest = candle_params("BTC-USDT", "5m", &query(None, Some(2_000)));
        assert_eq!(param(&newest, "before"), None);
        assert_eq!(param(&newest, "after"), Some("2001"));

        let oldest = candle_params("BTC-USDT", "5m", &query(Some(1_000), None));
        assert_eq!(param(&oldest, "before"), Some("999"));
        assert_eq!(param(&oldest, "after"), None);

        let unbounded = candle_params("BTC-USDT", "5m", &query(None, None));
        assert_eq!(param(&unbounded, "before"), None);
        assert_eq!(param(&unbounded, "after"), None);
    }

    /// The args of the liquidation subscription, as they go on the wire.
    ///
    /// OKX refuses this channel without the instrument type it selects, and the
    /// type is the scope's own: the subscription is opened for a scope rather
    /// than for one hard-coded product.
    #[test]
    fn a_force_order_subscription_should_select_the_scope_instrument_type() {
        let context = context(SWAP_SCOPE, CapabilityId::ForceOrder, &fixture_origin());
        let (args, kind, socket, authenticated) = subscription(
            &context,
            None,
            ProtocolRequest::SubscribeForceOrder(
                crate::market_participants::model::ForceOrderSubscriptionQuery {
                    route: route(SWAP_SCOPE),
                },
            ),
        )
        .expect("a perpetual force-order subscription maps to OKX args");

        assert!(!authenticated);
        assert!(matches!(socket, super::OkxSocket::Public));
        assert_eq!(
            serde_json::to_string(&args).expect("the OKX args encode"),
            r#"[{"channel":"liquidation-orders","instId":null,"instType":"SWAP"}]"#
        );
        assert!(matches!(
            &kind,
            super::SubscriptionKind::ForceOrder {
                instrument_type: OkxInstrumentType::Perpetual,
                ..
            }
        ));
    }

    /// The liquidation channel is a perpetual capability: a route that does not
    /// address a perpetual instrument type cannot open it.
    #[test]
    fn a_force_order_subscription_of_a_spot_scope_is_not_opened() {
        let context = context(SPOT_SCOPE, CapabilityId::ForceOrder, &fixture_origin());
        let error = subscription(
            &context,
            None,
            ProtocolRequest::SubscribeForceOrder(
                crate::market_participants::model::ForceOrderSubscriptionQuery {
                    route: route(SPOT_SCOPE),
                },
            ),
        )
        .expect_err("a spot route has no liquidation channel");

        assert!(matches!(
            error,
            MarketParticipantError::UnsupportedCapability {
                capability: CapabilityId::ForceOrder,
                ..
            }
        ));
    }

    /// The channel is shared by every instrument type subscribed to it, so a
    /// frame OKX addresses to another type is not this subscription's data.
    #[test]
    fn a_force_order_frame_of_another_instrument_type_is_left_alone() {
        let Ok(events) = decode(
            br#"{"arg":{"channel":"liquidation-orders","instType":"FUTURES"},"data":[{"instId":"BTC-USDT-SWAP","details":[{"posSide":"long","bkPx":"100","sz":"1","ts":"1700000000000"}]}]}"#,
            &force_order_kind(),
        ) else {
            panic!("a frame of another instrument type is not a failure");
        };
        assert!(events.is_empty());
    }

    /// One push lists every liquidation OKX settled for the instrument it names,
    /// and the liquidation's own fields live inside `details`.
    #[test]
    fn decodes_every_liquidation_of_one_okx_push() {
        let events = decode(
            br#"{"arg":{"channel":"liquidation-orders","instType":"SWAP"},"data":[{"instId":"BTC-USDT-SWAP","instType":"SWAP","uly":"BTC-USDT","instFamily":"BTC-USDT","details":[{"bkLoss":"0","bkPx":"40000","ccy":"","posSide":"long","side":"sell","sz":"2","ts":"1700000000000"},{"bkLoss":"0","bkPx":"41000","ccy":"","posSide":"short","side":"buy","sz":"3","ts":"1700000000001"}]}]}"#,
            &force_order_kind(),
        )
        .expect("a liquidation push decodes");

        let decoded: Vec<_> = events
            .into_iter()
            .map(|event| {
                let ProtocolEvent::ForceOrder(order) = event else {
                    panic!("expected a force-order event");
                };
                order
            })
            .collect();
        assert_eq!(decoded.len(), 2, "every settlement of the push is reported");
        let expected = super::OkxCodec
            .listing_key(
                &route(SWAP_SCOPE),
                OkxInstrumentType::Perpetual,
                "BTC-USDT-SWAP",
            )
            .expect("the fixture instrument id is a stable identity");
        assert_eq!(decoded[0].listing, expected);
        assert_eq!(decoded[1].listing, expected);
        assert_eq!(decoded[0].data.position_side, PositionSide::Long);
        assert_eq!(decoded[0].data.price, Price::from(40_000_u64));
        assert_eq!(decoded[0].data.quantity, Quantity::from(2_u64));
        assert_eq!(decoded[0].data.timestamp, 1_700_000_000_000);
        assert_eq!(decoded[1].data.position_side, PositionSide::Short);
        assert_eq!(decoded[1].data.price, Price::from(41_000_u64));
        assert_eq!(decoded[1].data.quantity, Quantity::from(3_u64));
        assert_eq!(decoded[1].data.timestamp, 1_700_000_000_001);
    }

    /// OKX labels an incremental `books` push `update` and names the instrument
    /// in `arg`: the `data` entries of that channel carry no `instId`.
    #[test]
    fn decodes_okx_order_book_updates_with_sequence_boundary() {
        let event = decode_one(
            br#"{"arg":{"channel":"books","instId":"BTC-USDT"},"action":"update","data":[{"asks":[],"bids":[],"ts":"1700000000000","seqId":105,"prevSeqId":100}]}"#,
            &order_book_kind(),
        )
        .expect("the OKX depth event matches");

        let ProtocolEvent::OrderBook(update) = event else {
            panic!("expected an order-book event");
        };
        assert!(!update.data.snapshot);
        assert_eq!(update.data.first_sequence, MarketSequence::Numeric(101));
        assert_eq!(update.data.last_sequence, MarketSequence::Numeric(105));
        assert_eq!(
            update.data.previous_sequence,
            Some(MarketSequence::Numeric(100))
        );
    }

    /// The `books5` channel publishes a whole book on every push and carries
    /// neither an `action` nor a `prevSeqId`, so every frame of it is a snapshot.
    #[test]
    fn decodes_okx_books5_snapshots_that_carry_no_action() {
        let event = decode_one(
            br#"{"arg":{"channel":"books5","instId":"BTC-USDT"},"data":[{"asks":[["81299.4","0.06","0","9"]],"bids":[["81299.3","1.2","0","3"]],"ts":"1700000000000","seqId":81320037369}]}"#,
            &order_book_kind(),
        )
        .expect("the OKX snapshot matches");

        let ProtocolEvent::OrderBook(update) = event else {
            panic!("expected an order-book event");
        };
        assert!(update.data.snapshot);
        assert_eq!(
            update.data.first_sequence,
            MarketSequence::Numeric(81_320_037_369)
        );
        assert_eq!(
            update.data.last_sequence,
            MarketSequence::Numeric(81_320_037_369)
        );
        assert_eq!(update.data.previous_sequence, None);
        assert_eq!(update.data.bids.len(), 1);
        assert_eq!(update.data.asks.len(), 1);
    }

    /// A push without a sequence cannot be compared with the one before it, so
    /// it is refused instead of being published under an invented sequence.
    #[test]
    fn a_book_push_without_a_sequence_is_refused() {
        let Err(error) = decode(
            br#"{"arg":{"channel":"books","instId":"BTC-USDT"},"action":"update","data":[{"asks":[],"bids":[],"ts":"1700000000000","prevSeqId":100}]}"#,
            &order_book_kind(),
        ) else {
            panic!("a push without a sequence is not replayable");
        };

        assert!(matches!(
            error,
            MarketParticipantError::ProtocolError { operation, .. }
                if operation == "decode OKX order book sequence"
        ));
    }

    /// A SPOT ticker counts base currency in `vol24h` and quote currency in
    /// `volCcy24h`.
    #[test]
    fn okx_spot_ticker_pushes_base_and_quote_currency() {
        let volumes = ticker_volumes(
            br#"{"arg":{"channel":"tickers","instId":"BTC-USDT"},"data":[{"instType":"SPOT","instId":"BTC-USDT","last":"105","open24h":"100","high24h":"110","low24h":"90","vol24h":"12","volCcy24h":"1260","ts":"1700000000000"}]}"#,
            &ticker_kind(SPOT_SCOPE, OkxInstrumentType::Spot, "BTC-USDT"),
        );

        assert_eq!(volumes, ("12".into(), "1260".into()));
    }

    /// A derivative ticker counts contracts in `vol24h` and base currency in
    /// `volCcy24h`, so the traded quantity is the contract count — the unit the
    /// listing's own quantity rule declares — and the notional is the base amount
    /// priced at the last price.
    #[test]
    fn okx_perpetual_ticker_pushes_contracts_and_a_priced_base_amount() {
        let volumes = ticker_volumes(
            br#"{"arg":{"channel":"tickers","instId":"BTC-USDT-SWAP"},"data":[{"instType":"SWAP","instId":"BTC-USDT-SWAP","last":"105","open24h":"100","high24h":"110","low24h":"90","vol24h":"1200","volCcy24h":"12","ts":"1700000000000"}]}"#,
            &ticker_kind(SWAP_SCOPE, OkxInstrumentType::Perpetual, "BTC-USDT-SWAP"),
        );

        assert_eq!(volumes, ("1200".into(), "1260".into()));
    }

    /// A live swap push of an instrument whose contract is worth a hundredth of
    /// its base currency: the contract count is a hundred times the base amount.
    /// The traded quantity is the contract count, because a contract is what the
    /// listing trades in, and the base amount is priced into the quote notional
    /// rather than being reported as if it were the traded quantity.
    #[test]
    fn okx_perpetual_ticker_does_not_report_the_base_amount_as_the_traded_quantity() {
        let volumes = ticker_volumes(
            br#"{"arg":{"channel":"tickers","instId":"BTC-USDT-SWAP"},"data":[{"instType":"SWAP","instId":"BTC-USDT-SWAP","last":"85777.1","open24h":"80715.9","high24h":"86319.6","low24h":"80541.3","volCcy24h":"135140.3501","vol24h":"13514035.01","ts":"1790004727178"}]}"#,
            &ticker_kind(SWAP_SCOPE, OkxInstrumentType::Perpetual, "BTC-USDT-SWAP"),
        );

        assert_eq!(volumes, ("13514035.01".into(), "11591947324.56271".into()));
    }

    /// A derivative candle counts contracts in the volume column and the base
    /// amount one field later. The traded quantity the model publishes from that
    /// column is therefore the contract count, labelled with the `Contracts` unit
    /// the listing's own quantity rule declares — the same number the `candle`
    /// channel pushes for the same row.
    #[test]
    fn a_derivative_candle_reports_the_contract_count_its_listing_declares() {
        let swap = swap_catalog_entry();
        assert_eq!(swap.rules.common.quantity.unit, QuantityUnit::Contracts);
        let row = br#"{"code":"0","msg":"","data":[["1700000000000","95000","96000","94000","95532.4","9732157.46","97321.5746","9297363593.31704","1"]]}"#;

        let http = super::decode_candlesticks(row).expect("the candle row decodes");
        let units = crate::market::units::ListingUnits::of(&swap.rules);
        let candle = crate::market::units::candlestick(&swap.listing, &http[0], &units)
            .expect("the candle maps into the listing's units");
        let traded = candle
            .traded_quantity
            .expect("a candle carries its traded volume");

        assert_eq!(
            traded.unit, swap.rules.common.quantity.unit,
            "the volume column is read in the unit the listing sizes orders in"
        );
        assert_eq!(traded.value.to_string(), "9732157.46");

        let Some(ProtocolEvent::Candlestick(pushed)) = decode_one(
            br#"{"arg":{"channel":"candle1m","instId":"BTC-USDT-SWAP"},"data":[["1700000000000","95000","96000","94000","95532.4","9732157.46","97321.5746","9297363593.31704","1"]]}"#,
            &candle_kind(SWAP_SCOPE, "BTC-USDT-SWAP"),
        ) else {
            panic!("expected a candlestick event");
        };
        assert_eq!(
            pushed.data.volume, http[0].volume,
            "the candle channel pushes the column the HTTP endpoint publishes"
        );
        assert_eq!(pushed.listing, swap.listing);
    }

    /// A spot candle counts the base currency in the same column, which is the
    /// unit a spot listing's own quantity rule declares.
    #[test]
    fn a_spot_candle_reports_the_base_amount_its_listing_trades_in() {
        let spot = catalog_entry(
            &route(SPOT_SCOPE),
            OkxInstrumentType::Spot,
            instrument(
                r#"{"instId":"BTC-USDT","baseCcy":"BTC","quoteCcy":"USDT","settleCcy":"","tickSz":"0.1","lotSz":"0.000001","minSz":"0.00001","maxLmtSz":"1000","state":"live"}"#,
            ),
        )
        .expect("the spot instrument decodes")
        .expect("a live spot instrument is published");

        let http = super::decode_candlesticks(
            br#"{"code":"0","msg":"","data":[["1700000000000","100","110","90","105","12","1260","1260","1"]]}"#,
        )
        .expect("the candle row decodes");
        let units = crate::market::units::ListingUnits::of(&spot.rules);
        let candle = crate::market::units::candlestick(&spot.listing, &http[0], &units)
            .expect("the candle maps into the listing's units");
        let traded = candle
            .traded_quantity
            .expect("a candle carries its traded volume");

        assert_eq!(traded.unit, spot.rules.common.quantity.unit);
        assert_eq!(
            traded.unit,
            QuantityUnit::AssetUnits {
                asset: super::asset("BTC").asset_id()
            }
        );
        assert_eq!(traded.value.to_string(), "12");
    }

    /// The statistics rows are positional arrays of the venue's own strings, and
    /// each of the three statistics endpoints has its own row shape.
    #[test]
    fn decodes_rubik_statistics_row_shapes() {
        let open_interest: Vec<super::OkxOpenInterestHistoryRow> = super::decode_public(
            br#"{"code":"0","msg":"","data":[["1700000000000","10","2.5","100000"]]}"#,
            "decode test open interest",
        )
        .expect("the open-interest history row decodes");
        let super::OkxOpenInterestHistoryRow([timestamp, _, quantity, notional]) =
            &open_interest[0];
        assert_eq!(timestamp, "1700000000000");
        assert_eq!(quantity, "2.5");
        assert_eq!(notional, "100000");

        let ratios: Vec<super::OkxRatioRow> = super::decode_public(
            br#"{"code":"0","msg":"","data":[["1700000000000","1.25"]]}"#,
            "decode test ratio",
        )
        .expect("the ratio row decodes");
        let super::OkxRatioRow([timestamp, ratio]) = &ratios[0];
        assert_eq!(timestamp, "1700000000000");
        assert_eq!(ratio, "1.25");

        let taker: Vec<super::OkxTakerVolumeRow> = super::decode_public(
            br#"{"code":"0","msg":"","data":[["1700000000000","12.5","20"]]}"#,
            "decode test taker volume",
        )
        .expect("the taker-volume row decodes");
        let super::OkxTakerVolumeRow([timestamp, sell, buy]) = &taker[0];
        assert_eq!(timestamp, "1700000000000");
        assert_eq!(sell, "12.5");
        assert_eq!(buy, "20");
    }

    /// A frame of another instrument is not this subscription's data.
    #[test]
    fn okx_ticker_frame_of_another_instrument_is_left_alone() {
        let events = decode(
            br#"{"arg":{"channel":"tickers","instId":"ETH-USDT-SWAP"},"data":[{"instType":"SWAP","instId":"ETH-USDT-SWAP","last":"105","open24h":"100","high24h":"110","low24h":"90","vol24h":"1200","volCcy24h":"12","ts":"1700000000000"}]}"#,
            &ticker_kind(SWAP_SCOPE, OkxInstrumentType::Perpetual, "BTC-USDT-SWAP"),
        )
        .expect("a frame of another instrument is not a failure");

        assert!(events.is_empty());
    }

    /// One connection carries every subscription, so a frame for another channel
    /// is not this subscription's to report.
    #[test]
    fn leaves_a_frame_of_another_channel_to_its_own_subscription() {
        assert!(matches!(
            decode(
                br#"{"arg":{"channel":"trades","instId":"BTC-USDT"},"data":[]}"#,
                &order_book_kind(),
            ),
            Ok(events) if events.is_empty()
        ));
    }

    /// A subscribe acknowledgement names this subscription's channel and arrives
    /// on the same stream, so it must not be mistaken for data.
    #[test]
    fn leaves_a_subscribe_acknowledgement_alone() {
        assert!(matches!(
            decode(
                br#"{"id":"7","event":"subscribe","arg":{"channel":"books","instId":"BTC-USDT"},"connId":"687770f1"}"#,
                &order_book_kind(),
            ),
            Ok(events) if events.is_empty()
        ));
    }

    /// A frame that names this subscription's channel and still does not match
    /// its shape is a failure: reporting it as nothing kept a wrong DTO silent.
    #[test]
    fn reports_a_frame_addressed_here_that_does_not_parse() {
        let Err(error) = decode(
            br#"{"arg":{"channel":"books","instId":"BTC-USDT"},"action":"update"}"#,
            &order_book_kind(),
        ) else {
            panic!("a frame of this channel that does not parse is an error");
        };

        assert!(matches!(
            error,
            MarketParticipantError::ProtocolError { operation, .. }
                if operation == "decode OKX order book event"
        ));
    }

    /// A subscription OKX refuses is reported through the request id the
    /// subscribe command was sent with, which is what makes an error frame
    /// attributable on a shared stream.
    #[test]
    fn reports_an_error_frame_that_echoes_this_subscription_request_id() {
        let Err(error) = decode(
            br#"{"id":"7","event":"error","code":"60012","msg":"Invalid request"}"#,
            &order_book_kind(),
        ) else {
            panic!("an error frame for this subscription is reported");
        };

        assert!(matches!(
            error,
            MarketParticipantError::Rejected { code, message, .. }
                if code == "60012" && message == "Invalid request"
        ));
    }

    /// An error frame that echoes another subscription's request id belongs to it.
    #[test]
    fn leaves_an_error_frame_of_another_subscription_alone() {
        assert!(matches!(
            decode(
                br#"{"id":"8","event":"error","code":"60012","msg":"Invalid request"}"#,
                &order_book_kind(),
            ),
            Ok(events) if events.is_empty()
        ));
    }

    /// An unsubscribe command repeats the args of its subscribe command, so the
    /// venue releases exactly the channel it opened.
    #[test]
    fn an_unsubscribe_command_repeats_the_arguments_it_releases() {
        let command = super::OkxCommand {
            id: 8,
            op: "unsubscribe",
            args: vec![
                super::OkxArg {
                    channel: "orders".into(),
                    inst_id: None,
                    inst_type: None,
                },
                super::OkxArg {
                    channel: "orders-algo".into(),
                    inst_id: None,
                    inst_type: None,
                },
            ],
        };

        assert_eq!(
            serde_json::to_string(&command).expect("the unsubscribe command encodes"),
            r#"{"id":8,"op":"unsubscribe","args":[{"channel":"orders","instId":null,"instType":null},{"channel":"orders-algo","instId":null,"instType":null}]}"#
        );
    }

    /// A spot instrument becomes a complete catalog entry: the canonical
    /// instrument, the venue's own typed identifiers and rules that hold for it.
    #[test]
    fn a_spot_instrument_becomes_a_complete_catalog_entry() {
        let entry = catalog_entry(
            &route(SPOT_SCOPE),
            OkxInstrumentType::Spot,
            instrument(
                r#"{"instId":"BTC-USDT","baseCcy":"BTC","quoteCcy":"USDT","settleCcy":"","tickSz":"0.1","lotSz":"0.000001","minSz":"0.00001","maxLmtSz":"1000","state":"live"}"#,
            ),
        )
        .expect("the spot instrument decodes")
        .expect("a live spot instrument is published");

        assert_eq!(entry.native_symbol.as_str(), "BTC-USDT");
        assert_eq!(entry.status, ListingStatus::Trading);
        assert_eq!(entry.instrument.kind(), InstrumentKind::Spot);
        assert_eq!(
            entry.identifiers,
            VenueListingIdentifiers::Okx(OkxListingIdentifiers {
                instrument_type: OkxInstrumentType::Spot,
                instrument_family: None,
                future_series: None,
                option_series: None,
            })
        );
        assert_eq!(
            entry.listing.listing_id,
            super::OkxCodec
                .listing_key(&route(SPOT_SCOPE), OkxInstrumentType::Spot, "BTC-USDT")
                .expect("the fixture instrument id is a stable identity")
                .listing_id
        );
        entry
            .rules
            .validate_for(&entry.instrument)
            .expect("the catalog rules hold for the instrument they describe");
        let InstrumentTerms::Spot(terms) = entry.instrument.terms() else {
            panic!("expected spot terms");
        };
        assert_eq!(terms.quote(), &super::asset("USDT").asset_id());
        assert_eq!(terms.base(), &super::asset("BTC").asset_id());
    }

    /// A linear swap becomes a complete catalog entry. Its margin requirements
    /// stay unloaded: the venue publishes them outside the instrument record and
    /// nothing reads them.
    #[test]
    fn a_linear_swap_becomes_a_complete_catalog_entry() {
        let entry = catalog_entry(
            &route(SWAP_SCOPE),
            OkxInstrumentType::Perpetual,
            instrument(
                r#"{"instId":"BTC-USDT-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"USDT","tickSz":"0.1","lotSz":"0.01","minSz":"0.01","maxLmtSz":"10000","ctVal":"0.01","ctType":"linear","uly":"BTC-USDT","instFamily":"BTC-USDT","state":"live"}"#,
            ),
        )
        .expect("the swap instrument decodes")
        .expect("a live linear swap is published");

        assert_eq!(entry.instrument.kind(), InstrumentKind::Perpetual);
        entry
            .rules
            .validate_for(&entry.instrument)
            .expect("the catalog rules hold for the instrument they describe");
        let ProductTradingRules::Perpetual(product) = entry.rules.product.as_ref() else {
            panic!("expected perpetual rules");
        };
        assert_eq!(product.risk, PerpetualRisk::Unloaded);
        assert_eq!(
            entry.identifiers,
            VenueListingIdentifiers::Okx(OkxListingIdentifiers {
                instrument_type: OkxInstrumentType::Perpetual,
                instrument_family: Some(
                    market_participants_model::identifier::NativeSymbol::new("BTC-USDT")
                        .expect("the fixture family is a stable identity")
                ),
                future_series: None,
                option_series: None,
            })
        );
    }

    /// A contract the venue has announced but not described carries no terms to
    /// publish, so it never becomes a listing.
    #[test]
    fn an_announced_contract_is_not_published() {
        let entry = catalog_entry(
            &route(SWAP_SCOPE),
            OkxInstrumentType::Perpetual,
            instrument(
                r#"{"instId":"NKE-USDT-SWAP","baseCcy":"","quoteCcy":"","state":"preopen","listTime":"1800000000000"}"#,
            ),
        )
        .expect("an announced contract is not a protocol failure");

        assert!(entry.is_none());
    }

    /// A row that describes only part of a contract contradicts itself, and
    /// publishing it would publish terms the venue does not trade.
    #[test]
    fn a_partially_described_contract_is_a_protocol_failure() {
        let error = catalog_entry(
            &route(SWAP_SCOPE),
            OkxInstrumentType::Perpetual,
            instrument(
                r#"{"instId":"BTC-USDT-SWAP","settleCcy":"USDT","tickSz":"0.1","lotSz":"0.01","ctVal":"0.01","ctType":"linear","uly":"BTC-USDT","state":"live"}"#,
            ),
        )
        .expect_err("a half-described contract is refused");

        assert!(matches!(
            error,
            MarketParticipantError::ProtocolError { operation, reason, .. }
                if operation == "decode OKX market instrument"
                    && reason.contains("only part of the terms")
        ));
    }

    /// A scheduled contract whose terms the venue already published is a listing
    /// with an availability instant, not an announced row.
    #[test]
    fn a_scheduled_swap_with_published_terms_carries_its_availability() {
        let entry = catalog_entry(
            &route(SWAP_SCOPE),
            OkxInstrumentType::Perpetual,
            instrument(
                r#"{"instId":"SOL-USDT-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"USDT","tickSz":"0.001","lotSz":"0.1","minSz":"0.1","maxLmtSz":"10000","ctVal":"0.1","ctType":"linear","uly":"SOL-USDT","instFamily":"SOL-USDT","state":"preopen","listTime":"1800000000000"}"#,
            ),
        )
        .expect("the scheduled swap decodes")
        .expect("a scheduled swap with published terms is a listing");

        assert_eq!(entry.status, ListingStatus::Scheduled);
        assert_eq!(entry.available_from, Some(1_800_000_000_000));
    }

    /// An inverse swap is published with the terms it really has: it tracks the
    /// asset it settles in, states its contract value as a notional of the price
    /// quote, and carries the conversion reference its settlement converts
    /// through.
    #[test]
    fn an_inverse_swap_is_published_as_an_inverse_perpetual() {
        let entry = catalog_entry(
            &route(SWAP_SCOPE),
            OkxInstrumentType::Perpetual,
            instrument(
                r#"{"instId":"BTC-USD-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"BTC","tickSz":"1","lotSz":"1","minSz":"1","maxLmtSz":"10000","ctVal":"100","ctType":"inverse","uly":"BTC-USD","instFamily":"BTC-USD","state":"live"}"#,
            ),
        )
        .expect("the inverse swap decodes")
        .expect("a live inverse swap is published");

        let InstrumentTerms::Perpetual(terms) = entry.instrument.terms() else {
            panic!("expected perpetual terms");
        };
        assert_eq!(terms.underlying().reference(), &UnderlyingRef::Asset(btc()));
        assert_eq!(terms.price_quote(), &usd());
        assert_eq!(terms.settlement_asset(), &btc());
        assert_eq!(terms.pnl_model(), DerivativePnlModel::Inverse);
        assert_eq!(
            terms.contract_size().denomination(),
            Some(&usd()),
            "an inverse contract is a notional of its price quote"
        );
        assert_eq!(
            terms.contract_size().cash_quantity(),
            Some(Quantity::from(100_u8))
        );
        let conversion = terms
            .conversion_reference()
            .expect("an inverse contract settles in an asset other than its quote");
        assert_eq!(
            conversion,
            &conversion_reference(&btc(), &usd())
                .expect("the fixture conversion reference is buildable")
                .reference_id()
                .expect("the fixture conversion reference is canonical")
        );
    }

    /// A swap that states the inverse shape while settling in something other
    /// than the asset it tracks is refused instead of being published as a
    /// linear contract, because its terms cannot be expressed at all.
    #[test]
    fn an_inverse_swap_settling_in_another_asset_is_refused() {
        let error = catalog_entry(
            &route(SWAP_SCOPE),
            OkxInstrumentType::Perpetual,
            instrument(
                r#"{"instId":"BTC-USD-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"USDT","tickSz":"1","lotSz":"1","minSz":"1","maxLmtSz":"10000","ctVal":"100","ctType":"inverse","uly":"BTC-USD","instFamily":"BTC-USD","state":"live"}"#,
            ),
        )
        .expect_err("an inverse swap cannot settle in a foreign asset");

        assert!(matches!(
            error,
            MarketParticipantError::ProtocolError { operation, .. }
                if operation == "decode OKX contract type"
        ));
    }

    /// A swap stating a shape this build cannot express is left out of the route
    /// rather than published as a shape the venue did not state. The row is a
    /// product this model has no terms for, not a broken route.
    #[test]
    fn a_swap_stating_an_unknown_contract_type_is_not_published() {
        let entry = catalog_entry(
            &route(SWAP_SCOPE),
            OkxInstrumentType::Perpetual,
            instrument(
                r#"{"instId":"BTC-USD-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"BTC","tickSz":"1","lotSz":"1","minSz":"1","maxLmtSz":"10000","ctVal":"100","ctType":"quanto","uly":"BTC-USD","instFamily":"BTC-USD","state":"live"}"#,
            ),
        )
        .expect("an unknown contract type is a listing to leave out, not a route to fail");

        assert!(
            entry.is_none(),
            "a swap this model cannot express must not be published"
        );
    }

    /// OKX publishes `ctValCcy` beside `ctVal`, which is the venue's own account of
    /// the currency its contract value is counted in. When it agrees with the shape
    /// `ctType` states it is a second, independent confirmation of the same contract;
    /// when it disagrees the row contradicts itself and publishing it either way
    /// would publish a contract the venue does not trade.
    #[test]
    fn a_swap_stating_its_own_contract_value_currency_should_agree_with_its_shape() {
        let linear = |ct_val_ccy: &str| {
            let row = format!(
                r#"{{"instId":"BTC-USDT-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"USDT","tickSz":"0.1","lotSz":"0.01","minSz":"0.01","maxLmtSz":"10000","ctVal":"0.01","ctValCcy":"{ct_val_ccy}","ctType":"linear","uly":"BTC-USDT","instFamily":"BTC-USDT","state":"live"}}"#
            );
            catalog_entry(
                &route(SWAP_SCOPE),
                OkxInstrumentType::Perpetual,
                instrument(&row),
            )
        };
        let inverse = |ct_val_ccy: &str| {
            let row = format!(
                r#"{{"instId":"BTC-USD-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"BTC","tickSz":"1","lotSz":"1","minSz":"1","maxLmtSz":"10000","ctVal":"100","ctValCcy":"{ct_val_ccy}","ctType":"inverse","uly":"BTC-USD","instFamily":"BTC-USD","state":"live"}}"#
            );
            catalog_entry(
                &route(SWAP_SCOPE),
                OkxInstrumentType::Perpetual,
                instrument(&row),
            )
        };

        // The linear shape values its contract in the asset it tracks.
        assert!(
            linear("BTC")
                .expect("a stated currency the shape agrees with is not a failure")
                .is_some(),
            "a linear swap agrees with its own contract value currency"
        );
        let error = linear("USDT").expect_err("the row contradicts itself");
        assert!(
            matches!(&error, MarketParticipantError::ProtocolError { operation, .. }
                if *operation == "decode OKX contract value currency"),
            "{error:?}"
        );

        // The inverse shape values it in the price quote instead.
        assert!(
            inverse("USD")
                .expect("a stated currency the shape agrees with is not a failure")
                .is_some(),
            "an inverse swap agrees with its own contract value currency"
        );
        assert!(
            inverse("BTC").is_err(),
            "an inverse swap that values its contract in the asset it tracks contradicts itself"
        );

        // A response that omits the field states nothing to disagree with, so the
        // shape `ctType` states is the contract that is published.
        assert!(
            linear_without_stated_currency().is_some(),
            "an omitted ctValCcy leaves the derived contract standing"
        );
    }

    /// A contract multiplier this build cannot fold into the contract value is a
    /// product decision and not a broken route: the venue trades a contract whose
    /// terms this model has no shape for, so the listing is left out of the catalog
    /// exactly as an unknown `ctType` is.
    #[test]
    fn a_swap_stating_a_multiplier_this_model_cannot_express_is_not_published() {
        let entry = catalog_entry(
            &route(SWAP_SCOPE),
            OkxInstrumentType::Perpetual,
            instrument(
                r#"{"instId":"BTC-USDT-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"USDT","tickSz":"0.1","lotSz":"0.01","minSz":"0.01","maxLmtSz":"10000","ctVal":"0.01","ctValCcy":"BTC","ctMult":"1","ctType":"linear","uly":"BTC-USDT","instFamily":"BTC-USDT","state":"live"}"#,
            ),
        )
        .expect("a stated multiplier of one is the contract the model already has");
        assert!(
            entry.is_some(),
            "a multiplier of one leaves the derived contract standing"
        );

        let entry = catalog_entry(
            &route(SWAP_SCOPE),
            OkxInstrumentType::Perpetual,
            instrument(
                r#"{"instId":"BTC-USDT-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"USDT","tickSz":"0.1","lotSz":"0.01","minSz":"0.01","maxLmtSz":"10000","ctVal":"0.01","ctValCcy":"BTC","ctMult":"10","ctType":"linear","uly":"BTC-USDT","instFamily":"BTC-USDT","state":"live"}"#,
            ),
        )
        .expect("a multiplier this build cannot express is a listing to leave out");
        assert!(
            entry.is_none(),
            "a contract whose multiplier the model cannot fold into its value must not be published"
        );
    }

    /// The fixture of a linear swap that states no contract value currency, which is
    /// the shape a response without `ctValCcy` has.
    fn linear_without_stated_currency() -> Option<NativeCatalogEntry> {
        catalog_entry(
            &route(SWAP_SCOPE),
            OkxInstrumentType::Perpetual,
            instrument(
                r#"{"instId":"BTC-USDT-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"USDT","tickSz":"0.1","lotSz":"0.01","minSz":"0.01","maxLmtSz":"10000","ctVal":"0.01","ctType":"linear","uly":"BTC-USDT","instFamily":"BTC-USDT","state":"live"}"#,
            ),
        )
        .expect("a swap that states no contract value currency still decodes")
    }

    /// A swap that omits the field its shape is stated by breaks the protocol: the
    /// field belongs to the required terms of every published swap, so the row is
    /// refused before a shape could be guessed for it.
    #[test]
    fn a_swap_omitting_its_contract_type_is_refused() {
        let error = catalog_entry(
            &route(SWAP_SCOPE),
            OkxInstrumentType::Perpetual,
            instrument(
                r#"{"instId":"BTC-USD-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"BTC","tickSz":"1","lotSz":"1","minSz":"1","maxLmtSz":"10000","ctVal":"100","uly":"BTC-USD","instFamily":"BTC-USD","state":"live"}"#,
            ),
        )
        .expect_err("a swap without a contract type is not described");

        assert!(matches!(
            error,
            MarketParticipantError::ProtocolError { operation, .. }
                if operation == "decode OKX market instrument"
        ));
    }

    /// The shape reader separates the two shapes this build expresses from the
    /// shapes it does not, and refuses a swap that states nothing.
    #[test]
    fn reading_a_contract_type_should_separate_known_shapes_from_unknown_ones() {
        let read = |value: &str| OkxSwapContractType::read(Some(&SmolStr::new(value)));

        assert_eq!(read("linear"), Ok(Some(OkxSwapContractType::Linear)));
        assert_eq!(read("inverse"), Ok(Some(OkxSwapContractType::Inverse)));
        assert!(
            matches!(
                OkxSwapContractType::read(None),
                Err(MarketParticipantError::ProtocolError { operation, .. })
                    if operation == "decode OKX contract type"
            ),
            "a swap that states nothing is a protocol failure"
        );
        for unknown in ["quanto", "spot", "LINEAR"] {
            assert_eq!(
                OkxSwapContractType::read(Some(&SmolStr::new(unknown))),
                Ok(None),
                "{unknown} is a shape this build cannot express"
            );
        }
    }

    /// A route that answers both a swap this build publishes and one it cannot
    /// express still publishes the first: leaving a product out is a decision about
    /// that listing alone.
    #[tokio::test]
    async fn a_route_should_publish_its_expressible_swaps_beside_an_unknown_one() {
        let (origin, server) = response_server(vec![
            r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"USDT","tickSz":"0.1","lotSz":"0.01","minSz":"0.01","maxLmtSz":"10000","ctVal":"0.01","ctType":"linear","uly":"BTC-USDT","instFamily":"BTC-USDT","state":"live"},{"instId":"BTC-USD-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"BTC","tickSz":"1","lotSz":"1","minSz":"1","maxLmtSz":"10000","ctVal":"100","ctType":"quanto","uly":"BTC-USD","instFamily":"BTC-USD","state":"live"}]}"#.to_owned(),
        ])
        .await;
        let context = context(SWAP_SCOPE, CapabilityId::MarketCatalog, &origin);

        let ProtocolResponse::Markets(entries) = fetch_markets(
            &context,
            MarketCatalogQuery {
                route: route(SWAP_SCOPE),
            },
        )
        .await
        .expect("the expressible swap keeps the route publishable") else {
            panic!("expected a market catalog response");
        };

        assert_eq!(entries.len(), 1, "only the expressible swap is published");
        assert_eq!(entries[0].native_symbol.as_str(), "BTC-USDT-SWAP");
        server.await.expect("the fixture server finishes");
    }

    /// A swap that claims the linear shape while settling in an asset other than
    /// its price quote is refused by the model rather than published with the
    /// settlement it does not have.
    #[test]
    fn a_linear_swap_settling_in_another_asset_is_refused() {
        let error = catalog_entry(
            &route(SWAP_SCOPE),
            OkxInstrumentType::Perpetual,
            instrument(
                r#"{"instId":"BTC-USD-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"BTC","tickSz":"1","lotSz":"1","minSz":"1","maxLmtSz":"10000","ctVal":"0.01","ctType":"linear","uly":"BTC-USD","instFamily":"BTC-USD","state":"live"}"#,
            ),
        )
        .expect_err("a linear swap settles in its price quote");

        assert!(matches!(
            error,
            MarketParticipantError::ProtocolError { operation, .. }
                if operation == "decode OKX market instrument"
        ));
    }

    /// The canonical assets the inverse fixtures name.
    fn btc() -> AssetId {
        super::asset("BTC").asset_id()
    }

    fn usd() -> AssetId {
        super::asset("USD").asset_id()
    }

    /// A scheduled instrument is published under the instant the venue says it
    /// becomes available, which is the only lower bound that makes it tradable.
    #[test]
    fn a_scheduled_instrument_carries_its_availability() {
        let entry = catalog_entry(
            &route(SPOT_SCOPE),
            OkxInstrumentType::Spot,
            instrument(
                r#"{"instId":"BTC-USDT","baseCcy":"BTC","quoteCcy":"USDT","tickSz":"0.1","lotSz":"0.000001","state":"preopen","listTime":"1800000000000"}"#,
            ),
        )
        .expect("the scheduled instrument decodes")
        .expect("a scheduled instrument with an availability instant is published");

        assert_eq!(entry.status, ListingStatus::Scheduled);
        assert_eq!(entry.available_from, Some(1_800_000_000_000));
    }

    /// The catalog of one swap route is fetched from OKX's instrument endpoint
    /// alone; the venue's position tiers are never read.
    #[tokio::test]
    async fn fetch_markets_converts_okx_contract_fields_to_catalog_entries() {
        let (origin, server) = response_server(vec![
            r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"USDT","tickSz":"0.1","lotSz":"0.01","minSz":"0.01","maxLmtSz":"10000","ctVal":"0.01","ctType":"linear","uly":"BTC-USDT","instFamily":"BTC-USDT","state":"live"}]}"#.to_owned(),
        ])
        .await;
        let context = context(SWAP_SCOPE, CapabilityId::MarketCatalog, &origin);

        let ProtocolResponse::Markets(entries) = fetch_markets(
            &context,
            MarketCatalogQuery {
                route: route(SWAP_SCOPE),
            },
        )
        .await
        .expect("the OKX catalog decodes") else {
            panic!("expected a market catalog response");
        };

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].native_symbol.as_str(), "BTC-USDT-SWAP");
        assert_eq!(entries[0].instrument.kind(), InstrumentKind::Perpetual);
        assert_eq!(entries[0].available_from, None);
        server.await.expect("the fixture server finishes");
    }

    /// A listing-scoped request whose listing the gateway could not resolve is
    /// refused instead of being encoded with an invented instrument.
    #[tokio::test]
    async fn a_listing_scoped_request_without_a_listing_is_refused() {
        let context = context(SPOT_SCOPE, CapabilityId::Tickers, &fixture_origin());
        let Err(error) = fetch_ticker(
            &context,
            None,
            TickerQuery {
                listing: super::OkxCodec
                    .listing_key(&route(SPOT_SCOPE), OkxInstrumentType::Spot, "BTC-USDT")
                    .expect("the fixture instrument id is a stable identity"),
            },
        )
        .await
        else {
            panic!("a ticker request needs the resolved listing");
        };

        assert!(matches!(
            error,
            MarketParticipantError::ProtocolError { operation, .. }
                if operation == "encode OKX request"
        ));
    }

    /// The subscription a decode test stands for, and the request id it was
    /// opened with.
    const REQUEST_ID: u64 = 7;

    fn order_book_kind() -> super::SubscriptionKind {
        super::SubscriptionKind::OrderBook {
            listing: super::OkxCodec
                .listing_key(&route(SPOT_SCOPE), OkxInstrumentType::Spot, "BTC-USDT")
                .expect("the fixture instrument id is a stable identity"),
            instrument_id: "BTC-USDT".into(),
        }
    }

    fn force_order_kind() -> super::SubscriptionKind {
        super::SubscriptionKind::ForceOrder {
            route: route(SWAP_SCOPE),
            instrument_type: OkxInstrumentType::Perpetual,
        }
    }

    fn ticker_kind(
        scope: &str,
        instrument_type: OkxInstrumentType,
        instrument_id: &str,
    ) -> super::SubscriptionKind {
        super::SubscriptionKind::Ticker {
            listing: super::OkxCodec
                .listing_key(&route(scope), instrument_type, instrument_id)
                .expect("the fixture instrument id is a stable identity"),
            instrument_id: instrument_id.into(),
            instrument_type,
        }
    }

    /// The one candle subscription a candle decode test stands for.
    fn candle_kind(scope: &str, instrument_id: &str) -> super::SubscriptionKind {
        super::SubscriptionKind::Candlestick {
            listing: super::OkxCodec
                .listing_key(&route(scope), OkxInstrumentType::Perpetual, instrument_id)
                .expect("the fixture instrument id is a stable identity"),
            instrument_id: instrument_id.into(),
            channel: "candle1m".into(),
        }
    }

    /// The published catalog entry of one live linear swap, whose margin
    /// requirements are still unloaded.
    fn swap_catalog_entry() -> NativeCatalogEntry {
        catalog_entry(
            &route(SWAP_SCOPE),
            OkxInstrumentType::Perpetual,
            instrument(
                r#"{"instId":"BTC-USDT-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"USDT","tickSz":"0.1","lotSz":"0.01","minSz":"0.01","maxLmtSz":"10000","ctVal":"0.01","ctType":"linear","uly":"BTC-USDT","instFamily":"BTC-USDT","state":"live"}"#,
            ),
        )
        .expect("the swap instrument decodes")
        .expect("a live linear swap is published")
    }

    fn ticker_volumes(payload: &[u8], kind: &super::SubscriptionKind) -> (String, String) {
        let Some(ProtocolEvent::Ticker(ticker)) = decode_one(payload, kind) else {
            panic!("expected a ticker event");
        };
        (
            ticker
                .data
                .traded_quantity
                .expect("OKX publishes a traded quantity")
                .to_string(),
            ticker
                .data
                .traded_notional
                .expect("OKX publishes a traded notional")
                .to_string(),
        )
    }

    fn decode(
        payload: &[u8],
        kind: &super::SubscriptionKind,
    ) -> Result<Vec<ProtocolEvent>, MarketParticipantError> {
        decode_frame(payload, kind, REQUEST_ID)
    }

    /// The single event a channel that publishes one event per frame carries.
    fn decode_one(payload: &[u8], kind: &super::SubscriptionKind) -> Option<ProtocolEvent> {
        decode(payload, kind)
            .expect("the frame decodes")
            .into_iter()
            .next()
    }
}

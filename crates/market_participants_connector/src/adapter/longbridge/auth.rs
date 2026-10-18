//! The authenticated quote session of one Longbridge connection.
//!
//! The venue's whole market-data surface lives on one authenticated socket, and the
//! session is what that socket's account turned out to be: the per-command pace the
//! venue published for it ([`crate::adapter::longbridge::quota`]), and whether it may
//! read US market data at all. Both are read from the market-data profile (cmd 4) that
//! the venue answers a fresh connection with, and both are this build's own bounds on
//! every later command.
//!
//! The three steps this module owns are the ones the venue's own client takes in the
//! same order: the one-time password over HTTP, the AUTH frame and its
//! acknowledgement, and the profile request. Which of them is bounded by which
//! constant is part of the design (§10.4): the OTP request and the acknowledgement
//! have bounds of their own, and one command over the socket waits
//! [`REQUEST_TIMEOUT`] with [`REQUEST_TIMEOUT_MILLIS`] written into its frame.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ahash::AHashMap;
use bytes::Bytes;
use longbridge_proto::{
    control::AuthRequest,
    quote::{UserQuoteProfileRequest, UserQuoteProfileResponse},
};
use market_participants_model::listing::route::VenueRoute;
use prost::Message as _;
use reqwest::{
    Client, StatusCode,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use smol_str::{SmolStr, format_smolstr};
use url::Url;

use super::{
    codec::{self, Packet},
    quota::{LongbridgeQuota, quotas_of},
    signature,
};
use crate::{
    connection::{Connection, ReauthenticationRequest, physical::ReauthenticationAcknowledgement},
    error::{AuthError, MarketParticipantError, NetworkError},
    runtime::LongbridgeCredentialValues,
};

/// The venue-level account identity every Longbridge socket authenticates as.
///
/// The venue's quote socket is one session for the account behind its credentials
/// rather than one per trading account, so this identity is the venue itself. It is
/// what the connection key carries — a key whose account is absent can never
/// complete a reauthentication — and what this session's own refusals name.
pub(super) const ACCOUNT: &str = "longbridge";

/// How long one request sent over the quote socket may take (design §10.3).
pub(super) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The value the packet's timeout field carries (the protocol caps it at 60_000).
pub(super) const REQUEST_TIMEOUT_MILLIS: u16 = 30_000;
/// How long one OTP request may take.
pub(super) const OTP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the market-data profile request (cmd 4) may take.
pub(super) const PROFILE_TIMEOUT: Duration = Duration::from_secs(10);
/// The language the profile request asks its descriptions in.
pub(super) const PROFILE_LANGUAGE: &str = "en";

/// The path of the venue's socket-token endpoint (§4.9).
///
/// The same string is both the path the request is addressed to and the path the
/// signature covers, and the two are one constant because a signature over a
/// different path than the one dialled is exactly what the venue refuses. The venue's
/// own fixture reads it back when it recomputes a request's signature, so that the
/// path a test asserts on is the path the adapter dialled.
pub(super) const SOCKET_TOKEN_PATH: &str = "/v1/socket/token";
/// The HTTP method the token request is signed as: the signature covers the method,
/// and the request is a read.
const SOCKET_TOKEN_METHOD: &str = "GET";

/// The request id an AUTH frame carries in its own packet.
///
/// The connection layer allocates its ids from one upwards and registers each one as
/// a pending request, so zero can never be a request of it: the acknowledgement is
/// therefore matched by its content rather than by an id, and a frame that carries
/// this id can never be delivered to a waiting request by mistake.
const AUTH_REQUEST_ID: u32 = 0;

/// The metadata key the venue reads the language of an authentication from.
const AUTH_LANGUAGE_KEY: &str = "accept-language";
/// The metadata key that turns the overnight session on.
const AUTH_OVERNIGHT_KEY: &str = "need_over_night_quote";
/// The value that asks for the overnight session (§4.1, decision 7).
const AUTH_OVERNIGHT_VALUE: &str = "true";

/// The market code of the US market in the venue's own permission profile.
const US_MARKET_CODE: &str = "US";

/// The refusal code this build states for a venue that is out of connection slots.
const CONNECTION_LIMIT_CODE: &str = "connection-limit";

/// What the venue's token endpoint hands out for one socket.
///
/// The three values are what the venue's envelope carries inside its `data` rather
/// than what the endpoint answers with: the live run measured
/// `{"code":0,"message":"success","data":{"otp":"…","limit":10,"online":0}}`, so the
/// three fields the design states are one level down.
#[derive(serde::Deserialize)]
struct SocketToken {
    /// The one-time password the socket authenticates with.
    #[serde(rename = "otp")]
    otp: SmolStr,
    /// How many connections the account may hold open at once.
    #[serde(rename = "limit")]
    limit: i32,
    /// How many it holds open right now.
    #[serde(rename = "online")]
    online: i32,
}

/// The envelope every answer of the venue's HTTP surface arrives in.
///
/// The `code` is the venue's own outcome — zero is success — and `message` is its own
/// sentence about it. A refusal carries the same two fields with a non-zero code, so
/// the envelope is what a refusal is read from rather than the HTTP status alone.
#[derive(serde::Deserialize)]
struct TokenEnvelope {
    /// The venue's own outcome code, zero when the request succeeded.
    #[serde(rename = "code")]
    code: i64,
    /// The venue's own sentence about that outcome.
    #[serde(rename = "message")]
    message: SmolStr,
    /// What the request asked for, present when the venue granted it.
    #[serde(rename = "data")]
    data: Option<SocketToken>,
}

/// What one connection's session said about the account behind it.
#[derive(Debug)]
pub(super) struct LongbridgeSession {
    /// The connection this session was read from.
    ///
    /// A session is a fact about one socket — the pace it may be commanded at and the
    /// permission of the account behind it — so it carries the socket it is about:
    /// every request this session paces and every subscription it gates is sent on
    /// this connection and no other.
    connection: Arc<Connection>,
    /// The key that connection is pooled under.
    ///
    /// The quotas and the permission are facts of one connection — the region it
    /// dialled, the credentials it presented and the generation of those credentials
    /// — so a session is reused for the key it was read on and for no other. A region
    /// change is a different endpoint and therefore a different key, which is what
    /// makes it a different session rather than the previous region's permissions.
    key: crate::connection::ConnectionKey,
    /// One token bucket per command code, as the venue published them.
    ///
    /// The field is public inside the session's own module tree, which is where the
    /// callers that read it live: the gate and the throttle are applied by the commands
    /// themselves.
    pub quotas: AHashMap<u8, LongbridgeQuota>,
    /// Whether the account may read US market data at all.
    pub us_market_data: bool,
}

impl LongbridgeSession {
    /// Opens the session of one connection before the venue has answered for it.
    ///
    /// The profile request is sent through this value, so a session exists before its
    /// own facts do: its table is empty, which throttles nothing — a command the
    /// venue has published no pace for is not throttled — and its permission is not
    /// yet granted, which is what the profile is about to answer.
    pub(super) fn pending(
        connection: Arc<Connection>,
        key: crate::connection::ConnectionKey,
    ) -> Self {
        Self {
            connection,
            key,
            quotas: AHashMap::with_capacity(8),
            us_market_data: false,
        }
    }

    /// Reports whether this session is the one a planned connection would read.
    ///
    /// A session whose socket is gone is not reused even under the same key: the
    /// venue's pace and permission would still be the account's, but the socket they
    /// were read on is not, and a connection this build dials again is one it reads
    /// the profile of again.
    pub(super) fn belongs_to(&self, key: &crate::connection::ConnectionKey) -> bool {
        self.key == *key && self.connection.is_healthy()
    }

    /// Returns the connection this session paces and gates.
    ///
    /// The candle wire is what reads it: a subscription is a lease on the socket this
    /// session belongs to, and the opening screen and the pushes are read over it.
    pub(super) fn connection(&self) -> &Arc<Connection> {
        &self.connection
    }

    /// Installs what the market-data profile answered.
    ///
    /// The venue's two counts are logged and nothing else: `subscribe_limit` and
    /// `history_candlestick_limit` are limits of the account's plan which the venue
    /// enforces itself, and the official client prints them for the same reason —
    /// there is nothing for this build to do with them that the venue does not
    /// already do.
    pub(super) fn install_profile(&mut self, profile: &UserQuoteProfileResponse) {
        log::debug!(
            "the Longbridge quote profile states level {} with a subscribe limit of {} and a \
             history candlestick limit of {}",
            profile.quote_level,
            profile.subscribe_limit,
            profile.history_candlestick_limit
        );
        self.quotas = quotas_of(&profile.rate_limit);
        self.us_market_data = has_us_market_data(profile);
    }

    /// Refuses one market-data command the account's profile grants no permission for.
    ///
    /// The judgement is the venue's own profile rather than an error the venue sent
    /// back: a command that reads a market's data is refused here, before any frame
    /// is built, when the profile carries no package for that market. It is the same
    /// answer for the history, the opening candles and both subscription commands,
    /// because the venue grants or withholds the whole market's data at once.
    pub(super) fn require_market_data(
        &self,
        route: &VenueRoute,
        command_code: u8,
    ) -> Result<(), MarketParticipantError> {
        if self.us_market_data || !codec::is_market_data(command_code) {
            return Ok(());
        }
        Err(authentication_failed(
            route,
            AuthError::NoMarketDataPermission,
        ))
    }

    /// Waits for one of a command's own tokens, without asking the permission gate.
    ///
    /// The pace a command may be sent at and the permission to read a market are two
    /// different facts, and one command this build sends is outside the gate while being
    /// inside the venue's rate-limit table: the trading days of a market (cmd 9), whose
    /// caller states why. A command the profile published no pace for is not paced, which
    /// is what the venue's own client does with the same table — the venue's refusal is
    /// what answers a request over an allowance nothing was published for.
    pub(super) async fn wait_for_quota(&self, command_code: u8) {
        if let Some(quota) = self.quotas.get(&command_code) {
            quota.acquire_one().await;
        }
    }

    /// Sends one command over the connection and decodes its answer.
    ///
    /// The order is the design's (§10.8): the permission gate, then one token of the
    /// command's own quota, then the frame. A command with no quota installed is sent
    /// without waiting, which is what the official client does with the same table,
    /// and the wait is the venue's own rate rather than a refusal of this build's.
    pub(super) async fn request<Res>(
        &self,
        route: &VenueRoute,
        command_code: u8,
        body: &impl prost::Message,
    ) -> Result<Res, MarketParticipantError>
    where
        Res: prost::Message + Default,
    {
        self.require_market_data(route, command_code)?;
        self.wait_for_quota(command_code).await;
        let request_id = self.connection.allocate_request_id();
        let wire_id =
            u32::try_from(request_id).map_err(|_| MarketParticipantError::ProtocolError {
                operation: SmolStr::new_static("encode Longbridge request"),
                reason: format_smolstr!(
                    "the request id {request_id} does not fit the protocol's 32-bit field"
                ),
                raw_data: None,
            })?;
        let payload = codec::encode_request(
            command_code,
            wire_id,
            REQUEST_TIMEOUT_MILLIS,
            &body.encode_to_vec(),
        )?;
        let answer = self
            .connection
            .send_and_wait_with_id(request_id, payload, REQUEST_TIMEOUT)
            .await?;
        let packet = codec::decode(&answer)?;
        let Packet::Response {
            command_code: answered,
            status,
            body,
            ..
        } = &packet
        else {
            return Err(unexpected_answer(command_code, &answer));
        };
        if *answered != command_code {
            return Err(unexpected_answer(command_code, &answer));
        }
        if let Some(refusal) = codec::refusal(*status, body)? {
            return Err(refusal);
        }
        Res::decode(body.as_ref()).map_err(|_| MarketParticipantError::DecodeFailed {
            operation: SmolStr::new_static("decode Longbridge response"),
            raw_data: body.clone(),
        })
    }
}

/// Whether one profile grants access to US market data.
///
/// The venue states its permission per market, and a market is granted by carrying at
/// least one package: the entry is present with an empty package list when the
/// account may not read it, and the whole detail is absent from a profile that
/// predates the field. Neither is a guess about an error code — the answer is the
/// venue's own statement about this account.
fn has_us_market_data(profile: &UserQuoteProfileResponse) -> bool {
    profile
        .quote_level_detail
        .as_ref()
        .and_then(|detail| detail.by_market_code.get(US_MARKET_CODE))
        .is_some_and(|market| !market.packages.is_empty())
}

/// Fetches the one-time password that authenticates one socket.
///
/// The venue's token endpoint is an HTTP read beside the socket rather than a command
/// on it, and it is the one request on the venue's HTTP surface that is *signed*: the
/// live run measured `401 {"code":401001,"message":"token empty"}` for a request that
/// carried only the two routing headers, and `403 {"code":403201,"message":"signature
/// invalid"}` for one that carried all three values under a wrong signature. What the
/// endpoint accepts is the signature [`signature::RequestSigning`] computes, presented
/// beside the two headers every Longbridge request carries: the language the answers are
/// worded in, and the data centre the credentials belong to.
///
/// The answer is an envelope rather than the three fields themselves: the venue states
/// its own `code` and `message` around a `data` object, and the one-time password, the
/// connection limit and the connection count are inside that object.
pub(super) async fn fetch_otp(
    client: &Client,
    origin: &Url,
    route: &VenueRoute,
    credentials: &LongbridgeCredentialValues,
) -> Result<SmolStr, MarketParticipantError> {
    let endpoint = origin.join(SOCKET_TOKEN_PATH).map_err(|_| {
        MarketParticipantError::Configuration("invalid Longbridge token path".into())
    })?;
    let safe_endpoint = format_smolstr!("{}", endpoint);
    let headers = token_headers(credentials)?;
    let response = tokio::time::timeout(OTP_TIMEOUT, client.get(endpoint).headers(headers).send())
        .await
        .map_err(|_| MarketParticipantError::Timeout {
            operation: SmolStr::new_static("request the Longbridge socket token"),
            timeout_ms: OTP_TIMEOUT.as_millis().try_into().unwrap_or(u64::MAX),
        })?
        .map_err(|error| MarketParticipantError::ConnectionFailed {
            route: route.clone(),
            endpoint: safe_endpoint.clone(),
            source: NetworkError::Io {
                message: format_smolstr!("{error}"),
            },
        })?;
    let status = response.status();
    let body =
        response
            .bytes()
            .await
            .map_err(|error| MarketParticipantError::ConnectionFailed {
                route: route.clone(),
                endpoint: safe_endpoint.clone(),
                source: NetworkError::Io {
                    message: format_smolstr!("{error}"),
                },
            })?;
    let token = socket_token(status, &body)?;
    if token.online >= token.limit {
        // The venue's own numbers travel with the refusal: the sentence a reader gets
        // says how many connections the account holds and how many it may hold, which
        // is what makes "close a session and try again" an instruction rather than a
        // guess. The refusal is retryable because another session may already have
        // ended by the time the operator retries.
        //
        // The rule is the design's own and is stated as it is written there (§4.9):
        // `online >= limit`. A venue that reported no limit at all — both counts zero —
        // is therefore read as a limit of zero, and that reading is pinned by a fixture
        // case rather than guarded against here: adding a condition the design does not
        // state would be this build inventing a rule about a field it does not own.
        return Err(MarketParticipantError::Rejected {
            code: SmolStr::new_static(CONNECTION_LIMIT_CODE),
            message: format_smolstr!(
                "the venue reports {} of its {} connections in use",
                token.online,
                token.limit
            ),
            retryable: true,
            raw_response: None,
        });
    }
    Ok(token.otp)
}

/// The headers one signed socket-token request presents.
///
/// The four headers the signature covers and is carried in are added to the two the
/// venue routes every request by, so the request states its language and its data
/// centre *and* proves which credentials asked. The signature is the last one written
/// and the first one the venue reads.
fn token_headers(
    credentials: &LongbridgeCredentialValues,
) -> Result<HeaderMap, MarketParticipantError> {
    let signing = signature::RequestSigning::new(SOCKET_TOKEN_METHOD, SOCKET_TOKEN_PATH, "", None);
    let mut headers = super::socket::region_headers(credentials);
    for (name, value) in signing.sign(credentials, unix_seconds()?) {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            MarketParticipantError::Configuration("invalid Longbridge header name".into())
        })?;
        let value = HeaderValue::from_str(&value).map_err(|_| {
            MarketParticipantError::Configuration("invalid Longbridge credential encoding".into())
        })?;
        headers.insert(name, value);
    }
    Ok(headers)
}

/// The current Unix time in whole seconds, which is what the signature states.
///
/// The instant is signed, so the clock is part of the credential material: a request
/// whose clock cannot be read is one this build cannot sign, and it is refused as the
/// configuration failure it is rather than sent under a wrong instant.
fn unix_seconds() -> Result<u64, MarketParticipantError> {
    let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| {
        MarketParticipantError::Configuration("the system clock is before the epoch".into())
    })?;
    Ok(since_epoch.as_secs())
}

/// The three fields one token answer carries, read out of the venue's envelope.
///
/// A non-2xx status is the transport's own refusal and a 2xx status may still state a
/// venue refusal in its envelope's `code`; both are answered as
/// [`MarketParticipantError::Rejected`], because both are the venue refusing this request. The
/// two are told apart by what they carry rather than by which one is checked first: an
/// envelope states the venue's own code and message, and a body that is not one leaves
/// the HTTP status as the only code there is.
fn socket_token(status: StatusCode, body: &Bytes) -> Result<SocketToken, MarketParticipantError> {
    let envelope: Option<TokenEnvelope> = serde_json::from_slice(body).ok();
    if let Some(envelope) = envelope {
        // The venue's retryability is stated rather than guessed: a 2xx answer that
        // carries a refusal is the venue declining this request, which the same
        // credentials would decline again, and a 5xx is the venue failing to answer.
        let retryable = status.is_server_error();
        if envelope.code != 0 || !status.is_success() {
            return Err(MarketParticipantError::Rejected {
                code: format_smolstr!("{}", envelope.code),
                message: envelope.message,
                retryable,
                raw_response: Some(body.clone()),
            });
        }
        let Some(data) = envelope.data else {
            return Err(MarketParticipantError::DecodeFailed {
                operation: SmolStr::new_static("decode the Longbridge socket token"),
                raw_data: body.clone(),
            });
        };
        return Ok(data);
    }
    if !status.is_success() {
        return Err(MarketParticipantError::Rejected {
            code: format_smolstr!("{}", status.as_u16()),
            message: refusal_message(status, body),
            retryable: status.is_server_error(),
            raw_response: Some(body.clone()),
        });
    }
    Err(MarketParticipantError::DecodeFailed {
        operation: SmolStr::new_static("decode the Longbridge socket token"),
        raw_data: body.clone(),
    })
}

/// The message one refused token request states.
///
/// The venue's own body is what a reader gets when it says anything, and the status's
/// canonical phrase is what it gets when it says nothing: an empty message would read
/// as a rejection with no reason at all, which is the sentence the frame decoder
/// refuses to build from an empty body for the same reason.
fn refusal_message(status: StatusCode, body: &Bytes) -> SmolStr {
    let body = String::from_utf8_lossy(body);
    let body = body.trim();
    if body.is_empty() {
        return SmolStr::new(status.canonical_reason().unwrap_or("no reason was stated"));
    }
    SmolStr::new(body)
}

/// The AUTH frame of one handshake.
fn auth_frame(otp: &str) -> Result<Bytes, MarketParticipantError> {
    let mut metadata = std::collections::HashMap::with_capacity(2);
    metadata.insert(AUTH_LANGUAGE_KEY.to_owned(), PROFILE_LANGUAGE.to_owned());
    metadata.insert(
        AUTH_OVERNIGHT_KEY.to_owned(),
        AUTH_OVERNIGHT_VALUE.to_owned(),
    );
    let body = AuthRequest {
        token: otp.to_owned(),
        metadata,
    };
    codec::encode_request(
        codec::COMMAND_CODE_AUTH,
        AUTH_REQUEST_ID,
        REQUEST_TIMEOUT_MILLIS,
        &body.encode_to_vec(),
    )
}

/// The authentication one socket presents, first handshake and reconnection alike.
///
/// The first handshake and every reauthentication build the same frame from the same
/// function, so what a reconnected socket replays is exactly what the caller sent
/// when the connection was opened. The OTP is the session token the connection layer
/// records: the venue issues no session id this build uses — there is no reconnect
/// command here — and the password that opened the socket is the honest answer to
/// "what is this connection authenticated with".
pub(super) fn authentication_request(
    otp: &str,
    route: &VenueRoute,
) -> Result<ReauthenticationRequest, MarketParticipantError> {
    Ok(ReauthenticationRequest {
        payload: auth_frame(otp)?,
        session_token: SmolStr::new(otp),
        acknowledged: acknowledgement(route.clone()),
    })
}

/// The content matcher the connection layer acknowledges an AUTH answer with.
///
/// It matches on content rather than on a request id, because the frame it waits for
/// carries id zero: every id the connection layer allocates is at least one, so no
/// pending request could ever be waiting for this answer. Anything that is not an
/// authentication response is handed back — `Ok(None)` — for the connection layer to
/// correlate or publish exactly as it would have; a rejected authentication is the
/// typed refusal it is, which fails the reconnection.
fn acknowledgement(route: VenueRoute) -> ReauthenticationAcknowledgement {
    Arc::new(move |payload: &[u8]| match codec::decode(payload)? {
        Packet::Response {
            command_code,
            status,
            ..
        } if command_code == codec::COMMAND_CODE_AUTH => {
            if status != 0 {
                return Err(authentication_failed(&route, AuthError::InvalidCredentials));
            }
            Ok(Some(()))
        }
        _ => Ok(None),
    })
}

/// Reads the market-data profile of one authenticated connection (cmd 4).
///
/// The step has a bound of its own, shorter than the socket's own request bound: a
/// connection that has not answered with the account's permissions within it is not
/// one to hand out, because without the profile there is no pace to honour and no
/// permission to judge a command by.
pub(super) async fn fetch_profile(
    session: &LongbridgeSession,
    route: &VenueRoute,
) -> Result<UserQuoteProfileResponse, MarketParticipantError> {
    let body = UserQuoteProfileRequest {
        language: PROFILE_LANGUAGE.to_owned(),
    };
    tokio::time::timeout(
        PROFILE_TIMEOUT,
        session.request(route, codec::QUERY_USER_QUOTE_PROFILE, &body),
    )
    .await
    .map_err(|_| MarketParticipantError::Timeout {
        operation: SmolStr::new_static("read the Longbridge quote profile"),
        timeout_ms: PROFILE_TIMEOUT.as_millis().try_into().unwrap_or(u64::MAX),
    })?
}

/// The failure of a frame that is not the answer to the request it arrived for.
fn unexpected_answer(command_code: u8, raw: &[u8]) -> MarketParticipantError {
    MarketParticipantError::ProtocolError {
        operation: SmolStr::new_static("decode Longbridge response"),
        reason: format_smolstr!("the frame is not a response of command {command_code}"),
        raw_data: Some(Bytes::copy_from_slice(raw)),
    }
}

/// The refusal of one authentication, which is always the same refusal.
///
/// The venue's own error body is worth a diagnostic and nothing more: this build
/// holds no table of the venue's authentication codes, and the one thing every one of
/// them means is that these credentials could not open a socket.
fn authentication_failed(route: &VenueRoute, source: AuthError) -> MarketParticipantError {
    MarketParticipantError::AuthenticationFailed {
        route: route.clone(),
        account: Some(SmolStr::new_static(ACCOUNT)),
        source,
    }
}

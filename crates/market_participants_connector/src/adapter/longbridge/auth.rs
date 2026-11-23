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

pub(super) const ACCOUNT: &str = "longbridge";

pub(super) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const REQUEST_TIMEOUT_MILLIS: u16 = 30_000;
pub(super) const OTP_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const PROFILE_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const PROFILE_LANGUAGE: &str = "en";

pub(super) const SOCKET_TOKEN_PATH: &str = "/v1/socket/token";
const SOCKET_TOKEN_METHOD: &str = "GET";

const AUTH_REQUEST_ID: u32 = 0;

const AUTH_LANGUAGE_KEY: &str = "accept-language";
const AUTH_OVERNIGHT_KEY: &str = "need_over_night_quote";
const AUTH_OVERNIGHT_VALUE: &str = "true";

const US_MARKET_CODE: &str = "US";

const CONNECTION_LIMIT_CODE: &str = "connection-limit";

#[derive(serde::Deserialize)]
struct SocketToken {
    #[serde(rename = "otp")]
    otp: SmolStr,
    #[serde(rename = "limit")]
    limit: i32,
    #[serde(rename = "online")]
    online: i32,
}

#[derive(serde::Deserialize)]
struct TokenEnvelope {
    #[serde(rename = "code")]
    code: i64,
    #[serde(rename = "message")]
    message: SmolStr,
    #[serde(rename = "data")]
    data: Option<SocketToken>,
}

#[derive(Debug)]
pub(super) struct LongbridgeSession {
    connection: Arc<Connection>,
    key: crate::connection::ConnectionKey,
    pub quotas: AHashMap<u8, LongbridgeQuota>,
    pub us_market_data: bool,
}

impl LongbridgeSession {
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

    pub(super) fn belongs_to(&self, key: &crate::connection::ConnectionKey) -> bool {
        self.key == *key && self.connection.is_healthy()
    }

    pub(super) fn connection(&self) -> &Arc<Connection> {
        &self.connection
    }

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

    pub(super) async fn wait_for_quota(&self, command_code: u8) {
        if let Some(quota) = self.quotas.get(&command_code) {
            quota.acquire_one().await;
        }
    }

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

fn has_us_market_data(profile: &UserQuoteProfileResponse) -> bool {
    profile
        .quote_level_detail
        .as_ref()
        .and_then(|detail| detail.by_market_code.get(US_MARKET_CODE))
        .is_some_and(|market| !market.packages.is_empty())
}

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

fn unix_seconds() -> Result<u64, MarketParticipantError> {
    let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| {
        MarketParticipantError::Configuration("the system clock is before the epoch".into())
    })?;
    Ok(since_epoch.as_secs())
}

fn socket_token(status: StatusCode, body: &Bytes) -> Result<SocketToken, MarketParticipantError> {
    let envelope: Option<TokenEnvelope> = serde_json::from_slice(body).ok();
    if let Some(envelope) = envelope {
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

fn refusal_message(status: StatusCode, body: &Bytes) -> SmolStr {
    let body = String::from_utf8_lossy(body);
    let body = body.trim();
    if body.is_empty() {
        return SmolStr::new(status.canonical_reason().unwrap_or("no reason was stated"));
    }
    SmolStr::new(body)
}

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

fn unexpected_answer(command_code: u8, raw: &[u8]) -> MarketParticipantError {
    MarketParticipantError::ProtocolError {
        operation: SmolStr::new_static("decode Longbridge response"),
        reason: format_smolstr!("the frame is not a response of command {command_code}"),
        raw_data: Some(Bytes::copy_from_slice(raw)),
    }
}

fn authentication_failed(route: &VenueRoute, source: AuthError) -> MarketParticipantError {
    MarketParticipantError::AuthenticationFailed {
        route: route.clone(),
        account: Some(SmolStr::new_static(ACCOUNT)),
        source,
    }
}

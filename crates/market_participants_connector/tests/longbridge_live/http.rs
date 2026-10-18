//! The HTTP surface a socket is opened through: the token endpoint (cmd 0's password) and
//! the signature its request carries (§4.9).
//!
//! The signature algorithm is restated here rather than reused, because the adapter's own
//! signing path is crate-private and the fact under test is that the **venue** accepts
//! this algorithm. The live run measured that an unsigned request is refused
//! (`401001 token empty`, and `403201 signature invalid` once the key and token are
//! presented), and the adapter's `fetch_otp` signs every token request since; case 1
//! prints both halves of that measurement.

use reqwest::{
    Client,
    header::{ACCEPT_LANGUAGE, HeaderMap, HeaderName, HeaderValue},
};
use serde::Deserialize;
use tokio::time::timeout;

use crate::{bounds::HTTP_TIMEOUT, credentials::Credentials, wire::Failure};

/// One request's signing values, as §4.9's algorithm reads them.
const SIGNED_HEADERS: &str = "authorization;x-api-key;x-timestamp";
/// What the token endpoint answered (§4.9).
pub struct SocketToken {
    pub limit: i32,
    pub online: i32,
    /// The one-time password. Never printed.
    pub otp: String,
}

/// What one token request measured, for the case that prints it.
pub struct TokenMeasurement {
    pub status: u16,
    pub limit: i32,
    pub online: i32,
    pub otp_length: usize,
    pub otp_fingerprint: u64,
    /// How many fields the answer's envelope carried, and how many its payload did.
    pub envelope_fields: usize,
    pub payload_fields: usize,
    /// The password itself, which no case prints.
    pub otp: String,
}

/// The signature one Longbridge HTTP request carries (§4.9).
///
/// The venue refused an unsigned token request with its own code — `403201 signature
/// invalid` once the token and key were presented — so this suite signs that request
/// exactly as the design states the algorithm: the string to sign is
/// `METHOD|path|query|values|headers|`, the message hashed with HMAC-SHA256 is
/// `HMAC-SHA256|` followed by that string's SHA-1 in hex, and the key is the
/// application secret. The signature is lowercase hex and travels in `x-api-signature`
/// beside the three headers it covers.
pub fn request_signature(
    secret: &str,
    method: &str,
    path: &str,
    query: &str,
    signed_values: &str,
    signed_headers: &str,
) -> String {
    use hmac::{KeyInit as _, Mac as _};
    use sha1::Digest as _;
    let string_to_sign = format!("{method}|{path}|{query}|{signed_values}|{signed_headers}|");
    let digested: String = sha1::Sha1::digest(string_to_sign.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let mut mac =
        hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("a key of any length");
    mac.update(format!("HMAC-SHA256|{digested}").as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The headers one token request presents.
///
/// The names are the three the design states, in the order the string to sign puts
/// them, and the values are the ones that will be sent.
pub fn token_headers(
    credentials: &Credentials,
    timestamp: u64,
    with_region_header: bool,
) -> HeaderMap {
    let signed_values = format!(
        "authorization:{}\nx-api-key:{}\nx-timestamp:{timestamp}\n",
        credentials.access_token, credentials.app_key
    );
    let signature = request_signature(
        &credentials.app_secret,
        "GET",
        "/v1/socket/token",
        "",
        &signed_values,
        SIGNED_HEADERS,
    );
    let mut headers = HeaderMap::with_capacity(6);
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en"));
    if with_region_header {
        headers.insert(
            HeaderName::from_static("x-dc-region"),
            HeaderValue::from_static(credentials.data_centre()),
        );
    }
    headers.insert(
        HeaderName::from_static("authorization"),
        HeaderValue::from_str(&credentials.access_token).expect("a token header"),
    );
    headers.insert(
        HeaderName::from_static("x-api-key"),
        HeaderValue::from_str(&credentials.app_key).expect("a key header"),
    );
    headers.insert(
        HeaderName::from_static("x-timestamp"),
        HeaderValue::from_str(&timestamp.to_string()).expect("a timestamp header"),
    );
    headers.insert(
        HeaderName::from_static("x-api-signature"),
        HeaderValue::from_str(&format!(
            "HMAC-SHA256 SignedHeaders={SIGNED_HEADERS}, Signature={signature}"
        ))
        .expect("a signature header"),
    );
    headers
}

/// The token endpoint beside the socket (§4.9).
pub fn token_endpoint() -> std::io::Result<url::Url> {
    url::Url::parse("https://openapi.longbridge.com")
        .and_then(|origin| origin.join("/v1/socket/token"))
        .map_err(std::io::Error::other)
}

/// Asks the venue for one socket's password.
pub async fn fetch_otp(
    client: &Client,
    credentials: &Credentials,
    with_region_header: bool,
) -> Result<String, Failure> {
    let measured = measure_token(client, credentials, with_region_header).await?;
    Ok(measured.otp)
}

/// Asks for one token and measures the answer.
pub async fn measure_token(
    client: &Client,
    credentials: &Credentials,
    with_region_header: bool,
) -> Result<TokenMeasurement, Failure> {
    let endpoint = token_endpoint().map_err(|error| Failure::Transport(error.to_string()))?;
    let timestamp = crate::grid::now_millis() / 1_000;
    let headers = token_headers(
        credentials,
        u64::try_from(timestamp).unwrap_or(0),
        with_region_header,
    );
    let response = timeout(HTTP_TIMEOUT, client.get(endpoint).headers(headers).send())
        .await
        .map_err(|_| Failure::Transport("the token request timed out".to_owned()))?
        .map_err(|error| Failure::Transport(error.to_string()))?;
    let status = response.status().as_u16();
    let body = timeout(HTTP_TIMEOUT, response.bytes())
        .await
        .map_err(|_| Failure::Transport("the token body timed out".to_owned()))?
        .map_err(|error| Failure::Transport(error.to_string()))?;
    let token = parse_socket_token(status, &body)?;
    let envelope_fields =
        serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&body)
            .map(|map| map.len())
            .unwrap_or(0);
    let payload_fields =
        serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&body)
            .ok()
            .and_then(|map| {
                map.get("data")
                    .and_then(|data| data.as_object().map(|data| data.len()))
            })
            .unwrap_or(0);
    Ok(TokenMeasurement {
        status,
        limit: token.limit,
        online: token.online,
        otp_length: token.otp.len(),
        otp_fingerprint: fingerprint(token.otp.as_bytes()),
        envelope_fields,
        payload_fields,
        otp: token.otp,
    })
}

/// Reads the three fields the token endpoint's 200 carries.
///
/// The venue answered the signed request with an envelope — `code`, `message`,
/// `data` — whose `data` holds the three fields the design states. The design writes
/// the 200 as the three fields alone, so this function accepts either shape and
/// reports which one arrived rather than assuming: the case that measures it prints
/// the envelope's size, and the divergence is a finding.
pub fn parse_socket_token(status: u16, body: &[u8]) -> Result<SocketToken, Failure> {
    if status != 200 {
        return Err(Failure::Fact(format!(
            "the token endpoint answered {status} rather than 200: {}",
            String::from_utf8_lossy(body).trim()
        )));
    }
    #[derive(Deserialize)]
    struct Token {
        otp: String,
        limit: i32,
        online: i32,
    }
    // The envelope first, then the flat shape the design states.
    if let Ok(envelope) = serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(body)
        && let Some(data) = envelope.get("data")
        && let Ok(token) = serde_json::from_value::<Token>(data.clone())
    {
        return Ok(SocketToken {
            limit: token.limit,
            online: token.online,
            otp: token.otp,
        });
    }
    let token: Token = serde_json::from_slice(body)
        .map_err(|error| Failure::Malformed(format!("the token body: {error}")))?;
    if token.otp.is_empty() {
        return Err(Failure::Fact(
            "the token body carried an empty otp".to_owned(),
        ));
    }
    Ok(SocketToken {
        limit: token.limit,
        online: token.online,
        otp: token.otp,
    })
}

/// A short fingerprint of a secret, so two reads can be compared without either
/// being stated.
pub fn fingerprint(value: &[u8]) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

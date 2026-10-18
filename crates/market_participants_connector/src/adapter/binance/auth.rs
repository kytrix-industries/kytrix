use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey};
use market_participants_model::{identifier::VenueId, listing::route::VenueRoute};
use reqwest::Client;
use serde::Deserialize;
use smol_str::SmolStr;

use super::BinanceCodec;
use crate::error::MarketParticipantError;

#[derive(Clone, Deserialize)]
pub(crate) struct ListenKeyResponse {
    #[serde(rename = "listenKey")]
    pub listen_key: SmolStr,
}

impl std::fmt::Debug for ListenKeyResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ListenKeyResponse")
            .field("listen_key", &"<redacted>")
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct ListenKeyManager {
    endpoint: SmolStr,
    api_key: SmolStr,
}

impl std::fmt::Debug for ListenKeyManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let endpoint = url::Url::parse(self.endpoint.as_str())
            .map(|mut endpoint| {
                endpoint.set_path("/");
                endpoint.set_query(None);
                endpoint.set_fragment(None);
                endpoint.to_string()
            })
            .unwrap_or_else(|_| "<redacted>".to_owned());
        formatter
            .debug_struct("ListenKeyManager")
            .field("endpoint", &endpoint)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl ListenKeyManager {
    pub(crate) fn new(endpoint: impl Into<SmolStr>, api_key: impl Into<SmolStr>) -> Self {
        Self {
            endpoint: endpoint.into(),
            api_key: api_key.into(),
        }
    }
    pub(crate) async fn acquire(
        &self,
        client: &Client,
        route: &VenueRoute,
    ) -> Result<SmolStr, MarketParticipantError> {
        let response = client
            .post(self.endpoint.as_str())
            .header("X-MBX-APIKEY", self.api_key.as_str())
            .send()
            .await
            .map_err(|_| MarketParticipantError::ConnectionFailed {
                route: route.clone(),
                endpoint: self.endpoint.clone(),
                source: network::error::NetworkError::Io {
                    message: "listen key request failed".into(),
                },
            })?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|_| MarketParticipantError::DecodeFailed {
                operation: "read Binance listen key response".into(),
                raw_data: bytes::Bytes::new(),
            })?;
        if !status.is_success() {
            return Err(BinanceCodec.response_error(route, self.endpoint.as_str(), status, body));
        }
        let decoded: ListenKeyResponse =
            serde_json::from_slice(&body).map_err(|_| MarketParticipantError::DecodeFailed {
                operation: "decode Binance listen key response".into(),
                raw_data: body.clone(),
            })?;
        Ok(decoded.listen_key)
    }
    pub(crate) async fn refresh(
        &self,
        client: &Client,
        listen_key: &str,
        route: &VenueRoute,
    ) -> Result<(), MarketParticipantError> {
        self.control(client, reqwest::Method::PUT, listen_key, route)
            .await
    }
    pub(crate) async fn delete(
        &self,
        client: &Client,
        listen_key: &str,
        route: &VenueRoute,
    ) -> Result<(), MarketParticipantError> {
        self.control(client, reqwest::Method::DELETE, listen_key, route)
            .await
    }

    async fn control(
        &self,
        client: &Client,
        method: reqwest::Method,
        listen_key: &str,
        route: &VenueRoute,
    ) -> Result<(), MarketParticipantError> {
        let endpoint = format!("{}?listenKey={listen_key}", self.endpoint);
        let response = client
            .request(method, endpoint)
            .header("X-MBX-APIKEY", self.api_key.as_str())
            .send()
            .await
            .map_err(|_| MarketParticipantError::ConnectionFailed {
                route: route.clone(),
                endpoint: self.endpoint.clone(),
                source: network::error::NetworkError::Io {
                    message: "listen key control request failed".into(),
                },
            })?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|_| MarketParticipantError::DecodeFailed {
                operation: "read Binance listen key response".into(),
                raw_data: bytes::Bytes::new(),
            })?;
        if !status.is_success() {
            // The endpoint a failure names never carries the listen key it
            // addressed: that key is the account's stream credential.
            return Err(BinanceCodec.response_error(route, self.endpoint.as_str(), status, body));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct Ed25519Signer {
    key: SigningKey,
}
impl Ed25519Signer {
    pub(crate) fn from_hex(value: &str) -> Result<Self, MarketParticipantError> {
        let bytes = const_hex::decode_to_array::<_, 32>(value.trim().trim_start_matches("0x"))
            .map_err(|_| MarketParticipantError::UnusableCredentials {
                venue: VenueId::Binance,
            })?;
        Ok(Self {
            key: SigningKey::from_bytes(&bytes),
        })
    }

    pub(crate) fn from_private_key(
        value: &str,
        passphrase: Option<&str>,
    ) -> Result<Self, MarketParticipantError> {
        use pkcs8::DecodePrivateKey;

        let value = value.trim();
        if value.contains("BEGIN") {
            let key = match passphrase {
                Some(passphrase) => SigningKey::from_pkcs8_encrypted_pem(value, passphrase),
                None => SigningKey::from_pkcs8_pem(value),
            }
            .map_err(|_| MarketParticipantError::UnusableCredentials {
                venue: VenueId::Binance,
            })?;
            return Ok(Self { key });
        }
        Self::from_hex(value)
    }
    pub(crate) fn sign(&self, payload: &[u8]) -> SmolStr {
        STANDARD.encode(self.key.sign(payload).to_bytes()).into()
    }
}

#[cfg(test)]
mod tests {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use ed25519_dalek::{Signature, Verifier};
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::{Ed25519Signer, ListenKeyManager, ListenKeyResponse};
    use crate::error::MarketParticipantError;

    /// The route a listen-key control call is reported against.
    fn route() -> VenueRoute {
        VenueRoute::new(
            VenueId::Binance,
            VenueScopeId::new("spot").expect("spot is a canonical scope id"),
        )
    }

    #[test]
    fn binance_ed25519_signatures_are_base64_and_verifiable() {
        let private_key = [7_u8; 32];
        let signer = Ed25519Signer::from_hex(&const_hex::encode(private_key)).unwrap();
        let encoded = signer.sign(b"payload");
        let signature_bytes = STANDARD.decode(encoded.as_bytes()).unwrap();
        let signature = Signature::from_slice(&signature_bytes).unwrap();
        let verifying_key = ed25519_dalek::SigningKey::from_bytes(&private_key).verifying_key();

        verifying_key
            .verify(b"payload", &signature)
            .expect("signature should verify with the matching public key");
    }

    #[test]
    fn listen_key_manager_debug_redacts_api_key() {
        let manager = ListenKeyManager::new(
            "https://api.binance.com/api/v3/userDataStream/listen-key-secret?apiKey=endpoint-secret",
            "api-key-secret",
        );
        let debug = format!("{manager:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("api-key-secret"));
        assert!(!debug.contains("listen-key-secret"));
        assert!(!debug.contains("endpoint-secret"));
    }

    #[test]
    fn listen_key_response_debug_redacts_token() {
        let response = ListenKeyResponse {
            listen_key: "listen-key-secret".into(),
        };
        let debug = format!("{response:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("listen-key-secret"));
    }

    #[tokio::test]
    async fn listen_key_manager_uses_binance_control_methods() {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();

        let (endpoint, response) =
            response_server("200 OK", r#"{"listenKey":"listen-key-1"}"#).await;
        let manager = ListenKeyManager::new(endpoint, "api-key");
        let listen_key = manager.acquire(&client, &route()).await.unwrap();
        assert_eq!(listen_key.as_str(), "listen-key-1");
        assert!(
            response
                .await
                .unwrap()
                .starts_with("POST /api/v3/userDataStream")
        );

        let (endpoint, response) = response_server("200 OK", "{}").await;
        let manager = ListenKeyManager::new(endpoint, "api-key");
        manager
            .refresh(&client, "listen-key-1", &route())
            .await
            .unwrap();
        assert!(
            response
                .await
                .unwrap()
                .starts_with("PUT /api/v3/userDataStream?listenKey=listen-key-1")
        );

        let (endpoint, response) = response_server("200 OK", "{}").await;
        let manager = ListenKeyManager::new(endpoint, "api-key");
        manager
            .delete(&client, "listen-key-1", &route())
            .await
            .unwrap();
        assert!(
            response
                .await
                .unwrap()
                .starts_with("DELETE /api/v3/userDataStream?listenKey=listen-key-1")
        );
    }

    #[tokio::test]
    async fn listen_key_manager_preserves_typed_http_and_decode_errors() {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let (endpoint, response) = response_server("500 Internal Server Error", "{}").await;
        let manager = ListenKeyManager::new(endpoint, "api-key");
        let error = manager
            .acquire(&client, &route())
            .await
            .expect_err("server error should fail");
        assert!(error.is_retryable());
        assert!(matches!(
            error,
            MarketParticipantError::ConnectionFailed { .. }
        ));
        response.await.unwrap();

        let (endpoint, response) = response_server(
            "400 Bad Request",
            r#"{"code":-1121,"msg":"Invalid symbol."}"#,
        )
        .await;
        let manager = ListenKeyManager::new(endpoint, "api-key");
        let error = manager
            .acquire(&client, &route())
            .await
            .expect_err("a refused request should fail");
        assert_eq!(error.market_participant_code(), Some("-1121"));
        assert!(matches!(
            error,
            MarketParticipantError::Rejected { message, retryable: false, .. } if message == "Invalid symbol."
        ));
        response.await.unwrap();

        let (endpoint, response) = response_server("403 Forbidden", "IP is not whitelisted").await;
        let manager = ListenKeyManager::new(endpoint, "api-key");
        let error = manager
            .refresh(&client, "listen-key-1", &route())
            .await
            .expect_err("a refused control request should fail");
        assert!(matches!(
            error,
            MarketParticipantError::Rejected { code, message, .. }
                if code == "403" && message == "IP is not whitelisted"
        ));
        response.await.unwrap();

        let (endpoint, response) = response_server("200 OK", r#"{"unexpected":"field"}"#).await;
        let manager = ListenKeyManager::new(endpoint, "api-key");
        let error = manager
            .acquire(&client, &route())
            .await
            .expect_err("malformed response should fail");
        assert!(
            matches!(error, MarketParticipantError::DecodeFailed { operation, .. } if operation == "decode Binance listen key response")
        );
        response.await.unwrap();
    }

    async fn response_server(
        status: &str,
        body: &str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_owned();
        let body = body.to_owned();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 512];
                let count = socket.read(&mut chunk).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request)
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned()
        });
        (format!("http://{address}/api/v3/userDataStream"), task)
    }
}

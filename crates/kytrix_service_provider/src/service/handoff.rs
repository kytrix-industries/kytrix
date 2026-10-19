//! Browser sign-in handoff creation against the Kytrix API.
//!
//! The desktop session authorizes the request with its ordinary access token and a
//! DPoP proof; the API answers with the one-time code the browser will redeem.

use app_config::network::{Config, KytrixServiceCapability, RouteTarget, ServiceTarget};
use bytes::BytesMut;
use network::transport::{http::HttpClientClass, hub::TransportHub};
use serde::Deserialize;
use url::Url;
use widget_tokio_runtime::runtime::Runtime;

use crate::auth::model::AuthError;

const MAX_RESPONSE_BYTES: usize = 16 * 1024;
const HANDOFF_CODE_CHARS: usize = 43;

#[derive(Deserialize)]
struct DashboardHandoffResponse {
    code: String,
}

pub(super) async fn request_dashboard_handoff(
    config: &Config,
    transport: &TransportHub,
    runtime: &Runtime,
    endpoint: &Url,
    access_token: &str,
    proof: &str,
) -> Result<String, AuthError> {
    let _operation = transport
        .begin_operation()
        .map_err(|_| AuthError::SessionClosed)?;
    let target = RouteTarget::Service(ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::Account,
    });
    let cancellation = transport.cancellation().child_token();
    let proxy =
        network::proxy::resolve_async_cancellable(config, &target, endpoint, cancellation, runtime)
            .await
            .map_err(|_| AuthError::Route)?;
    let client = transport
        .http()
        .client(&proxy, HttpClientClass::KytrixService)
        .map_err(|_| AuthError::HttpClient)?;
    let mut response = client
        .post(endpoint.clone())
        .header("Authorization", format!("DPoP {access_token}"))
        .header("DPoP", proof)
        .send()
        .await
        .map_err(|_| AuthError::Request)?;
    if !response.status().is_success() {
        return Err(AuthError::Status(response.status()));
    }
    let mut body = BytesMut::with_capacity(MAX_RESPONSE_BYTES);
    while let Some(chunk) = response.chunk().await.map_err(|_| AuthError::Request)? {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(AuthError::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    let response: DashboardHandoffResponse =
        serde_json::from_slice(&body).map_err(|_| AuthError::InvalidResponse)?;
    if !valid_code(&response.code) {
        return Err(AuthError::InvalidResponse);
    }
    Ok(response.code)
}

fn valid_code(code: &str) -> bool {
    code.len() == HANDOFF_CODE_CHARS
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::test_support::{OAuthHttpBehavior, TestOAuthServer, test_transport};

    const CODE: &str = "0123456789012345678901234567890123456789012";

    #[tokio::test]
    async fn requests_the_handoff_with_the_access_token_and_dpop_proof() {
        let mut server = TestOAuthServer::spawn(vec![OAuthHttpBehavior::Respond {
            status: 200,
            body: serde_json::json!({ "code": CODE }).to_string(),
        }])
        .await;
        let transport = test_transport();
        let endpoint = server.origin.join("v1/account/dashboard-handoff").unwrap();

        let code = request_dashboard_handoff(
            &Config::default(),
            &transport,
            &Runtime::for_test(),
            &endpoint,
            "access-token",
            "dpop-proof",
        )
        .await
        .expect("the handoff response should decode");

        assert_eq!(code, CODE);
        let request = server.request().await;
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/v1/account/dashboard-handoff");
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some("DPoP access-token")
        );
        assert_eq!(
            request.headers.get("dpop").map(String::as_str),
            Some("dpop-proof")
        );

        transport.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn rejects_status_and_malformed_code() {
        let mut server = TestOAuthServer::spawn(vec![
            OAuthHttpBehavior::Respond {
                status: 503,
                body: "{}".to_owned(),
            },
            OAuthHttpBehavior::Respond {
                status: 200,
                body: serde_json::json!({ "code": "short" }).to_string(),
            },
        ])
        .await;
        let transport = test_transport();
        let endpoint = server.origin.join("v1/account/dashboard-handoff").unwrap();

        let status = request_dashboard_handoff(
            &Config::default(),
            &transport,
            &Runtime::for_test(),
            &endpoint,
            "access-token",
            "dpop-proof",
        )
        .await;
        assert!(matches!(
            status,
            Err(AuthError::Status(reqwest::StatusCode::SERVICE_UNAVAILABLE))
        ));
        let _ = server.request().await;

        let malformed = request_dashboard_handoff(
            &Config::default(),
            &transport,
            &Runtime::for_test(),
            &endpoint,
            "access-token",
            "dpop-proof",
        )
        .await;
        assert!(matches!(malformed, Err(AuthError::InvalidResponse)));
        let _ = server.request().await;

        transport.shutdown().await;
        server.shutdown().await;
    }
}

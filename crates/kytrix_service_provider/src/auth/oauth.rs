use std::sync::Arc;

use app_config::network::{Config, KytrixServiceCapability, RouteTarget, ServiceTarget};
use bytes::BytesMut;
use futures::future::BoxFuture;
use network::transport::{http::HttpClientClass, hub::TransportHub};
use serde::de::DeserializeOwned;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;
use widget_tokio_runtime::runtime::Runtime;

use super::{
    API_RESOURCE, AccessToken, AccountIdentity, AccountWalletKind, AuthAuthority, AuthError,
    CLIENT_ID, DpopKey, MAX_RESPONSE_BYTES, OAuthErrorBody, RefreshedCredential, SCOPE,
    TOKEN_TTL_SECS, TokenResponse, TokenWalletKind, timestamp,
};

pub(super) struct OAuthAuthority {
    pub config: watch::Receiver<Arc<Config>>,
    pub transport: Arc<TransportHub>,
    pub account_origin: Url,
    /// The application runtime every route resolution of this authority runs on.
    pub runtime: Runtime,
}

impl AuthAuthority for OAuthAuthority {
    fn exchange_authorization_code<'a>(
        &'a self,
        key: &'a DpopKey,
        code: &'a str,
        redirect_uri: &'a str,
        code_verifier: &'a str,
    ) -> BoxFuture<'a, Result<TokenResponse, AuthError>> {
        Box::pin(async move {
            let _operation = self
                .transport
                .begin_operation()
                .map_err(|_| AuthError::SessionClosed)?;
            let endpoint = self.endpoint("/oauth/token")?;
            let config = self.config.borrow().clone();
            let cancellation = self.transport.cancellation().child_token();
            let client = self.client(&config, &endpoint, &cancellation).await?;
            let response = client
                .post(endpoint.clone())
                .header("DPoP", key.proof("POST", endpoint.as_str(), None, None)?)
                .form(&[
                    ("grant_type", "authorization_code"),
                    ("client_id", CLIENT_ID),
                    ("code", code),
                    ("redirect_uri", redirect_uri),
                    ("code_verifier", code_verifier),
                    ("resource", API_RESOURCE),
                ])
                .send()
                .await
                .map_err(|_| AuthError::Request)?;
            token_response(response).await
        })
    }

    fn refresh<'a>(
        &'a self,
        user_id: &'a str,
        refresh_token: &'a str,
        key: &'a DpopKey,
    ) -> BoxFuture<'a, Result<RefreshedCredential, AuthError>> {
        Box::pin(self.refresh(user_id, refresh_token, key))
    }

    fn revoke<'a>(
        &'a self,
        refresh_token: &'a str,
        key: &'a DpopKey,
    ) -> BoxFuture<'a, Result<(), AuthError>> {
        Box::pin(self.revoke(refresh_token, key))
    }
}

impl OAuthAuthority {
    fn endpoint(&self, path: &str) -> Result<Url, AuthError> {
        self.account_origin
            .join(path)
            .map_err(|_| AuthError::InvalidResponse)
    }

    pub(super) async fn client(
        &self,
        config: &Config,
        destination: &Url,
        cancellation: &CancellationToken,
    ) -> Result<reqwest::Client, AuthError> {
        let target = RouteTarget::Service(ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::Account,
        });
        let proxy = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(AuthError::SessionClosed),
            result = network::proxy::resolve_async_cancellable(
                config,
                &target,
                destination,
                cancellation.clone(),
                &self.runtime,
            ) => result.map_err(|_| AuthError::Route)?,
        };
        self.transport
            .http()
            .client(&proxy, HttpClientClass::KytrixService)
            .map_err(|_| AuthError::HttpClient)
    }

    async fn refresh(
        &self,
        user_id: &str,
        refresh_token: &str,
        key: &DpopKey,
    ) -> Result<RefreshedCredential, AuthError> {
        let _operation = self
            .transport
            .begin_operation()
            .map_err(|_| AuthError::SessionClosed)?;
        let endpoint = self.endpoint("/oauth/token")?;
        let config = self.config.borrow().clone();
        let cancellation = self.transport.cancellation().child_token();
        let client = self.client(&config, &endpoint, &cancellation).await?;
        refresh_from_authority(client, &endpoint, user_id, refresh_token, key).await
    }

    async fn revoke(&self, refresh_token: &str, key: &DpopKey) -> Result<(), AuthError> {
        let _operation = self
            .transport
            .begin_operation()
            .map_err(|_| AuthError::SessionClosed)?;
        let endpoint = self.endpoint("/oauth/revoke")?;
        let config = self.config.borrow().clone();
        let cancellation = self.transport.cancellation().child_token();
        let client = self.client(&config, &endpoint, &cancellation).await?;
        revoke_at_authority(client, &endpoint, refresh_token, key).await
    }
}

async fn refresh_from_authority(
    client: reqwest::Client,
    endpoint: &Url,
    user_id: &str,
    refresh_token: &str,
    key: &DpopKey,
) -> Result<RefreshedCredential, AuthError> {
    let response = client
        .post(endpoint.clone())
        .header("DPoP", key.proof("POST", endpoint.as_str(), None, None)?)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token),
            ("resource", API_RESOURCE),
        ])
        .send()
        .await
        .map_err(|error| {
            if error.is_connect() {
                AuthError::Request
            } else {
                AuthError::RefreshOutcomeUnknown
            }
        })?;
    if response.status().is_server_error() {
        return Err(AuthError::RefreshOutcomeUnknown);
    }
    let success = response.status().is_success();
    let response = token_response(response).await.map_err(|error| {
        if success && matches!(error, AuthError::Request | AuthError::InvalidResponse) {
            AuthError::RefreshOutcomeUnknown
        } else {
            error
        }
    })?;
    if response.kytrix_user_id != user_id || validate_account(&response).is_err() {
        return Err(AuthError::RefreshOutcomeUnknown);
    }
    let expires_at = timestamp()
        .map_err(|_| AuthError::RefreshOutcomeUnknown)?
        .checked_add(response.expires_in)
        .ok_or(AuthError::RefreshOutcomeUnknown)?;
    Ok(RefreshedCredential {
        refresh_token: response.refresh_token,
        access: AccessToken {
            value: response.access_token,
            expires_at,
        },
        plan_code: response.kytrix_plan_code,
    })
}

/// Tells the authority to drop a refresh token.
///
/// A sign-out does not wait for this answer and does nothing with it: the local
/// credential is already gone, so the only thing a failure changes is that the
/// authority keeps a token until it expires, which is why the reason it failed is
/// reported rather than classified into a local outcome.
async fn revoke_at_authority(
    client: reqwest::Client,
    endpoint: &Url,
    refresh_token: &str,
    key: &DpopKey,
) -> Result<(), AuthError> {
    let proof = key.proof("POST", endpoint.as_str(), None, None)?;
    let response = client
        .post(endpoint.clone())
        .header("DPoP", proof)
        .form(&[
            ("token", refresh_token),
            ("token_type_hint", "refresh_token"),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .await
        .map_err(|_| AuthError::Request)?;
    if !response.status().is_success() {
        return Err(AuthError::Status(response.status()));
    }
    Ok(())
}

async fn token_response(response: reqwest::Response) -> Result<TokenResponse, AuthError> {
    if !response.status().is_success() {
        let status = response.status();
        let body = read_json::<OAuthErrorBody>(response).await.ok();
        return if body
            .as_ref()
            .is_some_and(|body| body.error == "invalid_grant")
        {
            Err(AuthError::InvalidGrant)
        } else {
            Err(AuthError::Status(status))
        };
    }
    let response = read_json::<TokenResponse>(response).await?;
    if response.token_type != "DPoP"
        || response.expires_in != TOKEN_TTL_SECS
        || response.scope != SCOPE
        || response.access_token.is_empty()
        || response.refresh_token.is_empty()
    {
        return Err(AuthError::InvalidResponse);
    }
    Ok(response)
}

pub(super) fn validate_account(response: &TokenResponse) -> Result<AccountIdentity, AuthError> {
    if !canonical_user_id(&response.kytrix_user_id) {
        return Err(AuthError::InvalidResponse);
    }
    let wallet_kind = match response.kytrix_wallet_kind {
        TokenWalletKind::Ethereum if canonical_ethereum(&response.kytrix_wallet_address) => {
            AccountWalletKind::Ethereum
        }
        TokenWalletKind::Solana if canonical_solana(&response.kytrix_wallet_address) => {
            AccountWalletKind::Solana
        }
        _ => return Err(AuthError::InvalidResponse),
    };
    Ok(AccountIdentity::new(
        response.kytrix_user_id.clone(),
        response.kytrix_wallet_address.clone(),
        wallet_kind,
    ))
}

pub(super) fn canonical_user_id(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|user_id| user_id.hyphenated().to_string() == value)
}

fn canonical_ethereum(value: &str) -> bool {
    value.len() == 42
        && value.starts_with("0x")
        && value[2..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn canonical_solana(value: &str) -> bool {
    bs58::decode(value)
        .into_vec()
        .is_ok_and(|bytes| bytes.len() == 32 && bs58::encode(bytes).into_string() == value)
}

async fn read_json<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T, AuthError> {
    let mut body = BytesMut::with_capacity(MAX_RESPONSE_BYTES);
    while let Some(chunk) = response.chunk().await.map_err(|_| AuthError::Request)? {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(AuthError::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| AuthError::InvalidResponse)
}
#[cfg(test)]
mod tests {
    use super::{
        super::{ACCOUNT_ORIGIN, SessionChange, SessionControl},
        *,
    };
    use crate::auth::test_support::*;

    #[tokio::test]
    async fn oauth_authority_should_reuse_the_shared_kytrix_service_client_class() {
        let (_, config) = watch::channel(Arc::new(Config::default()));
        let transport = test_transport();
        let authority = OAuthAuthority {
            config,
            transport: Arc::clone(&transport),
            account_origin: Url::parse(ACCOUNT_ORIGIN).unwrap(),
            runtime: Runtime::for_test(),
        };

        let config = authority.config.borrow().clone();
        let cancellation = CancellationToken::new();
        authority
            .client(&config, &authority.account_origin, &cancellation)
            .await
            .expect("the direct Account route should build");
        authority
            .client(&config, &authority.account_origin, &cancellation)
            .await
            .expect("the same Account route should reuse its client");
        assert_eq!(transport.http().client_count(), 1);

        transport
            .http()
            .client(
                &app_config::network::ResolvedProxy::NoProxy,
                HttpClientClass::Public,
            )
            .expect("the comparison client should build");
        assert_eq!(transport.http().client_count(), 2);
    }

    #[cfg(not(feature = "production"))]
    #[tokio::test]
    async fn development_oauth_should_exchange_authorization_code_over_local_http() {
        let mut server = TestOAuthServer::spawn(vec![OAuthHttpBehavior::Respond {
            status: 200,
            body: oauth_token_body("refresh-exchange", "access-exchange"),
        }])
        .await;
        let authority = oauth_authority(server.origin.clone(), test_transport());
        let key = DpopKey::generate().unwrap();

        let response = authority
            .exchange_authorization_code(
                &key,
                "authorization-code",
                "http://127.0.0.1/callback",
                "pkce-verifier",
            )
            .await
            .expect("development OAuth should exchange through local HTTP");

        assert_eq!(response.access_token, "access-exchange");
        assert_eq!(server.request().await.target, "/oauth/token");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn oauth_authority_should_fail_closed_for_a_dangling_account_proxy() {
        let mut config = Config::default();
        config.proxy_applications.insert(
            ServiceTarget::KytrixService {
                capability: KytrixServiceCapability::Account,
            },
            ProxyRoute::named("missing-proxy"),
        );
        let (_, config) = watch::channel(Arc::new(config));
        let transport = test_transport();
        let authority = OAuthAuthority {
            config,
            transport: Arc::clone(&transport),
            account_origin: Url::parse(ACCOUNT_ORIGIN).unwrap(),
            runtime: Runtime::for_test(),
        };

        let config = authority.config.borrow().clone();
        assert!(matches!(
            authority
                .client(
                    &config,
                    &authority.account_origin,
                    &CancellationToken::new(),
                )
                .await,
            Err(AuthError::Route)
        ));
        assert_eq!(transport.http().client_count(), 0);
    }

    #[tokio::test]
    async fn cancelled_oauth_system_proxy_resolution_is_session_closed() {
        let config = Config {
            proxy_default: ProxyRoute::System,
            ..Config::default()
        };
        let (_, receiver) = watch::channel(Arc::new(config));
        let transport = test_transport();
        let authority = OAuthAuthority {
            config: receiver,
            transport,
            account_origin: Url::parse(ACCOUNT_ORIGIN).unwrap(),
            runtime: Runtime::for_test(),
        };
        let config = authority.config.borrow().clone();
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        assert!(matches!(
            authority
                .client(&config, &authority.account_origin, &cancellation)
                .await,
            Err(AuthError::SessionClosed)
        ));
    }

    #[tokio::test]
    async fn oauth_route_should_issue_each_oauth_request() {
        let mut server = TestOAuthServer::spawn(vec![
            OAuthHttpBehavior::Respond {
                status: 200,
                body: oauth_token_body("refresh-exchange", "access-exchange"),
            },
            OAuthHttpBehavior::Respond {
                status: 200,
                body: oauth_token_body("refresh-rotated", "access-refreshed"),
            },
            OAuthHttpBehavior::Respond {
                status: 204,
                body: String::new(),
            },
        ])
        .await;
        let target = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::Account,
        };
        let mut config = Config::default();
        config.proxy_applications.insert(target, ProxyRoute::Direct);
        let (_, config) = watch::channel(Arc::new(config));
        let transport = test_transport();
        let authority = OAuthAuthority {
            config,
            transport: Arc::clone(&transport),
            account_origin: server.origin.clone(),
            runtime: Runtime::for_test(),
        };
        let key = DpopKey::generate().unwrap();
        authority
            .exchange_authorization_code(
                &key,
                "authorization-code",
                "http://127.0.0.1/callback",
                "pkce-verifier",
            )
            .await
            .expect("authorization-code exchange should succeed");
        authority
            .refresh(TEST_USER_ID, "refresh-exchange", &key)
            .await
            .expect("refresh should succeed");
        authority
            .revoke("refresh-rotated", &key)
            .await
            .expect("revoke should succeed");

        assert_eq!(server.request().await.target, "/oauth/token");
        assert_eq!(server.request().await.target, "/oauth/token");
        assert_eq!(server.request().await.target, "/oauth/revoke");

        transport.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn production_oauth_http_contract_binds_forms_dpop_and_account_transport() {
        let mut server = TestOAuthServer::spawn(vec![
            OAuthHttpBehavior::Respond {
                status: 200,
                body: oauth_token_body("refresh-exchange", "access-exchange"),
            },
            OAuthHttpBehavior::Respond {
                status: 200,
                body: oauth_token_body("refresh-rotated", "access-refreshed"),
            },
            OAuthHttpBehavior::Respond {
                status: 204,
                body: String::new(),
            },
        ])
        .await;
        let transport = test_transport();
        let authority = oauth_authority(server.origin.clone(), Arc::clone(&transport));
        let key = DpopKey::generate().unwrap();

        let exchanged = authority
            .exchange_authorization_code(
                &key,
                "authorization-code",
                "http://127.0.0.1/callback",
                "pkce-verifier",
            )
            .await
            .expect("the production authorization-code helper should decode a token response");
        assert_eq!(exchanged.refresh_token, "refresh-exchange");
        let exchange_request = server.request().await;
        assert_eq!(exchange_request.method, "POST");
        assert_eq!(exchange_request.target, "/oauth/token");
        assert_eq!(
            form_fields(&exchange_request.body),
            std::collections::BTreeMap::from([
                ("client_id".to_owned(), CLIENT_ID.to_owned()),
                ("code".to_owned(), "authorization-code".to_owned()),
                ("code_verifier".to_owned(), "pkce-verifier".to_owned()),
                ("grant_type".to_owned(), "authorization_code".to_owned()),
                (
                    "redirect_uri".to_owned(),
                    "http://127.0.0.1/callback".to_owned()
                ),
                ("resource".to_owned(), API_RESOURCE.to_owned()),
            ])
        );
        let exchange_claims = dpop_claims(&exchange_request);
        assert_eq!(exchange_claims["htm"], "POST");
        assert_eq!(
            exchange_claims["htu"],
            server.origin.join("/oauth/token").unwrap().as_str()
        );
        assert!(exchange_claims.get("ath").is_none());

        let store = Arc::new(StallingCredentialStore::new(Some(credential_payload(
            TEST_USER_ID,
            "refresh-exchange",
            &key,
        ))));
        let (changes, _) = watch::channel(SessionChange::default());
        let session = AuthSession::spawn_with_control(
            store,
            Arc::new(SessionControl::new(0, 0, changes)),
            authority,
        )
        .unwrap();
        let (profile, (access_token, api_proof)) = session
            .headers(None, "GET", "https://api.kytrix.xyz/v1/news")
            .await
            .expect("the production refresh helper should establish the session");
        assert_eq!(profile.account_id(), TEST_USER_ID);
        assert_eq!(access_token, "access-refreshed");
        let refresh_request = server.request().await;
        assert_eq!(refresh_request.method, "POST");
        assert_eq!(refresh_request.target, "/oauth/token");
        assert_eq!(
            form_fields(&refresh_request.body).get("refresh_token"),
            Some(&"refresh-exchange".to_owned())
        );
        let refresh_claims = dpop_claims(&refresh_request);
        assert_eq!(refresh_claims["htm"], "POST");
        assert_eq!(
            refresh_claims["htu"],
            server.origin.join("/oauth/token").unwrap().as_str()
        );
        let api_parts = api_proof.split('.').collect::<Vec<_>>();
        let api_claims: serde_json::Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(api_parts[1])
                .expect("valid API DPoP"),
        )
        .unwrap();
        assert_eq!(api_claims["htm"], "GET");
        assert_eq!(api_claims["htu"], "https://api.kytrix.xyz/v1/news");
        assert_eq!(
            api_claims["ath"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(b"access-refreshed"))
        );

        let discarded = session.discard_local_session();
        assert!(
            discarded.removed(),
            "the credential is gone from this machine"
        );
        session
            .revoke_session(
                discarded
                    .into_revocation()
                    .expect("the session held a token to revoke"),
            )
            .await;
        let revoke_request = server.request().await;
        assert_eq!(revoke_request.method, "POST");
        assert_eq!(revoke_request.target, "/oauth/revoke");
        assert_eq!(
            form_fields(&revoke_request.body),
            std::collections::BTreeMap::from([
                ("client_id".to_owned(), CLIENT_ID.to_owned()),
                ("token".to_owned(), "refresh-rotated".to_owned()),
                ("token_type_hint".to_owned(), "refresh_token".to_owned()),
            ])
        );
        assert_eq!(dpop_claims(&revoke_request)["htm"], "POST");
        assert!(transport.http().contains(
            &app_config::network::ResolvedProxy::NoProxy,
            HttpClientClass::KytrixService
        ));

        session.shutdown().await;
        transport.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn production_oauth_rejects_invalid_grant_oversized_body_and_revoke_status() {
        let mut server = TestOAuthServer::spawn(vec![
            OAuthHttpBehavior::Respond {
                status: 400,
                body: serde_json::json!({ "error": "invalid_grant" }).to_string(),
            },
            OAuthHttpBehavior::Respond {
                status: 200,
                body: "x".repeat(MAX_RESPONSE_BYTES + 1),
            },
            OAuthHttpBehavior::Respond {
                status: 400,
                body: serde_json::json!({ "error": "invalid_request" }).to_string(),
            },
        ])
        .await;
        let transport = test_transport();
        let authority = oauth_authority(server.origin.clone(), Arc::clone(&transport));
        let key = DpopKey::generate().unwrap();

        assert!(matches!(
            authority
                .refresh(TEST_USER_ID, "invalid-refresh", &key)
                .await,
            Err(AuthError::InvalidGrant)
        ));
        assert_eq!(
            form_fields(&server.request().await.body).get("refresh_token"),
            Some(&"invalid-refresh".to_owned())
        );
        assert!(matches!(
            authority
                .exchange_authorization_code(
                    &key,
                    "oversized",
                    "http://127.0.0.1/callback",
                    "verifier",
                )
                .await,
            Err(AuthError::InvalidResponse)
        ));
        let _ = server.request().await;
        let revoke_error = authority
            .revoke("secret-refresh-token", &key)
            .await
            .expect_err("a non-success revoke response should be explicit");
        let _ = server.request().await;
        assert!(matches!(
            revoke_error,
            AuthError::Status(reqwest::StatusCode::BAD_REQUEST)
        ));
        let debug = format!("{revoke_error:?}");
        assert!(!debug.contains("secret-refresh-token"));
        assert!(!debug.contains(&key.encode_private()));

        transport.shutdown().await;
        server.shutdown().await;
    }

    #[test]
    fn token_response_should_require_a_known_plan_code() {
        let valid = serde_json::from_str::<serde_json::Value>(&oauth_token_body(
            "refresh-token",
            "access-token",
        ))
        .unwrap();
        for plan_code in ["free", "plus", "pro"] {
            let mut response = valid.clone();
            response["kytrix_plan_code"] = serde_json::Value::String(plan_code.to_owned());
            assert!(serde_json::from_value::<TokenResponse>(response).is_ok());
        }

        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove("kytrix_plan_code");
        assert!(serde_json::from_value::<TokenResponse>(missing).is_err());
        let mut unknown = valid;
        unknown["kytrix_plan_code"] = serde_json::Value::String("enterprise".to_owned());
        assert!(serde_json::from_value::<TokenResponse>(unknown).is_err());
    }

    #[test]
    fn user_id_requires_the_existing_canonical_uuid_form() {
        assert!(canonical_user_id(TEST_USER_ID));
        assert!(canonical_user_id("00000000-0000-0000-0000-000000000000"));
        assert!(!canonical_user_id("AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA"));
        assert!(!canonical_user_id("11111111111141118111111111111111"));
    }
}

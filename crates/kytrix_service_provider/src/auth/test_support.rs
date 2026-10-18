pub(crate) use std::sync::atomic::Ordering;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize},
    },
};

pub(crate) use app_config::network::{Config, ProxyRoute};
pub(crate) use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
pub(crate) use bytes::Bytes;
pub(crate) use futures::future::BoxFuture;
pub(crate) use network::transport::hub::{TransportHub, TransportPolicy};
pub(crate) use p256::{ecdsa::SigningKey, elliptic_curve::Generate};
pub(crate) use sha2::{Digest, Sha256};
pub(crate) use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
    sync::{Notify, mpsc, watch},
    task::JoinHandle,
    time::{Duration, timeout},
};
pub(crate) use tokio_util::sync::CancellationToken;
pub(crate) use url::Url;
use widget_tokio_runtime::runtime::Runtime;

use super::*;
pub(crate) use crate::auth::session::AuthSession;

/// The transport policy every test session shares.
///
/// Production callers construct their own policy; tests reach the loopback
/// `/oauth` and `/rpc` fixtures over `http://` and `ws://`.
pub(crate) fn test_transport_policy() -> TransportPolicy {
    TransportPolicy {
        allow_plaintext: true,
        extra_root_certificates: Vec::new(),
    }
}

/// Builds the plaintext-tolerant transport that policy backs.
pub(crate) fn test_transport() -> Arc<TransportHub> {
    Arc::new(TransportHub::new(test_transport_policy()))
}

#[derive(Default)]
pub(crate) struct InMemoryCredentialStore {
    pub payload: Mutex<Option<Bytes>>,
    fail_next_store: AtomicBool,
}

impl CredentialStore for InMemoryCredentialStore {
    fn load(&self) -> Result<Bytes, AuthError> {
        self.payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or(AuthError::MissingCredential)
    }

    fn store(&self, payload: Bytes) -> Result<(), AuthError> {
        if self.fail_next_store.swap(false, Ordering::AcqRel) {
            return Err(AuthError::CredentialStore);
        }
        *self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(payload);
        Ok(())
    }

    fn delete(&self) -> Result<(), AuthError> {
        *self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        Ok(())
    }
}

impl AuthSession {
    pub(super) fn spawn_with_control(
        store: Arc<dyn CredentialStore>,
        control: Arc<SessionControl>,
        authority: Arc<dyn AuthAuthority>,
    ) -> Result<Self, AuthError> {
        Self::spawn_with_control_and_startup(store, control, authority, false, Runtime::for_test())
    }

    pub(crate) fn for_test(transport: Arc<TransportHub>) -> Self {
        Self::for_test_with_revision(transport, 0, 0)
    }

    pub(crate) fn for_test_with_revision(
        transport: Arc<TransportHub>,
        revision: u64,
        epoch: u64,
    ) -> Self {
        let _ = transport;
        let key = DpopKey::generate().expect("test DPoP key should generate");
        let store = Arc::new(InMemoryCredentialStore {
            payload: Mutex::new(Some(credential_payload(
                "11111111-1111-4111-8111-111111111111",
                "test-refresh-token",
                &key,
            ))),
            fail_next_store: AtomicBool::new(false),
        });
        let authority = Arc::new(ServiceTestAuthority);
        let (changes, _) = watch::channel(SessionChange::default());
        Self::spawn_with_control(
            store,
            Arc::new(SessionControl::new(revision, epoch, changes)),
            authority,
        )
        .expect("the test auth actor should start")
    }
}

struct ServiceTestAuthority;

impl AuthAuthority for ServiceTestAuthority {
    fn exchange_authorization_code<'a>(
        &'a self,
        _key: &'a DpopKey,
        _code: &'a str,
        _redirect_uri: &'a str,
        _code_verifier: &'a str,
    ) -> BoxFuture<'a, Result<TokenResponse, AuthError>> {
        Box::pin(async { Err(AuthError::Request) })
    }

    fn refresh<'a>(
        &'a self,
        _user_id: &'a str,
        _refresh_token: &'a str,
        _key: &'a DpopKey,
    ) -> BoxFuture<'a, Result<RefreshedCredential, AuthError>> {
        Box::pin(async move {
            Ok(RefreshedCredential {
                refresh_token: "test-refresh-token".to_owned(),
                access: AccessToken {
                    value: "test-renewed-access-token".to_owned(),
                    expires_at: i64::MAX,
                },
                plan_code: PlanCode::Free,
            })
        })
    }

    fn revoke<'a>(
        &'a self,
        _refresh_token: &'a str,
        _key: &'a DpopKey,
    ) -> BoxFuture<'a, Result<(), AuthError>> {
        Box::pin(async { Ok(()) })
    }
}

pub(crate) const TEST_USER_ID: &str = "11111111-1111-4111-8111-111111111111";
pub(crate) const TEST_REPLACEMENT_USER_ID: &str = "22222222-2222-4222-8222-222222222222";
pub(crate) const TEST_OTHER_USER_ID: &str = "33333333-3333-4333-8333-333333333333";

pub(crate) enum OAuthHttpBehavior {
    Respond { status: u16, body: String },
    DropResponse,
    Stall,
}

pub(crate) struct RecordedOAuthRequest {
    pub method: String,
    pub target: String,
    pub headers: std::collections::BTreeMap<String, String>,
    pub body: String,
}

pub(crate) struct TestOAuthServer {
    pub origin: Url,
    pub requests: mpsc::Receiver<RecordedOAuthRequest>,
    cancellation: CancellationToken,
    task: JoinHandle<()>,
}

impl TestOAuthServer {
    pub(crate) async fn spawn(behaviors: Vec<OAuthHttpBehavior>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (requests, request_receiver) = mpsc::channel(behaviors.len().max(1));
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            let mut behaviors = VecDeque::from(behaviors);
            while let Some(behavior) = behaviors.pop_front() {
                let accepted = tokio::select! {
                    biased;
                    () = task_cancellation.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((mut stream, _)) = accepted else { break };
                let Some(request) = read_oauth_request(&mut stream).await else {
                    break;
                };
                if requests.send(request).await.is_err() {
                    break;
                }
                match behavior {
                    OAuthHttpBehavior::Respond { status, body } => {
                        let reason = match status {
                            200 => "OK",
                            204 => "No Content",
                            400 => "Bad Request",
                            502 => "Bad Gateway",
                            _ => "Status",
                        };
                        let response = format!(
                            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        if stream.write_all(response.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                    OAuthHttpBehavior::DropResponse => {}
                    OAuthHttpBehavior::Stall => task_cancellation.cancelled().await,
                }
            }
        });
        Self {
            origin,
            requests: request_receiver,
            cancellation,
            task,
        }
    }

    pub(crate) async fn request(&mut self) -> RecordedOAuthRequest {
        timeout(Duration::from_secs(2), self.requests.recv())
            .await
            .expect("OAuth request should arrive before the deadline")
            .expect("OAuth request channel should remain open")
    }

    pub(crate) async fn shutdown(self) {
        self.cancellation.cancel();
        let _ = self.task.await;
    }
}

async fn read_oauth_request(stream: &mut tokio::net::TcpStream) -> Option<RecordedOAuthRequest> {
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        if bytes.len() >= 64 * 1024 {
            return None;
        }
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
    };
    let head = std::str::from_utf8(&bytes[..header_end]).ok()?;
    let mut lines = head.lines();
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_owned();
    let target = request_line.next()?.to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let body_len = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_default();
    while bytes.len().saturating_sub(header_end) < body_len {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    let body = std::str::from_utf8(&bytes[header_end..header_end + body_len])
        .ok()?
        .to_owned();
    Some(RecordedOAuthRequest {
        method,
        target,
        headers,
        body,
    })
}

pub(super) fn oauth_token_body(refresh_token: &str, access_token: &str) -> String {
    serde_json::json!({
        "access_token": access_token,
        "token_type": "DPoP",
        "expires_in": TOKEN_TTL_SECS,
        "refresh_token": refresh_token,
        "scope": SCOPE,
        "kytrix_user_id": TEST_USER_ID,
        "kytrix_wallet_kind": "ethereum",
        "kytrix_wallet_address": "0x1111111111111111111111111111111111111111",
        "kytrix_plan_code": "free"
    })
    .to_string()
}

pub(super) fn oauth_authority(origin: Url, transport: Arc<TransportHub>) -> Arc<OAuthAuthority> {
    let (_, config) = watch::channel(Arc::new(Config::default()));
    Arc::new(OAuthAuthority {
        config,
        transport,
        account_origin: origin,
        runtime: Runtime::for_test(),
    })
}

pub(super) fn form_fields(body: &str) -> std::collections::BTreeMap<String, String> {
    url::form_urlencoded::parse(body.as_bytes())
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect()
}

pub(super) fn dpop_claims(request: &RecordedOAuthRequest) -> serde_json::Value {
    let proof = request
        .headers
        .get("dpop")
        .expect("OAuth request should carry DPoP");
    let payload = proof
        .split('.')
        .nth(1)
        .expect("DPoP should contain a claims segment");
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap()
}

pub(crate) struct StallingCredentialStore {
    pub payload: Mutex<Option<Bytes>>,
    stall_next: AtomicBool,
    stall_store_after: AtomicUsize,
    failed_stores_remaining: AtomicUsize,
    fail_next_delete: AtomicBool,
    released: Mutex<bool>,
    release: Condvar,
    pub started: Notify,
    pub active: AtomicUsize,
    pub max_active: AtomicUsize,
    pub loads: AtomicUsize,
    pub stores: AtomicUsize,
    pub deletes: AtomicUsize,
}

impl StallingCredentialStore {
    pub(crate) fn new(payload: Option<Bytes>) -> Self {
        Self {
            payload: Mutex::new(payload),
            stall_next: AtomicBool::new(false),
            stall_store_after: AtomicUsize::new(0),
            failed_stores_remaining: AtomicUsize::new(0),
            fail_next_delete: AtomicBool::new(false),
            released: Mutex::new(false),
            release: Condvar::new(),
            started: Notify::new(),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            loads: AtomicUsize::new(0),
            stores: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
        }
    }

    pub(crate) fn stall_next(&self) {
        *self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
        self.stall_next.store(true, Ordering::Release);
    }

    pub(crate) fn stall_next_store(&self) {
        self.stall_store_after(1);
    }

    pub(crate) fn stall_store_after(&self, calls: usize) {
        assert_ne!(calls, 0, "a store stall needs a future call");
        *self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
        self.stall_store_after.store(calls, Ordering::Release);
    }

    pub(crate) fn release(&self) {
        *self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.release.notify_all();
    }

    pub(crate) fn fail_next_store(&self) {
        self.fail_next_stores(1);
    }

    pub(crate) fn fail_next_stores(&self, count: usize) {
        self.failed_stores_remaining.store(count, Ordering::Release);
    }

    pub(crate) fn fail_next_delete(&self) {
        self.fail_next_delete.store(true, Ordering::Release);
    }

    fn enter(&self, stall: bool) -> ActiveStoreOperation<'_> {
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.max_active.fetch_max(active, Ordering::AcqRel);
        if self.stall_next.swap(false, Ordering::AcqRel) || stall {
            self.started.notify_one();
            let released = self
                .released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop(
                self.release
                    .wait_while(released, |released| !*released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        }
        ActiveStoreOperation(&self.active)
    }
}

struct ActiveStoreOperation<'a>(&'a AtomicUsize);

impl Drop for ActiveStoreOperation<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl CredentialStore for StallingCredentialStore {
    fn load(&self) -> Result<Bytes, AuthError> {
        let _active = self.enter(false);
        self.loads.fetch_add(1, Ordering::AcqRel);
        self.payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or(AuthError::MissingCredential)
    }

    fn store(&self, payload: Bytes) -> Result<(), AuthError> {
        let is_refresh_barrier = serde_json::from_slice::<StoredCredential>(&payload)
            .is_ok_and(|record| matches!(record, StoredCredential::RefreshOutcomeUnknown));
        let stall = !is_refresh_barrier
            && self
                .stall_store_after
                .try_update(Ordering::AcqRel, Ordering::Acquire, |remaining: usize| {
                    remaining.checked_sub(1)
                })
                .is_ok_and(|remaining| remaining == 1);
        let _active = self.enter(stall);
        self.stores.fetch_add(1, Ordering::AcqRel);
        if !is_refresh_barrier
            && self
                .failed_stores_remaining
                .try_update(Ordering::AcqRel, Ordering::Acquire, |remaining: usize| {
                    remaining.checked_sub(1)
                })
                .is_ok()
        {
            return Err(AuthError::CredentialStore);
        }
        *self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(payload);
        Ok(())
    }

    fn delete(&self) -> Result<(), AuthError> {
        let _active = self.enter(false);
        self.deletes.fetch_add(1, Ordering::AcqRel);
        if self.fail_next_delete.swap(false, Ordering::AcqRel) {
            return Err(AuthError::CredentialStore);
        }
        *self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        Ok(())
    }
}

pub(crate) enum FakeRefreshOutcome {
    Current,
    Rotated(String),
    Error(AuthError),
}

pub(crate) struct FakeRefreshAuthority {
    outcomes: Mutex<VecDeque<FakeRefreshOutcome>>,
    plans: Mutex<VecDeque<PlanCode>>,
    calls: Mutex<Vec<(String, String)>>,
    revoke_outcomes: Mutex<VecDeque<AuthError>>,
    revocations: Mutex<Vec<String>>,
    call_count: AtomicUsize,
}

impl FakeRefreshAuthority {
    pub(crate) fn new() -> Self {
        Self {
            outcomes: Mutex::new(VecDeque::new()),
            plans: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
            revoke_outcomes: Mutex::new(VecDeque::new()),
            revocations: Mutex::new(Vec::new()),
            call_count: AtomicUsize::new(0),
        }
    }

    pub(crate) fn fail_next(&self, error: AuthError) {
        self.outcomes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(FakeRefreshOutcome::Error(error));
    }

    pub(crate) fn rotate_next(&self, refresh_token: &str) {
        self.outcomes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(FakeRefreshOutcome::Rotated(refresh_token.to_owned()));
    }

    pub(crate) fn plan_next(&self, plan_code: PlanCode) {
        self.plans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(plan_code);
    }

    pub(crate) fn calls(&self) -> Vec<(String, String)> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn fail_next_revoke(&self, error: AuthError) {
        self.revoke_outcomes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(error);
    }

    pub(crate) fn revocations(&self) -> Vec<String> {
        self.revocations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl AuthAuthority for FakeRefreshAuthority {
    fn exchange_authorization_code<'a>(
        &'a self,
        _key: &'a DpopKey,
        _code: &'a str,
        _redirect_uri: &'a str,
        _code_verifier: &'a str,
    ) -> BoxFuture<'a, Result<TokenResponse, AuthError>> {
        Box::pin(async { Err(AuthError::Request) })
    }

    fn refresh<'a>(
        &'a self,
        user_id: &'a str,
        refresh_token: &'a str,
        _key: &'a DpopKey,
    ) -> BoxFuture<'a, Result<RefreshedCredential, AuthError>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((user_id.to_owned(), refresh_token.to_owned()));
            let outcome = self
                .outcomes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .unwrap_or(FakeRefreshOutcome::Current);
            let refresh_token = match outcome {
                FakeRefreshOutcome::Current => refresh_token.to_owned(),
                FakeRefreshOutcome::Rotated(refresh_token) => refresh_token,
                FakeRefreshOutcome::Error(error) => return Err(error),
            };
            let call = self.call_count.fetch_add(1, Ordering::AcqRel) + 1;
            let plan_code = self
                .plans
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .unwrap_or(PlanCode::Free);
            Ok(RefreshedCredential {
                refresh_token,
                access: AccessToken {
                    value: format!("access-{call}"),
                    expires_at: i64::MAX,
                },
                plan_code,
            })
        })
    }

    fn revoke<'a>(
        &'a self,
        refresh_token: &'a str,
        _key: &'a DpopKey,
    ) -> BoxFuture<'a, Result<(), AuthError>> {
        Box::pin(async move {
            self.revocations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(refresh_token.to_owned());
            match self
                .revoke_outcomes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
            {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
    }
}

pub(crate) struct StallingRefreshAuthority {
    pub started: Notify,
    pub future_dropped: AtomicBool,
}

impl StallingRefreshAuthority {
    pub(crate) fn new() -> Self {
        Self {
            started: Notify::new(),
            future_dropped: AtomicBool::new(false),
        }
    }
}

struct FutureDropGuard<'a>(&'a AtomicBool);

impl Drop for FutureDropGuard<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl AuthAuthority for StallingRefreshAuthority {
    fn exchange_authorization_code<'a>(
        &'a self,
        _key: &'a DpopKey,
        _code: &'a str,
        _redirect_uri: &'a str,
        _code_verifier: &'a str,
    ) -> BoxFuture<'a, Result<TokenResponse, AuthError>> {
        Box::pin(async { Err(AuthError::Request) })
    }

    fn refresh<'a>(
        &'a self,
        _user_id: &'a str,
        _refresh_token: &'a str,
        _key: &'a DpopKey,
    ) -> BoxFuture<'a, Result<RefreshedCredential, AuthError>> {
        Box::pin(async move {
            let _drop_guard = FutureDropGuard(&self.future_dropped);
            self.started.notify_one();
            std::future::pending().await
        })
    }

    fn revoke<'a>(
        &'a self,
        _refresh_token: &'a str,
        _key: &'a DpopKey,
    ) -> BoxFuture<'a, Result<(), AuthError>> {
        Box::pin(async { Err(AuthError::Request) })
    }
}

pub(crate) fn credential_payload(user_id: &str, refresh_token: &str, key: &DpopKey) -> Bytes {
    serde_json::to_vec(&StoredCredential::Active {
        user_id: user_id.to_owned(),
        refresh_token: refresh_token.to_owned(),
        private_key: key.encode_private(),
    })
    .expect("test credential should encode")
    .into()
}

pub(super) fn stored_active_credential(store: &StallingCredentialStore) -> (String, String) {
    let record: StoredCredential = serde_json::from_slice(
        store
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_deref()
            .expect("an active credential should persist"),
    )
    .expect("the active credential should decode");
    match record {
        StoredCredential::Active {
            user_id,
            refresh_token,
            ..
        } => (user_id, refresh_token),
        StoredCredential::RefreshOutcomeUnknown | StoredCredential::Invalidated => {
            panic!("expected an active credential")
        }
    }
}

pub(super) fn token_response(
    user_id: &str,
    refresh_token: &str,
    access_token: &str,
) -> TokenResponse {
    token_response_with_expiry(
        user_id,
        refresh_token,
        access_token,
        TOKEN_TTL_SECS,
        PlanCode::Free,
    )
}

pub(super) fn token_response_with_plan(
    user_id: &str,
    refresh_token: &str,
    access_token: &str,
    plan_code: PlanCode,
) -> TokenResponse {
    token_response_with_expiry(
        user_id,
        refresh_token,
        access_token,
        TOKEN_TTL_SECS,
        plan_code,
    )
}

pub(super) fn token_response_with_expiry(
    user_id: &str,
    refresh_token: &str,
    access_token: &str,
    expires_in: i64,
    plan_code: PlanCode,
) -> TokenResponse {
    TokenResponse {
        access_token: access_token.to_owned(),
        token_type: "DPoP".to_owned(),
        expires_in,
        refresh_token: refresh_token.to_owned(),
        scope: SCOPE.to_owned(),
        kytrix_user_id: user_id.to_owned(),
        kytrix_wallet_kind: TokenWalletKind::Ethereum,
        kytrix_wallet_address: "0x0000000000000000000000000000000000000000".to_owned(),
        kytrix_plan_code: plan_code,
    }
}

pub(super) fn test_session(
    store: Arc<dyn CredentialStore>,
    revision: u64,
    epoch: u64,
) -> (AuthSession, watch::Receiver<SessionChange>) {
    test_session_with_authority(
        store,
        revision,
        epoch,
        Arc::new(FakeRefreshAuthority::new()),
    )
}

pub(super) fn test_session_with_authority(
    store: Arc<dyn CredentialStore>,
    revision: u64,
    epoch: u64,
    authority: Arc<FakeRefreshAuthority>,
) -> (AuthSession, watch::Receiver<SessionChange>) {
    let (session, receiver, _) =
        test_session_with_authority_and_control(store, revision, epoch, authority);
    (session, receiver)
}

pub(super) fn test_session_with_authority_and_control(
    store: Arc<dyn CredentialStore>,
    revision: u64,
    epoch: u64,
    authority: Arc<FakeRefreshAuthority>,
) -> (
    AuthSession,
    watch::Receiver<SessionChange>,
    Arc<SessionControl>,
) {
    let (changes, receiver) = watch::channel(SessionChange::default());
    let control = Arc::new(SessionControl::new(revision, epoch, changes));
    let session = AuthSession::spawn_with_control(store, control.clone(), authority)
        .expect("the auth actor thread should start");
    (session, receiver, control)
}

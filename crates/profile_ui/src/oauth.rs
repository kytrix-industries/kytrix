//! Signing in: the desktop half of the account service's OAuth authorization.
//!
//! The application never sees the user's credentials. It opens the account service in
//! the system browser, which authorizes the request and sends the browser back to a
//! loopback address this process is listening on. Three properties make that safe, and
//! all three are decided here:
//!
//! * the listener binds an **ephemeral loopback port**, so nothing outside this
//!   machine can reach the callback and the redirect URI is known only to the browser
//!   that was just opened;
//! * the request carries a **single-use `state`** and a **PKCE challenge**, so a
//!   callback that was not produced for this attempt is rejected instead of exchanged;
//! * the exchange happens **on this machine, over the loopback**, with a key that never
//!   leaves the process — the browser only ever carries an authorization code.
//!
//! An attempt is not open forever: it lives for [`ATTEMPT_TTL`], after which the
//! listener is dropped and the port is released. Nothing about a failed or expired
//! attempt is retried; the user asks again.

use std::{
    io,
    net::{Ipv4Addr, TcpListener as StdTcpListener},
    time::Duration,
};

use app_config::settings::AccountConfig;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::{Bytes, BytesMut};
use gpui_kit::{App, SharedString};
use i18n::t;
use kytrix_service_provider::{
    auth::{
        constants::{ACCOUNT_ORIGIN, API_RESOURCE, CLIENT_ID, SCOPE},
        dpop::DpopKey,
        model::AuthError,
    },
    service::connector::KytrixServiceConnector,
};
use rand::TryRng as _;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    time::timeout,
};
use url::Url;
use zeroize::Zeroizing;

use crate::account::account_from_identity;

const RESPONSE_TYPE: &str = "code";
const PKCE_METHOD: &str = "S256";
const CALLBACK_PATH: &str = "/oauth/callback";
const RANDOM_TOKEN_BYTES: usize = 32;

/// How long an attempt stays open.
///
/// It is a session in a browser, not a request: the user may have to sign in, create
/// an account, or approve a wallet before the callback arrives.
pub(crate) const ATTEMPT_TTL: Duration = Duration::from_secs(5 * 60);

/// How long one callback connection may take to send its request line and headers.
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest callback request that is read before it is rejected.
const MAX_CALLBACK_REQUEST_BYTES: usize = 8 * 1024;

/// What the browser shows once the callback was exchanged successfully.
const SUCCESS_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><meta name="referrer" content="no-referrer"><meta name="color-scheme" content="light dark"><title>Kytrix</title><style>body{font-family:system-ui,sans-serif;display:grid;place-items:center;min-height:100vh;margin:0}main{text-align:center;padding:24px}h1{font-size:22px}p{opacity:.72}</style><main><h1>Authentication complete</h1><p>You can return to Kytrix.</p></main>"#;

/// What the browser shows for a callback that was not exchanged.
const FAILURE_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><meta name="referrer" content="no-referrer"><meta name="color-scheme" content="light dark"><title>Kytrix</title><style>body{font-family:system-ui,sans-serif;display:grid;place-items:center;min-height:100vh;margin:0}main{text-align:center;padding:24px}h1{font-size:22px}p{opacity:.72}</style><main><h1>Authentication failed</h1><p>Return to Kytrix and try again.</p></main>"#;

/// A prepared but not yet authorized sign-in.
///
/// It owns the callback listener, so dropping it — a cancelled attempt, an expired
/// one — gives the loopback port back.
pub(crate) struct LoginAttempt {
    listener: StdTcpListener,
    state: String,
    code_verifier: Zeroizing<String>,
    dpop_key: DpopKey,
    redirect_uri: String,
    authorization_url: String,
}

impl LoginAttempt {
    /// The address the user has to open to authorize this attempt.
    pub(crate) fn authorization_url(&self) -> &str {
        &self.authorization_url
    }
}

/// Why a sign-in did not produce an account.
#[derive(Debug, Error)]
pub(crate) enum LoginError {
    #[error("failed to bind the OAuth callback listener")]
    CallbackListener(#[source] io::Error),
    #[error("failed to configure the OAuth callback listener")]
    ConfigureCallback(#[source] io::Error),
    #[error("failed to generate secure OAuth request data")]
    SecureRandom(#[source] rand::rngs::SysError),
    #[error("failed to construct the OAuth authorization URL")]
    AuthorizationUrl(#[source] url::ParseError),
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("failed to accept an OAuth callback")]
    AcceptCallback(#[source] io::Error),
    #[error("failed to read an OAuth callback")]
    ReadCallback(#[source] io::Error),
    #[error("invalid OAuth callback")]
    InvalidCallback,
    #[error("the OAuth attempt expired before it was authorized")]
    Expired,
}

impl LoginError {
    /// The failure as the menu reports it.
    ///
    /// The message is produced where the failure happens, so a failure that is still
    /// on screen when the language changes keeps the words it was reported in.
    pub(crate) fn message(&self, cx: &App) -> SharedString {
        match self {
            Self::CallbackListener(_) | Self::ConfigureCallback(_) => {
                t!(cx, "error.account.login_listener")
            }
            Self::SecureRandom(_) => t!(cx, "error.account.secure_random"),
            Self::AuthorizationUrl(_) => t!(cx, "error.account.route"),
            Self::Auth(error) => service_failure(error, cx),
            Self::AcceptCallback(_) | Self::ReadCallback(_) | Self::InvalidCallback => {
                t!(cx, "error.account.login_callback")
            }
            Self::Expired => t!(cx, "error.account.login_expired"),
        }
    }
}

/// The account service could not do what was asked of it.
///
/// Every variant is named, including the ones that only mean "the answer was lost": an
/// outcome that is unknown must not be reported as a plain failure, because the only
/// honest thing to do about it is to ask again rather than to assume.
pub(crate) fn service_failure(error: &AuthError, cx: &App) -> SharedString {
    match error {
        AuthError::CredentialStore => t!(cx, "error.account.credential_store"),
        AuthError::MissingCredential => t!(cx, "error.account.missing_credential"),
        AuthError::InvalidCredential => t!(cx, "error.account.invalid_credential"),
        AuthError::HttpClient | AuthError::Request => t!(cx, "error.account.request"),
        AuthError::Route => t!(cx, "error.account.route"),
        AuthError::RefreshOutcomeUnknown => t!(cx, "error.account.refresh_unknown"),
        AuthError::Status(status) => t!(cx, "error.account.status", status = status.as_u16()),
        AuthError::InvalidResponse => t!(cx, "error.account.response"),
        AuthError::InvalidGrant => t!(cx, "error.account.invalid_grant"),
        AuthError::SystemTime => t!(cx, "error.account.system_time"),
        AuthError::SecureRandom => t!(cx, "error.account.secure_random"),
        AuthError::SessionClosed => t!(cx, "error.account.session_closed"),
        AuthError::SequenceExhausted => t!(cx, "error.account.sequence_exhausted"),
    }
}

/// Prepares a sign-in and binds its callback listener.
///
/// The returned attempt is inert until its URL is opened: nothing is listening for
/// anything but the one callback it was built for.
pub(crate) fn prepare_login(dpop_key: DpopKey) -> Result<LoginAttempt, LoginError> {
    let listener =
        StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(LoginError::CallbackListener)?;
    listener
        .set_nonblocking(true)
        .map_err(LoginError::ConfigureCallback)?;
    let port = listener
        .local_addr()
        .map_err(LoginError::ConfigureCallback)?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let state = random_token()?;
    let code_verifier = Zeroizing::new(random_token()?);
    let code_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
    let mut authorization_url = Url::parse(ACCOUNT_ORIGIN).map_err(LoginError::AuthorizationUrl)?;
    authorization_url.set_path("/login");
    authorization_url
        .query_pairs_mut()
        .append_pair("mode", "oauth")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", RESPONSE_TYPE)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("state", &state)
        .append_pair("code_challenge", &code_challenge)
        .append_pair("code_challenge_method", PKCE_METHOD)
        .append_pair("resource", API_RESOURCE)
        .append_pair("scope", SCOPE)
        .append_pair("dpop_jkt", &dpop_key.thumbprint());

    Ok(LoginAttempt {
        listener,
        state,
        code_verifier,
        dpop_key,
        redirect_uri,
        authorization_url: authorization_url.into(),
    })
}

/// Serves the callback listener until the attempt is authorized or expires.
///
/// The exchange is a parameter rather than a call so the flow can be driven without an
/// account service: everything decided here — which callbacks are accepted, what the
/// browser is told, when the attempt ends — is the same either way.
pub(crate) async fn run_login<F, Fut>(
    attempt: LoginAttempt,
    exchange: F,
) -> Result<AccountConfig, LoginError>
where
    F: FnOnce(DpopKey, String, String, Zeroizing<String>) -> Fut,
    Fut: Future<Output = Result<AccountConfig, AuthError>>,
{
    let LoginAttempt {
        listener,
        state,
        code_verifier,
        dpop_key,
        redirect_uri,
        ..
    } = attempt;
    let listener = TcpListener::from_std(listener).map_err(LoginError::ConfigureCallback)?;
    let serve = serve_callbacks(
        listener,
        state,
        code_verifier,
        dpop_key,
        redirect_uri,
        exchange,
    );

    match timeout(ATTEMPT_TTL, serve).await {
        Ok(result) => result,
        Err(_) => Err(LoginError::Expired),
    }
}

/// Answers callbacks until one of them is exchanged.
async fn serve_callbacks<F, Fut>(
    listener: TcpListener,
    state: String,
    code_verifier: Zeroizing<String>,
    dpop_key: DpopKey,
    redirect_uri: String,
    exchange: F,
) -> Result<AccountConfig, LoginError>
where
    F: FnOnce(DpopKey, String, String, Zeroizing<String>) -> Fut,
    Fut: Future<Output = Result<AccountConfig, AuthError>>,
{
    let expected_host = redirect_uri
        .strip_prefix("http://")
        .and_then(|value| value.strip_suffix(CALLBACK_PATH))
        .ok_or(LoginError::InvalidCallback)?;

    // Everything that is not this attempt's callback — a port scan, a stale tab, a
    // forged `state` — is answered with a failure page and the listener keeps waiting.
    // Only a callback carrying this attempt's `state` and a well-formed code ends the
    // loop, because that code is about to be exchanged for an account; the connection
    // is held until the exchange says what the browser should be told.
    let (code, mut stream) = loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(LoginError::AcceptCallback)?;
        let request = match timeout(CALLBACK_READ_TIMEOUT, read_callback_request(&mut stream)).await
        {
            Ok(Ok(request)) => request,
            _ => {
                let _ = write_callback_page(&mut stream, false).await;
                continue;
            }
        };
        match parse_callback_request(request.as_ref(), &state, expected_host) {
            Ok(code) => break (code, stream),
            Err(_) => {
                let _ = write_callback_page(&mut stream, false).await;
            }
        }
    };

    match exchange(dpop_key, code, redirect_uri, code_verifier).await {
        Ok(account) => {
            let _ = write_callback_page(&mut stream, true).await;
            Ok(account)
        }
        Err(error) => {
            let _ = write_callback_page(&mut stream, false).await;
            Err(LoginError::Auth(error))
        }
    }
}

/// Calls the account service for the authorization code the browser delivered.
pub(crate) async fn exchange_authorization_code(
    connector: &KytrixServiceConnector,
    dpop_key: DpopKey,
    code: String,
    redirect_uri: String,
    code_verifier: Zeroizing<String>,
) -> Result<AccountConfig, AuthError> {
    let identity = connector
        .exchange_account_authorization_code(&dpop_key, &code, &redirect_uri, &code_verifier)
        .await?;

    Ok(account_from_identity(&identity))
}

async fn read_callback_request(stream: &mut TcpStream) -> Result<Bytes, LoginError> {
    let mut request = BytesMut::with_capacity(1024);
    let mut buffer = [0_u8; 1024];
    loop {
        let read = stream
            .read(&mut buffer)
            .await
            .map_err(LoginError::ReadCallback)?;
        if read == 0 || request.len() + read > MAX_CALLBACK_REQUEST_BYTES {
            return Err(LoginError::InvalidCallback);
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(request.freeze());
        }
    }
}

/// Reads the authorization code out of one callback request.
///
/// The request must be exactly a `GET` for [`CALLBACK_PATH`] addressed to the host this
/// attempt bound, and its query must be exactly `code` and `state`: a request that
/// carries anything else is not the callback that was asked for.
fn parse_callback_request(
    request: &[u8],
    expected_state: &str,
    expected_host: &str,
) -> Result<String, LoginError> {
    if request.len() > MAX_CALLBACK_REQUEST_BYTES {
        return Err(LoginError::InvalidCallback);
    }
    let header_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(LoginError::InvalidCallback)?;
    if request.len() != header_end + 4 {
        return Err(LoginError::InvalidCallback);
    }
    let headers =
        std::str::from_utf8(&request[..header_end]).map_err(|_| LoginError::InvalidCallback)?;
    let mut lines = headers.split("\r\n");
    let mut request_line = lines.next().ok_or(LoginError::InvalidCallback)?.split(' ');
    let (Some("GET"), Some(target), Some("HTTP/1.1"), None) = (
        request_line.next(),
        request_line.next(),
        request_line.next(),
        request_line.next(),
    ) else {
        return Err(LoginError::InvalidCallback);
    };

    let mut host = None;
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(LoginError::InvalidCallback)?;
        if name.eq_ignore_ascii_case("host") && host.replace(value.trim()).is_some() {
            return Err(LoginError::InvalidCallback);
        }
    }
    if host != Some(expected_host) || !target.starts_with('/') {
        return Err(LoginError::InvalidCallback);
    }

    let url = Url::parse(&format!("http://{expected_host}{target}"))
        .map_err(|_| LoginError::InvalidCallback)?;
    if url.path() != CALLBACK_PATH || url.fragment().is_some() {
        return Err(LoginError::InvalidCallback);
    }
    let mut code = None;
    let mut state = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" if code.is_none() => code = Some(value.into_owned()),
            "state" if state.is_none() => state = Some(value.into_owned()),
            _ => return Err(LoginError::InvalidCallback),
        }
    }
    let (Some(code), Some(state)) = (code, state) else {
        return Err(LoginError::InvalidCallback);
    };
    if state != expected_state
        || !is_urlsafe_token(&state, RANDOM_TOKEN_BYTES)
        || !is_urlsafe_token(&code, RANDOM_TOKEN_BYTES)
    {
        return Err(LoginError::InvalidCallback);
    }
    Ok(code)
}

async fn write_callback_page(stream: &mut TcpStream, success: bool) -> io::Result<()> {
    let (status, page) = if success {
        ("200 OK", SUCCESS_PAGE)
    } else {
        ("400 Bad Request", FAILURE_PAGE)
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

fn random_token() -> Result<String, LoginError> {
    let mut bytes = [0_u8; RANDOM_TOKEN_BYTES];
    rand::rngs::SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(LoginError::SecureRandom)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn is_urlsafe_token(value: &str, byte_length: usize) -> bool {
    URL_SAFE_NO_PAD
        .decode(value)
        .is_ok_and(|bytes| bytes.len() == byte_length && URL_SAFE_NO_PAD.encode(bytes) == value)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, time::Duration};

    use app_config::settings::{AccountConfig, AccountWalletKind};
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use kytrix_service_provider::auth::{dpop::DpopKey, model::AuthError};
    use sha2::{Digest as _, Sha256};
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpStream,
        time::timeout,
    };
    use url::Url;

    use super::{
        ACCOUNT_ORIGIN, API_RESOURCE, ATTEMPT_TTL, CALLBACK_PATH, CALLBACK_READ_TIMEOUT, CLIENT_ID,
        LoginAttempt, LoginError, MAX_CALLBACK_REQUEST_BYTES, PKCE_METHOD, RANDOM_TOKEN_BYTES,
        RESPONSE_TYPE, SCOPE, is_urlsafe_token, parse_callback_request, prepare_login, run_login,
    };

    const WALLET: &str = "0x0123456789abcdef0123456789abcdef01234567";

    fn attempt() -> LoginAttempt {
        prepare_login(DpopKey::generate().expect("secure random should be available"))
            .expect("the OAuth attempt should initialize")
    }

    fn callback_host(attempt: &LoginAttempt) -> String {
        attempt
            .redirect_uri
            .strip_prefix("http://")
            .and_then(|value| value.strip_suffix(CALLBACK_PATH))
            .expect("the redirect URI is a loopback callback")
            .to_owned()
    }

    fn callback_request(host: &str, code: &str, state: &str) -> String {
        format!(
            "GET {CALLBACK_PATH}?code={code}&state={state} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
        )
    }

    fn code() -> String {
        URL_SAFE_NO_PAD.encode([2_u8; RANDOM_TOKEN_BYTES])
    }

    fn account() -> AccountConfig {
        AccountConfig {
            user_id: "11111111-1111-4111-8111-111111111111".to_owned(),
            wallet_address: WALLET.to_owned(),
            wallet_kind: AccountWalletKind::Ethereum,
        }
    }

    async fn raw_request(host: &str, request: &str) -> String {
        let mut stream = TcpStream::connect(host)
            .await
            .expect("the callback listener should accept a connection");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("the request should be sent");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("the response should be readable");

        String::from_utf8(response).expect("the callback page should be UTF-8")
    }

    async fn send_callback(host: &str, code: &str, state: &str) -> String {
        raw_request(host, &callback_request(host, code, state)).await
    }

    #[test]
    fn a_prepared_login_should_bind_an_ephemeral_loopback_and_build_a_pkce_url() {
        let attempt = attempt();
        let callback_port = attempt
            .listener
            .local_addr()
            .expect("the callback listener should have an address")
            .port();
        let url = Url::parse(attempt.authorization_url()).expect("the authorization URL parses");
        let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();

        assert_eq!(url.origin().ascii_serialization(), ACCOUNT_ORIGIN);
        assert_eq!(url.path(), "/login");
        assert_eq!(query.get("mode").map(String::as_str), Some("oauth"));
        assert_eq!(query.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(
            query.get("response_type").map(String::as_str),
            Some(RESPONSE_TYPE)
        );
        assert_eq!(
            query.get("redirect_uri").map(String::as_str),
            Some(attempt.redirect_uri.as_str())
        );
        assert_eq!(
            attempt.redirect_uri,
            format!("http://127.0.0.1:{callback_port}{CALLBACK_PATH}")
        );
        assert_eq!(query.get("state"), Some(&attempt.state));
        assert!(is_urlsafe_token(&attempt.state, RANDOM_TOKEN_BYTES));
        assert_eq!(
            query.get("code_challenge").map(String::as_str),
            Some(
                URL_SAFE_NO_PAD
                    .encode(Sha256::digest(attempt.code_verifier.as_bytes()))
                    .as_str()
            )
        );
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some(PKCE_METHOD)
        );
        assert_eq!(
            query.get("resource").map(String::as_str),
            Some(API_RESOURCE)
        );
        assert_eq!(query.get("scope").map(String::as_str), Some(SCOPE));
        assert_eq!(
            query.get("dpop_jkt").map(String::as_str),
            Some(attempt.dpop_key.thumbprint().as_str())
        );
    }

    #[test]
    fn every_attempt_should_get_its_own_state_verifier_and_port() {
        let first = attempt();
        let second = attempt();

        assert_ne!(first.state, second.state);
        assert_ne!(*first.code_verifier, *second.code_verifier);
        assert_ne!(first.redirect_uri, second.redirect_uri);
    }

    #[test]
    fn a_callback_should_require_exact_get_host_path_state_and_code() {
        let state = URL_SAFE_NO_PAD.encode([1_u8; RANDOM_TOKEN_BYTES]);
        let code = URL_SAFE_NO_PAD.encode([2_u8; RANDOM_TOKEN_BYTES]);
        let host = "127.0.0.1:49152";
        let valid = callback_request(host, &code, &state);

        assert_eq!(
            parse_callback_request(valid.as_bytes(), &state, host)
                .expect("a valid callback should parse"),
            code
        );

        for invalid in [
            valid.replace("GET ", "POST "),
            valid.replace(host, "localhost:49152"),
            valid.replace(CALLBACK_PATH, "/other"),
            valid.replace(&state, &URL_SAFE_NO_PAD.encode([3_u8; RANDOM_TOKEN_BYTES])),
            valid.replace(
                &format!("state={state}"),
                &format!("state={state}&extra=true"),
            ),
            valid.replace(
                &format!("state={state}"),
                &format!("state={state}&state={state}"),
            ),
            valid.replace(&format!("code={code}"), "code=short"),
        ] {
            assert!(
                parse_callback_request(invalid.as_bytes(), &state, host).is_err(),
                "{invalid}"
            );
        }
        assert!(
            parse_callback_request(&vec![b'a'; MAX_CALLBACK_REQUEST_BYTES + 1], &state, host)
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_attempt_that_is_never_authorized_should_expire() {
        let attempt = attempt();
        let host = callback_host(&attempt);

        let result = run_login(attempt, |_, _, _, _| async {
            panic!("an expired attempt must not exchange anything")
        })
        .await;

        assert!(matches!(result, Err(LoginError::Expired)), "{result:?}");
        assert!(
            TcpStream::connect(&host).await.is_err(),
            "an expired attempt must release its loopback port"
        );
    }

    #[tokio::test]
    async fn a_callback_should_be_exchanged_and_answered() {
        let attempt = attempt();
        let host = callback_host(&attempt);
        let exchange_host = host.clone();
        let state = attempt.state.clone();
        let code = code();
        let expected = code.clone();
        let login = tokio::spawn(run_login(
            attempt,
            move |_, code, redirect_uri, verifier| {
                let expected = expected.clone();
                async move {
                    assert_eq!(code, expected);
                    assert_eq!(
                        redirect_uri,
                        format!("http://{exchange_host}{CALLBACK_PATH}")
                    );
                    assert_eq!(verifier.len(), URL_SAFE_NO_PAD.encode([0_u8; 32]).len());
                    Ok(account())
                }
            },
        ));

        let page = send_callback(&host, &code, &state).await;

        assert!(page.starts_with("HTTP/1.1 200 OK"), "{page}");
        let account = login
            .await
            .expect("the login task should not panic")
            .expect("a valid callback should be exchanged");
        assert_eq!(account.wallet_address, WALLET);
    }

    #[tokio::test]
    async fn a_forged_state_should_be_refused_and_the_attempt_should_keep_waiting() {
        let attempt = attempt();
        let host = callback_host(&attempt);
        let state = attempt.state.clone();
        let code = code();
        let forged = URL_SAFE_NO_PAD.encode([3_u8; RANDOM_TOKEN_BYTES]);
        let login = tokio::spawn(run_login(attempt, move |_, _, _, _| async {
            Ok(account())
        }));

        let refused = send_callback(&host, &code, &forged).await;
        assert!(refused.starts_with("HTTP/1.1 400 Bad Request"), "{refused}");
        assert!(
            !login.is_finished(),
            "a refused callback must not end the attempt"
        );

        let accepted = send_callback(&host, &code, &state).await;
        assert!(accepted.starts_with("HTTP/1.1 200 OK"), "{accepted}");
        login
            .await
            .expect("the login task should not panic")
            .expect("the real callback should be exchanged");
    }

    #[tokio::test]
    async fn an_exchange_that_fails_should_tell_the_browser_and_report_the_failure() {
        let attempt = attempt();
        let host = callback_host(&attempt);
        let state = attempt.state.clone();
        let code = code();
        let login = tokio::spawn(run_login(attempt, |_, _, _, _| async {
            Err(AuthError::InvalidGrant)
        }));

        let page = send_callback(&host, &code, &state).await;

        assert!(page.starts_with("HTTP/1.1 400 Bad Request"), "{page}");
        assert!(matches!(
            login.await.expect("the login task should not panic"),
            Err(LoginError::Auth(AuthError::InvalidGrant))
        ));
    }

    #[tokio::test]
    async fn a_request_that_is_not_a_callback_should_be_refused() {
        let attempt = attempt();
        let host = callback_host(&attempt);
        let login = tokio::spawn(run_login(attempt, |_, _, _, _| async { Ok(account()) }));

        let request = format!("GET /other HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        let page = raw_request(&host, &request).await;

        assert!(page.starts_with("HTTP/1.1 400 Bad Request"), "{page}");
        assert!(!login.is_finished(), "the attempt should still be waiting");
        login.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_callback_that_never_finishes_its_request_should_not_stall_the_attempt() {
        let attempt = attempt();
        let host = callback_host(&attempt);
        let state = attempt.state.clone();
        let code = code();
        let login = tokio::spawn(run_login(attempt, |_, _, _, _| async { Ok(account()) }));

        let mut stalled = TcpStream::connect(&host)
            .await
            .expect("the callback listener should accept a connection");
        stalled
            .write_all(b"GET /oauth/callback?code=")
            .await
            .expect("the connection should accept a partial request");
        let mut response = Vec::new();
        timeout(
            CALLBACK_READ_TIMEOUT + Duration::from_secs(5),
            stalled.read_to_end(&mut response),
        )
        .await
        .expect("a stalled callback should be answered once its read times out")
        .expect("the response should be readable");
        assert!(
            String::from_utf8_lossy(&response).starts_with("HTTP/1.1 400 Bad Request"),
            "{}",
            String::from_utf8_lossy(&response)
        );

        let accepted = send_callback(&host, &code, &state).await;
        assert!(accepted.starts_with("HTTP/1.1 200 OK"), "{accepted}");
        login
            .await
            .expect("the login task should not panic")
            .expect("a stalled connection must not consume the attempt");
    }

    #[tokio::test(start_paused = true)]
    async fn an_attempt_should_not_outlive_its_time_to_live() {
        let attempt = attempt();
        let host = callback_host(&attempt);
        let login = tokio::spawn(run_login(attempt, |_, _, _, _| async { Ok(account()) }));
        let until_expiry = ATTEMPT_TTL - Duration::from_secs(1);
        tokio::time::sleep(until_expiry).await;

        assert!(!login.is_finished(), "the attempt should still be waiting");

        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(matches!(
            login.await.expect("the login task should not panic"),
            Err(LoginError::Expired)
        ));
        assert!(TcpStream::connect(&host).await.is_err());
    }
}

//! HTTP client pool and bounded response reading.

use std::{sync::Mutex, time::Duration};

use app_config::network::ResolvedProxy;
use bytes::{Bytes, BytesMut};
use reqwest::{Client, Response};
use tokio_util::sync::CancellationToken;

use crate::transport::{
    cache::InsertionOrderedCache,
    client::{self, ClientSpec},
    error::TransportError,
    hub::TransportPolicy,
};

const MIB: usize = 1024 * 1024;
const STANDARD_RESPONSE_BODY_LIMIT: usize = 30 * MIB;
const MAX_HTTP_CLIENTS: usize = 64;

/// A fixed HTTP policy for one class of network operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HttpClientClass {
    Public,
    Private,
    KytrixService,
    XMedia,
    UpdaterMetadata,
    UpdaterDownload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HttpClientProfile {
    connect_timeout: Duration,
    request_timeout: Option<Duration>,
    redirect_limit: usize,
    response_body_limit: usize,
    user_agent: &'static str,
}

impl HttpClientClass {
    fn profile(self) -> HttpClientProfile {
        match self {
            Self::Public => HttpClientProfile {
                connect_timeout: Duration::from_secs(10),
                request_timeout: Some(Duration::from_secs(15)),
                redirect_limit: 0,
                response_body_limit: STANDARD_RESPONSE_BODY_LIMIT,
                user_agent: concat!("Kytrix/", env!("CARGO_PKG_VERSION")),
            },
            Self::Private => HttpClientProfile {
                connect_timeout: Duration::from_secs(10),
                request_timeout: Some(Duration::from_secs(15)),
                redirect_limit: 0,
                response_body_limit: STANDARD_RESPONSE_BODY_LIMIT,
                user_agent: concat!("Kytrix/", env!("CARGO_PKG_VERSION")),
            },
            Self::KytrixService => HttpClientProfile {
                connect_timeout: Duration::from_secs(10),
                request_timeout: Some(Duration::from_secs(15)),
                redirect_limit: 0,
                response_body_limit: STANDARD_RESPONSE_BODY_LIMIT,
                user_agent: concat!("Kytrix/", env!("CARGO_PKG_VERSION")),
            },
            Self::XMedia => HttpClientProfile {
                connect_timeout: Duration::from_secs(10),
                request_timeout: Some(Duration::from_secs(30)),
                redirect_limit: 5,
                response_body_limit: 64 * MIB,
                user_agent: concat!("Kytrix/", env!("CARGO_PKG_VERSION")),
            },
            Self::UpdaterMetadata => HttpClientProfile {
                connect_timeout: Duration::from_secs(10),
                request_timeout: Some(Duration::from_secs(30)),
                redirect_limit: 10,
                // Not a small budget: this is the whole of `latest.json`, which the
                // release job publishes with both `release_notes/vX.Y.Z/{en,zh-CN}.md`
                // files as its `notes` plus one entry per platform. 4 KiB was smaller
                // than the metadata this pipeline actually writes: the assembled
                // v0.1.0 file measured 5294 bytes with one language, and every check
                // would have failed with `ResponseBodyTooLarge` — silently, because a
                // failed check only retries. The release job asserts the assembled
                // file stays under this number.
                response_body_limit: 64 * 1024,
                user_agent: concat!("Kytrix/", env!("CARGO_PKG_VERSION")),
            },
            Self::UpdaterDownload => HttpClientProfile {
                connect_timeout: Duration::from_secs(5),
                request_timeout: None,
                redirect_limit: 10,
                response_body_limit: 1024 * MIB,
                user_agent: concat!("Kytrix/", env!("CARGO_PKG_VERSION")),
            },
        }
    }

    /// Maximum decompressed response bytes accepted by [`read_response_body`].
    pub fn response_body_limit(self) -> usize {
        self.profile().response_body_limit
    }

    /// Hop budget for callers that resolve redirects themselves.
    pub fn redirect_limit(self) -> usize {
        self.profile().redirect_limit
    }
}

/// A proxy- and policy-isolated pool of HTTP clients.
pub struct HttpClientPool {
    clients: Mutex<InsertionOrderedCache<(ResolvedProxy, HttpClientClass), Client>>,
    policy: TransportPolicy,
}

impl HttpClientPool {
    pub fn new(policy: TransportPolicy) -> Self {
        Self {
            clients: Mutex::new(InsertionOrderedCache::new(MAX_HTTP_CLIENTS)),
            policy,
        }
    }

    /// Returns a client for the exact resolved route and operation class.
    ///
    /// Environment and system proxies stay disabled, and reqwest's implicit
    /// retries are off. A bad route fails closed: it is never replaced with a
    /// direct client.
    pub fn client(
        &self,
        route: &ResolvedProxy,
        class: HttpClientClass,
    ) -> Result<Client, TransportError> {
        let key = (route.clone(), class);
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| TransportError::ClientPoolUnavailable)?;
        if let Some(client) = clients.get(&key) {
            return Ok(client.clone());
        }
        let client = self.build(route, class)?;
        clients.insert(key, client.clone());
        Ok(client)
    }

    fn build(
        &self,
        route: &ResolvedProxy,
        class: HttpClientClass,
    ) -> Result<Client, TransportError> {
        let profile = class.profile();
        client::build(
            route,
            ClientSpec {
                connect_timeout: profile.connect_timeout,
                request_timeout: profile.request_timeout,
                user_agent: Some(profile.user_agent),
                default_headers: None,
                tcp_nodelay: false,
                http1_only: false,
            },
            !self.policy.allow_plaintext,
            &self.policy.extra_root_certificates,
        )
    }

    /// Number of cached clients, for callers asserting cache keying.
    #[cfg(feature = "test-support")]
    pub fn client_count(&self) -> usize {
        self.clients
            .lock()
            .map(|clients| clients.len())
            .unwrap_or_default()
    }

    /// Whether a client is cached for the exact route and class.
    #[cfg(feature = "test-support")]
    pub fn contains(&self, route: &ResolvedProxy, class: HttpClientClass) -> bool {
        self.clients
            .lock()
            .is_ok_and(|clients| clients.contains_key(&(route.clone(), class)))
    }

    /// Seeds a client so a caller can exercise a transport it cannot build.
    #[cfg(feature = "test-support")]
    pub fn insert_for_test(&self, route: ResolvedProxy, class: HttpClientClass, client: Client) {
        if let Ok(mut clients) = self.clients.lock() {
            clients.insert((route, class), client);
        }
    }
}

/// Reads a response with the selected class's decompressed body limit, reporting
/// the accepted bytes as they arrive.
///
/// `on_chunk` receives `(bytes_read, total)`:
///
/// * `bytes_read` is the cumulative number of decompressed body bytes accepted
///   so far, so it never decreases.
/// * `total` is the exact decoded body size the response exposes — its
///   `Content-Length` — when that is known, and `None` when it is not: a chunked
///   response, or a response reqwest decodes, whose decoded length cannot be
///   known up front. It is read once, before the first byte, and is the same
///   value on every call.
///
/// The callback is invoked in this order, on the task driving the read:
///
/// 1. Exactly one `(0, total)` call, after both checks below have passed and
///    before any body byte is read. A caller therefore knows the total as soon
///    as the response headers are available and can show a determinate bar
///    without waiting for the first byte; with `total` of `None` the same call
///    still announces that the read has started.
/// 2. One call per chunk returned by the response, after that chunk passes the
///    limit check and is appended, with `bytes_read` including it.
///
/// A successful read of a non-empty body always produces at least one call
/// after the `(0, total)` call, and its final call reports the full body
/// length, matching the returned [`Bytes`]. The callback must not block.
///
/// Nothing is reported before the first read. An already-cancelled `cancellation`
/// returns [`TransportError::Cancelled`] and a `Content-Length` above the class
/// limit returns [`TransportError::ResponseBodyTooLarge`] without any call. A
/// chunk that would push the body past the limit is rejected without being
/// reported, so a reported count never exceeds the limit and the error leaves
/// the accepted bytes reported exactly as far as they were accumulated. The
/// same holds for [`TransportError::Cancelled`] and [`TransportError::ResponseReadFailed`]
/// raised after some chunks were accepted.
pub async fn read_response_body_with_progress(
    mut response: Response,
    class: HttpClientClass,
    cancellation: &CancellationToken,
    on_chunk: impl Fn(u64, Option<u64>),
) -> Result<Bytes, TransportError> {
    let limit = class.response_body_limit();
    if cancellation.is_cancelled() {
        return Err(TransportError::Cancelled);
    }
    let total = response.content_length();
    if total.is_some_and(|length| length > limit as u64) {
        return Err(TransportError::ResponseBodyTooLarge { limit });
    }
    on_chunk(0, total);

    let mut body = BytesMut::with_capacity(limit.min(64 * 1024));
    loop {
        let chunk = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(TransportError::Cancelled),
            chunk = response.chunk() => chunk.map_err(classify_response_error)?,
        };
        let Some(chunk) = chunk else {
            return Ok(body.freeze());
        };
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(TransportError::ResponseBodyTooLarge { limit });
        }
        body.extend_from_slice(&chunk);
        on_chunk(body.len() as u64, total);
    }
}

/// Reads a response with the selected class's decompressed body limit.
///
/// Reports no progress; the call sequence and the failure behaviour are those of
/// [`read_response_body_with_progress`].
pub async fn read_response_body(
    response: Response,
    class: HttpClientClass,
    cancellation: &CancellationToken,
) -> Result<Bytes, TransportError> {
    read_response_body_with_progress(response, class, cancellation, |_, _| {}).await
}

fn classify_response_error(error: reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::RequestTimedOut
    } else {
        TransportError::ResponseReadFailed
    }
}

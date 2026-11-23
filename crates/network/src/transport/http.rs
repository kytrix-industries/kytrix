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
#[cfg(target_pointer_width = "64")]
const MODEL_DOWNLOAD_RESPONSE_BODY_LIMIT: usize = 16 * 1024 * MIB;
#[cfg(not(target_pointer_width = "64"))]
const MODEL_DOWNLOAD_RESPONSE_BODY_LIMIT: usize = usize::MAX;
const MAX_HTTP_CLIENTS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HttpClientClass {
    Public,
    Private,
    KytrixService,
    XMedia,
    UpdaterMetadata,
    UpdaterDownload,
    ModelDownload,
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
            Self::ModelDownload => HttpClientProfile {
                connect_timeout: Duration::from_secs(10),
                request_timeout: None,
                redirect_limit: 10,
                response_body_limit: MODEL_DOWNLOAD_RESPONSE_BODY_LIMIT,
                user_agent: concat!("Kytrix/", env!("CARGO_PKG_VERSION")),
            },
        }
    }

    pub fn response_body_limit(self) -> usize {
        self.profile().response_body_limit
    }

    pub fn redirect_limit(self) -> usize {
        self.profile().redirect_limit
    }
}

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

    #[cfg(feature = "test-support")]
    pub fn client_count(&self) -> usize {
        self.clients
            .lock()
            .map(|clients| clients.len())
            .unwrap_or_default()
    }

    #[cfg(feature = "test-support")]
    pub fn contains(&self, route: &ResolvedProxy, class: HttpClientClass) -> bool {
        self.clients
            .lock()
            .is_ok_and(|clients| clients.contains_key(&(route.clone(), class)))
    }

    #[cfg(feature = "test-support")]
    pub fn insert_for_test(&self, route: ResolvedProxy, class: HttpClientClass, client: Client) {
        if let Ok(mut clients) = self.clients.lock() {
            clients.insert((route, class), client);
        }
    }
}

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

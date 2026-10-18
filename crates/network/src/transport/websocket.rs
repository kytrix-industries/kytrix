//! WebSocket sessions dialled through an HTTP client bound to a resolved route.

use std::time::Duration;

use app_config::network::ResolvedProxy;
use futures::SinkExt;
use reqwest::{Client, header::HeaderMap};
use smol_str::{SmolStr, format_smolstr};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use url::Url;
use yawc::{Frame, HttpWebSocket, Options, WebSocketError};

use crate::transport::{
    client::{self, ClientSpec},
    error::{TransportError, UpgradeFailure},
    hub::TransportPolicy,
};

const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// The client one upgrade runs on.
///
/// It is pinned to HTTP/1.1 because RFC 6455 upgrades an HTTP/1.1 connection: a
/// server reached over `h2` has no `Upgrade` handshake to answer with, so it resets
/// the stream instead of switching protocols. Which version a reqwest client offers
/// in ALPN otherwise follows feature unification — anything in the binary enabling
/// `reqwest/http2` switches every client in the process to offering `h2` — so the
/// pin is what keeps the handshake independent of unrelated crates.
pub(super) fn upgrade_client_spec(headers: HeaderMap) -> ClientSpec {
    ClientSpec {
        connect_timeout: CONNECT_TIMEOUT,
        request_timeout: None,
        user_agent: None,
        default_headers: Some(headers),
        tcp_nodelay: true,
        http1_only: true,
    }
}

/// Classifies what a failed handshake reported.
///
/// Only what the error itself proves is classified: a timeout, the status a peer
/// answered with, and the handshake shapes yawc rejects by name. Everything else is
/// reported with the chain that says which leg broke.
pub(super) fn upgrade_failure(error: &WebSocketError) -> UpgradeFailure {
    match error {
        WebSocketError::Reqwest(error) if error.is_timeout() => UpgradeFailure::Timeout,
        WebSocketError::Reqwest(error) => UpgradeFailure::Transport {
            detail: error_chain(error),
        },
        WebSocketError::InvalidStatusCode(status) => UpgradeFailure::Rejected { status: *status },
        WebSocketError::Redirected { status_code, .. } => UpgradeFailure::Rejected {
            status: *status_code,
        },
        other => UpgradeFailure::Handshake {
            detail: format_smolstr!("{other}"),
        },
    }
}

/// Renders an error and everything that caused it as one line.
///
/// The part of a transport failure worth reading is usually its cause: the outermost
/// error only says that a request failed, and the chain below it — a proxy refusal, a
/// TLS alert, an HTTP/2 reset — says which leg did.
fn error_chain(error: &(dyn std::error::Error + 'static)) -> SmolStr {
    let mut rendered = SmolStr::new(error.to_string());
    let mut source = error.source();
    while let Some(current) = source {
        rendered = format_smolstr!("{rendered}: {current}");
        source = current.source();
    }
    rendered
}

/// Stable identity for a physical WebSocket endpoint.
///
/// The name describes the socket a session was opened on: the configured origin
/// it dialed, spelled as it was dialed, plus the protocol family it carries.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WsEndpointId {
    pub protocol_family: SmolStr,
    pub host: SmolStr,
    pub port: u16,
    pub path: SmolStr,
    pub region: Option<SmolStr>,
    pub access: EndpointAccess,
}

/// Whether a WebSocket endpoint transports public or authenticated traffic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EndpointAccess {
    Public,
    Private,
}

/// The exact routing identity used to share one physical WebSocket session.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WsSessionKey {
    pub endpoint: WsEndpointId,
    pub proxy: ResolvedProxy,
    /// OS proxy settings hash for System routes; `0` for Direct/Named.
    pub os_proxy_fingerprint: u64,
}

/// One WebSocket session with cancellation- and timeout-bounded I/O.
pub struct RawWebSocket {
    inner: HttpWebSocket,
    /// The client that performed the upgrade is kept alive for as long as the
    /// upgraded connection runs, so the session never outlives its transport.
    _client: Client,
    cancellation: CancellationToken,
}

impl RawWebSocket {
    /// Dials `url` through `route`, sending `headers` with the upgrade request.
    pub async fn connect(
        url: &Url,
        route: &ResolvedProxy,
        headers: HeaderMap,
        cancellation: &CancellationToken,
        policy: &TransportPolicy,
    ) -> Result<Self, TransportError> {
        let use_tls = match url.scheme() {
            "ws" => false,
            "wss" => true,
            _ => return Err(TransportError::InvalidWebSocketEndpoint),
        };
        if !use_tls && !policy.allow_plaintext {
            return Err(TransportError::InvalidWebSocketEndpoint);
        }
        let client = client::build(
            route,
            upgrade_client_spec(headers),
            !policy.allow_plaintext,
            &policy.extra_root_certificates,
        )?;
        // `permessage-deflate` stays at yawc's low-latency preset: market and
        // account streams are latency-sensitive, not bandwidth-bound.
        let options = Options::default()
            .with_max_payload_read(MAX_FRAME_BYTES)
            .with_max_fragment_size(64 * 1024)
            .with_low_latency_compression();
        let inner = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(TransportError::Cancelled),
            result = timeout(
                CONNECT_TIMEOUT,
                HttpWebSocket::reqwest(url.clone(), client.clone(), options),
            ) => match result {
                Ok(Ok(websocket)) => websocket,
                Ok(Err(error)) => {
                    return Err(TransportError::WebSocketConnect {
                        failure: upgrade_failure(&error),
                    });
                }
                Err(_) => {
                    return Err(TransportError::WebSocketConnect {
                        failure: UpgradeFailure::Timeout,
                    });
                }
            },
        };
        Ok(Self {
            inner,
            _client: client,
            cancellation: cancellation.clone(),
        })
    }

    pub async fn send(&mut self, frame: Frame) -> Result<(), TransportError> {
        tokio::select! {
            biased;
            () = self.cancellation.cancelled() => Err(TransportError::Cancelled),
            result = timeout(IO_TIMEOUT, self.inner.send(frame)) => result
                .map_err(|_| TransportError::WebSocketIo)?
                .map_err(|_| TransportError::WebSocketIo),
        }
    }

    pub async fn receive(&mut self) -> Result<Frame, TransportError> {
        tokio::select! {
            biased;
            () = self.cancellation.cancelled() => Err(TransportError::Cancelled),
            result = self.inner.next_frame() => result.map_err(|_| TransportError::WebSocketIo),
        }
    }

    /// Reads the next frame with the caller's own timeout and cancellation.
    pub async fn next_frame(&mut self) -> Result<Frame, WebSocketError> {
        self.inner.next_frame().await
    }

    pub async fn close(&mut self) -> Result<(), TransportError> {
        tokio::select! {
            biased;
            () = self.cancellation.cancelled() => Err(TransportError::Cancelled),
            result = timeout(CLOSE_TIMEOUT, self.inner.close()) => result
                .map_err(|_| TransportError::WebSocketIo)?
                .map_err(|_| TransportError::WebSocketIo),
        }
    }
}

impl futures::Sink<Frame> for RawWebSocket {
    type Error = WebSocketError;

    fn poll_ready(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut self.inner).poll_ready(context)
    }

    fn start_send(mut self: std::pin::Pin<&mut Self>, frame: Frame) -> Result<(), Self::Error> {
        std::pin::Pin::new(&mut self.inner).start_send(frame)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut self.inner).poll_close(context)
    }
}

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

fn error_chain(error: &(dyn std::error::Error + 'static)) -> SmolStr {
    let mut rendered = SmolStr::new(error.to_string());
    let mut source = error.source();
    while let Some(current) = source {
        rendered = format_smolstr!("{rendered}: {current}");
        source = current.source();
    }
    rendered
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WsEndpointId {
    pub protocol_family: SmolStr,
    pub host: SmolStr,
    pub port: u16,
    pub path: SmolStr,
    pub region: Option<SmolStr>,
    pub access: EndpointAccess,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EndpointAccess {
    Public,
    Private,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WsSessionKey {
    pub endpoint: WsEndpointId,
    pub proxy: ResolvedProxy,
    pub os_proxy_fingerprint: u64,
}

pub struct RawWebSocket {
    inner: HttpWebSocket,
    _client: Client,
    cancellation: CancellationToken,
}

impl RawWebSocket {
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

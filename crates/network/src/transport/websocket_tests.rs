use std::{net::Ipv4Addr, time::Duration};

use app_config::network::ResolvedProxy;
use bytes::Bytes;
use reqwest::header::HeaderMap;
use smol_str::SmolStr;
use tokio::{io::AsyncReadExt, net::TcpListener, time::timeout_at};
use tokio_util::sync::CancellationToken;

use crate::transport::{
    error::{TransportError, UpgradeFailure},
    hub::TransportPolicy,
    websocket::{
        EndpointAccess, RawWebSocket, WsEndpointId, WsSessionKey, upgrade_client_spec,
        upgrade_failure,
    },
};

/// An upgrade is an HTTP/1.1 exchange, and the version a reqwest client offers in
/// ALPN is decided by feature unification: a dependency enabling `reqwest/http2`
/// anywhere in the binary makes every client offer `h2`, and a server that takes it
/// answers a reset stream instead of `101`. Nothing about the default test build
/// would notice the pin going missing, so the invariant is asserted here.
#[test]
fn an_upgrade_client_never_offers_http2() {
    assert!(upgrade_client_spec(HeaderMap::new()).http1_only);
}

/// Serves one HTTP response for the upgrade request this test sends.
async fn spawn_upgrade_answer(response: &'static str) -> std::net::SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let mut request = Vec::new();
        loop {
            if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                break;
            }
            let mut chunk = [0_u8; 256];
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(count) => request.extend_from_slice(&chunk[..count]),
            }
        }
        let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
    });
    address
}

async fn dial(address: std::net::SocketAddr) -> TransportError {
    let url = url::Url::parse(&format!("ws://{address}/ws")).unwrap();
    RawWebSocket::connect(
        &url,
        &ResolvedProxy::NoProxy,
        HeaderMap::new(),
        &CancellationToken::new(),
        &TransportPolicy {
            allow_plaintext: true,
            extra_root_certificates: Vec::new(),
        },
    )
    .await
    .err()
    .expect("the upgrade must fail")
}

/// An answer that is not a protocol switch is the peer's decision, and the status is
/// what tells a rejected token from a rejected route.
#[tokio::test]
async fn an_upgrade_the_peer_refuses_reports_the_status() {
    let address = spawn_upgrade_answer(
        "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;

    assert_eq!(
        dial(address).await,
        TransportError::WebSocketConnect {
            failure: UpgradeFailure::Rejected { status: 403 }
        }
    );
}

/// A connection that dies before it answers says nothing about the peer, so the
/// cause has to come from the transport chain.
#[tokio::test]
async fn a_transport_that_breaks_before_answering_reports_the_chain() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = listener.accept().await;
    });

    let TransportError::WebSocketConnect {
        failure: UpgradeFailure::Transport { detail },
    } = dial(address).await
    else {
        panic!("a closed connection is a transport failure");
    };
    assert!(!detail.is_empty(), "{detail}");

    server.abort();
    let _ = server.await;
}

/// The connect budget is the one failure worth repeating exactly as it stands.
#[tokio::test]
async fn an_attempt_that_runs_out_of_time_reports_a_timeout() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        // Hold the connection open without ever answering.
        let _held = stream;
        std::future::pending::<()>().await;
    });

    let request = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(50))
        .build()
        .unwrap();
    let error = timeout_at(
        tokio::time::Instant::now() + Duration::from_secs(5),
        request.get(format!("http://{address}/slow")).send(),
    )
    .await
    .expect("the request timeout must fire")
    .unwrap_err();
    assert!(error.is_timeout(), "{error:?}");

    assert_eq!(
        upgrade_failure(&yawc::WebSocketError::Reqwest(error)),
        UpgradeFailure::Timeout
    );

    server.abort();
    let _ = server.await;
}

/// yawc names the handshake shapes it will not start a session from.
#[test]
fn a_completed_handshake_that_cannot_start_a_session_is_named() {
    assert_eq!(
        upgrade_failure(&yawc::WebSocketError::InvalidUpgradeHeader),
        UpgradeFailure::Handshake {
            detail: SmolStr::new_static("Invalid upgrade header")
        }
    );
}

/// A redirect is the peer's answer, not a broken transport.
#[test]
fn a_redirected_upgrade_reports_the_status() {
    assert_eq!(
        upgrade_failure(&yawc::WebSocketError::Redirected {
            status_code: 302,
            location: "https://elsewhere.test/ws".to_owned(),
        }),
        UpgradeFailure::Rejected { status: 302 }
    );
}

#[test]
fn yawc_frames_use_shared_bytes_payloads() {
    let payload = Bytes::from_static(b"shared");
    let frame = yawc::Frame::binary(payload.clone());
    let received: Bytes = frame.payload().clone();
    assert_eq!(received, payload);
    let second = frame.payload().clone();
    assert_eq!(second, received);
}

#[test]
fn websocket_session_key_should_include_every_transport_dimension() {
    let endpoint = WsEndpointId {
        protocol_family: SmolStr::new_static("market-data-primary"),
        host: SmolStr::new_static("stream.example.test"),
        port: 443,
        path: SmolStr::new_static("/ws"),
        region: None,
        access: EndpointAccess::Public,
    };
    let base = WsSessionKey {
        endpoint,
        proxy: ResolvedProxy::NoProxy,
        os_proxy_fingerprint: 0,
    };
    let mut variants = Vec::new();
    let mut changed = base.clone();
    changed.endpoint.protocol_family = SmolStr::new_static("market-data-secondary");
    variants.push(changed);
    let mut changed = base.clone();
    changed.endpoint.host = SmolStr::new_static("alternate.example.test");
    variants.push(changed);
    let mut changed = base.clone();
    changed.endpoint.port = 8443;
    variants.push(changed);
    let mut changed = base.clone();
    changed.endpoint.path = SmolStr::new_static("/stream");
    variants.push(changed);
    let mut changed = base.clone();
    changed.endpoint.region = Some(SmolStr::new_static("us"));
    variants.push(changed);
    let mut changed = base.clone();
    changed.endpoint.access = EndpointAccess::Private;
    variants.push(changed);
    let mut changed = base.clone();
    changed.proxy = ResolvedProxy::Socks5 {
        host: SmolStr::new_static("127.0.0.1"),
        port: 1080,
        credentials: None,
        resolve_dns_at_remote: true,
    };
    variants.push(changed);
    let mut fingerprint = base.clone();
    fingerprint.os_proxy_fingerprint = 7;
    variants.push(fingerprint);

    for changed in variants {
        assert_ne!(base, changed);
    }
}

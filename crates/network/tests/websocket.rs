//! End-to-end WebSocket session against a real upgrade server.

use std::{net::Ipv4Addr, time::Duration};

use app_config::network::ResolvedProxy;
use futures::{SinkExt, StreamExt};
use hyper::{Request, body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use network::transport::hub::{TransportHub, TransportPolicy};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use tokio::{net::TcpListener, sync::mpsc};
use url::Url;

/// Starts a server that reports the handshake `Authorization` header and then
/// echoes every data frame it receives.
async fn spawn_echo_server() -> (std::net::SocketAddr, mpsc::UnboundedReceiver<String>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let (observed_sender, observed) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let service = service_fn(move |mut request: Request<Incoming>| {
            let observed_sender = observed_sender.clone();
            async move {
                let authorization = request
                    .headers()
                    .get(AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                let (response, upgrade) = yawc::HttpWebSocket::upgrade(&mut request)
                    .expect("the request is a valid upgrade");
                tokio::spawn(async move {
                    let Ok(mut socket) = upgrade.await else {
                        return;
                    };
                    while let Some(frame) = socket.next().await {
                        if frame.opcode().is_control() {
                            continue;
                        }
                        if socket.send(frame).await.is_err() {
                            return;
                        }
                    }
                });
                let _ = observed_sender.send(authorization);
                Ok::<_, hyper::Error>(response)
            }
        });
        let _ = http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .with_upgrades()
            .await;
    });
    (address, observed)
}

#[tokio::test]
async fn a_session_sends_its_handshake_headers_and_round_trips_frames() {
    let (address, mut observed) = spawn_echo_server().await;
    let hub = TransportHub::new(TransportPolicy {
        allow_plaintext: true,
        extra_root_certificates: Vec::new(),
    });
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer handshake-token"),
    );
    let url = Url::parse(&format!("ws://{address}/stream")).unwrap();

    let mut socket = hub
        .connect_websocket(
            &url,
            &ResolvedProxy::NoProxy,
            headers,
            &hub.cancellation().child_token(),
        )
        .await
        .expect("the session connects");

    assert_eq!(
        observed.recv().await.as_deref(),
        Some("Bearer handshake-token")
    );

    socket
        .send(yawc::Frame::text("hello"))
        .await
        .expect("the frame is written");
    let echoed = tokio::time::timeout(Duration::from_secs(5), socket.receive())
        .await
        .expect("the echo arrives")
        .expect("the echo is a frame");

    assert_eq!(echoed.as_str(), "hello");
    socket.close().await.expect("the session closes");
}

#[tokio::test]
async fn a_cancelled_session_stops_reading() {
    let (address, _observed) = spawn_echo_server().await;
    let hub = TransportHub::new(TransportPolicy {
        allow_plaintext: true,
        extra_root_certificates: Vec::new(),
    });
    let cancellation = hub.cancellation().child_token();
    let url = Url::parse(&format!("ws://{address}/stream")).unwrap();
    let mut socket = hub
        .connect_websocket(
            &url,
            &ResolvedProxy::NoProxy,
            HeaderMap::new(),
            &cancellation,
        )
        .await
        .expect("the session connects");

    cancellation.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), socket.receive())
        .await
        .expect("a cancelled session returns immediately");

    assert!(matches!(
        result,
        Err(network::transport::error::TransportError::Cancelled)
    ));
}

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use app_config::network::ResolvedProxy;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use hyper::{
    Request,
    body::Incoming,
    header::{HeaderMap, HeaderName},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use network::transport::hub::{TransportHub, TransportPolicy};
use serde::Deserialize;
use tokio::{
    net::TcpListener,
    sync::mpsc,
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use url::Url;
use yawc::{Frame, OpCode, WebSocket};

use super::{
    Connection, ConnectionKey, ConnectionState, Keepalive, Reauthentication,
    ReauthenticationRequest, ReconnectPolicy, SubscriptionTopic,
    key::UpgradeHeaders,
    physical::{ConnectionProtocol, no_response_id},
};
use crate::{adapter::hyperliquid::HyperliquidCodec, error::MarketParticipantError};

fn protocol() -> ConnectionProtocol {
    ConnectionProtocol::new(no_response_id)
}

#[derive(Debug)]
struct ReceivedFrame {
    connection: usize,
    payload: String,
}

#[derive(Debug)]
struct UpgradeObservation {
    connection: usize,
    headers: HeaderMap,
}

const HANDSHAKE_HEADERS: [HeaderName; 8] = [
    hyper::header::HOST,
    hyper::header::UPGRADE,
    hyper::header::CONNECTION,
    hyper::header::SEC_WEBSOCKET_KEY,
    hyper::header::SEC_WEBSOCKET_VERSION,
    hyper::header::ACCEPT,
    hyper::header::ACCEPT_ENCODING,
    hyper::header::SEC_WEBSOCKET_EXTENSIONS,
];

impl UpgradeObservation {
    fn declared_headers(&self) -> HeaderMap {
        let mut declared = self.headers.clone();
        for handshake in HANDSHAKE_HEADERS {
            declared.remove(&handshake);
        }
        declared
    }
}

#[derive(Deserialize)]
struct ControlMessage {
    op: Option<String>,
}

#[derive(Deserialize)]
struct LoginAcknowledgement {
    event: Option<String>,
}

#[derive(Deserialize)]
struct RequestWithId {
    id: Option<u64>,
}

struct ReconnectServer {
    url: Url,
    frames: mpsc::UnboundedReceiver<ReceivedFrame>,
    upgrades: mpsc::UnboundedReceiver<UpgradeObservation>,
    connections: Arc<AtomicUsize>,
    cancellation: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Clone, Copy)]
enum DisconnectOn {
    Never,
    Subscribe,
    SubscribeAndRejectReconnect,
    Unsubscribe,
    HyperliquidResponse,
    AnswerKeepalive,
    PingPeriodically,
}

const PING_INTERVAL: Duration = Duration::from_millis(50);

const LIVENESS_TIMEOUT: Duration = Duration::from_millis(300);

const IDLE_TIMEOUT: Duration = Duration::from_millis(200);

fn test_transport() -> Arc<TransportHub> {
    Arc::new(TransportHub::new(TransportPolicy {
        allow_plaintext: true,
        extra_root_certificates: Vec::new(),
    }))
}

impl ReconnectServer {
    async fn spawn(disconnect_on: DisconnectOn) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("local listener should bind");
        let url = Url::parse(&format!("ws://{}/ws", listener.local_addr().unwrap()))
            .expect("local WebSocket URL should parse");
        let (frame_sender, frames) = mpsc::unbounded_channel();
        let (upgrade_sender, upgrades) = mpsc::unbounded_channel();
        let connections = Arc::new(AtomicUsize::new(0));
        let task_connections = Arc::clone(&connections);
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    () = task_cancellation.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((stream, _)) = accepted else { break };
                let connection = task_connections.fetch_add(1, Ordering::AcqRel) + 1;
                if connection > 1
                    && matches!(disconnect_on, DisconnectOn::SubscribeAndRejectReconnect)
                {
                    drop(stream);
                    continue;
                }
                let sender = frame_sender.clone();
                let upgrade_sender = upgrade_sender.clone();
                let connection_cancellation = task_cancellation.clone();
                tokio::spawn(async move {
                    let service_cancellation = connection_cancellation.clone();
                    let service = service_fn(move |mut request: Request<Incoming>| {
                        let sender = sender.clone();
                        let upgrade_sender = upgrade_sender.clone();
                        let connection_cancellation = connection_cancellation.clone();
                        async move {
                            let observation = UpgradeObservation {
                                connection,
                                headers: request.headers().clone(),
                            };
                            if upgrade_sender.send(observation).is_err() {
                                return Err(yawc::WebSocketError::ConnectionClosed);
                            }
                            let (response, upgrade) = WebSocket::upgrade(&mut request)?;
                            tokio::spawn(async move {
                                let Ok(mut websocket) = upgrade.await else {
                                    return;
                                };
                                let pings = matches!(disconnect_on, DisconnectOn::PingPeriodically);
                                let mut ping_tick = tokio::time::interval(PING_INTERVAL);
                                loop {
                                    tokio::select! {
                                        () = connection_cancellation.cancelled() => return,
                                        _ = ping_tick.tick(), if pings => {
                                            if websocket
                                                .send(Frame::ping(Bytes::from_static(b"kytrix")))
                                                .await
                                                .is_err()
                                            {
                                                return;
                                            }
                                        }
                                        frame = websocket.next_frame() => {
                                            let Ok(frame) = frame else { return };
                                            if !matches!(frame.opcode(), OpCode::Text | OpCode::Binary) {
                                                continue;
                                            }
                                            let payload = String::from_utf8_lossy(frame.payload()).into_owned();
                                            if sender.send(ReceivedFrame { connection, payload: payload.clone() }).is_err() {
                                                return;
                                            }
                                            if payload == "ping"
                                                && matches!(disconnect_on, DisconnectOn::AnswerKeepalive)
                                                && websocket.send(Frame::text("pong")).await.is_err()
                                            {
                                                return;
                                            }
                                            if matches!(disconnect_on, DisconnectOn::HyperliquidResponse)
                                                && let Ok(request) = serde_json::from_str::<RequestWithId>(&payload)
                                                && let Some(id) = request.id
                                                && websocket
                                                    .send(Frame::text(format!(
                                                        r#"{{"channel":"post","data":{{"id":{id}}}}}"#
                                                    )))
                                                    .await
                                                    .is_err()
                                            {
                                                return;
                                            }
                                            let Ok(value) = serde_json::from_str::<ControlMessage>(&payload) else {
                                                continue;
                                            };
                                            match value.op.as_deref() {
                                                Some("login") => {
                                                    if websocket.send(Frame::text(r#"{"event":"login","code":"0"}"#)).await.is_err() {
                                                        return;
                                                    }
                                                }
                                                Some("subscribe")
                                                    if connection == 1
                                                        && matches!(
                                                            disconnect_on,
                                                            DisconnectOn::Subscribe
                                                                | DisconnectOn::SubscribeAndRejectReconnect
                                                        ) =>
                                                {
                                                    let _ = websocket.close().await;
                                                    return;
                                                }
                                                Some("unsubscribe")
                                                    if connection == 1
                                                        && matches!(disconnect_on, DisconnectOn::Unsubscribe) =>
                                                {
                                                    let _ = websocket.close().await;
                                                    return;
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                }
                            });
                            Ok::<_, yawc::WebSocketError>(response)
                        }
                    });
                    tokio::select! {
                        () = service_cancellation.cancelled() => {}
                        _ = http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .with_upgrades() => {}
                    }
                });
            }
        });
        Self {
            url,
            frames,
            upgrades,
            connections,
            cancellation,
            task,
        }
    }

    async fn next_frame(&mut self) -> ReceivedFrame {
        timeout(Duration::from_secs(2), self.frames.recv())
            .await
            .expect("server should receive a frame before the deadline")
            .expect("server task should keep the frame channel open")
    }

    async fn next_upgrade(&mut self) -> UpgradeObservation {
        timeout(Duration::from_secs(2), self.upgrades.recv())
            .await
            .expect("the server should be asked to upgrade before the deadline")
            .expect("server task should keep the upgrade channel open")
    }

    async fn assert_no_frame(&mut self) {
        assert!(
            timeout(Duration::from_millis(100), self.frames.recv())
                .await
                .is_err(),
            "server received an unexpected frame"
        );
    }

    async fn shutdown(self) {
        self.cancellation.cancel();
        let _ = self.task.await;
    }
}

#[tokio::test]
async fn stateful_connection_reauthenticates_and_replays_subscriptions() {
    let mut server = ReconnectServer::spawn(DisconnectOn::Subscribe).await;
    let reauthentication_count = Arc::new(AtomicUsize::new(0));
    let callback_count = Arc::clone(&reauthentication_count);
    let reauthentication: Reauthentication = Arc::new(move || {
        let callback_count = Arc::clone(&callback_count);
        Box::pin(async move {
            callback_count.fetch_add(1, Ordering::AcqRel);
            Ok(ReauthenticationRequest {
                payload: Bytes::from_static(br#"{"op":"login"}"#),
                session_token: "session".into(),
                acknowledged: Arc::new(|payload| {
                    let Ok(value) = serde_json::from_slice::<LoginAcknowledgement>(payload) else {
                        return Ok(None);
                    };
                    if value.event.as_deref() == Some("login") {
                        Ok(Some(()))
                    } else {
                        Ok(None)
                    }
                }),
            })
        })
    });
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let mut key = ConnectionKey::new(
        server.url.clone(),
        ResolvedProxy::NoProxy,
        Some("account".into()),
        0,
        &protocol(),
    );
    key.account = Some(
        market_participants_model::identifier::MarketParticipantAccountId::new("account")
            .expect("the fixture account id should be canonical"),
    );
    let connection = Connection::connect_with_reauthentication(
        key,
        ConnectionState::stateful(),
        ConnectionProtocol::new(no_response_id),
        cancellation.clone(),
        ReconnectPolicy {
            initial_delay: Duration::from_millis(5),
            multiplier: 1,
            max_delay: Duration::from_millis(5),
            max_attempts: Some(3),
        },
        Some(reauthentication),
        Arc::clone(&transport),
    )
    .await
    .expect("initial WebSocket connection should succeed");

    let initial_login = server.next_frame().await;
    assert_eq!(initial_login.connection, 1);
    assert_eq!(
        serde_json::from_str::<ControlMessage>(&initial_login.payload)
            .unwrap()
            .op
            .as_deref(),
        Some("login")
    );

    timeout(Duration::from_secs(1), async {
        while !connection.state().is_authenticated() {
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("initial login should be acknowledged");

    let topic = SubscriptionTopic::from(Bytes::from_static(b"candles"));
    let first_subscription = connection
        .subscribe(
            topic.clone(),
            Bytes::from_static(br#"{"op":"subscribe","channel":"candles"}"#),
            Bytes::from_static(br#"{"op":"unsubscribe","channel":"candles"}"#),
        )
        .expect("subscription should be sent");
    let initial_subscription = server.next_frame().await;
    assert_eq!(initial_subscription.connection, 1);
    let second_subscription = connection
        .subscribe(
            topic,
            Bytes::from_static(br#"{"op":"subscribe","channel":"candles"}"#),
            Bytes::from_static(br#"{"op":"unsubscribe","channel":"candles"}"#),
        )
        .expect("the duplicate logical subscription should share the topic");

    let replayed_login = server.next_frame().await;
    assert_eq!(replayed_login.connection, 2);
    assert_eq!(
        serde_json::from_str::<ControlMessage>(&replayed_login.payload)
            .unwrap()
            .op
            .as_deref(),
        Some("login")
    );
    let replayed_subscription = server.next_frame().await;
    assert_eq!(replayed_subscription.connection, 2);
    assert_eq!(
        replayed_subscription.payload,
        r#"{"op":"subscribe","channel":"candles"}"#
    );

    timeout(Duration::from_secs(1), async {
        while reauthentication_count.load(Ordering::Acquire) < 2
            || !connection.state().is_authenticated()
        {
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("reconnection should authenticate before replaying subscriptions");

    first_subscription.cancel();
    server.assert_no_frame().await;
    assert!(!connection.is_closed());

    cancellation.cancel();
    drop(second_subscription);
    server.shutdown().await;
}

#[tokio::test]
async fn released_topic_is_unsubscribed_and_not_replayed_after_reconnect() {
    let mut server = ReconnectServer::spawn(DisconnectOn::Unsubscribe).await;
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let connection = Connection::connect(
        ConnectionKey::new(
            server.url.clone(),
            ResolvedProxy::NoProxy,
            None,
            0,
            &protocol(),
        ),
        ConnectionState::stateless(),
        ConnectionProtocol::new(no_response_id),
        cancellation.clone(),
        ReconnectPolicy {
            initial_delay: Duration::from_millis(5),
            multiplier: 1,
            max_delay: Duration::from_millis(5),
            max_attempts: Some(3),
        },
        Arc::clone(&transport),
    )
    .await
    .expect("initial WebSocket connection should succeed");
    let topic = SubscriptionTopic::from(Bytes::from_static(b"candles:1m"));
    let first = connection
        .subscribe(
            topic.clone(),
            Bytes::from_static(br#"{"op":"subscribe","channel":"candles:1m"}"#),
            Bytes::from_static(br#"{"op":"unsubscribe","channel":"candles:1m"}"#),
        )
        .expect("first topic lease should be acquired");

    let subscribe = server.next_frame().await;
    assert_eq!(subscribe.connection, 1);
    assert_eq!(
        serde_json::from_str::<ControlMessage>(&subscribe.payload)
            .expect("subscribe frame should decode")
            .op
            .as_deref(),
        Some("subscribe")
    );

    let second = connection
        .subscribe(
            topic,
            Bytes::from_static(br#"{"op":"subscribe","channel":"candles:1m"}"#),
            Bytes::from_static(br#"{"op":"unsubscribe","channel":"candles:1m"}"#),
        )
        .expect("second topic lease should share the registration");
    server.assert_no_frame().await;

    first.cancel();
    server.assert_no_frame().await;
    assert!(!connection.is_closed());

    second.cancel();
    let unsubscribe = server.next_frame().await;
    assert_eq!(unsubscribe.connection, 1);
    assert_eq!(
        serde_json::from_str::<ControlMessage>(&unsubscribe.payload)
            .expect("unsubscribe frame should decode")
            .op
            .as_deref(),
        Some("unsubscribe")
    );

    timeout(Duration::from_secs(1), async {
        while server.connections.load(Ordering::Acquire) < 2 {
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the physical connection should reconnect after the server closes it");
    server.assert_no_frame().await;
    assert!(!connection.is_closed());

    cancellation.cancel();
    server.shutdown().await;
}

#[tokio::test]
async fn failed_reconnect_closes_connection_and_cancels_lifecycle() {
    let mut server = ReconnectServer::spawn(DisconnectOn::SubscribeAndRejectReconnect).await;
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let connection = Connection::connect(
        ConnectionKey::new(
            server.url.clone(),
            ResolvedProxy::NoProxy,
            None,
            0,
            &protocol(),
        ),
        ConnectionState::stateless(),
        ConnectionProtocol::new(no_response_id),
        cancellation.clone(),
        ReconnectPolicy {
            initial_delay: Duration::from_millis(5),
            multiplier: 1,
            max_delay: Duration::from_millis(5),
            max_attempts: None,
        },
        Arc::clone(&transport),
    )
    .await
    .expect("initial WebSocket connection should succeed");
    let topic = SubscriptionTopic::from(Bytes::from_static(b"candles"));
    let _subscription = connection
        .subscribe(
            topic,
            Bytes::from_static(br#"{"op":"subscribe","channel":"candles"}"#),
            Bytes::from_static(br#"{"op":"unsubscribe","channel":"candles"}"#),
        )
        .expect("initial topic should be subscribed");
    server.next_frame().await;
    timeout(Duration::from_secs(1), async {
        while connection.is_healthy() {
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the first socket closure should be observed");

    timeout(Duration::from_secs(1), async {
        while !connection.is_closed() {
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("a failed reconnect should close the connection");
    assert!(connection.is_closed());
    cancellation.cancel();
    server.shutdown().await;
}

#[tokio::test]
async fn resubscribed_topic_during_disconnect_is_replayed_without_stale_commands() {
    let mut server = ReconnectServer::spawn(DisconnectOn::Subscribe).await;
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let connection = Connection::connect(
        ConnectionKey::new(
            server.url.clone(),
            ResolvedProxy::NoProxy,
            None,
            0,
            &protocol(),
        ),
        ConnectionState::stateless(),
        ConnectionProtocol::new(no_response_id),
        cancellation.clone(),
        ReconnectPolicy {
            initial_delay: Duration::from_millis(200),
            multiplier: 1,
            max_delay: Duration::from_millis(200),
            max_attempts: Some(3),
        },
        Arc::clone(&transport),
    )
    .await
    .expect("initial WebSocket connection should succeed");
    let topic = SubscriptionTopic::from(Bytes::from_static(b"candles:1m"));
    let first = connection
        .subscribe(
            topic.clone(),
            Bytes::from_static(br#"{"op":"subscribe","channel":"candles:1m"}"#),
            Bytes::from_static(br#"{"op":"unsubscribe","channel":"candles:1m"}"#),
        )
        .expect("initial topic lease should be acquired");
    let initial_subscribe = server.next_frame().await;
    assert_eq!(initial_subscribe.connection, 1);

    timeout(Duration::from_secs(1), async {
        while connection.is_healthy() {
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the first socket closure should be observed");

    first.cancel();
    let second = connection
        .subscribe(
            topic,
            Bytes::from_static(br#"{"op":"subscribe","channel":"candles:1m"}"#),
            Bytes::from_static(br#"{"op":"unsubscribe","channel":"candles:1m"}"#),
        )
        .expect("the topic should be reacquired during reconnect backoff");

    let replayed_subscribe = server.next_frame().await;
    assert_eq!(replayed_subscribe.connection, 2);
    assert_eq!(
        serde_json::from_str::<ControlMessage>(&replayed_subscribe.payload)
            .expect("replayed subscribe should decode")
            .op
            .as_deref(),
        Some("subscribe")
    );
    server.assert_no_frame().await;
    assert!(!connection.is_closed());

    second.cancel();
    let unsubscribe = server.next_frame().await;
    assert_eq!(unsubscribe.connection, 2);
    assert_eq!(
        serde_json::from_str::<ControlMessage>(&unsubscribe.payload)
            .expect("unsubscribe should decode")
            .op
            .as_deref(),
        Some("unsubscribe")
    );
    assert!(!connection.is_closed());

    cancellation.cancel();
    server.shutdown().await;
}

#[tokio::test]
async fn hyperliquid_nested_response_id_resolves_pending_request() {
    let mut server = ReconnectServer::spawn(DisconnectOn::HyperliquidResponse).await;
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let connection = Connection::connect(
        ConnectionKey::new(
            server.url.clone(),
            ResolvedProxy::NoProxy,
            None,
            0,
            &protocol(),
        ),
        ConnectionState::stateless(),
        ConnectionProtocol::new(HyperliquidCodec::extract_response_id),
        cancellation.clone(),
        ReconnectPolicy {
            initial_delay: Duration::from_millis(5),
            multiplier: 1,
            max_delay: Duration::from_millis(5),
            max_attempts: Some(1),
        },
        Arc::clone(&transport),
    )
    .await
    .expect("initial WebSocket connection should succeed");

    let request_id = connection.allocate_request_id();
    let response = connection
        .send_and_wait_with_id(
            request_id,
            Bytes::from(format!(r#"{{"method":"post","id":{request_id}}}"#)),
            Duration::from_secs(1),
        )
        .await
        .expect("nested Hyperliquid response ID should resolve the request");
    assert_eq!(
        response,
        Bytes::from(format!(
            r#"{{"channel":"post","data":{{"id":{request_id}}}}}"#
        ))
    );
    let request = server.next_frame().await;
    assert_eq!(request.connection, 1);
    cancellation.cancel();
    server.shutdown().await;
}

#[tokio::test]
async fn an_inbound_ping_keeps_a_quiet_connection_trusted() {
    let server = ReconnectServer::spawn(DisconnectOn::PingPeriodically).await;
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let connection = Connection::connect(
        ConnectionKey::new(
            server.url.clone(),
            ResolvedProxy::NoProxy,
            None,
            0,
            &protocol(),
        ),
        ConnectionState::stateless(),
        ConnectionProtocol::new(no_response_id).with_liveness_timeout(LIVENESS_TIMEOUT),
        cancellation.clone(),
        keepalive_reconnect_policy(),
        Arc::clone(&transport),
    )
    .await
    .expect("initial WebSocket connection should succeed");

    sleep(LIVENESS_TIMEOUT * 3).await;

    assert!(
        connection.is_healthy(),
        "a connection the venue is pinging is alive however quiet its data is"
    );

    cancellation.cancel();
    server.shutdown().await;
}

#[tokio::test]
async fn a_silent_connection_is_not_trusted_past_its_window() {
    let server = ReconnectServer::spawn(DisconnectOn::Never).await;
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let connection = Connection::connect(
        ConnectionKey::new(
            server.url.clone(),
            ResolvedProxy::NoProxy,
            None,
            0,
            &protocol(),
        ),
        ConnectionState::stateless(),
        ConnectionProtocol::new(no_response_id).with_liveness_timeout(LIVENESS_TIMEOUT),
        cancellation.clone(),
        keepalive_reconnect_policy(),
        Arc::clone(&transport),
    )
    .await
    .expect("initial WebSocket connection should succeed");

    sleep(LIVENESS_TIMEOUT * 3).await;

    assert!(
        !connection.is_healthy(),
        "a connection that has said nothing for its whole window is not alive"
    );

    cancellation.cancel();
    server.shutdown().await;
}

#[tokio::test]
async fn a_connection_whose_last_subscription_is_released_is_collected() {
    let mut server = ReconnectServer::spawn(DisconnectOn::Never).await;
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let connection = Connection::connect(
        ConnectionKey::new(
            server.url.clone(),
            ResolvedProxy::NoProxy,
            None,
            0,
            &protocol(),
        ),
        ConnectionState::stateless(),
        ConnectionProtocol::new(no_response_id).with_idle_timeout(IDLE_TIMEOUT),
        cancellation.clone(),
        keepalive_reconnect_policy(),
        Arc::clone(&transport),
    )
    .await
    .expect("initial WebSocket connection should succeed");
    let subscription = connection
        .subscribe(
            SubscriptionTopic::from(Bytes::from_static(b"candles")),
            Bytes::from_static(br#"{"op":"subscribe","channel":"candles"}"#),
            Bytes::from_static(br#"{"op":"unsubscribe","channel":"candles"}"#),
        )
        .expect("subscription should be sent");
    server.next_frame().await;

    subscription.cancel();

    timeout(Duration::from_secs(2), async {
        while !connection.is_closed() {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("an unsubscribed connection should be collected once it has been idle");

    cancellation.cancel();
    server.shutdown().await;
}

#[tokio::test]
async fn a_connection_resubscribed_within_the_idle_window_is_kept() {
    let mut server = ReconnectServer::spawn(DisconnectOn::Never).await;
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let connection = Connection::connect(
        ConnectionKey::new(
            server.url.clone(),
            ResolvedProxy::NoProxy,
            None,
            0,
            &protocol(),
        ),
        ConnectionState::stateless(),
        ConnectionProtocol::new(no_response_id).with_idle_timeout(IDLE_TIMEOUT),
        cancellation.clone(),
        keepalive_reconnect_policy(),
        Arc::clone(&transport),
    )
    .await
    .expect("initial WebSocket connection should succeed");
    let first = connection
        .subscribe(
            SubscriptionTopic::from(Bytes::from_static(b"candles:1m")),
            Bytes::from_static(br#"{"op":"subscribe","channel":"candles:1m"}"#),
            Bytes::from_static(br#"{"op":"unsubscribe","channel":"candles:1m"}"#),
        )
        .expect("subscription should be sent");
    server.next_frame().await;

    first.cancel();
    let _second = connection
        .subscribe(
            SubscriptionTopic::from(Bytes::from_static(b"candles:5m")),
            Bytes::from_static(br#"{"op":"subscribe","channel":"candles:5m"}"#),
            Bytes::from_static(br#"{"op":"unsubscribe","channel":"candles:5m"}"#),
        )
        .expect("the replacement subscription should share the connection");

    sleep(IDLE_TIMEOUT * 3).await;

    assert!(
        !connection.is_closed(),
        "a connection a pane came back to should not have been collected"
    );

    cancellation.cancel();
    server.shutdown().await;
}

#[tokio::test]
async fn a_stateful_connection_is_kept_after_its_subscriptions_are_released() {
    let mut server = ReconnectServer::spawn(DisconnectOn::Never).await;
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let connection = Connection::connect(
        ConnectionKey::new(
            server.url.clone(),
            ResolvedProxy::NoProxy,
            Some("account".into()),
            0,
            &protocol(),
        ),
        ConnectionState::stateful(),
        ConnectionProtocol::new(no_response_id).with_idle_timeout(IDLE_TIMEOUT),
        cancellation.clone(),
        keepalive_reconnect_policy(),
        Arc::clone(&transport),
    )
    .await
    .expect("initial WebSocket connection should succeed");
    let subscription = connection
        .subscribe(
            SubscriptionTopic::from(Bytes::from_static(b"orders")),
            Bytes::from_static(br#"{"op":"subscribe","channel":"orders"}"#),
            Bytes::from_static(br#"{"op":"unsubscribe","channel":"orders"}"#),
        )
        .expect("subscription should be sent");
    server.next_frame().await;

    subscription.cancel();
    sleep(IDLE_TIMEOUT * 3).await;

    assert!(
        !connection.is_closed(),
        "a connection that carries credentials is kept for the requests that use them"
    );

    cancellation.cancel();
    server.shutdown().await;
}

#[tokio::test]
async fn retiring_a_stateful_connection_whose_last_subscription_already_drained_ends_it() {
    let mut server = ReconnectServer::spawn(DisconnectOn::Never).await;
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let connection = Connection::connect(
        ConnectionKey::new(
            server.url.clone(),
            ResolvedProxy::NoProxy,
            Some("account".into()),
            0,
            &protocol(),
        ),
        ConnectionState::stateful(),
        ConnectionProtocol::new(no_response_id).with_idle_timeout(IDLE_TIMEOUT),
        cancellation.clone(),
        keepalive_reconnect_policy(),
        Arc::clone(&transport),
    )
    .await
    .expect("initial WebSocket connection should succeed");
    let subscription = connection
        .subscribe(
            SubscriptionTopic::from(Bytes::from_static(b"orders")),
            Bytes::from_static(br#"{"op":"subscribe","channel":"orders"}"#),
            Bytes::from_static(br#"{"op":"unsubscribe","channel":"orders"}"#),
        )
        .expect("subscription should be sent");
    server.next_frame().await;

    drop(subscription);
    server.next_frame().await;
    sleep(IDLE_TIMEOUT * 3).await;
    assert!(
        !connection.is_closed(),
        "a stateful connection is kept after its last subscription drains"
    );

    connection.retire();

    timeout(Duration::from_secs(1), async {
        while !connection.is_closed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect(
        "a retired connection whose subscriptions already drained should end on its idle clock",
    );

    cancellation.cancel();
    server.shutdown().await;
}

#[tokio::test]
async fn a_stateful_connection_retired_while_subscribed_ends_when_its_last_subscription_drains() {
    let mut server = ReconnectServer::spawn(DisconnectOn::Never).await;
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let connection = Connection::connect(
        ConnectionKey::new(
            server.url.clone(),
            ResolvedProxy::NoProxy,
            Some("account".into()),
            0,
            &protocol(),
        ),
        ConnectionState::stateful(),
        ConnectionProtocol::new(no_response_id).with_idle_timeout(IDLE_TIMEOUT),
        cancellation.clone(),
        keepalive_reconnect_policy(),
        Arc::clone(&transport),
    )
    .await
    .expect("initial WebSocket connection should succeed");
    let subscription = connection
        .subscribe(
            SubscriptionTopic::from(Bytes::from_static(b"orders")),
            Bytes::from_static(br#"{"op":"subscribe","channel":"orders"}"#),
            Bytes::from_static(br#"{"op":"unsubscribe","channel":"orders"}"#),
        )
        .expect("subscription should be sent");
    server.next_frame().await;

    connection.retire();
    sleep(IDLE_TIMEOUT * 3).await;
    assert!(
        !connection.is_closed(),
        "a retired connection is kept while a subscription is still reading it"
    );

    drop(subscription);
    server.next_frame().await;

    timeout(Duration::from_secs(1), async {
        while !connection.is_closed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a retired connection whose last subscription drains should end on its idle clock");

    cancellation.cancel();
    server.shutdown().await;
}

fn keepalive_protocol() -> ConnectionProtocol {
    ConnectionProtocol::new(no_response_id).with_keepalive(Keepalive {
        message: "ping",
        answer: "pong",
        interval: Duration::from_millis(50),
        maximum_missed: 1,
        send_while_active: false,
    })
}

fn keepalive_reconnect_policy() -> ReconnectPolicy {
    ReconnectPolicy {
        initial_delay: Duration::from_millis(5),
        multiplier: 1,
        max_delay: Duration::from_millis(5),
        max_attempts: Some(3),
    }
}

#[tokio::test]
async fn an_idle_connection_sends_the_venue_keepalive_message() {
    let mut server = ReconnectServer::spawn(DisconnectOn::Never).await;
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let connection = Connection::connect(
        ConnectionKey::new(
            server.url.clone(),
            ResolvedProxy::NoProxy,
            None,
            0,
            &protocol(),
        ),
        ConnectionState::stateless(),
        keepalive_protocol(),
        cancellation.clone(),
        keepalive_reconnect_policy(),
        Arc::clone(&transport),
    )
    .await
    .expect("initial WebSocket connection should succeed");

    let keepalive = server.next_frame().await;
    assert_eq!(keepalive.connection, 1);
    assert_eq!(keepalive.payload, "ping");
    assert!(!connection.is_closed());

    cancellation.cancel();
    server.shutdown().await;
}

#[tokio::test]
async fn a_connection_that_never_answers_the_keepalive_is_reconnected() {
    let mut server = ReconnectServer::spawn(DisconnectOn::Never).await;
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let connection = Connection::connect(
        ConnectionKey::new(
            server.url.clone(),
            ResolvedProxy::NoProxy,
            None,
            0,
            &protocol(),
        ),
        ConnectionState::stateless(),
        keepalive_protocol(),
        cancellation.clone(),
        keepalive_reconnect_policy(),
        Arc::clone(&transport),
    )
    .await
    .expect("initial WebSocket connection should succeed");

    timeout(Duration::from_secs(2), async {
        loop {
            if server.next_frame().await.connection == 2 {
                break;
            }
        }
    })
    .await
    .expect("missing keepalive answers should replace the connection");
    assert!(!connection.is_closed());

    cancellation.cancel();
    server.shutdown().await;
}

#[tokio::test]
async fn the_venue_keepalive_answer_is_not_published_as_an_event() {
    let mut server = ReconnectServer::spawn(DisconnectOn::AnswerKeepalive).await;
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let connection = Connection::connect(
        ConnectionKey::new(
            server.url.clone(),
            ResolvedProxy::NoProxy,
            None,
            0,
            &protocol(),
        ),
        ConnectionState::stateless(),
        keepalive_protocol(),
        cancellation.clone(),
        keepalive_reconnect_policy(),
        Arc::clone(&transport),
    )
    .await
    .expect("initial WebSocket connection should succeed");
    let mut events = Box::pin(connection.event_stream());

    let keepalive = server.next_frame().await;
    assert_eq!(keepalive.payload, "ping");

    assert!(
        timeout(Duration::from_millis(300), events.next())
            .await
            .is_err(),
        "the answer should not be published as an event"
    );

    cancellation.cancel();
    server.shutdown().await;
}

#[tokio::test]
async fn closing_a_connection_ends_its_streams_with_the_reason() {
    let mut server = ReconnectServer::spawn(DisconnectOn::Subscribe).await;
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let connection = Connection::connect(
        ConnectionKey::new(
            server.url.clone(),
            ResolvedProxy::NoProxy,
            None,
            0,
            &protocol(),
        ),
        ConnectionState::stateless(),
        ConnectionProtocol::new(no_response_id),
        cancellation.clone(),
        ReconnectPolicy {
            initial_delay: Duration::from_millis(5),
            multiplier: 1,
            max_delay: Duration::from_millis(5),
            max_attempts: Some(3),
        },
        Arc::clone(&transport),
    )
    .await
    .expect("initial WebSocket connection should succeed");
    let mut events = Box::pin(connection.event_stream());
    let topic = SubscriptionTopic::from(Bytes::from_static(b"candles"));
    let _subscription = connection
        .subscribe(
            topic,
            Bytes::from_static(br#"{"op":"subscribe","channel":"candles"}"#),
            Bytes::from_static(br#"{"op":"unsubscribe","channel":"candles"}"#),
        )
        .expect("subscription should be sent");
    server.next_frame().await;

    connection.close();

    assert!(
        matches!(
            timeout(Duration::from_secs(1), events.next()).await,
            Ok(Some(Err(MarketParticipantError::ServiceClosed)))
        ),
        "a closed connection should end its streams with the reason it closed"
    );
    assert!(
        matches!(
            timeout(Duration::from_secs(1), events.next()).await,
            Ok(None)
        ),
        "the reason should be the last item of the stream"
    );

    cancellation.cancel();
    server.shutdown().await;
}

#[tokio::test]
async fn a_failed_reconnect_ends_its_streams_with_that_failure() {
    let mut server = ReconnectServer::spawn(DisconnectOn::SubscribeAndRejectReconnect).await;
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let connection = Connection::connect(
        ConnectionKey::new(
            server.url.clone(),
            ResolvedProxy::NoProxy,
            None,
            0,
            &protocol(),
        ),
        ConnectionState::stateless(),
        ConnectionProtocol::new(no_response_id),
        cancellation.clone(),
        ReconnectPolicy {
            initial_delay: Duration::from_millis(5),
            multiplier: 1,
            max_delay: Duration::from_millis(5),
            max_attempts: Some(1),
        },
        Arc::clone(&transport),
    )
    .await
    .expect("initial WebSocket connection should succeed");
    let mut events = Box::pin(connection.event_stream());
    let topic = SubscriptionTopic::from(Bytes::from_static(b"candles"));
    let _subscription = connection
        .subscribe(
            topic,
            Bytes::from_static(br#"{"op":"subscribe","channel":"candles"}"#),
            Bytes::from_static(br#"{"op":"unsubscribe","channel":"candles"}"#),
        )
        .expect("subscription should be sent");
    server.next_frame().await;

    assert!(
        matches!(
            timeout(Duration::from_secs(2), events.next()).await,
            Ok(Some(Err(MarketParticipantError::ConnectionFailed { .. })))
        ),
        "a connection that could not be re-established should end its streams with that"
    );

    cancellation.cancel();
    server.shutdown().await;
}

#[tokio::test]
async fn declared_upgrade_headers_reach_the_websocket_upgrade_request() {
    let mut server = ReconnectServer::spawn(DisconnectOn::Never).await;
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let declared = upgrade_header_map(&[("accept-language", "en"), ("x-dc-region", "us")]);
    let declaration = ConnectionProtocol::new(no_response_id)
        .with_upgrade_headers(UpgradeHeaders::from_header_map(&declared));
    let key = ConnectionKey::new(
        server.url.clone(),
        ResolvedProxy::NoProxy,
        None,
        0,
        &declaration,
    );
    let connection = Connection::connect(
        key.clone(),
        ConnectionState::stateless(),
        declaration,
        cancellation.clone(),
        keepalive_reconnect_policy(),
        Arc::clone(&transport),
    )
    .await
    .expect("a connection with declared upgrade headers should connect");

    let upgrade = server.next_upgrade().await;
    assert_eq!(
        upgrade.declared_headers(),
        declared,
        "the upgrade request should carry exactly the headers the protocol declared"
    );
    assert_eq!(
        key.upgrade_headers,
        UpgradeHeaders::from_header_map(&upgrade.declared_headers()),
        "the key of the socket the venue served is the declaration that dial presented"
    );

    cancellation.cancel();
    drop(connection);
    server.shutdown().await;
}

#[tokio::test]
async fn a_protocol_without_upgrade_headers_still_upgrades() {
    let mut server = ReconnectServer::spawn(DisconnectOn::Never).await;
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let connection = Connection::connect(
        ConnectionKey::new(
            server.url.clone(),
            ResolvedProxy::NoProxy,
            None,
            0,
            &protocol(),
        ),
        ConnectionState::stateless(),
        ConnectionProtocol::new(no_response_id),
        cancellation.clone(),
        keepalive_reconnect_policy(),
        Arc::clone(&transport),
    )
    .await
    .expect("a connection with no declared upgrade headers should connect");

    let upgrade = server.next_upgrade().await;
    assert_eq!(
        upgrade.declared_headers(),
        HeaderMap::new(),
        "an upgrade whose protocol declared nothing should carry no headers of its caller's"
    );
    assert!(
        connection.is_healthy(),
        "the socket the fixture upgraded should be the one the connection is running on"
    );

    cancellation.cancel();
    drop(connection);
    server.shutdown().await;
}

#[tokio::test]
async fn a_reconnect_carries_the_declared_upgrade_headers_again() {
    let mut server = ReconnectServer::spawn(DisconnectOn::Subscribe).await;
    let transport = test_transport();
    let cancellation = transport.cancellation().child_token();
    let declared = upgrade_header_map(&[("accept-language", "zh-CN"), ("x-dc-region", "ap")]);
    let declaration = ConnectionProtocol::new(no_response_id)
        .with_upgrade_headers(UpgradeHeaders::from_header_map(&declared));
    let key = ConnectionKey::new(
        server.url.clone(),
        ResolvedProxy::NoProxy,
        None,
        0,
        &declaration,
    );
    let connection = Connection::connect(
        key.clone(),
        ConnectionState::stateless(),
        declaration,
        cancellation.clone(),
        keepalive_reconnect_policy(),
        Arc::clone(&transport),
    )
    .await
    .expect("a connection with declared upgrade headers should connect");

    let first = server.next_upgrade().await;
    assert_eq!(first.connection, 1);
    assert_eq!(first.declared_headers(), declared);

    let subscription = connection
        .subscribe(
            SubscriptionTopic::from(Bytes::from_static(b"candles")),
            Bytes::from_static(br#"{"op":"subscribe","channel":"candles"}"#),
            Bytes::from_static(br#"{"op":"unsubscribe","channel":"candles"}"#),
        )
        .expect("subscription should be sent");
    server.next_frame().await;

    let second = server.next_upgrade().await;
    assert_eq!(
        second.connection, 2,
        "the server should have been asked to serve a second socket"
    );
    assert_eq!(
        second.declared_headers(),
        declared,
        "the dial after the disconnect should carry the same headers as the first"
    );
    assert_eq!(
        key.upgrade_headers,
        UpgradeHeaders::from_header_map(&second.declared_headers()),
        "the key of the connection is the declaration every dial of it presented"
    );

    cancellation.cancel();
    drop(subscription);
    server.shutdown().await;
}

fn upgrade_header_map(headers: &[(&str, &str)]) -> HeaderMap {
    let mut declared = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let name =
            HeaderName::from_bytes(name.as_bytes()).expect("the header name should be valid");
        assert!(
            !HANDSHAKE_HEADERS.contains(&name),
            "{name} belongs to the WebSocket handshake, so declaring it would be filtered out \
             of both sides of the comparison a declared-header test makes"
        );
        declared.insert(
            name,
            value.parse().expect("the header value should be valid"),
        );
    }
    declared
}

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use ahash::AHashMap;
use app_config::network::ResolvedProxy;
use bytes::Bytes;
use futures::future::BoxFuture;
use network::{
    error::NetworkError,
    transport::{error::TransportError, hub::TransportHub, websocket::RawWebSocket},
};
use parking_lot::Mutex;
use reqwest::header::HeaderMap;
use smol_str::SmolStr;
use tokio::{
    sync::{Notify, broadcast, mpsc, oneshot},
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use url::Url;
use yawc::{Frame, OpCode};

use super::{
    ConnectionKey,
    heartbeat::{Heartbeat, Keepalive},
    key::UpgradeHeaders,
};
use crate::{
    connection::{ConnectionState, ReconnectPolicy},
    error::{AuthError, MarketParticipantError},
};

const DEFAULT_LIVENESS_TIMEOUT: Duration = Duration::from_secs(60);

const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const REAUTHENTICATION_TIMEOUT: Duration = Duration::from_secs(10);

pub type ResponseIdExtractor = fn(&[u8]) -> Option<u64>;

#[cfg(test)]
pub(crate) fn no_response_id(_: &[u8]) -> Option<u64> {
    None
}

fn connect_failure(endpoint: SmolStr, error: TransportError) -> NetworkError {
    match error {
        TransportError::WebSocketConnect { failure } => {
            NetworkError::WebSocketUpgradeFailed { endpoint, failure }
        }
        other => NetworkError::Io {
            message: smol_str::format_smolstr!("{other}"),
        },
    }
}

#[derive(Clone)]
pub struct ConnectionProtocol {
    pub response_id_extractor: ResponseIdExtractor,
    pub upgrade_headers: UpgradeHeaders,
    pub keepalive: Option<Keepalive>,
    pub liveness_timeout: Duration,
    pub idle_timeout: Duration,
}

impl ConnectionProtocol {
    pub fn new(response_id_extractor: ResponseIdExtractor) -> Self {
        Self {
            response_id_extractor,
            upgrade_headers: UpgradeHeaders::default(),
            keepalive: None,
            liveness_timeout: DEFAULT_LIVENESS_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
        }
    }

    pub fn with_upgrade_headers(mut self, headers: UpgradeHeaders) -> Self {
        self.upgrade_headers = headers;
        self
    }

    pub fn with_upgrade_header_map(self, headers: &HeaderMap) -> Self {
        self.with_upgrade_headers(UpgradeHeaders::from_header_map(headers))
    }

    pub const fn with_keepalive(mut self, keepalive: Keepalive) -> Self {
        self.keepalive = Some(keepalive);
        self
    }

    pub const fn with_liveness_timeout(mut self, liveness_timeout: Duration) -> Self {
        self.liveness_timeout = liveness_timeout;
        self
    }

    #[cfg(test)]
    pub const fn with_idle_timeout(mut self, idle_timeout: Duration) -> Self {
        self.idle_timeout = idle_timeout;
        self
    }
}

pub type ReauthenticationAcknowledgement =
    Arc<dyn Fn(&[u8]) -> Result<Option<()>, MarketParticipantError> + Send + Sync>;

pub struct ReauthenticationRequest {
    pub payload: Bytes,
    pub session_token: SmolStr,
    pub acknowledged: ReauthenticationAcknowledgement,
}

pub type Reauthentication = Arc<
    dyn Fn() -> BoxFuture<'static, Result<ReauthenticationRequest, MarketParticipantError>>
        + Send
        + Sync,
>;

#[derive(Clone, Debug)]
pub struct PendingRequest {
    pub id: u64,
    pub payload: Bytes,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SubscriptionTopic(Bytes);

impl From<Bytes> for SubscriptionTopic {
    fn from(value: Bytes) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug)]
struct ActiveSubscription {
    id: u64,
    subscribe_payload: Bytes,
    unsubscribe_payload: Bytes,
    subscriber_count: usize,
}

#[must_use = "dropping the lease releases the logical subscription"]
pub struct ConnectionSubscription {
    connection: Arc<Connection>,
    topic: SubscriptionTopic,
    active: AtomicBool,
}

impl ConnectionSubscription {
    pub fn cancel(&self) {
        if self.active.swap(false, Ordering::AcqRel) {
            self.connection.release_subscription(&self.topic);
        }
    }
}

impl Drop for ConnectionSubscription {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct ConnectionInner {
    state: ConnectionState,
    pending: AHashMap<u64, oneshot::Sender<Result<Bytes, MarketParticipantError>>>,
    next_request_id: u64,
    subscriptions: AHashMap<SubscriptionTopic, ActiveSubscription>,
    closed: bool,
    last_heartbeat: Instant,
    liveness_timeout: Duration,
    idle_timeout: Duration,
    idle_since: Option<Instant>,
    retired: bool,
    io_error: bool,
    reauthentication: Option<Reauthentication>,
    events: Option<broadcast::Sender<Bytes>>,
    termination: Option<MarketParticipantError>,
}

pub struct Connection {
    key: ConnectionKey,
    inner: Mutex<ConnectionInner>,
    outbound: Option<mpsc::Sender<PendingRequest>>,
    subscription_outbound: Option<mpsc::UnboundedSender<PendingRequest>>,
    cancellation: CancellationToken,
    idle_wakeup: Notify,
    response_id_extractor: ResponseIdExtractor,
}

struct OutboundReceivers {
    requests: mpsc::Receiver<PendingRequest>,
    subscriptions: mpsc::UnboundedReceiver<PendingRequest>,
}

struct SocketActorConfig {
    endpoint: Url,
    proxy: ResolvedProxy,
    upgrade_headers: UpgradeHeaders,
    keepalive: Option<Keepalive>,
    cancellation: CancellationToken,
    reconnect: ReconnectPolicy,
    transport: Arc<TransportHub>,
}

pub(crate) struct ConnectionConnectOptions {
    pub key: ConnectionKey,
    pub endpoint: Url,
    pub state: ConnectionState,
    pub protocol: ConnectionProtocol,
    pub cancellation: CancellationToken,
    pub reconnect: ReconnectPolicy,
    pub reauthentication: Option<Reauthentication>,
    pub transport: Arc<TransportHub>,
}

impl Connection {
    #[cfg(test)]
    pub(crate) fn with_outbound(
        key: ConnectionKey,
        state: ConnectionState,
        outbound: mpsc::Sender<PendingRequest>,
        subscription_outbound: mpsc::UnboundedSender<PendingRequest>,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(64);
        Arc::new(Self {
            key,
            inner: Mutex::new(ConnectionInner {
                state,
                pending: AHashMap::with_capacity(8),
                next_request_id: 1,
                subscriptions: AHashMap::with_capacity(4),
                closed: false,
                last_heartbeat: Instant::now(),
                liveness_timeout: DEFAULT_LIVENESS_TIMEOUT,
                idle_timeout: DEFAULT_IDLE_TIMEOUT,
                idle_since: None,
                retired: false,
                io_error: false,
                reauthentication: None,
                events: Some(events),
                termination: None,
            }),
            outbound: Some(outbound),
            subscription_outbound: Some(subscription_outbound),
            cancellation: CancellationToken::new(),
            idle_wakeup: Notify::new(),
            response_id_extractor: no_response_id,
        })
    }

    #[cfg(test)]
    pub async fn connect(
        key: ConnectionKey,
        state: ConnectionState,
        protocol: ConnectionProtocol,
        cancellation: CancellationToken,
        reconnect: ReconnectPolicy,
        transport: Arc<TransportHub>,
    ) -> Result<Arc<Self>, MarketParticipantError> {
        Self::connect_with_reauthentication(
            key,
            state,
            protocol,
            cancellation,
            reconnect,
            None,
            transport,
        )
        .await
    }

    #[cfg(test)]
    pub async fn connect_with_reauthentication(
        key: ConnectionKey,
        state: ConnectionState,
        protocol: ConnectionProtocol,
        cancellation: CancellationToken,
        reconnect: ReconnectPolicy,
        reauthentication: Option<Reauthentication>,
        transport: Arc<TransportHub>,
    ) -> Result<Arc<Self>, MarketParticipantError> {
        let endpoint = key.endpoint.clone();
        Self::connect_with_endpoint(ConnectionConnectOptions {
            key,
            endpoint,
            state,
            protocol,
            cancellation,
            reconnect,
            reauthentication,
            transport,
        })
        .await
    }

    pub(crate) async fn connect_with_endpoint(
        options: ConnectionConnectOptions,
    ) -> Result<Arc<Self>, MarketParticipantError> {
        let ConnectionConnectOptions {
            key,
            endpoint,
            state,
            protocol,
            cancellation,
            reconnect,
            reauthentication,
            transport,
        } = options;
        let proxy = key.proxy.clone();
        let route = key.route.clone();
        let safe_endpoint = smol_str::format_smolstr!("{}", key.redacted_endpoint());
        let upgrade_headers = protocol.upgrade_headers.clone();
        let (outbound, receiver) = mpsc::channel(64);
        let (subscription_outbound, subscription_receiver) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(64);
        let connection = Arc::new(Self {
            key,
            inner: Mutex::new(ConnectionInner {
                state,
                pending: AHashMap::with_capacity(8),
                next_request_id: 1,
                subscriptions: AHashMap::with_capacity(4),
                closed: false,
                last_heartbeat: Instant::now(),
                liveness_timeout: protocol.liveness_timeout,
                idle_timeout: protocol.idle_timeout,
                idle_since: None,
                retired: false,
                io_error: false,
                reauthentication,
                events: Some(events),
                termination: None,
            }),
            outbound: Some(outbound),
            subscription_outbound: Some(subscription_outbound),
            cancellation: cancellation.child_token(),
            idle_wakeup: Notify::new(),
            response_id_extractor: protocol.response_id_extractor,
        });
        let initial = transport
            .connect_websocket(
                &endpoint,
                &proxy,
                upgrade_headers.to_header_map(),
                &cancellation,
            )
            .await
            .map_err(|error| MarketParticipantError::ConnectionFailed {
                route,
                endpoint: safe_endpoint.clone(),
                source: connect_failure(safe_endpoint, error),
            })?;
        let actor_connection = Arc::clone(&connection);
        let actor_cancellation = connection.cancellation.clone();
        let actor_transport = Arc::clone(&transport);
        tokio::spawn(async move {
            run_socket_actor(
                actor_connection,
                initial,
                OutboundReceivers {
                    requests: receiver,
                    subscriptions: subscription_receiver,
                },
                SocketActorConfig {
                    endpoint,
                    proxy,
                    upgrade_headers,
                    keepalive: protocol.keepalive,
                    cancellation: actor_cancellation,
                    reconnect,
                    transport: actor_transport,
                },
            )
            .await;
        });
        Ok(connection)
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub fn state(&self) -> ConnectionState {
        self.inner.lock().state.clone()
    }

    pub fn authenticate(&self, account_id: impl Into<SmolStr>, session_token: impl Into<SmolStr>) {
        self.inner
            .lock()
            .state
            .authenticate(account_id, session_token);
    }

    pub fn unauthenticate(&self) {
        self.inner.lock().state.unauthenticate();
    }

    pub fn set_reauthentication(&self, reauthentication: Reauthentication) {
        self.inner.lock().reauthentication = Some(reauthentication);
    }

    fn reauthentication(&self) -> Option<Reauthentication> {
        self.inner.lock().reauthentication.clone()
    }

    fn response_id_extractor(&self) -> ResponseIdExtractor {
        self.response_id_extractor
    }

    #[cfg(test)]
    pub async fn send_and_wait(
        &self,
        payload: Bytes,
        timeout_duration: Duration,
    ) -> Result<Bytes, MarketParticipantError> {
        let request_id = self.allocate_request_id();
        self.send_and_wait_with_id(request_id, payload, timeout_duration)
            .await
    }

    pub fn allocate_request_id(&self) -> u64 {
        let mut inner = self.inner.lock();
        let request_id = inner.next_request_id;
        inner.next_request_id = inner.next_request_id.saturating_add(1).max(1);
        request_id
    }

    pub async fn send_and_wait_with_id(
        &self,
        request_id: u64,
        payload: Bytes,
        timeout_duration: Duration,
    ) -> Result<Bytes, MarketParticipantError> {
        let Some(outbound) = self.outbound.as_ref() else {
            return Err(MarketParticipantError::Configuration(
                "connection has no WebSocket transport".into(),
            ));
        };
        let receiver = {
            let mut inner = self.inner.lock();
            if inner.closed {
                return Err(MarketParticipantError::ServiceClosed);
            }
            if request_id >= inner.next_request_id {
                inner.next_request_id = request_id.saturating_add(1).max(1);
            }
            let (sender, receiver) = oneshot::channel();
            inner.pending.insert(request_id, sender);
            receiver
        };
        if outbound
            .send(PendingRequest {
                id: request_id,
                payload,
            })
            .await
            .is_err()
        {
            self.mark_io_error();
            self.fail_request(request_id, MarketParticipantError::ServiceClosed);
            return Err(MarketParticipantError::ServiceClosed);
        }
        match timeout(timeout_duration, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(MarketParticipantError::ServiceClosed),
            Err(_) => {
                self.fail_request(
                    request_id,
                    MarketParticipantError::Timeout {
                        operation: SmolStr::new_static("WebSocket request"),
                        timeout_ms: timeout_duration.as_millis().try_into().unwrap_or(u64::MAX),
                    },
                );
                Err(MarketParticipantError::Timeout {
                    operation: SmolStr::new_static("WebSocket request"),
                    timeout_ms: timeout_duration.as_millis().try_into().unwrap_or(u64::MAX),
                })
            }
        }
    }

    pub async fn send(&self, payload: Bytes) -> Result<u64, MarketParticipantError> {
        let request_id = self.allocate_request_id();
        let Some(outbound) = &self.outbound else {
            return Err(MarketParticipantError::Configuration(
                "connection has no WebSocket transport".into(),
            ));
        };
        outbound
            .send(PendingRequest {
                id: request_id,
                payload,
            })
            .await
            .map_err(|_| MarketParticipantError::ServiceClosed)?;
        Ok(request_id)
    }

    pub fn subscribe(
        self: &Arc<Self>,
        topic: SubscriptionTopic,
        payload: Bytes,
        unsubscribe_payload: Bytes,
    ) -> Result<ConnectionSubscription, MarketParticipantError> {
        let id = self.allocate_request_id();
        self.subscribe_with_id(id, topic, payload, unsubscribe_payload)
    }

    pub fn subscribe_with_id(
        self: &Arc<Self>,
        id: u64,
        topic: SubscriptionTopic,
        payload: Bytes,
        unsubscribe_payload: Bytes,
    ) -> Result<ConnectionSubscription, MarketParticipantError> {
        let Some(outbound) = &self.subscription_outbound else {
            return Err(MarketParticipantError::Configuration(
                "connection has no WebSocket transport".into(),
            ));
        };
        if outbound.is_closed() {
            return Err(MarketParticipantError::ServiceClosed);
        }
        let mut inner = self.inner.lock();
        if inner.closed {
            return Err(MarketParticipantError::ServiceClosed);
        }
        if id >= inner.next_request_id {
            inner.next_request_id = id.saturating_add(1).max(1);
        }
        if let Some(subscription) = inner.subscriptions.get_mut(&topic) {
            subscription.subscriber_count += 1;
            return Ok(ConnectionSubscription {
                connection: Arc::clone(self),
                topic,
                active: AtomicBool::new(true),
            });
        }
        inner.subscriptions.insert(
            topic.clone(),
            ActiveSubscription {
                id,
                subscribe_payload: payload.clone(),
                unsubscribe_payload,
                subscriber_count: 1,
            },
        );
        inner.idle_since = None;
        if outbound.send(PendingRequest { id, payload }).is_err() {
            inner.subscriptions.remove(&topic);
            return Err(MarketParticipantError::ServiceClosed);
        }
        Ok(ConnectionSubscription {
            connection: Arc::clone(self),
            topic,
            active: AtomicBool::new(true),
        })
    }

    fn release_subscription(&self, topic: &SubscriptionTopic) {
        let mut inner = self.inner.lock();
        let Some(subscription) = inner.subscriptions.get_mut(topic) else {
            return;
        };
        if subscription.subscriber_count > 1 {
            subscription.subscriber_count -= 1;
            return;
        }
        let Some(subscription) = inner.subscriptions.remove(topic) else {
            return;
        };
        let armed = inner.subscriptions.is_empty() && (!inner.state.is_stateful() || inner.retired);
        if armed {
            inner.idle_since = Some(Instant::now());
        }
        if !inner.closed
            && let Some(outbound) = &self.subscription_outbound
        {
            let _ = outbound.send(PendingRequest {
                id: subscription.id,
                payload: subscription.unsubscribe_payload,
            });
        }
    }

    fn subscriptions_for_reconnect(
        &self,
        receiver: &mut mpsc::UnboundedReceiver<PendingRequest>,
    ) -> Vec<PendingRequest> {
        let inner = self.inner.lock();
        while receiver.try_recv().is_ok() {}
        inner
            .subscriptions
            .values()
            .map(|subscription| PendingRequest {
                id: subscription.id,
                payload: subscription.subscribe_payload.clone(),
            })
            .collect()
    }

    pub fn event_stream(
        self: &Arc<Self>,
    ) -> impl futures::Stream<Item = Result<Bytes, MarketParticipantError>> + Send + 'static {
        futures::stream::unfold(
            ConnectionEvents {
                connection: Arc::clone(self),
                receiver: self
                    .inner
                    .lock()
                    .events
                    .as_ref()
                    .map(broadcast::Sender::subscribe),
                finished: false,
            },
            |mut state| async move {
                if state.finished {
                    return None;
                }
                let Some(mut receiver) = state.receiver.take() else {
                    state.finished = true;
                    return Some((Err(state.connection.termination()), state));
                };
                loop {
                    match receiver.recv().await {
                        Ok(payload) => {
                            state.receiver = Some(receiver);
                            return Some((Ok(payload), state));
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => {
                            state.finished = true;
                            return Some((Err(state.connection.termination()), state));
                        }
                    }
                }
            },
        )
    }

    pub fn publish(&self, payload: Bytes) {
        let inner = self.inner.lock();
        if let Some(events) = inner.events.as_ref() {
            let _ = events.send(payload);
        }
    }

    fn termination(&self) -> MarketParticipantError {
        self.inner
            .lock()
            .termination
            .clone()
            .unwrap_or(MarketParticipantError::ServiceClosed)
    }

    fn idle_deadline(&self) -> Option<Instant> {
        let inner = self.inner.lock();
        inner
            .idle_since
            .map(|idle_since| idle_since + inner.idle_timeout)
    }

    pub(crate) fn has_subscriptions(&self) -> bool {
        !self.inner.lock().subscriptions.is_empty()
    }

    pub(crate) fn retire(&self) {
        let mut inner = self.inner.lock();
        inner.retired = true;
        let armed = inner.subscriptions.is_empty();
        if armed {
            inner.idle_since = Some(Instant::now());
        }
        drop(inner);
        if armed {
            self.idle_wakeup.notify_one();
        }
    }

    pub fn resolve_request(&self, request_id: u64, payload: Bytes) -> bool {
        self.inner
            .lock()
            .pending
            .remove(&request_id)
            .map(|sender| sender.send(Ok(payload)).is_ok())
            .unwrap_or(false)
    }

    pub fn fail_request(&self, request_id: u64, error: MarketParticipantError) -> bool {
        self.inner
            .lock()
            .pending
            .remove(&request_id)
            .map(|sender| sender.send(Err(error)).is_ok())
            .unwrap_or(false)
    }

    pub fn mark_heartbeat(&self) {
        let mut inner = self.inner.lock();
        inner.last_heartbeat = Instant::now();
        inner.io_error = false;
    }

    pub fn mark_io_error(&self) {
        self.inner.lock().io_error = true;
    }

    pub fn is_healthy(&self) -> bool {
        let inner = self.inner.lock();
        !inner.closed && !inner.io_error && inner.last_heartbeat.elapsed() <= inner.liveness_timeout
    }

    pub fn close(&self) {
        self.close_with_error(MarketParticipantError::ServiceClosed);
    }

    fn close_with_error(&self, error: MarketParticipantError) {
        let mut inner = self.inner.lock();
        inner.closed = true;
        inner.termination = Some(error.clone());
        let pending = std::mem::take(&mut inner.pending);
        inner.subscriptions.clear();
        let events = inner.events.take();
        self.cancellation.cancel();
        drop(inner);
        drop(events);
        for sender in pending.into_values() {
            let _ = sender.send(Err(error.clone()));
        }
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().closed
    }
}

struct ConnectionEvents {
    connection: Arc<Connection>,
    receiver: Option<broadcast::Receiver<Bytes>>,
    finished: bool,
}

async fn run_socket_actor(
    connection: Arc<Connection>,
    mut socket: RawWebSocket,
    mut receivers: OutboundReceivers,
    config: SocketActorConfig,
) {
    let SocketActorConfig {
        endpoint,
        proxy,
        upgrade_headers,
        keepalive,
        cancellation,
        reconnect,
        transport,
    } = config;
    let mut backoff = crate::connection::ExponentialBackoff::new(reconnect);
    let response_id_extractor = connection.response_id_extractor();
    let mut heartbeat = Heartbeat::start(keepalive);
    if let Err(error) = run_reauthentication(&connection, &mut socket, response_id_extractor).await
    {
        connection.close_with_error(error);
        return;
    }
    loop {
        let disconnected = run_socket_cycle(
            &connection,
            &mut socket,
            &mut receivers,
            response_id_extractor,
            &cancellation,
            &mut heartbeat,
        )
        .await;
        if !disconnected || cancellation.is_cancelled() || connection.is_closed() {
            return;
        }
        connection.mark_io_error();
        if connection.state().is_stateful() {
            connection.unauthenticate();
        }
        let Some(delay) = backoff.next_delay() else {
            connection.close();
            return;
        };
        sleep(delay).await;
        if cancellation.is_cancelled() || connection.is_closed() {
            return;
        }
        match transport
            .connect_websocket(
                &endpoint,
                &proxy,
                upgrade_headers.to_header_map(),
                &cancellation,
            )
            .await
        {
            Ok(next_socket) => {
                socket = next_socket;
                heartbeat = Heartbeat::start(keepalive);
                connection.mark_heartbeat();
                backoff.reset();
                if let Err(error) =
                    run_reauthentication(&connection, &mut socket, response_id_extractor).await
                {
                    connection.close_with_error(error);
                    return;
                }
                for subscription in
                    connection.subscriptions_for_reconnect(&mut receivers.subscriptions)
                {
                    if socket
                        .send(frame_from_payload(subscription.payload))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            Err(error) => {
                let endpoint = smol_str::format_smolstr!("{}", connection.key.redacted_endpoint());
                connection.close_with_error(MarketParticipantError::ConnectionFailed {
                    route: connection.key.route.clone(),
                    endpoint: endpoint.clone(),
                    source: connect_failure(endpoint, error),
                });
                return;
            }
        }
    }
}

async fn run_reauthentication(
    connection: &Arc<Connection>,
    socket: &mut RawWebSocket,
    response_id_extractor: ResponseIdExtractor,
) -> Result<(), MarketParticipantError> {
    let Some(authenticate) = connection.reauthentication() else {
        return Ok(());
    };
    let request = authenticate().await?;
    if socket
        .send(frame_from_payload(request.payload))
        .await
        .is_err()
    {
        return Err(MarketParticipantError::AuthenticationFailed {
            route: connection.key.route.clone(),
            account: connection
                .key
                .account
                .as_ref()
                .map(|account| account.as_str().into()),
            source: AuthError::ServiceClosed,
        });
    }

    let deadline = tokio::time::sleep(REAUTHENTICATION_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            biased;
            _ = &mut deadline => return Err(MarketParticipantError::AuthenticationFailed {
                route: connection.key.route.clone(),
                account: connection.key.account.as_ref().map(|account| account.as_str().into()),
                source: AuthError::HandshakeTimeout,
            }),
            frame = socket.receive() => {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(_) => return Err(MarketParticipantError::AuthenticationFailed {
                        route: connection.key.route.clone(),
                        account: connection.key.account.as_ref().map(|account| account.as_str().into()),
                        source: AuthError::ServiceClosed,
                    }),
                };
                match frame.opcode() {
                    OpCode::Text | OpCode::Binary => {
                        let payload = frame.payload().clone();
                        connection.mark_heartbeat();
                        if (request.acknowledged)(&payload)?.is_some() {
                            let Some(account_id) = connection.key.account.as_ref() else {
                                return Err(MarketParticipantError::AuthenticationFailed {
                                    route: connection.key.route.clone(),
                                    account: None,
                                    source: AuthError::AccountIdentityMissing,
                                });
                            };
                            connection.authenticate(account_id.as_str(), request.session_token.clone());
                            return Ok(());
                        }
                        let resolved = response_id_extractor(&payload)
                            .is_some_and(|id| connection.resolve_request(id, payload.clone()));
                        if !resolved {
                            connection.publish(payload);
                        }
                    }
                    OpCode::Pong => connection.mark_heartbeat(),
                    OpCode::Close => return Err(MarketParticipantError::AuthenticationFailed {
                        route: connection.key.route.clone(),
                        account: connection.key.account.as_ref().map(|account| account.as_str().into()),
                        source: AuthError::ServiceClosed,
                    }),
                    OpCode::Ping => connection.mark_heartbeat(),
                    OpCode::Continuation => {}
                }
            }
        }
    }
}

async fn run_socket_cycle(
    connection: &Arc<Connection>,
    socket: &mut RawWebSocket,
    receivers: &mut OutboundReceivers,
    response_id_extractor: ResponseIdExtractor,
    cancellation: &CancellationToken,
    heartbeat: &mut Heartbeat,
) -> bool {
    loop {
        let idle_armed = connection.idle_wakeup.notified();
        let keepalive_due = heartbeat.deadline();
        let idle_due = connection.idle_deadline();
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return false,
            () = idle_armed => continue,
            () = wait_for_deadline(idle_due) => {
                if connection.has_subscriptions() {
                    continue;
                }
                connection.close();
                return false;
            }
            () = wait_for_deadline(keepalive_due) => {
                if let Some(message) = heartbeat.message()
                    && socket.send(Frame::text(message)).await.is_err()
                {
                    return true;
                }
                heartbeat.on_message_sent();
                if !heartbeat.is_healthy() {
                    return true;
                }
            }
            command = receivers.subscriptions.recv() => {
                let Some(command) = command else { return false; };
                if socket.send(frame_from_payload(command.payload)).await.is_err() {
                    return true;
                }
            }
            command = receivers.requests.recv() => {
                let Some(command) = command else { return false; };
                if socket.send(frame_from_payload(command.payload)).await.is_err() {
                    connection.fail_request(command.id, MarketParticipantError::ServiceClosed);
                    return true;
                }
            }
            frame = socket.receive() => {
                let Ok(frame) = frame else { return true; };
                match frame.opcode() {
                    OpCode::Text | OpCode::Binary => {
                        let payload = frame.payload().clone();
                        connection.mark_heartbeat();
                        if heartbeat.on_inbound(&payload) {
                            continue;
                        }
                        let resolved = response_id_extractor(&payload)
                            .is_some_and(|id| connection.resolve_request(id, payload.clone()));
                        if !resolved {
                            connection.publish(payload);
                        }
                    }
                    OpCode::Ping => connection.mark_heartbeat(),
                    OpCode::Pong => connection.mark_heartbeat(),
                    OpCode::Close => return true,
                    OpCode::Continuation => {}
                }
            }
        }
    }
}

async fn wait_for_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

fn frame_from_payload(payload: Bytes) -> Frame {
    if std::str::from_utf8(payload.as_ref()).is_ok() {
        return Frame::text(payload);
    }
    Frame::binary(payload)
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Connection")
            .field("key", &self.key)
            .field("healthy", &self.is_healthy())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use app_config::network::ResolvedProxy;
    use bytes::Bytes;
    use tokio::sync::mpsc;
    use url::Url;

    use super::{
        Connection, ConnectionKey, ConnectionProtocol, ConnectionState, PendingRequest,
        SubscriptionTopic, no_response_id,
    };
    use crate::error::MarketParticipantError;

    fn protocol() -> ConnectionProtocol {
        ConnectionProtocol::new(no_response_id)
    }

    #[tokio::test]
    async fn closing_a_connection_completes_pending_requests() {
        let (sender, mut receiver) = mpsc::channel::<PendingRequest>(1);
        let (subscription_sender, _subscription_receiver) = mpsc::unbounded_channel();
        let connection = Connection::with_outbound(
            ConnectionKey::new(
                Url::parse("wss://example.test/ws").expect("test endpoint should parse"),
                ResolvedProxy::NoProxy,
                None,
                0,
                &protocol(),
            ),
            ConnectionState::stateless(),
            sender,
            subscription_sender,
        );
        let pending = {
            let connection = connection.clone();
            tokio::spawn(async move {
                connection
                    .send_and_wait(Bytes::from_static(b"request"), Duration::from_secs(60))
                    .await
            })
        };
        receiver
            .recv()
            .await
            .expect("request should reach the outbound channel");

        connection.close();

        assert_eq!(
            pending.await.expect("request task should finish"),
            Err(MarketParticipantError::ServiceClosed)
        );
    }

    #[tokio::test]
    async fn closing_a_connection_cancels_its_lifecycle() {
        let (sender, _receiver) = mpsc::channel::<PendingRequest>(1);
        let (subscription_sender, _subscription_receiver) = mpsc::unbounded_channel();
        let connection = Connection::with_outbound(
            ConnectionKey::new(
                Url::parse("wss://example.test/ws").expect("test endpoint should parse"),
                ResolvedProxy::NoProxy,
                None,
                0,
                &protocol(),
            ),
            ConnectionState::stateless(),
            sender,
            subscription_sender,
        );
        let lifecycle = connection.cancellation();
        connection.close();
        tokio::time::timeout(Duration::from_secs(1), lifecycle.cancelled())
            .await
            .expect("connection close should cancel its lifecycle");
    }

    #[test]
    fn connection_debug_redacts_endpoint_credentials() {
        let (sender, _receiver) = mpsc::channel::<PendingRequest>(1);
        let (subscription_sender, _subscription_receiver) = mpsc::unbounded_channel();
        let connection = Connection::with_outbound(
            ConnectionKey::new(
                Url::parse("wss://example.test/ws/listen-key-secret?apiKey=api-secret")
                    .expect("test endpoint should parse"),
                ResolvedProxy::NoProxy,
                None,
                0,
                &protocol(),
            ),
            ConnectionState::stateless(),
            sender,
            subscription_sender,
        );
        let debug = format!("{connection:?}");
        assert!(!debug.contains("listen-key-secret"));
        assert!(!debug.contains("api-secret"));
    }

    #[tokio::test]
    async fn failed_subscription_send_is_not_replayed_after_transport_closes() {
        let (sender, _receiver) = mpsc::channel::<PendingRequest>(1);
        let (subscription_sender, subscription_receiver) = mpsc::unbounded_channel();
        drop(subscription_receiver);
        let connection = Connection::with_outbound(
            ConnectionKey::new(
                Url::parse("wss://example.test/ws").expect("test endpoint should parse"),
                ResolvedProxy::NoProxy,
                None,
                0,
                &protocol(),
            ),
            ConnectionState::stateless(),
            sender,
            subscription_sender,
        );

        assert_eq!(
            connection
                .subscribe(
                    SubscriptionTopic::from(Bytes::from_static(b"candles:1m")),
                    Bytes::from_static(b"subscription"),
                    Bytes::from_static(b"unsubscription"),
                )
                .map(|_| ()),
            Err(MarketParticipantError::ServiceClosed)
        );
        assert!(connection.inner.lock().subscriptions.is_empty());
    }

    #[test]
    fn topic_is_unsubscribed_only_after_its_last_lease_is_released() {
        let (sender, _receiver) = mpsc::channel::<PendingRequest>(1);
        let (subscription_sender, mut subscription_receiver) = mpsc::unbounded_channel();
        let connection = Connection::with_outbound(
            ConnectionKey::new(
                Url::parse("wss://example.test/ws").expect("test endpoint should parse"),
                ResolvedProxy::NoProxy,
                None,
                0,
                &protocol(),
            ),
            ConnectionState::stateless(),
            sender,
            subscription_sender,
        );
        let topic = SubscriptionTopic::from(Bytes::from_static(b"candles:1m"));
        let first = connection
            .subscribe(
                topic.clone(),
                Bytes::from_static(b"subscribe:1m"),
                Bytes::from_static(b"unsubscribe:1m"),
            )
            .expect("first topic lease should be acquired");
        assert_eq!(
            connection
                .inner
                .lock()
                .subscriptions
                .get(&topic)
                .map(|subscription| subscription.subscriber_count),
            Some(1)
        );
        let second = connection
            .subscribe(
                topic.clone(),
                Bytes::from_static(b"subscribe:1m:duplicate"),
                Bytes::from_static(b"unsubscribe:1m:duplicate"),
            )
            .expect("second topic lease should be acquired");

        assert_eq!(
            subscription_receiver
                .try_recv()
                .expect("the first lease should enqueue subscribe")
                .payload,
            Bytes::from_static(b"subscribe:1m")
        );
        assert!(subscription_receiver.try_recv().is_err());
        assert_eq!(
            connection
                .inner
                .lock()
                .subscriptions
                .get(&topic)
                .map(|subscription| subscription.subscriber_count),
            Some(2)
        );

        first.cancel();

        assert!(subscription_receiver.try_recv().is_err());
        assert_eq!(
            connection
                .inner
                .lock()
                .subscriptions
                .get(&topic)
                .map(|subscription| subscription.subscriber_count),
            Some(1)
        );
        assert!(!connection.is_closed());

        second.cancel();

        assert_eq!(
            subscription_receiver
                .try_recv()
                .expect("the last lease should enqueue unsubscribe")
                .payload,
            Bytes::from_static(b"unsubscribe:1m")
        );
        assert!(connection.inner.lock().subscriptions.is_empty());
        assert!(!connection.is_closed());

        drop(first);
        drop(second);
        assert!(subscription_receiver.try_recv().is_err());
    }

    #[test]
    fn duplicate_subscription_fails_when_the_control_actor_is_unavailable() {
        let (sender, _receiver) = mpsc::channel::<PendingRequest>(1);
        let (subscription_sender, mut subscription_receiver) = mpsc::unbounded_channel();
        let connection = Connection::with_outbound(
            ConnectionKey::new(
                Url::parse("wss://example.test/ws").expect("test endpoint should parse"),
                ResolvedProxy::NoProxy,
                None,
                0,
                &protocol(),
            ),
            ConnectionState::stateless(),
            sender,
            subscription_sender,
        );
        let topic = SubscriptionTopic::from(Bytes::from_static(b"candles:1m"));
        let _lease = connection
            .subscribe(
                topic.clone(),
                Bytes::from_static(b"subscribe:1m"),
                Bytes::from_static(b"unsubscribe:1m"),
            )
            .expect("first topic lease should be acquired");
        subscription_receiver
            .try_recv()
            .expect("the first subscription should be enqueued");
        drop(subscription_receiver);

        assert_eq!(
            connection
                .subscribe(
                    topic,
                    Bytes::from_static(b"subscribe:1m"),
                    Bytes::from_static(b"unsubscribe:1m"),
                )
                .map(|_| ()),
            Err(MarketParticipantError::ServiceClosed)
        );
        assert_eq!(
            connection
                .inner
                .lock()
                .subscriptions
                .values()
                .next()
                .map(|subscription| subscription.subscriber_count),
            Some(1)
        );
    }

    #[test]
    fn switching_one_lease_to_another_topic_preserves_the_shared_topic() {
        let (sender, _receiver) = mpsc::channel::<PendingRequest>(1);
        let (subscription_sender, mut subscription_receiver) = mpsc::unbounded_channel();
        let connection = Connection::with_outbound(
            ConnectionKey::new(
                Url::parse("wss://example.test/ws").expect("test endpoint should parse"),
                ResolvedProxy::NoProxy,
                None,
                0,
                &protocol(),
            ),
            ConnectionState::stateless(),
            sender,
            subscription_sender,
        );
        let minute_one = SubscriptionTopic::from(Bytes::from_static(b"candles:1m"));
        let first_widget = connection
            .subscribe(
                minute_one.clone(),
                Bytes::from_static(b"subscribe:1m"),
                Bytes::from_static(b"unsubscribe:1m"),
            )
            .expect("first 1m lease should be acquired");
        let second_widget = connection
            .subscribe(
                minute_one.clone(),
                Bytes::from_static(b"subscribe:1m"),
                Bytes::from_static(b"unsubscribe:1m"),
            )
            .expect("second 1m lease should be acquired");
        subscription_receiver
            .try_recv()
            .expect("the first 1m lease should enqueue subscribe");

        first_widget.cancel();
        let minute_five = SubscriptionTopic::from(Bytes::from_static(b"candles:5m"));
        let switched_widget = connection
            .subscribe(
                minute_five.clone(),
                Bytes::from_static(b"subscribe:5m"),
                Bytes::from_static(b"unsubscribe:5m"),
            )
            .expect("5m lease should be acquired");

        assert_eq!(
            subscription_receiver
                .try_recv()
                .expect("the new 5m topic should enqueue subscribe")
                .payload,
            Bytes::from_static(b"subscribe:5m")
        );
        assert!(subscription_receiver.try_recv().is_err());
        let inner = connection.inner.lock();
        assert_eq!(
            inner
                .subscriptions
                .get(&minute_one)
                .map(|subscription| subscription.subscriber_count),
            Some(1)
        );
        assert_eq!(
            inner
                .subscriptions
                .get(&minute_five)
                .map(|subscription| subscription.subscriber_count),
            Some(1)
        );
        drop(inner);
        assert!(!connection.is_closed());

        drop(second_widget);
        drop(switched_widget);
    }

    #[test]
    fn retiring_a_connection_whose_last_subscription_already_drained_starts_its_idle_clock() {
        let (sender, _receiver) = mpsc::channel::<PendingRequest>(1);
        let (subscription_sender, _subscription_receiver) = mpsc::unbounded_channel();
        let connection = Connection::with_outbound(
            ConnectionKey::new(
                Url::parse("wss://example.test/ws").expect("test endpoint should parse"),
                ResolvedProxy::NoProxy,
                None,
                0,
                &protocol(),
            ),
            ConnectionState::stateful(),
            sender,
            subscription_sender,
        );
        let lease = connection
            .subscribe(
                SubscriptionTopic::from(Bytes::from_static(b"orders")),
                Bytes::from_static(b"subscribe:orders"),
                Bytes::from_static(b"unsubscribe:orders"),
            )
            .expect("the fixture connection should accept a subscription");
        drop(lease);

        assert!(
            connection.idle_deadline().is_none(),
            "a stateful connection is kept after its last subscription drains"
        );

        connection.retire();

        assert!(
            connection.idle_deadline().is_some(),
            "a retired connection whose subscriptions already drained is idle from the moment it is retired, not kept"
        );
    }
}

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

/// The liveness window of a venue whose protocol does not declare a longer one.
///
/// Measured server push cadences: Binance spot and OKX prove liveness every 20 seconds and
/// the Binance futures endpoints every 3 minutes, so a venue that pushes or pings slower
/// than this has to say so through [`ConnectionProtocol::with_liveness_timeout`].
const DEFAULT_LIVENESS_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a connection nothing is subscribed to is kept for reuse.
///
/// A pane that changes instrument or interval releases one subscription and takes another
/// within moments, so the connection behind it is kept for a while rather than rebuilt each
/// time; one that is never subscribed to again is closed instead of sitting in the pool for
/// the life of the process.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const REAUTHENTICATION_TIMEOUT: Duration = Duration::from_secs(10);

/// Extracts a request identifier from a market-participant WebSocket response frame.
pub type ResponseIdExtractor = fn(&[u8]) -> Option<u64>;

/// A response id extractor for a protocol whose frames carry no response id.
///
/// It is the extractor of every fixture connection that is not exercising response
/// correlation, so the fixtures that build such a protocol share this one.
#[cfg(test)]
pub(crate) fn no_response_id(_: &[u8]) -> Option<u64> {
    None
}

/// Classifies a dial that never became a socket.
///
/// A failed upgrade already says which leg broke, and that is the part a widget can
/// act on — a timeout is worth repeating, a refusal is not, a settings mistake is
/// neither — so it is carried through instead of flattened into a message. Only the
/// failures that say nothing beyond themselves stay generic.
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

/// Market-participant-specific behavior used by a physical connection.
#[derive(Clone)]
pub struct ConnectionProtocol {
    pub response_id_extractor: ResponseIdExtractor,
    /// Headers the WebSocket upgrade request has to carry, besides the handshake's own,
    /// in the canonical form [`ConnectionKey`] compares and every dial materialises.
    ///
    /// A venue that routes or identifies its data centre by a request header has to see
    /// that header on the upgrade itself, not only on its HTTP calls: it is the header
    /// that decides which gateway the socket is served from, and a header set after the
    /// handshake cannot change where the socket already is. The headers belong to the
    /// protocol rather than to one dial because the connection outlives any single
    /// socket: every reconnect of it is the same connection to the same venue and
    /// carries what the first dial carried.
    ///
    /// They are the one declaration of this connection, not a copy of one: the key reads
    /// them from here rather than taking a map of its own, so the pool never hands back a
    /// socket whose key says one gateway while the dial asked for another. Changing them
    /// changes both, which is why a declaration that follows something the key does not
    /// otherwise name — a credential, and the region a venue derives from it — is safe to
    /// keep here.
    pub upgrade_headers: UpgradeHeaders,
    /// How the venue keeps a connection that stops carrying data open, if it needs to.
    pub keepalive: Option<Keepalive>,
    /// How long this connection may go without hearing anything before it stops being
    /// trusted. It has to cover the venue's own push and ping cadence.
    pub liveness_timeout: Duration,
    /// How long a connection nothing is subscribed to is kept before it is collected.
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

    /// Declares the headers this venue's WebSocket upgrade request has to carry.
    ///
    /// A protocol with none — every venue that does not route its sockets by a request
    /// header — dials with the handshake's own headers alone: a declaration starts empty,
    /// and a venue that makes one makes it in this canonical form rather than as a map the
    /// key would have to be kept in step with.
    ///
    /// This is the production seam the upgrade headers exist for, and it is also what the
    /// connection key of that connection is built from
    /// ([`ConnectionKey::for_route`] reads the declaration off this protocol), so a
    /// caller declares the headers once and both the key and the dial carry that one
    /// value. The caller this seam was added for is Longbridge's adapter (design §10.4:
    /// `x-dc-region`, derived from the credentials, required on the upgrade), which is
    /// also the only venue that declares any; every other venue this build serves dials
    /// without them.
    ///
    /// A declaration is the canonical form, which an adapter that holds its headers as a
    /// map can read off with [`UpgradeHeaders::from_header_map`] or declare with
    /// [`ConnectionProtocol::with_upgrade_header_map`].
    pub fn with_upgrade_headers(mut self, headers: UpgradeHeaders) -> Self {
        self.upgrade_headers = headers;
        self
    }

    /// Declares the headers this venue's upgrade request has to carry, as a header map.
    ///
    /// This is the shape an adapter has its headers in — a `HeaderMap` is what it also
    /// puts on its HTTP requests, where the same region header is required — so the
    /// conversion into the canonical form lives here rather than in the caller, and the
    /// connection key [`ConnectionKey::for_route`] builds from this protocol carries what
    /// this map declared. Entries no upgrade request could carry are left out by
    /// [`UpgradeHeaders::from_header_map`], which is the same rule every other
    /// declaration is read by; a venue declares the headers it can dial with.
    pub fn with_upgrade_header_map(self, headers: &HeaderMap) -> Self {
        self.with_upgrade_headers(UpgradeHeaders::from_header_map(headers))
    }

    /// Declares the keepalive this venue expects from a connection that goes silent.
    pub const fn with_keepalive(mut self, keepalive: Keepalive) -> Self {
        self.keepalive = Some(keepalive);
        self
    }

    /// Declares how long this venue's connection may stay silent and still be trusted.
    ///
    /// A venue that pushes or pings slowly needs a window that covers it: judging a
    /// connection by a window shorter than its own cadence discards healthy sockets.
    pub const fn with_liveness_timeout(mut self, liveness_timeout: Duration) -> Self {
        self.liveness_timeout = liveness_timeout;
        self
    }

    /// Declares how long a connection nothing is subscribed to is kept for reuse.
    ///
    /// Production keeps the default window: how long an unused socket is worth holding is
    /// the same question for every venue. Tests dial it down to wait it out.
    #[cfg(test)]
    pub const fn with_idle_timeout(mut self, idle_timeout: Duration) -> Self {
        self.idle_timeout = idle_timeout;
        self
    }
}

/// Payload and acknowledgement contract for a stateful reconnect.
///
/// The connection actor owns the socket and therefore waits for the market participant
/// acknowledgement before changing the state to authenticated.  Adapters only
/// provide the market-participant-specific matcher.
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
    /// When the idle clock was started for this connection: by the release that left nothing
    /// subscribed to it, for a stateless connection or a retired one, or by the retirement of
    /// a connection whose subscriptions had already drained. `None` for a connection that is
    /// subscribed, and for one that never was.
    idle_since: Option<Instant>,
    /// Whether the pool that dialled this connection has retired it: it is no longer the
    /// connection its key is answered from, so the pool never hands it out again and the
    /// subscriptions already bound to it are its only reason to stay open.
    retired: bool,
    io_error: bool,
    reauthentication: Option<Reauthentication>,
    /// Where inbound frames are published. Taking it away on close is what ends every
    /// subscription stream sharing this connection.
    events: Option<broadcast::Sender<Bytes>>,
    /// Why this connection stopped publishing, which its streams report as their last item.
    termination: Option<MarketParticipantError>,
}

/// A reusable physical connection with request correlation and health tracking.
pub struct Connection {
    key: ConnectionKey,
    inner: Mutex<ConnectionInner>,
    outbound: Option<mpsc::Sender<PendingRequest>>,
    subscription_outbound: Option<mpsc::UnboundedSender<PendingRequest>>,
    cancellation: CancellationToken,
    /// Signalled when a retirement starts the idle clock from outside the actor.
    ///
    /// Sampling the deadline at the top of a cycle cannot see a start that lands after
    /// that sample, and a retirement has no command behind it for the actor to wake on, so
    /// retirement signals this. A release needs no signal of its own: the unsubscribe it
    /// enqueues under the same lock is what the cycle's select waits on, and the pass after
    /// the one that carries it to the socket reads the deadlines again. Clearing the clock
    /// needs no signal either: it travels with the subscription command the actor wakes on.
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
    /// What every dial this actor makes has to carry, not only the one that opened the
    /// connection: the venue routes each socket by these. It is the same declaration the
    /// connection's key was built from, and the actor materialises it per dial.
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

    /// Establishes a WebSocket-backed connection and starts its I/O actor.
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

    /// Acquires a topic lease and sends the subscription on its first acquisition.
    pub fn subscribe(
        self: &Arc<Self>,
        topic: SubscriptionTopic,
        payload: Bytes,
        unsubscribe_payload: Bytes,
    ) -> Result<ConnectionSubscription, MarketParticipantError> {
        let id = self.allocate_request_id();
        self.subscribe_with_id(id, topic, payload, unsubscribe_payload)
    }

    /// Acquires a topic lease using a caller-selected wire request ID.
    ///
    /// Adapters use this when the market participant echoes the subscription ID in its
    /// acknowledgement. The ID must come from [`Connection::allocate_request_id`].
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
        // A connection with a subscription is never an idle one, however quiet it is.
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
        // The last subscription is what made this connection worth keeping. A stateless
        // connection exists for its subscriptions alone, so it starts the idle clock the
        // actor collects it on; a stateful one carries credentials and is kept for the
        // requests that use them, and there is one of those per account rather than one per
        // instrument. A retired one is kept by nobody — the pool that would have asked it
        // for those requests has replaced it — so its last subscription starts that clock
        // too.
        let armed = inner.subscriptions.is_empty() && (!inner.state.is_stateful() || inner.retired);
        if armed {
            inner.idle_since = Some(Instant::now());
        }
        // The unsubscribe is enqueued inside the critical section, which is the only thing
        // that orders it against a concurrent subscribe of the same topic: sent once the
        // lock is released, a subscribe that took the lock in between would enqueue its
        // subscribe first, and the venue would end up unsubscribed while this connection
        // books the topic as subscribed. An unbounded send never waits, so holding the lock
        // across it is free.
        //
        // Enqueuing it is also what wakes the actor for the clock started above: the command
        // is a branch of the cycle's select, so a parked actor is woken by it, and the pass
        // after the one that carries it to the socket reads the deadlines again. A
        // retirement has no command behind it, which is why that path signals the idle
        // wake-up and this one does not.
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

    /// The frames this connection publishes, ending with the reason it stopped.
    ///
    /// The stream outlives the socket: a reconnect keeps publishing into the same channel,
    /// and a connection that is taken down ends every stream by closing it and reporting
    /// why. A subscription that only ever saw frames would otherwise wait forever.
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
                        // A subscription that fell behind loses the frames it missed rather
                        // than the stream, exactly as it did while the connection published.
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

    /// Publishes one inbound frame to every subscription sharing this connection.
    pub fn publish(&self, payload: Bytes) {
        let inner = self.inner.lock();
        if let Some(events) = inner.events.as_ref() {
            let _ = events.send(payload);
        }
    }

    /// Why this connection stopped publishing, for the streams that outlive it.
    fn termination(&self) -> MarketParticipantError {
        self.inner
            .lock()
            .termination
            .clone()
            .unwrap_or(MarketParticipantError::ServiceClosed)
    }

    /// When this connection became idle, if nothing is subscribed to it.
    ///
    /// A connection that has never carried a subscription is not idle: it is kept for the
    /// requests it serves, and those are what the pool holds it for.
    fn idle_deadline(&self) -> Option<Instant> {
        let inner = self.inner.lock();
        inner
            .idle_since
            .map(|idle_since| idle_since + inner.idle_timeout)
    }

    pub(crate) fn has_subscriptions(&self) -> bool {
        !self.inner.lock().subscriptions.is_empty()
    }

    /// Retires this connection from the pool that dialled it.
    ///
    /// The pool retires a connection it replaced while it still had subscribers: the
    /// streams bound to it outlive the dial that lost it, so it stays open and keeps
    /// publishing, but it is out of the pool and the key it was dialled under is answered
    /// by the new dial from here on. That is what makes its own subscriptions its only
    /// reason to stay open, so the last one draining starts the idle clock the actor
    /// collects it on — whatever its state — instead of the connection being kept for
    /// requests a connection out of the pool is no longer asked for. A connection whose
    /// subscriptions have already drained is idle from here on rather than kept.
    pub(crate) fn retire(&self) {
        let mut inner = self.inner.lock();
        inner.retired = true;
        let armed = inner.subscriptions.is_empty();
        if armed {
            inner.idle_since = Some(Instant::now());
        }
        drop(inner);
        // A connection retired with nothing subscribed to it arms the clock from a task
        // other than the actor's, while the actor may be parked in a cycle that sampled it
        // unarmed; the signal is what brings that cycle back around to read it.
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
        // Taking the publisher away closes the broadcast channel, which is how every
        // subscription sharing this connection learns that no further frame will come.
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

/// The state of one subscription's stream.
///
/// A subscription reads a connection that outlives the socket it publishes from, so closing
/// the channel is not enough on its own to end the stream: it also has to say what closed
/// it, which a stream ending with a bare `None` cannot.
struct ConnectionEvents {
    connection: Arc<Connection>,
    /// `None` once the connection has stopped publishing, which ends the stream.
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
                // A fresh socket has its own silence window, and nothing has been asked
                // of it yet.
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
                    // An inbound ping is the venue saying the connection is alive, so it
                    // counts as a heartbeat even though it carries no data.
                    OpCode::Ping => connection.mark_heartbeat(),
                    OpCode::Continuation => {}
                }
            }
        }
    }
}

/// Runs one socket until it fails, the caller stops it, or nothing is subscribed to it.
///
/// Returns whether the connection is worth re-establishing. A cycle that ends because the
/// connection was closed — by its absent subscribers, or by a cancelled actor — returns
/// `false`, which is what keeps the actor from rebuilding a socket nobody wants.
async fn run_socket_cycle(
    connection: &Arc<Connection>,
    socket: &mut RawWebSocket,
    receivers: &mut OutboundReceivers,
    response_id_extractor: ResponseIdExtractor,
    cancellation: &CancellationToken,
    heartbeat: &mut Heartbeat,
) -> bool {
    loop {
        // The idle clock can be started from another task, and a deadline read cannot see a
        // start that lands after it. A release is carried by the command it enqueues, but a
        // retirement has none, so it signals the `Notify` waited on below: a `Notify` with no
        // waiter stores its permit rather than dropping the wake-up, so a start that lands
        // between this line and the idle read below fires here, and the pass that follows
        // reads the deadlines again.
        let idle_armed = connection.idle_wakeup.notified();
        let keepalive_due = heartbeat.deadline();
        let idle_due = connection.idle_deadline();
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return false,
            // A `Notify` nobody is waiting on stores a permit rather than losing the
            // wake-up, so an arm that landed before this future was polled still fires, and
            // a `Notified` that loses the race to another branch hands the permit on rather
            // than swallowing it. A permit buys one more pass, which re-reads the deadlines
            // before it can park, so the branch cannot spin on its own.
            () = idle_armed => continue,
            // A connection nothing is subscribed to any more is closed instead of sitting
            // in the pool until the process ends.
            () = wait_for_deadline(idle_due) => {
                // A subscription that arrived while this timer ran is what the connection
                // is for, so it is not idle after all.
                if connection.has_subscriptions() {
                    continue;
                }
                connection.close();
                return false;
            }
            // A venue that closes a silent connection is kept open by one message; a venue
            // that needs none waits here for as long as the connection lives.
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
                        // The venue's answer to a keepalive is liveness rather than
                        // subscription data, so it stops here instead of being published
                        // to every subscription sharing this connection.
                        if heartbeat.on_inbound(&payload) {
                            continue;
                        }
                        let resolved = response_id_extractor(&payload)
                            .is_some_and(|id| connection.resolve_request(id, payload.clone()));
                        if !resolved {
                            connection.publish(payload);
                        }
                    }
                    // An inbound ping is the venue saying the connection is alive, so it
                    // counts as a heartbeat even though it carries no data.
                    OpCode::Ping => connection.mark_heartbeat(),
                    OpCode::Pong => connection.mark_heartbeat(),
                    OpCode::Close => return true,
                    OpCode::Continuation => {}
                }
            }
        }
    }
}

/// Waits for a deadline; a connection with nothing due waits forever.
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

    /// The protocol of a fixture connection that declares no upgrade headers.
    ///
    /// A fixture key is built from the protocol its dial would use, so this is what a
    /// fixture names instead of an empty map.
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

    /// A connection retired once its subscriptions have already drained is idle rather than
    /// kept.
    ///
    /// The pool chooses between retiring and closing on a check the last lease of the
    /// connection can drain under, so this is the other side of that window, and the side
    /// no release reaches: a stateful connection that stayed kept from here on is a socket
    /// whose key is answered by another dial, with its actor and its listen-key refresh
    /// running for the rest of the process.
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

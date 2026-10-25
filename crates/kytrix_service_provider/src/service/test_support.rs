use std::sync::{
    Arc, Mutex, RwLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use app_config::network::{
    Config, KytrixServiceCapability, NetworkRouteGeneration, ProxyRoute, ServiceTarget,
};
use app_config_provider::notification::ConfigNotification;
use futures::SinkExt as _;
use hyper::{Request, body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use kytrix_proto::{
    client::{Request as ClientRequest, request::Request as ClientRequestKind},
    event as proto_event,
    server::{self as proto_response, ServerMessage, server_message},
    types as proto_types,
};
use network::transport::subscription::{Subscription, subscription_channel_with_wake};
use prost::Message;
use tokio::{
    net::TcpListener,
    sync::{Notify, broadcast, mpsc, watch},
    task::JoinHandle,
    time::{Duration, timeout},
};
use tokio_util::sync::CancellationToken;
use url::Url;
use yawc::{Frame, OpCode, WebSocket, WebSocketError};

use crate::{
    account_profile::AccountProfile,
    auth::model::{SessionAvailability, SessionChange},
    service::engine::{
        ErrorCode, IndexTopic, KYTRIX_SERVICE_WS_URL, ServiceSessionActor, ServiceTopic,
        SubscriberSink, WalletTransfer,
    },
};

pub(super) struct TestConfigManager {
    config: Arc<Config>,
    notification: ConfigNotification<Config>,
    next_route_generation: u64,
}

impl TestConfigManager {
    pub(super) fn in_memory(config: Config) -> Self {
        let next_route_generation = config
            .route_generations
            .values()
            .map(|generation| generation.get())
            .chain(std::iter::once(config.default_route_generation.get()))
            .max()
            .unwrap_or_default()
            .saturating_add(1);
        let config = Arc::new(config);
        Self {
            notification: ConfigNotification::new(config.clone()),
            config,
            next_route_generation,
        }
    }

    pub(super) fn read(&self) -> Arc<Config> {
        self.config.clone()
    }

    pub(super) fn notification(&self) -> ConfigNotification<Config> {
        self.notification.clone()
    }

    pub(super) fn update(&mut self, update: impl FnOnce(&mut Config)) {
        let previous = self.config.as_ref();
        let mut config = previous.clone();
        update(&mut config);

        let previous_default_proxy = previous
            .proxy_default
            .named_name()
            .and_then(|name| previous.proxies.get(name));
        let current_default_proxy = config
            .proxy_default
            .named_name()
            .and_then(|name| config.proxies.get(name));
        if previous.proxy_default != config.proxy_default
            || previous_default_proxy != current_default_proxy
        {
            config.default_route_generation =
                NetworkRouteGeneration::new(self.next_route_generation);
            self.next_route_generation = self.next_route_generation.saturating_add(1);
        }

        let mut targets = [
            KytrixServiceCapability::Account,
            KytrixServiceCapability::Etf,
            KytrixServiceCapability::News,
            KytrixServiceCapability::Indices,
            KytrixServiceCapability::X,
            KytrixServiceCapability::Wallet,
            KytrixServiceCapability::Ais,
            KytrixServiceCapability::XMedia,
            KytrixServiceCapability::Cmc,
            KytrixServiceCapability::FinancialCalendar,
            KytrixServiceCapability::PentagonPizzaIndex,
            KytrixServiceCapability::UpdaterMetadata,
            KytrixServiceCapability::UpdaterDownload,
        ]
        .into_iter()
        .map(|capability| ServiceTarget::KytrixService { capability })
        .collect::<std::collections::BTreeSet<_>>();
        targets.extend(previous.proxy_applications.keys().cloned());
        targets.extend(config.proxy_applications.keys().cloned());
        targets.extend(previous.service_route_generations.keys().cloned());
        targets.extend(config.service_route_generations.keys().cloned());

        for target in targets {
            let previous_route = previous.service_proxy_route_for(&target);
            let current_route = config.service_proxy_route_for(&target);
            let previous_proxy = previous_route
                .named_name()
                .and_then(|name| previous.proxies.get(name));
            let current_proxy = current_route
                .named_name()
                .and_then(|name| config.proxies.get(name));
            if previous_route != current_route || previous_proxy != current_proxy {
                let generation = NetworkRouteGeneration::new(self.next_route_generation);
                self.next_route_generation = self.next_route_generation.saturating_add(1);
                config.service_route_generations.insert(target, generation);
            }
        }

        self.config = Arc::new(config);
        self.notification.publish_for_test(self.config.clone());
    }
}

pub(super) const SET_SUBSCRIPTIONS_WIRE: &[u8] = &[
    0x08, 0x07, 0xaa, 0x06, 0x0d, 0x0a, 0x02, 0x0a, 0x00, 0x0a, 0x07, 0x12, 0x05, 0x0a, 0x03, b'S',
    b'P', b'X',
];
pub(super) const NEWS_EVENT_WIRE: &[u8] = &[
    0x12, 0x13, 0x0a, 0x11, 0x08, 0x02, 0x12, 0x01, b'T', 0x1a, 0x01, b'C', 0x21, 0xd2, 0x04, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00,
];
pub(super) const INDEX_EVENT_WIRE: &[u8] = &[
    0x12, 0x1c, 0x12, 0x1a, 0x0a, 0x03, b'S', b'P', b'X', 0x10, 0x65, 0x18, 0x5a, 0x20, 0x6e, 0x28,
    0x50, 0x30, 0x5f, 0x38, 0x01, 0x41, 0xd2, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
pub(super) const X_PROFILE_EVENT_WIRE: &[u8] =
    b"\x12\x24\x2a\x22\x0a\x02\x34\x32\x12\x02ky\x1a\x02Ky\x22\x10https://x.test/a\x28\x01\x30\x07";
pub(super) const WALLET_EVENT_WIRE: &[u8] = b"\x12\x5c\x22\x5a\x0a\x01e\x12\x2011111111111111111111111111111111\x1a\x2011111111111111111111111111111111\x22\x031.5\x2a\x03SOL\x31\xd2\x04\x00\x00\x00\x00\x00\x00";
pub(super) const UNKNOWN_SERVER_ONEOF_WIRE: &[u8] = &[0x2a, 0x00];

pub(super) const TEST_ACCOUNT_ID: &str = "11111111-1111-4111-8111-111111111111";

pub(super) fn index_topic(code: &str) -> IndexTopic {
    IndexTopic::from_code(code).expect("test index code should be valid")
}

pub(super) fn index_service_topic(code: &str) -> ServiceTopic {
    ServiceTopic::Index(index_topic(code))
}

pub(super) fn index_manifest() -> proto_types::IndexManifest {
    proto_types::IndexManifest {
        categories: vec![proto_types::IndexCategory {
            code: "americas".to_owned(),
            name: Some(proto_types::LocalizedName {
                en: "Americas".to_owned(),
                zh: "美洲".to_owned(),
            }),
        }],
        indices: vec![proto_types::IndexDefinition {
            request_code: ".SPX".to_owned(),
            code: "SPX".to_owned(),
            category: "americas".to_owned(),
            name: Some(proto_types::LocalizedName {
                en: "S&P 500 Index".to_owned(),
                zh: "标普500指数".to_owned(),
            }),
        }],
    }
}

/// The fear and greed snapshot the service serves, shaped as the collector publishes it.
pub(super) fn cmc_fear_greed_index() -> proto_types::CmcFearGreedIndex {
    let point = |date: &str, score: u32| proto_types::CmcFearGreedIndexPoint {
        date: date.to_owned(),
        score,
    };
    proto_types::CmcFearGreedIndex {
        current: 46,
        yesterday: Some(point("2026-10-23", 45)),
        last_week: Some(point("2026-10-17", 40)),
        last_month: Some(point("2026-09-24", 28)),
        last_year: Some(point("2025-10-24", 72)),
        yearly_high: Some(point("2025-11-20", 88)),
        yearly_low: Some(point("2026-04-15", 12)),
        timestamp: 1_790_000_000_000,
    }
}

/// The Pentagon Pizza Index snapshot the service serves, shaped as its collector publishes it.
///
/// The level is the index's own rung, which counts *down* as the alert rises: five is
/// the calmest and one the most severe. The collection time follows the moment it
/// observed rather than preceding it, because a snapshot is taken of a point that
/// already exists.
pub(super) fn pentagon_pizza_index() -> proto_types::PentagonPizzaIndex {
    proto_types::PentagonPizzaIndex {
        overall_index: 3,
        observed_at: 1_792_821_600_000,
        timestamp: 1_792_821_660_000,
    }
}

/// The financial calendar the service serves, shaped as the collector publishes it.
pub(super) fn financial_calendar() -> proto_types::FinancialCalendar {
    let event = |timestamp: i64,
                 country: &str,
                 name: &str,
                 importance: proto_types::EventImportance,
                 reference: Option<&str>| {
        proto_types::FinancialCalendarEvent {
            timestamp,
            country_code: country.to_owned(),
            name: name.to_owned(),
            importance: importance as i32,
            reference: reference.map(str::to_owned),
        }
    };
    proto_types::FinancialCalendar {
        events: vec![
            event(
                1_790_006_400_000,
                "US",
                "Nonfarm Payrolls",
                proto_types::EventImportance::High,
                Some("SEP"),
            ),
            event(
                1_790_002_400_000,
                "DE",
                "Ifo Business Climate",
                proto_types::EventImportance::Medium,
                None,
            ),
            event(
                1_790_000_000_000,
                "CN",
                "Industrial Profits (YTD) YoY",
                proto_types::EventImportance::Low,
                Some("AUG"),
            ),
        ],
        timestamp: 1_790_000_100_000,
    }
}

pub(super) fn tweet(tweet_id: &str, timestamp: i64) -> proto_types::Tweet {
    proto_types::Tweet {
        tweet_id: tweet_id.to_owned(),
        timestamp,
        x_user_id: "42".to_owned(),
        text_segments: vec![proto_types::TweetTextSegment {
            text: "hello".to_owned(),
            url: None,
        }],
        reference: None,
        media: Vec::new(),
    }
}

pub(super) fn x_profile() -> proto_types::XProfile {
    proto_types::XProfile {
        x_user_id: "42".to_owned(),
        username: "kytrix".to_owned(),
        display_name: "Kytrix".to_owned(),
        avatar_url: "https://example.com/avatar.jpg".to_owned(),
        verified: true,
        revision: 1,
        description: "profile".to_owned(),
        followers_count: 10,
        following_count: 2,
        protected: false,
    }
}

/// One AIS push as the collector publishes it: one vessel, one position, one departure.
pub(super) fn ais_delta() -> proto_types::AisDelta {
    proto_types::AisDelta {
        area: proto_types::AisArea::Hormuz as i32,
        epoch: 5,
        sequence: 7,
        base: 6,
        keyframe: false,
        statics: vec![proto_types::AisStatic {
            mmsi: 636_000_001,
            imo: 9_000_001,
            call_sign: "C6AB1".to_owned(),
            name: "HORMUZ STAR".to_owned(),
            destination: "FUJAIRAH".to_owned(),
            ship_type: 80,
            flag_mid: 636,
            draught: 123,
            dim_to_bow: 100,
            dim_to_stern: 20,
            dim_to_port: 10,
            dim_to_starboard: 11,
            eta_month: 6,
            eta_day: 15,
            eta_hour: 12,
            eta_minute: 30,
        }],
        positions: vec![proto_types::AisPosition {
            mmsi: 636_000_001,
            latitude: 26_566_000,
            longitude: 56_250_000,
            sog: 123,
            cog: 456,
            heading: 47,
            nav_status: 0,
            last_update: 1_700_000_000_000,
        }],
        removed: vec![636_000_002],
        timestamp: 1_700_000_000_000,
    }
}

/// The keyframe the collector publishes once it has a picture.
///
/// A keyframe is the whole picture and therefore carries no departures.
pub(super) fn ais_keyframe() -> proto_types::AisDelta {
    proto_types::AisDelta {
        keyframe: true,
        base: 0,
        removed: Vec::new(),
        ..ais_delta()
    }
}

pub(super) fn test_x_selection() -> proto_types::XSelection {
    proto_types::XSelection {
        x_user_id: "42".to_owned(),
        status: proto_types::XSelectionStatus::XActive as i32,
        created_at: 1,
        updated_at: 2,
        profile: Some(x_profile()),
    }
}

/// The row the service answers a first-ever add with: it carries no profile until the account
/// is active, which is the only case the service sends one in.
///
/// Both timestamps are the one the service wrote the row with, because adding a selection
/// stamps `created_at` and `updated_at` from the same reading of the clock.
pub(super) fn pending_x_selection() -> proto_types::XSelection {
    proto_types::XSelection {
        x_user_id: "42".to_owned(),
        status: proto_types::XSelectionStatus::XPending as i32,
        created_at: 4,
        updated_at: 4,
        profile: None,
    }
}

pub(super) fn test_wallet_selection(address: String) -> proto_types::WalletSelection {
    proto_types::WalletSelection {
        network: proto_types::WalletNetwork::Ethereum as i32,
        address,
        status: proto_types::WalletSelectionStatus::Active as i32,
        created_at: 1,
        updated_at: 1,
    }
}

pub(super) fn control_response(id: u32, topics: &[ServiceTopic]) -> proto_response::Response {
    let topics = topics.iter().map(Into::into).collect();
    proto_response::Response {
        id,
        response: Some(proto_response::response::Response::SetSubscriptions(
            proto_response::SetSubscriptionsResponse { topics },
        )),
    }
}

pub(super) fn error_response(id: u32, code: i32) -> proto_response::Response {
    proto_response::Response {
        id,
        response: Some(proto_response::response::Response::Error(
            proto_response::ErrorResponse { code },
        )),
    }
}

pub(super) fn actor_with_wallet_subscriber(
    invalidations: watch::Receiver<SessionChange>,
) -> (ServiceSessionActor, Subscription<WalletTransfer>) {
    let (_commands, receiver) = mpsc::channel(1);
    let wake = Arc::new(Notify::new());
    let mut actor = ServiceSessionActor::with_url(
        receiver,
        CancellationToken::new(),
        wake.clone(),
        KYTRIX_SERVICE_WS_URL.clone(),
        Arc::new(RwLock::new(None)),
        invalidations,
    );
    let profile = AccountProfile::new(TEST_ACCOUNT_ID, 7, 1);
    actor
        .install_account_profile(profile.clone())
        .expect("the test profile should install");
    let (publisher, subscription) = subscription_channel_with_wake(8, wake)
        .expect("the test subscription should have positive capacity");
    actor.add_subscriber_inner(
        1,
        vec![ServiceTopic::Wallet],
        SubscriberSink::Wallet(publisher),
        Some(profile),
        NetworkRouteGeneration::default(),
    );
    (actor, subscription)
}

pub(super) fn credential_change_watch()
-> (watch::Sender<SessionChange>, watch::Receiver<SessionChange>) {
    watch::channel(SessionChange::published(1, SessionAvailability::Available))
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum ObservedRequest {
    SetSubscriptions(Vec<ServiceTopic>),
    Reauthenticate {
        access_token: String,
        dpop_proof: String,
    },
    Rpc(&'static str),
}

pub(super) struct TestServiceServer {
    pub url: Url,
    pub connection_count: Arc<AtomicUsize>,
    pub challenge_sender: broadcast::Sender<ServerMessage>,
    pub requests: mpsc::UnboundedReceiver<(usize, ObservedRequest)>,
    pub error_next: Arc<AtomicBool>,
    pub duplicate_topics_next: Arc<AtomicBool>,
    pub unknown_oneof_next: Arc<AtomicBool>,
    pub drop_next_response: Arc<AtomicBool>,
    pub override_next_response: Arc<Mutex<Option<proto_response::response::Response>>>,
    pub pending_x_add_next: Arc<AtomicBool>,
    pub stall_next_response: Arc<AtomicBool>,
    pub response_stalled: Arc<Notify>,
    pub resume_response: Arc<Notify>,
    pub cancellation: CancellationToken,
    pub task: JoinHandle<()>,
}

impl TestServiceServer {
    pub(super) async fn spawn(disconnect_first_subscribe: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test service should bind");
        let url = format!("ws://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let connection_count = Arc::new(AtomicUsize::new(0));
        let task_connection_count = connection_count.clone();
        let handshake_valid = Arc::new(AtomicBool::new(true));
        let task_handshake_valid = handshake_valid;
        let (request_sender, requests) = mpsc::unbounded_channel();
        let (pong_sender, _) = mpsc::unbounded_channel();
        let (challenge_sender, _) = broadcast::channel::<ServerMessage>(8);
        let task_challenge_sender = challenge_sender.clone();
        let (ping_sender, _) = broadcast::channel::<Vec<u8>>(8);
        let task_ping_sender = ping_sender;
        let disconnect_first = Arc::new(Notify::new());
        let task_disconnect_first = disconnect_first;
        let error_next = Arc::new(AtomicBool::new(false));
        let task_error_next = error_next.clone();
        let duplicate_topics_next = Arc::new(AtomicBool::new(false));
        let task_duplicate_topics_next = duplicate_topics_next.clone();
        let unknown_oneof_next = Arc::new(AtomicBool::new(false));
        let task_unknown_oneof_next = unknown_oneof_next.clone();
        let authentication_unavailable_next = Arc::new(AtomicBool::new(false));
        let task_authentication_unavailable_next = authentication_unavailable_next;
        let drop_next_response = Arc::new(AtomicBool::new(false));
        let task_drop_next_response = drop_next_response.clone();
        let override_next_response = Arc::new(Mutex::new(None));
        let task_override_next_response = override_next_response.clone();
        let pending_x_add_next = Arc::new(AtomicBool::new(false));
        let task_pending_x_add_next = pending_x_add_next.clone();
        let stall_next_response = Arc::new(AtomicBool::new(false));
        let task_stall_next_response = stall_next_response.clone();
        let response_stalled = Arc::new(Notify::new());
        let task_response_stalled = response_stalled.clone();
        let resume_response = Arc::new(Notify::new());
        let task_resume_response = resume_response.clone();
        let send_old_event_before_response = Arc::new(AtomicBool::new(false));
        let task_send_old_event_before_response = send_old_event_before_response;
        let old_event_attempted = Arc::new(Notify::new());
        let task_old_event_attempted = old_event_attempted;
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    () = task_cancellation.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((stream, _)) = accepted else { break };
                let connection_id = task_connection_count.fetch_add(1, Ordering::AcqRel) + 1;
                let request_sender = request_sender.clone();
                let pong_sender = pong_sender.clone();
                let disconnect_first = task_disconnect_first.clone();
                let error_next = task_error_next.clone();
                let duplicate_topics_next = task_duplicate_topics_next.clone();
                let unknown_oneof_next = task_unknown_oneof_next.clone();
                let authentication_unavailable_next = task_authentication_unavailable_next.clone();
                let drop_next_response = task_drop_next_response.clone();
                let override_next_response = task_override_next_response.clone();
                let pending_x_add_next = task_pending_x_add_next.clone();
                let stall_next_response = task_stall_next_response.clone();
                let response_stalled = task_response_stalled.clone();
                let resume_response = task_resume_response.clone();
                let send_old_event_before_response = task_send_old_event_before_response.clone();
                let old_event_attempted = task_old_event_attempted.clone();
                let handshake_valid = task_handshake_valid.clone();
                let challenge_sender = task_challenge_sender.clone();
                let ping_sender = task_ping_sender.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |mut request: Request<Incoming>| {
                        let request_sender = request_sender.clone();
                        let pong_sender = pong_sender.clone();
                        let disconnect_first = disconnect_first.clone();
                        let error_next = error_next.clone();
                        let duplicate_topics_next = duplicate_topics_next.clone();
                        let unknown_oneof_next = unknown_oneof_next.clone();
                        let authentication_unavailable_next =
                            authentication_unavailable_next.clone();
                        let drop_next_response = drop_next_response.clone();
                        let override_next_response = override_next_response.clone();
                        let pending_x_add_next = pending_x_add_next.clone();
                        let stall_next_response = stall_next_response.clone();
                        let response_stalled = response_stalled.clone();
                        let resume_response = resume_response.clone();
                        let send_old_event_before_response = send_old_event_before_response.clone();
                        let old_event_attempted = old_event_attempted.clone();
                        let handshake_valid = handshake_valid.clone();
                        let challenge_sender = challenge_sender.clone();
                        let ping_sender = ping_sender.clone();
                        async move {
                            let valid = request.method() == hyper::Method::GET
                                && request
                                    .headers()
                                    .get("authorization")
                                    .and_then(|value| value.to_str().ok())
                                    == Some("DPoP test-renewed-access-token")
                                && request.headers().get("dpop").is_some()
                                && request.headers().get("x-real-ip").is_none();
                            handshake_valid.fetch_and(valid, Ordering::AcqRel);
                            let (response, upgrade) = WebSocket::upgrade(&mut request)?;
                            tokio::spawn(async move {
                                let Ok(mut websocket) = upgrade.await else {
                                    return;
                                };
                                let mut challenges = challenge_sender.subscribe();
                                let mut pings = ping_sender.subscribe();
                                let session_state = ServerMessage {
                                    message: Some(server_message::Message::SessionState(
                                        proto_response::SessionState {
                                            entitlement_revision: 1,
                                            news_read: true,
                                            index_topic_limit: 3,
                                            wallet_address_limit: 3,
                                            next_change_at: None,
                                            ais_read: true,
                                        },
                                    )),
                                };
                                if websocket
                                    .send(Frame::binary(session_state.encode_to_vec()))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                                loop {
                                    let frame = tokio::select! {
                                        frame = websocket.next_frame() => {
                                            let Ok(frame) = frame else { return };
                                            frame
                                        }
                                        message = challenges.recv() => {
                                            let Ok(message) = message else { return };
                                            if websocket
                                                .send(Frame::binary(message.encode_to_vec()))
                                                .await
                                                .is_err()
                                            {
                                                return;
                                            }
                                            continue;
                                        }
                                        payload = pings.recv() => {
                                            let Ok(payload) = payload else { return };
                                            if websocket.send(Frame::ping(payload)).await.is_err() {
                                                return;
                                            }
                                            continue;
                                        }
                                    };
                                    if frame.opcode() == OpCode::Pong {
                                        if pong_sender
                                            .send((connection_id, frame.payload().to_vec()))
                                            .is_err()
                                        {
                                            return;
                                        }
                                        continue;
                                    }
                                    if frame.opcode() != OpCode::Binary {
                                        continue;
                                    }
                                    let Ok(request) =
                                        ClientRequest::decode(frame.payload().as_ref())
                                    else {
                                        return;
                                    };
                                    let Some(request_kind) = request.request else {
                                        return;
                                    };
                                    let (observed, response, disconnect) = match request_kind {
                                        ClientRequestKind::SetSubscriptions(set) => {
                                            let Ok(observed_topics) = set
                                                .topics
                                                .iter()
                                                .map(ServiceTopic::try_from)
                                                .collect::<Result<Vec<_>, _>>()
                                            else {
                                                return;
                                            };
                                            (
                                                ObservedRequest::SetSubscriptions(
                                                    observed_topics,
                                                ),
                                                proto_response::response::Response::SetSubscriptions(
                                                    proto_response::SetSubscriptionsResponse {
                                                        topics: set.topics,
                                                    },
                                                ),
                                                disconnect_first_subscribe
                                                    && connection_id == 1,
                                            )
                                        }
                                        ClientRequestKind::Reauthenticate(reauthenticate) => (
                                            ObservedRequest::Reauthenticate {
                                                access_token: reauthenticate.access_token,
                                                dpop_proof: reauthenticate.dpop_proof,
                                            },
                                            proto_response::response::Response::Reauthenticate(
                                                proto_response::ReauthenticateResponse {},
                                            ),
                                            false,
                                        ),
                                        ClientRequestKind::GetEtf(_) => (
                                            ObservedRequest::Rpc("etf"),
                                            proto_response::response::Response::Etf(
                                                proto_response::EtfResponse {
                                                    etf: Some(proto_types::Etf {
                                                        timestamp: 42,
                                                        ..proto_types::Etf::default()
                                                    }),
                                                },
                                            ),
                                            false,
                                        ),
                                        ClientRequestKind::GetIndexManifest(_) => (
                                            ObservedRequest::Rpc("index_manifest"),
                                            proto_response::response::Response::IndexManifest(
                                                proto_response::IndexManifestResponse {
                                                    manifest: Some(index_manifest()),
                                                },
                                            ),
                                            false,
                                        ),
                                        ClientRequestKind::XSearch(_) => (
                                            ObservedRequest::Rpc("x_search"),
                                            proto_response::response::Response::XSearch(
                                                proto_response::XSearchResponse {
                                                    profiles: vec![x_profile()],
                                                },
                                            ),
                                            false,
                                        ),
                                        ClientRequestKind::XSelections(_) => (
                                            ObservedRequest::Rpc("x_selections"),
                                            proto_response::response::Response::XSelections(
                                                proto_response::XSelectionsResponse {
                                                    selections: vec![test_x_selection()],
                                                    x_account_limit: 3,
                                                },
                                            ),
                                            false,
                                        ),
                                        ClientRequestKind::XAddSelection(_) => (
                                            ObservedRequest::Rpc("x_add_selection"),
                                            proto_response::response::Response::XAddSelection(
                                                proto_response::XAddSelectionResponse {
                                                    selection: Some(test_x_selection()),
                                                },
                                            ),
                                            false,
                                        ),
                                        ClientRequestKind::XDeleteSelection(_) => (
                                            ObservedRequest::Rpc("x_delete_selection"),
                                            proto_response::response::Response::XDeleteSelection(
                                                proto_response::XDeleteSelectionResponse {},
                                            ),
                                            false,
                                        ),
                                        ClientRequestKind::XTweets(_) => (
                                            ObservedRequest::Rpc("x_tweets"),
                                            proto_response::response::Response::XTweets(
                                                proto_response::XTweetsResponse {
                                                    batch: Some(proto_types::TweetBatch::default()),
                                                },
                                            ),
                                            false,
                                        ),
                                        ClientRequestKind::WalletSelections(_) => (
                                            ObservedRequest::Rpc("wallet_selections"),
                                            proto_response::response::Response::WalletSelections(
                                                proto_response::WalletSelectionsResponse {
                                                    selections: Vec::new(),
                                                    wallet_address_limit: 3,
                                                },
                                            ),
                                            false,
                                        ),
                                        ClientRequestKind::WalletAddSelection(request) => (
                                            ObservedRequest::Rpc("wallet_add_selection"),
                                            proto_response::response::Response::WalletAddSelection(
                                                proto_response::WalletAddSelectionResponse {
                                                    selection: Some(test_wallet_selection(
                                                        request.address,
                                                    )),
                                                },
                                            ),
                                            false,
                                        ),
                                        ClientRequestKind::WalletDeleteSelection(_) => (
                                            ObservedRequest::Rpc("wallet_delete_selection"),
                                            proto_response::response::Response::WalletDeleteSelection(
                                                proto_response::WalletDeleteSelectionResponse {},
                                            ),
                                            false,
                                        ),
                                        ClientRequestKind::GetFinancialCalendar(_) => (
                                            ObservedRequest::Rpc("financial_calendar"),
                                            proto_response::response::Response::FinancialCalendar(
                                                proto_response::FinancialCalendarResponse {
                                                    financial_calendar: Some(
                                                        financial_calendar(),
                                                    ),
                                                },
                                            ),
                                            false,
                                        ),
                                        ClientRequestKind::GetCmcFearGreedIndex(_) => (
                                            ObservedRequest::Rpc("cmc_fear_greed_index"),
                                            proto_response::response::Response::CmcFearGreedIndex(
                                                proto_response::CmcFearGreedIndexResponse {
                                                    cmc_fear_greed_index: Some(
                                                        cmc_fear_greed_index(),
                                                    ),
                                                },
                                            ),
                                            false,
                                        ),
                                        ClientRequestKind::GetPentagonPizzaIndex(_) => (
                                            ObservedRequest::Rpc("pentagon_pizza_index"),
                                            proto_response::response::Response::PentagonPizzaIndex(
                                                proto_response::PentagonPizzaIndexResponse {
                                                    pentagon_pizza_index: Some(
                                                        pentagon_pizza_index(),
                                                    ),
                                                },
                                            ),
                                            false,
                                        ),
                                        // Nothing in the workspace reads the pizza index over
                                        // the service yet, so the mock answers an empty snapshot.
                                        ClientRequestKind::GetPentagonPizzaIndex(_) => (
                                            ObservedRequest::Rpc("pentagon_pizza_index"),
                                            proto_response::response::Response::PentagonPizzaIndex(
                                                proto_response::PentagonPizzaIndexResponse {
                                                    pentagon_pizza_index: None,
                                                },
                                            ),
                                            false,
                                        ),
                                    };
                                    if request_sender.send((connection_id, observed)).is_err() {
                                        return;
                                    }
                                    if drop_next_response.swap(false, Ordering::AcqRel) {
                                        return;
                                    }
                                    if stall_next_response.swap(false, Ordering::AcqRel) {
                                        response_stalled.notify_one();
                                        loop {
                                            tokio::select! {
                                                () = resume_response.notified() => break,
                                                message = challenges.recv() => {
                                                    let Ok(message) = message else { return };
                                                    if websocket
                                                        .send(Frame::binary(
                                                            message.encode_to_vec(),
                                                        ))
                                                        .await
                                                        .is_err()
                                                    {
                                                        return;
                                                    }
                                                }
                                            }
                                        }
                                        if send_old_event_before_response
                                            .swap(false, Ordering::AcqRel)
                                        {
                                            let old_event = ServerMessage {
                                                message: Some(
                                                    server_message::Message::Event(
                                                        proto_event::Event {
                                                            event: Some(
                                                                proto_event::event::Event::WalletTransfer(
                                                                    proto_types::WalletTransfer {
                                                                        event_id: "old-session-event".to_owned(),
                                                                        from_address: "0x0000000000000000000000000000000000000001".to_owned(),
                                                                        to_address: "0x0000000000000000000000000000000000000002".to_owned(),
                                                                        amount: "1".to_owned(),
                                                                        token_symbol: "ETH".to_owned(),
                                                                        timestamp: 1,
                                                                    },
                                                                ),
                                                            ),
                                                        },
                                                    ),
                                                ),
                                            };
                                            let sent = websocket
                                                .send(Frame::binary(old_event.encode_to_vec()))
                                                .await;
                                            old_event_attempted.notify_one();
                                            if sent.is_err() {
                                                return;
                                            }
                                        }
                                    }
                                    let response = override_next_response
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .take()
                                        .unwrap_or(response);
                                    if unknown_oneof_next.swap(false, Ordering::AcqRel) {
                                        let _ = websocket
                                            .send(Frame::binary(UNKNOWN_SERVER_ONEOF_WIRE.to_vec()))
                                            .await;
                                        return;
                                    }
                                    let mut response = if matches!(
                                        &response,
                                        proto_response::response::Response::Reauthenticate(_)
                                    ) && authentication_unavailable_next
                                        .swap(false, Ordering::AcqRel)
                                    {
                                        proto_response::response::Response::Error(
                                            proto_response::ErrorResponse {
                                                code: ErrorCode::AuthenticationUnavailable as i32,
                                            },
                                        )
                                    } else if error_next.swap(false, Ordering::AcqRel) {
                                        proto_response::response::Response::Error(
                                            proto_response::ErrorResponse {
                                                code: ErrorCode::RateLimit as i32,
                                            },
                                        )
                                    } else {
                                        response
                                    };
                                    if duplicate_topics_next.swap(false, Ordering::AcqRel)
                                        && let proto_response::response::Response::SetSubscriptions(
                                            subscriptions,
                                        ) = &mut response
                                        && let Some(topic) = subscriptions.topics.first().cloned()
                                    {
                                        subscriptions.topics.push(topic);
                                    }
                                    if pending_x_add_next.swap(false, Ordering::AcqRel)
                                        && let proto_response::response::Response::XAddSelection(
                                            add,
                                        ) = &mut response
                                    {
                                        add.selection = Some(pending_x_selection());
                                    }
                                    let response = ServerMessage {
                                        message: Some(server_message::Message::Response(
                                            proto_response::Response {
                                                id: request.id,
                                                response: Some(response),
                                            },
                                        )),
                                    };
                                    if websocket
                                        .send(Frame::binary(response.encode_to_vec()))
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                    if disconnect {
                                        disconnect_first.notified().await;
                                        return;
                                    }
                                }
                            });
                            Ok::<_, WebSocketError>(response)
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades()
                        .await;
                });
            }
        });
        Self {
            url,
            connection_count,
            challenge_sender,
            requests,
            error_next,
            duplicate_topics_next,
            unknown_oneof_next,
            drop_next_response,
            override_next_response,
            pending_x_add_next,
            stall_next_response,
            response_stalled,
            resume_response,
            cancellation,
            task,
        }
    }

    pub(super) async fn next_request(&mut self) -> (usize, ObservedRequest) {
        timeout(Duration::from_secs(2), self.requests.recv())
            .await
            .expect("Kytrix service request should arrive")
            .expect("test service request channel should remain open")
    }

    pub(super) async fn assert_no_request(&mut self) {
        assert!(
            timeout(Duration::from_millis(100), self.requests.recv())
                .await
                .is_err(),
            "service should not receive another request"
        );
    }

    pub(super) fn send_wallet_transfer(&self, event_id: &str) {
        self.send_event(proto_event::event::Event::WalletTransfer(
            proto_types::WalletTransfer {
                event_id: event_id.to_owned(),
                from_address: "0x0000000000000000000000000000000000000001".to_owned(),
                to_address: "0x0000000000000000000000000000000000000002".to_owned(),
                amount: "1".to_owned(),
                token_symbol: "ETH".to_owned(),
                timestamp: 1,
            },
        ));
    }

    pub(super) fn send_ais_delta(&self) {
        self.send_event(proto_event::event::Event::Ais(ais_delta()));
    }

    pub(super) fn send_ais_keyframe(&self) {
        self.send_event(proto_event::event::Event::Ais(ais_keyframe()));
    }

    pub(super) fn send_news(&self, content: &str) {
        self.send_event(proto_event::event::Event::News(proto_types::News {
            timestamp: 1,
            title: None,
            content: content.to_owned(),
            kind: proto_types::NewsKind::Crypto as i32,
        }));
    }

    pub(super) fn send_index(&self, code: &str) {
        self.send_event(proto_event::event::Event::Index(proto_types::Index {
            code: code.to_owned(),
            last_price: 101,
            open_price: 100,
            high_price: 102,
            low_price: 99,
            close_price: 100,
            is_trading: true,
            timestamp: 1,
        }));
    }

    pub(super) fn send_x_profile(&self) {
        self.send_event(proto_event::event::Event::XProfile(x_profile()));
    }

    pub(super) fn send_event(&self, event: proto_event::event::Event) {
        self.challenge_sender
            .send(ServerMessage {
                message: Some(server_message::Message::Event(proto_event::Event {
                    event: Some(event),
                })),
            })
            .expect("connected service should receive the event");
    }

    pub(super) fn fail_next_request(&self) {
        self.error_next.store(true, Ordering::Release);
    }

    pub(super) fn duplicate_topics_in_next_ack(&self) {
        self.duplicate_topics_next.store(true, Ordering::Release);
    }

    pub(super) fn send_unknown_oneof_instead_of_next_ack(&self) {
        self.unknown_oneof_next.store(true, Ordering::Release);
    }

    pub(super) fn drop_next_response(&self) {
        self.drop_next_response.store(true, Ordering::Release);
    }

    /// Answers the next request with `response` instead of the one its kind would produce.
    ///
    /// This is how a caller is tested against a service that answers a valid request with
    /// something else: a response of another kind, or one of the same kind without data.
    pub(super) fn answer_next_request_with(&self, response: proto_response::response::Response) {
        *self
            .override_next_response
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(response);
    }

    /// Answers the next X add with the row a first-ever add gets back, which has no profile.
    pub(super) fn answer_x_add_with_pending_selection(&self) {
        self.pending_x_add_next.store(true, Ordering::Release);
    }

    pub(super) fn stall_next_response(&self) {
        self.stall_next_response.store(true, Ordering::Release);
    }

    pub(super) async fn wait_for_stalled_response(&self) {
        timeout(Duration::from_secs(2), self.response_stalled.notified())
            .await
            .expect("Kytrix service response should stall");
    }

    pub(super) fn resume_response(&self) {
        self.resume_response.notify_one();
    }

    pub(super) async fn shutdown(self) {
        self.cancellation.cancel();
        self.task.await.expect("test service should stop cleanly");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manager stands in for the application's whole route table in every test that
    /// routes through it, so a capability missing from this list would silently keep the
    /// generation a route change must advance.
    #[test]
    fn every_kytrix_service_capability_should_be_stamped_when_the_default_line_moves() {
        let mut manager = TestConfigManager::in_memory(Config::default());

        manager.update(|config| config.proxy_default = ProxyRoute::System);

        let stamped = &manager.read().service_route_generations;
        assert_eq!(stamped.len(), 12, "every capability is a stamped target");
        assert!(
            stamped.contains_key(&ServiceTarget::KytrixService {
                capability: KytrixServiceCapability::Cmc,
            }),
            "the CMC capability should have a route generation"
        );
    }
}

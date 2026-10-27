use std::{
    collections::BTreeMap,
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        Arc, LazyLock, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ahash::{AHashMap, AHashSet};
use app_config::network::{Config, KytrixServiceCapability, NetworkRouteGeneration, ResolvedProxy};
use app_config_provider::notification::ConfigNotification;
use bytes::Bytes;
use kytrix_proto::{
    client::request::Request as ClientRequestKind, server as proto_response, types as proto_types,
};
use network::transport::{
    error::TransportError,
    hub::{TransportHub, TransportPolicy, TransportScope},
    subscription::{SubscriptionPublisher, SubscriptionSendError},
    websocket::{EndpointAccess, RawWebSocket, WsEndpointId, WsSessionKey},
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use thiserror::Error;
use tokio::{
    sync::{Notify, mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Instant, sleep_until, timeout_at},
};
use tokio_util::sync::CancellationToken;
use url::Url;
use widget_tokio_runtime::runtime::Runtime;
use yawc::{Frame, WebSocketError};

use super::{
    ais::{AisArea, AisUpdate},
    connector::KytrixServiceConnector,
};
use crate::{
    account_profile::AccountProfile,
    auth::{
        model::{AuthError, SessionChange},
        session::AuthSession,
    },
};

pub type ErrorCode = kytrix_proto::server::ErrorCode;

pub(super) static KYTRIX_SERVICE_WS_URL: LazyLock<Url> = LazyLock::new(|| {
    cfg_select! {
        feature = "production" => "wss://api.kytrix.xyz",
        _ => "ws://api.localhost",
    }
    .parse()
    .unwrap_or_else(|_| panic!("invalid ws url"))
});
pub(super) const COMMAND_CAPACITY: usize = 128;
pub(super) const SUBSCRIBER_CAPACITY: usize = 128;
pub(super) const OPERATION_TIMEOUT: Duration = Duration::from_secs(15);
pub(super) const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const HEARTBEAT_MIN_SECONDS: u64 = 225;
pub(super) const HEARTBEAT_MAX_SECONDS: u64 = 255;
pub(super) const PONG_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const SUBSCRIBER_SWEEP_INTERVAL: Duration = Duration::from_secs(1);
pub(super) const RENEWAL_RETRY_INTERVAL: Duration = Duration::from_secs(5);
pub(super) const RENEWAL_RESPONSE_GRACE: Duration = Duration::from_secs(15);
pub(super) const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(250);
pub(super) const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(8);
pub(super) const MAX_PENDING_RPCS: usize = 32;

pub(super) async fn await_account_operation<T>(
    shutdown: &CancellationToken,
    cancellation: &CancellationToken,
    deadline: Instant,
    operation: impl Future<Output = Result<T, KytrixServiceError>>,
) -> Result<T, KytrixServiceError> {
    tokio::select! {
        biased;
        () = shutdown.cancelled() => Err(KytrixServiceError::Closed),
        () = cancellation.cancelled() => Err(KytrixServiceError::Closed),
        result = timeout_at(deadline, operation) => {
            result.map_err(|_| KytrixServiceError::Timeout)?
        }
    }
}

pub(super) async fn service_account_profile(
    auth: &AuthSession,
    expected_account_id: &str,
) -> Result<AccountProfile, KytrixServiceError> {
    let profile = auth
        .current_profile(expected_account_id)
        .await
        .map_err(|error| {
            KytrixServiceError::Authentication(smol_str::format_smolstr!("{}", error))
        })?;
    Ok(profile)
}

pub(super) async fn service_authorization_headers_for_renewal(
    auth: &AuthSession,
    profile: &AccountProfile,
    method: &str,
    request_url: &Url,
    nonce: &str,
) -> Result<(String, String), RenewalAttemptError> {
    auth.authorization_headers_for_renewal(profile, method, request_url.as_str(), nonce)
        .await
        .map_err(classify_renewal_auth_error)
}

pub(super) fn classify_renewal_auth_error(error: AuthError) -> RenewalAttemptError {
    match error {
        AuthError::RefreshOutcomeUnknown => RenewalAttemptError::OutcomeUnknown,
        AuthError::Request | AuthError::HttpClient | AuthError::SecureRandom => {
            RenewalAttemptError::Temporary
        }
        AuthError::Status(status)
            if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS =>
        {
            RenewalAttemptError::Temporary
        }
        error => RenewalAttemptError::Fatal(KytrixServiceError::Authentication(
            smol_str::format_smolstr!("{}", error),
        )),
    }
}

pub(super) async fn service_authorization_headers_for(
    auth: &AuthSession,
    profile: &AccountProfile,
    method: &str,
    request_url: &Url,
) -> Result<(String, String), KytrixServiceError> {
    auth.authorization_headers_for(profile, method, request_url.as_str())
        .await
        .map_err(|error| KytrixServiceError::Authentication(smol_str::format_smolstr!("{}", error)))
}

pub(super) async fn service_authorization_headers_with_profile(
    auth: &AuthSession,
    method: &str,
    request_url: &Url,
) -> Result<(AccountProfile, (String, String)), KytrixServiceError> {
    let (profile, headers) = auth
        .authorization_headers_with_profile(method, request_url.as_str())
        .await
        .map_err(|error| {
            KytrixServiceError::Authentication(smol_str::format_smolstr!("{}", error))
        })?;
    Ok((profile, headers))
}

pub(super) fn authorization_url(request_url: &Url) -> Result<Url, KytrixServiceError> {
    let mut url = request_url.clone();
    let scheme = match request_url.scheme() {
        "http" | "https" => return Ok(url),
        "ws" => "http",
        "wss" => "https",
        scheme => {
            return Err(KytrixServiceError::InvalidUrl(smol_str::format_smolstr!(
                "unsupported Kytrix service URL scheme {scheme}"
            )));
        }
    };
    url.set_scheme(scheme).map_err(|_| {
        KytrixServiceError::InvalidUrl("Kytrix service URL scheme is invalid".into())
    })?;
    url.set_fragment(None);
    Ok(url)
}

pub(super) fn kytrix_service_endpoint(url: &Url) -> Result<WsEndpointId, KytrixServiceError> {
    let host = url
        .host_str()
        .ok_or_else(|| KytrixServiceError::InvalidUrl("Kytrix service host is missing".into()))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| KytrixServiceError::InvalidUrl("Kytrix service port is missing".into()))?;
    let mut path = url.path().to_owned();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    Ok(WsEndpointId {
        protocol_family: SmolStr::new_static("kytrix-service"),
        host: host.into(),
        port,
        path: path.into(),
        region: None,
        access: EndpointAccess::Private,
    })
}

pub(super) fn normalize_topics(topics: &[ServiceTopic]) -> Vec<ServiceTopic> {
    let mut topics = topics.to_vec();
    topics.sort_unstable();
    topics.dedup();
    topics
}

/// A news kind carried by service events.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum NewsKind {
    #[serde(rename = "general")]
    General,
    #[serde(rename = "crypto")]
    Crypto,
    #[serde(rename = "political")]
    Political,
}

/// A validated index code exposed by the Kytrix service.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct FinancialIndexTopic(SmolStr);

impl FinancialIndexTopic {
    /// Returns the wire/API code for this topic.
    pub fn code(&self) -> &str {
        &self.0
    }

    /// Validates a protocol/API code supplied by the server or persisted config.
    pub fn from_code(code: &str) -> Option<Self> {
        (!code.is_empty() && code.trim() == code && code.chars().count() <= 64)
            .then(|| Self(code.into()))
    }
}

impl<'de> Deserialize<'de> for FinancialIndexTopic {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let code = SmolStr::deserialize(deserializer)?;
        Self::from_code(&code).ok_or_else(|| {
            <D::Error as serde::de::Error>::custom(
                "index code must be 1 to 64 characters without surrounding whitespace",
            )
        })
    }
}

impl fmt::Display for FinancialIndexTopic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

/// A topic a Kytrix service subscription follows.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ServiceTopic {
    News,
    FinancialIndex(FinancialIndexTopic),
    X,
    CryptocurrencyWalletMonitoring,
    Ais(AisArea),
}

/// A news event received from the service.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct NewsEvent {
    pub timestamp: i64,
    pub title: Option<SmolStr>,
    pub content: SmolStr,
    pub category: NewsKind,
}

/// An index event received from the service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinancialIndexEvent {
    pub topic: FinancialIndexTopic,
    pub last_price: u32,
    pub open_price: u32,
    pub high_price: u32,
    pub low_price: u32,
    pub close_price: u32,
    pub is_trading: bool,
    pub timestamp: i64,
}

/// A validated X profile received from the service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XProfileEvent {
    pub x_user_id: SmolStr,
    pub username: SmolStr,
    pub display_name: SmolStr,
    pub avatar_url: SmolStr,
    pub verified: bool,
    pub revision: u64,
}

/// A validated Tweet reference received from the service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TweetReference {
    Repost(Box<TweetEvent>),
    Quote(Box<TweetEvent>),
}

/// Validated media attached to a Tweet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TweetMedia {
    Image {
        image_url: SmolStr,
        width: u32,
        height: u32,
    },
    Video {
        playback_url: SmolStr,
        thumbnail_url: SmolStr,
        width: u32,
        height: u32,
    },
    Gif {
        animation_url: SmolStr,
        image_url: SmolStr,
        width: u32,
        height: u32,
    },
}

/// A validated text segment in a Tweet received from the service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TweetTextSegment {
    pub text: SmolStr,
    pub url: Option<SmolStr>,
}

/// A validated Tweet received from the service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TweetEvent {
    pub tweet_id: SmolStr,
    pub timestamp: i64,
    pub x_user_id: SmolStr,
    pub text_segments: Vec<TweetTextSegment>,
    pub reference: Option<TweetReference>,
    pub media: Vec<TweetMedia>,
}

/// A validated X timeline snapshot received over HTTP.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TweetBatch {
    pub tweets: Vec<TweetEvent>,
    pub profiles: Vec<XProfileEvent>,
}

/// A validated cryptocurrency_wallet_monitoring transfer received from the service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CryptocurrencyWalletMonitoringTransfer {
    pub event_id: SmolStr,
    pub from_address: SmolStr,
    pub to_address: SmolStr,
    pub amount: SmolStr,
    pub token_symbol: SmolStr,
    pub timestamp: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum KytrixServiceEvent {
    News(NewsEvent),
    FinancialIndex(FinancialIndexEvent),
    XProfile(XProfileEvent),
    Tweet(TweetEvent),
    CryptocurrencyWalletMonitoringTransfer(CryptocurrencyWalletMonitoringTransfer),
    Ais(AisUpdate),
    SessionState(KytrixSessionState),
}

pub(super) enum SubscriberSink {
    News(SubscriptionPublisher<NewsEvent>),
    FinancialIndices(SubscriptionPublisher<FinancialIndexEvent>),
    X(SubscriptionPublisher<XSubscriptionEvent>),
    CryptocurrencyWalletMonitoring(SubscriptionPublisher<CryptocurrencyWalletMonitoringTransfer>),
    Ais(SubscriptionPublisher<AisUpdate>),
}

impl SubscriberSink {
    pub(super) const fn capability(&self) -> KytrixServiceCapability {
        match self {
            Self::News(_) => KytrixServiceCapability::News,
            Self::FinancialIndices(_) => KytrixServiceCapability::FinancialIndices,
            Self::X(_) => KytrixServiceCapability::X,
            Self::CryptocurrencyWalletMonitoring(_) => {
                KytrixServiceCapability::CryptocurrencyWalletMonitoring
            }
            Self::Ais(_) => KytrixServiceCapability::Ais,
        }
    }

    pub(super) fn activate(&self) {
        match self {
            Self::News(publisher) => publisher.activate(),
            Self::FinancialIndices(publisher) => publisher.activate(),
            Self::X(publisher) => publisher.activate(),
            Self::CryptocurrencyWalletMonitoring(publisher) => publisher.activate(),
            Self::Ais(publisher) => publisher.activate(),
        }
    }

    pub(super) fn reconnecting(&self) {
        match self {
            Self::News(publisher) => publisher.reconnecting(),
            Self::FinancialIndices(publisher) => publisher.reconnecting(),
            Self::X(publisher) => publisher.reconnecting(),
            Self::CryptocurrencyWalletMonitoring(publisher) => publisher.reconnecting(),
            Self::Ais(publisher) => publisher.reconnecting(),
        }
    }

    pub(super) fn fail(&self, error: TransportError) {
        match self {
            Self::News(publisher) => publisher.fail(error),
            Self::FinancialIndices(publisher) => publisher.fail(error),
            Self::X(publisher) => publisher.fail(error),
            Self::CryptocurrencyWalletMonitoring(publisher) => publisher.fail(error),
            Self::Ais(publisher) => publisher.fail(error),
        }
    }

    pub(super) fn finish(&self) {
        match self {
            Self::News(publisher) => publisher.finish(),
            Self::FinancialIndices(publisher) => publisher.finish(),
            Self::X(publisher) => publisher.finish(),
            Self::CryptocurrencyWalletMonitoring(publisher) => publisher.finish(),
            Self::Ais(publisher) => publisher.finish(),
        }
    }

    pub(super) fn is_cancelled(&self) -> bool {
        match self {
            Self::News(publisher) => publisher.is_cancelled(),
            Self::FinancialIndices(publisher) => publisher.is_cancelled(),
            Self::X(publisher) => publisher.is_cancelled(),
            Self::CryptocurrencyWalletMonitoring(publisher) => publisher.is_cancelled(),
            Self::Ais(publisher) => publisher.is_cancelled(),
        }
    }

    pub(super) fn try_send(&self, event: &KytrixServiceEvent) -> Result<(), TransportError> {
        match (self, event) {
            (Self::News(publisher), KytrixServiceEvent::News(event)) => publisher
                .try_send(event.clone())
                .map_err(subscription_send_error),
            (Self::FinancialIndices(publisher), KytrixServiceEvent::FinancialIndex(event)) => {
                publisher
                    .try_send(event.clone())
                    .map_err(subscription_send_error)
            }
            (Self::X(publisher), KytrixServiceEvent::XProfile(profile)) => publisher
                .try_send(XSubscriptionEvent::Profile(profile.clone()))
                .map_err(subscription_send_error),
            (Self::X(publisher), KytrixServiceEvent::Tweet(tweet)) => publisher
                .try_send(XSubscriptionEvent::Tweet(tweet.clone()))
                .map_err(subscription_send_error),
            (
                Self::CryptocurrencyWalletMonitoring(publisher),
                KytrixServiceEvent::CryptocurrencyWalletMonitoringTransfer(transfer),
            ) => publisher
                .try_send(transfer.clone())
                .map_err(subscription_send_error),
            (Self::Ais(publisher), KytrixServiceEvent::Ais(update)) => publisher
                .try_send(update.clone())
                .map_err(subscription_send_error),
            _ => Ok(()),
        }
    }
}

pub(super) fn subscription_send_error<T>(error: SubscriptionSendError<T>) -> TransportError {
    match error {
        SubscriptionSendError::SlowSubscriber(_) => TransportError::SlowSubscriber,
        SubscriptionSendError::Closed(_) => TransportError::Closed,
    }
}

/// The entitlement value the Kytrix service publishes for one feature.
pub type KytrixEntitlementValue = proto_types::EntitlementValue;

/// One catalogue entry, as the Kytrix service publishes it.
pub type KytrixFeatureDefinition = proto_types::FeatureDefinition;

/// The entitlements the Kytrix service published for one session.
#[derive(Clone, Debug, PartialEq)]
pub struct KytrixSessionState {
    pub entitlement_revision: u64,
    pub plan_code: proto_types::PlanCode,
    pub entitlements: BTreeMap<String, KytrixEntitlementValue>,
    pub catalog: Vec<KytrixFeatureDefinition>,
    pub next_change_at: Option<i64>,
}

#[derive(Clone)]
pub(super) struct SessionStateRegistry {
    pub states: Arc<Mutex<BTreeMap<u64, KytrixSessionState>>>,
    pub observed: watch::Sender<Option<KytrixSessionState>>,
}

impl SessionStateRegistry {
    pub(super) fn new() -> Self {
        let (observed, _) = watch::channel(None);
        Self {
            states: Arc::new(Mutex::new(BTreeMap::new())),
            observed,
        }
    }

    pub(super) fn subscribe(&self) -> watch::Receiver<Option<KytrixSessionState>> {
        self.observed.subscribe()
    }

    pub(super) fn publish(&self, generation: u64, state: KytrixSessionState) {
        let next = {
            let mut states = self
                .states
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            states.insert(generation, state);
            states.last_key_value().map(|(_, state)| state.clone())
        };
        self.observed.send_replace(next);
    }

    pub(super) fn clear(&self, generation: u64) {
        let next = {
            let mut states = self
                .states
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            states.remove(&generation);
            states.last_key_value().map(|(_, state)| state.clone())
        };
        self.observed.send_replace(next);
    }

    pub(super) fn clear_all(&self) {
        self.states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.observed.send_replace(None);
    }
}

/// Errors raised by the shared Kytrix service connection.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum KytrixServiceError {
    #[error("Kytrix service URL is invalid: {0}")]
    InvalidUrl(SmolStr),
    #[error("Kytrix service connection failed: {0}")]
    Connect(SmolStr),
    #[error("Kytrix service authentication failed: {0}")]
    Authentication(SmolStr),
    #[error("Kytrix service read failed: {0}")]
    Read(SmolStr),
    #[error("Kytrix service write failed: {0}")]
    Write(SmolStr),
    #[error("Kytrix service operation timed out")]
    Timeout,
    #[error("Kytrix service connector is closed")]
    Closed,
    #[error("Kytrix service protocol error: {0}")]
    Protocol(SmolStr),
    #[error("Kytrix service server error: {}", server_error_name(*.0))]
    Server(ErrorCode),
    #[error("Kytrix service WebSocket disconnected")]
    Disconnected,
    #[error("Kytrix service subscription fell behind")]
    SlowSubscriber,
    #[error("Kytrix service account session changed")]
    AccountChanged,
    #[error("Kytrix service proxy route changed")]
    RouteChanged,
}

pub(super) const fn server_error_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::Unspecified => "UNSPECIFIED",
        ErrorCode::RateLimit => "RATE_LIMIT",
        ErrorCode::EntitlementRequired => "ENTITLEMENT_REQUIRED",
        ErrorCode::FinancialIndexTopicLimitExceeded => "INDEX_TOPIC_LIMIT_EXCEEDED",
        ErrorCode::InvalidTopic => "INVALID_TOPIC",
        ErrorCode::AuthenticationUnavailable => "AUTHENTICATION_UNAVAILABLE",
        ErrorCode::InvalidRequest => "INVALID_REQUEST",
        ErrorCode::NotReady => "NOT_READY",
        ErrorCode::InvalidArgument => "INVALID_ARGUMENT",
        ErrorCode::NotFound => "NOT_FOUND",
        ErrorCode::QuotaExceeded => "QUOTA_EXCEEDED",
        ErrorCode::ProtectedAccount => "PROTECTED_ACCOUNT",
        ErrorCode::UpstreamUnavailable => "UPSTREAM_UNAVAILABLE",
        ErrorCode::Timeout => "TIMEOUT",
        ErrorCode::MutationUnknown => "MUTATION_UNKNOWN",
        ErrorCode::Protocol => "PROTOCOL",
    }
}

pub(super) fn subscription_setup_error(error: TransportError) -> KytrixServiceError {
    KytrixServiceError::Protocol(smol_str::format_smolstr!("{}", error))
}

pub(super) fn subscription_transport_error(error: &KytrixServiceError) -> TransportError {
    match error {
        KytrixServiceError::SlowSubscriber => TransportError::SlowSubscriber,
        KytrixServiceError::AccountChanged | KytrixServiceError::Authentication(_) => {
            TransportError::CredentialChanged
        }
        KytrixServiceError::RouteChanged => TransportError::RouteChanged,
        KytrixServiceError::Closed => TransportError::Closed,
        KytrixServiceError::Connect(_)
        | KytrixServiceError::Read(_)
        | KytrixServiceError::Write(_)
        | KytrixServiceError::Timeout
        | KytrixServiceError::Protocol(_)
        | KytrixServiceError::Server(_)
        | KytrixServiceError::Disconnected
        | KytrixServiceError::InvalidUrl(_) => TransportError::WebSocketIo,
    }
}

/// Account-bound handle for service HTTP calls and event subscriptions.
#[derive(Clone)]
pub struct KytrixAccountSession {
    pub connector: KytrixServiceConnector,
    pub profile: AccountProfile,
}

pub(super) struct KytrixServiceConnectorInner {
    pub commands: mpsc::Sender<SupervisorCommand>,
    pub auth: AuthSession,
    pub lifecycle: TransportScope,
    pub transport: Arc<TransportHub>,
    pub websocket_url: Url,
    pub config: ConfigNotification<Config>,
    pub cancellation: CancellationToken,
    pub wake: Arc<Notify>,
    pub supervisor_task: Mutex<Option<JoinHandle<()>>>,
    /// The task that turns one publication of the network projection into a
    /// [`SupervisorCommand::Reconfigure`] for every live session.
    ///
    /// [`KytrixServiceConnector::install`] is the production entry point for
    /// this task; every `KytrixServiceConnector` constructor starts it.
    pub config_task: Mutex<Option<JoinHandle<()>>>,
    pub next_subscriber_id: AtomicU64,
    pub session_state: SessionStateRegistry,
    /// The application runtime every route resolution of this connector runs on.
    pub runtime: Runtime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum XSubscriptionEvent {
    Profile(XProfileEvent),
    Tweet(TweetEvent),
}

pub(super) enum SupervisorCommand {
    /// Delivers one command to the session owning `key`, after checking that the
    /// caller's profile and route generation are still current.
    Send {
        key: WsSessionKey,
        profile: AccountProfile,
        command: Box<Command>,
    },
    AcquireAccount {
        profile: AccountProfile,
        cancellation: CancellationToken,
        deadline: Instant,
        respond_to: oneshot::Sender<Result<(), KytrixServiceError>>,
    },
    /// Tells every live session to re-read the network projection it routes by
    /// and retire the subscriptions the new projection no longer serves.
    Reconfigure,
}

pub(super) enum Command {
    Subscribe {
        id: u64,
        topics: Vec<ServiceTopic>,
        expected_profile: Option<AccountProfile>,
        route_generation: NetworkRouteGeneration,
        sink: SubscriberSink,
        cancellation: CancellationToken,
        deadline: Instant,
        respond_to: oneshot::Sender<Result<(), KytrixServiceError>>,
    },
    Rpc {
        request: Option<ClientRequestKind>,
        expected_profile: AccountProfile,
        capability: KytrixServiceCapability,
        route_generation: NetworkRouteGeneration,
        cancellation: CancellationToken,
        possibly_submitted: Arc<AtomicBool>,
        deadline: Instant,
        respond_to: oneshot::Sender<Result<proto_response::Response, KytrixServiceError>>,
    },
    RetireCredentials,
    /// Re-reads the network projection and retires the subscriptions whose
    /// route generation or resolved route it no longer serves.
    Reconfigure,
}

pub(super) struct LocalSubscriber {
    pub topics: Vec<ServiceTopic>,
    pub capability: KytrixServiceCapability,
    pub route_generation: NetworkRouteGeneration,
    pub expected_profile: Option<AccountProfile>,
    pub sink: SubscriberSink,
}

pub(super) struct SubscriptionRouteFence {
    pub subscriber_id: u64,
    pub topics: Vec<ServiceTopic>,
    pub capability: KytrixServiceCapability,
    pub route_generation: NetworkRouteGeneration,
}

pub(super) struct PendingRpc {
    pub expected_profile: AccountProfile,
    pub capability: KytrixServiceCapability,
    pub route_generation: NetworkRouteGeneration,
    pub cancellation: CancellationToken,
    pub deadline: Instant,
    pub respond_to: oneshot::Sender<Result<proto_response::Response, KytrixServiceError>>,
}

pub(super) struct BeginRpcRequest {
    pub request: Option<ClientRequestKind>,
    pub expected_profile: AccountProfile,
    pub capability: KytrixServiceCapability,
    pub route_generation: NetworkRouteGeneration,
    pub deadline: Instant,
    pub cancellation: CancellationToken,
    pub possibly_submitted: Arc<AtomicBool>,
    pub respond_to: oneshot::Sender<Result<proto_response::Response, KytrixServiceError>>,
}

pub(super) struct RpcCallError {
    pub error: KytrixServiceError,
    pub possibly_submitted: bool,
}

impl RpcCallError {
    pub(super) fn not_submitted(error: KytrixServiceError) -> Self {
        Self {
            error,
            possibly_submitted: false,
        }
    }

    pub(super) fn into_error(self) -> KytrixServiceError {
        self.error
    }

    pub(super) fn into_parts(self) -> (KytrixServiceError, bool) {
        (self.error, self.possibly_submitted)
    }
}

pub(super) type RenewalFuture =
    Pin<Box<dyn Future<Output = Result<(String, String), RenewalAttemptError>> + Send + 'static>>;

#[derive(Debug, Eq, PartialEq)]
pub(super) enum RenewalAttemptError {
    Temporary,
    OutcomeUnknown,
    Fatal(KytrixServiceError),
}

#[derive(Clone)]
pub(super) struct RenewalChallenge {
    pub nonce: String,
    pub expires_at: i64,
}

pub(super) enum RenewalStage {
    Preparing(RenewalFuture),
    Waiting(Pin<Box<tokio::time::Sleep>>),
    WaitingForExpiry,
    AwaitingResponse { request_id: u32 },
}

pub(super) struct PendingRenewal {
    pub challenge: RenewalChallenge,
    pub stage: RenewalStage,
    pub deadline: Instant,
}

pub(super) enum RenewalPoll {
    Prepared(Result<(String, String), RenewalAttemptError>),
    RetryReady,
    Expired,
}

pub(super) fn renewal_pollable(renewal: &Option<PendingRenewal>) -> bool {
    renewal.is_some()
}

pub(super) async fn poll_renewal(renewal: &mut Option<PendingRenewal>) -> RenewalPoll {
    let Some(renewal) = renewal.as_mut() else {
        return RenewalPoll::Expired;
    };
    let stage = &mut renewal.stage;
    let deadline = renewal.deadline;
    tokio::select! {
        biased;
        () = sleep_until(deadline) => RenewalPoll::Expired,
        poll = async {
            match stage {
                RenewalStage::Preparing(future) => RenewalPoll::Prepared(future.await),
                RenewalStage::Waiting(timer) => {
                    timer.await;
                    RenewalPoll::RetryReady
                }
                RenewalStage::WaitingForExpiry => {
                    std::future::pending::<RenewalPoll>().await
                }
                RenewalStage::AwaitingResponse { .. } => {
                    std::future::pending::<RenewalPoll>().await
                }
            }
        } => poll,
    }
}

pub(super) fn service_unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

pub(super) fn service_unix_deadline(timestamp: i64) -> Instant {
    Instant::now()
        + Duration::from_secs(
            u64::try_from(timestamp.saturating_sub(service_unix_now())).unwrap_or(0),
        )
}

pub(super) fn valid_protocol_identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 200 && value.bytes().all(|byte| byte.is_ascii_graphic())
}

pub(super) struct ServiceSessionEntry {
    pub generation: u64,
    pub commands: mpsc::Sender<Command>,
}

pub(super) struct ServiceSessionExit {
    pub key: WsSessionKey,
    pub generation: u64,
}

#[derive(Clone)]
pub(super) struct ServiceSessionContext {
    pub websocket_url: Url,
    pub config: ConfigNotification<Config>,
    pub auth: AuthSession,
    pub account_profile: Arc<RwLock<Option<AccountProfile>>>,
    pub credential_invalidations: watch::Receiver<SessionChange>,
    pub session_state: SessionStateRegistry,
    pub cancellation: CancellationToken,
    pub wake: Arc<Notify>,
    pub transport_policy: TransportPolicy,
    /// The application runtime every route resolution of a session runs on.
    pub runtime: Runtime,
}

pub(super) struct ServiceSessionActor {
    pub commands: mpsc::Receiver<Command>,
    pub cancellation: CancellationToken,
    pub wake: Arc<Notify>,
    pub websocket_url: Url,
    pub proxy: ResolvedProxy,
    pub os_proxy_fingerprint: u64,
    pub config: ConfigNotification<Config>,
    pub auth: AuthSession,

    pub next_request_id: u32,
    pub pending_rpcs: AHashMap<u32, PendingRpc>,
    pub subscription_route_change_reuses_socket: bool,
    pub subscribers: AHashMap<u64, LocalSubscriber>,
    pub topics: AHashMap<ServiceTopic, AHashSet<u64>>,
    pub latest_financial_indices: AHashMap<FinancialIndexTopic, FinancialIndexEvent>,
    /// The last keyframe a session delivered for each AIS area.
    ///
    /// A later local subscriber of an area the session is already subscribed to would
    /// otherwise join the stream after its keyframe: the server replays a keyframe only
    /// for a topic a connection adds, and this session is one connection. The pane would
    /// be left without a baseline until the server's next keyframe (up to its 300 s
    /// fallback), so the session hands the joining subscriber the last keyframe it saw.
    pub latest_ais_keyframes: AHashMap<AisArea, AisUpdate>,
    pub session_state: Option<KytrixSessionState>,
    pub subscriptions_dirty: bool,
    pub websocket: Option<RawWebSocket>,
    pub heartbeat_deadline: Instant,
    pub sweep_deadline: Instant,
    pub pending_pong: Option<[u8; 8]>,
    pub renewal: Option<PendingRenewal>,
    pub account_profile: Arc<RwLock<Option<AccountProfile>>>,
    pub highest_credential_revision: u64,
    pub credential_invalidations: watch::Receiver<SessionChange>,
    pub credential_events_open: bool,
    pub committed_credential_change: SessionChange,
    pub observed_session_state: SessionStateRegistry,
    pub session_generation: u64,
    pub transport_policy: TransportPolicy,
    pub runtime: Runtime,
}

pub(super) enum ConnectedAction {
    Command(Result<(), KytrixServiceError>),
    Frame(Result<Frame, WebSocketError>),
    CredentialsChanged(bool),
    Renewal(RenewalPoll),
    Heartbeat,
    Sweep,
    Wake,
    SystemProxyRefreshCompleted,
}

pub(super) enum DisconnectedAction {
    Retry,
    SystemProxyRefreshCompleted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FrameDisposition {
    Continue,
    CredentialsChanged,
}

pub(super) enum IncomingMessage {
    Response(proto_response::Response),
    Event(Option<(ServiceTopic, KytrixServiceEvent)>),
    SessionState(KytrixSessionState),
    AuthChallenge(proto_response::AuthChallenge),
    Pong(Bytes),
}

pub(super) struct CancelOnDrop(pub CancellationToken);

impl CancelOnDrop {
    pub(super) fn disarm(&mut self) {
        self.0 = CancellationToken::new();
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    pub(super) fn session_state_registry_should_preserve_other_live_sessions() {
        let registry = SessionStateRegistry::new();
        let observed = registry.subscribe();
        let first = KytrixSessionState {
            entitlement_revision: 1,
            plan_code: proto_types::PlanCode::Free,
            entitlements: BTreeMap::from([("news".to_owned(), enabled_value(false))]),
            catalog: Vec::new(),
            next_change_at: None,
        };
        let second = KytrixSessionState {
            entitlement_revision: 4,
            plan_code: proto_types::PlanCode::Pro,
            entitlements: BTreeMap::from([("news".to_owned(), enabled_value(true))]),
            catalog: Vec::new(),
            next_change_at: Some(7),
        };

        registry.publish(1, first.clone());
        registry.publish(2, second.clone());
        registry.clear(1);
        assert_eq!(*observed.borrow(), Some(second));

        registry.publish(1, first.clone());
        registry.clear(2);
        assert_eq!(*observed.borrow(), Some(first));

        registry.clear(1);
        assert_eq!(*observed.borrow(), None);
    }

    fn enabled_value(enabled: bool) -> KytrixEntitlementValue {
        KytrixEntitlementValue {
            value: Some(proto_types::entitlement_value::Value::Enabled(enabled)),
        }
    }

    #[tokio::test(start_paused = true)]
    pub(super) async fn stalled_websocket_authorization_obeys_the_operation_deadline() {
        let (_commands, receiver) = mpsc::channel(1);
        let mut actor =
            ServiceSessionActor::new(receiver, CancellationToken::new(), Arc::new(Notify::new()));
        let task = tokio::spawn(async move {
            actor
                .connect_websocket_with_authorization(
                    Instant::now() + Duration::from_secs(1),
                    CancellationToken::new(),
                    std::future::pending::<
                        Result<(AccountProfile, (String, String)), KytrixServiceError>,
                    >(),
                )
                .await
        });
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(1)).await;

        assert!(matches!(
            task.await.unwrap(),
            Err(KytrixServiceError::Timeout)
        ));
    }
}

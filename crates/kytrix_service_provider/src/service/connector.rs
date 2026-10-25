use std::{
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use app_config::network::{Config, KytrixServiceCapability, RouteTarget, ServiceTarget};
#[cfg(test)]
use app_config::network::{NetworkRouteGeneration, ResolvedProxy};
use app_config_provider::{
    events::{ConfigEventBridge, ConfigTopics},
    notification::ConfigNotification,
};
use gpui_kit::App;
use kytrix_proto::{
    client::{
        GetCmcFearGreedIndex, GetEtf, GetIndexManifest, GetPentagonPizzaIndex, WalletAddSelection,
        WalletDeleteSelection, WalletSelections, XAddSelection, XDeleteSelection, XSearch,
        XSelections, XTweets, request::Request as ClientRequestKind,
    },
    server::{self as proto_response, response::Response as ServerResponseKind},
    types as proto_types,
};
#[cfg(test)]
use network::transport::error::TransportError;
use network::transport::{
    hub::{TransportHub, TransportPermit},
    subscription::{Subscription, subscription_channel_with_wake},
    websocket::WsSessionKey,
};
#[cfg(test)]
use smol_str::SmolStr;
use tokio::{
    sync::{Notify, mpsc, oneshot, watch},
    time::{Instant, timeout, timeout_at},
};
use tokio_util::sync::CancellationToken;
use url::Url;
use widget_tokio_runtime::runtime::Runtime;

use super::{
    ais::{AisArea, AisUpdate},
    handoff::request_dashboard_handoff,
    supervisor::run_service_supervisor,
    wallet::{
        self, WalletApiError, WalletMutationOutcome, WalletMutationUnknown, WalletSelection,
        WalletSelectionsResponse,
    },
    x::{
        self, TweetsResponse, XApiError, XMutationOutcome, XMutationUnknown, XProfile, XSelection,
        XSelectionsResponse,
    },
};
#[cfg(test)]
use crate::auth::test_support::test_transport;
#[cfg(test)]
use crate::service::engine::{ErrorCode, PendingRpc, XProfileEvent};
use crate::{
    account_profile::AccountProfile,
    auth::{
        constants::{API_RESOURCE, DASHBOARD_HANDOFF_PATH},
        dpop::DpopKey,
        model::{AccountIdentity, AuthError, LocalSessionStatus, PlanCode, SessionChange},
        session::{AuthSession, Revocation},
    },
    service::{
        engine::{
            COMMAND_CAPACITY, CancelOnDrop, Command, IndexEvent, IndexTopic, KYTRIX_SERVICE_WS_URL,
            KytrixAccountSession, KytrixServiceConnectorInner, KytrixServiceError,
            KytrixSessionState, NewsEvent, OPERATION_TIMEOUT, RpcCallError, SHUTDOWN_TIMEOUT,
            SUBSCRIBER_CAPACITY, ServiceSessionContext, ServiceTopic, SessionStateRegistry,
            SubscriberSink, SupervisorCommand, TweetBatch, WalletTransfer, XSubscriptionEvent,
            authorization_url, await_account_operation, kytrix_service_endpoint, normalize_topics,
            service_account_profile, service_authorization_headers_with_profile,
            subscription_setup_error,
        },
        protocol::{valid_x_id, valid_x_username, validate_rpc_response},
    },
};

/// A cloneable handle to the single Kytrix service connection actor.
#[derive(Clone)]
pub struct KytrixServiceConnector {
    inner: Arc<KytrixServiceConnectorInner>,
}

impl gpui_kit::Global for KytrixServiceConnector {}

/// Keeps the Kytrix service bridge alive for the life of the application.
///
/// The bridge holds `Subscription`s, which are neither `Send` nor `Sync`, while
/// the connector is an `Arc` shared with background work: the subscription lives
/// here, owned by the application, and only the notification it publishes into
/// travels with the connector.
struct ServiceConfigBridge {
    _bridge: ConfigEventBridge,
}

impl gpui_kit::Global for ServiceConfigBridge {}

impl KytrixAccountSession {
    pub fn profile(&self) -> &AccountProfile {
        &self.profile
    }

    pub async fn subscribe_wallet(
        &self,
    ) -> Result<Subscription<WalletTransfer>, KytrixServiceError> {
        let (publisher, subscription) = subscription_channel_with_wake(
            SUBSCRIBER_CAPACITY,
            self.connector.inner().wake.clone(),
        )
        .map_err(subscription_setup_error)?;
        self.connector
            .subscribe_topics_for_profile(
                &self.profile,
                &[ServiceTopic::Wallet],
                SubscriberSink::Wallet(publisher),
            )
            .await?;
        Ok(subscription)
    }

    pub async fn subscribe_ais(
        &self,
        area: AisArea,
    ) -> Result<Subscription<AisUpdate>, KytrixServiceError> {
        let (publisher, subscription) = subscription_channel_with_wake(
            SUBSCRIBER_CAPACITY,
            self.connector.inner().wake.clone(),
        )
        .map_err(subscription_setup_error)?;
        self.connector
            .subscribe_topics_for_profile(
                &self.profile,
                &[ServiceTopic::Ais(area)],
                SubscriberSink::Ais(publisher),
            )
            .await?;
        Ok(subscription)
    }

    pub async fn subscribe_x(
        &self,
    ) -> Result<Subscription<XSubscriptionEvent>, KytrixServiceError> {
        let (publisher, subscription) = subscription_channel_with_wake(
            SUBSCRIBER_CAPACITY,
            self.connector.inner().wake.clone(),
        )
        .map_err(subscription_setup_error)?;
        self.connector
            .subscribe_topics_for_profile(
                &self.profile,
                &[ServiceTopic::X],
                SubscriberSink::X(publisher),
            )
            .await?;
        Ok(subscription)
    }

    pub fn subscribe_session_state(&self) -> watch::Receiver<Option<KytrixSessionState>> {
        self.connector.subscribe_session_state()
    }

    pub async fn fetch_wallet_selections(
        &self,
    ) -> Result<WalletSelectionsResponse, WalletApiError> {
        let response = self
            .connector
            .call_rpc(
                Some(&self.profile),
                KytrixServiceCapability::Wallet,
                ClientRequestKind::WalletSelections(WalletSelections {}),
            )
            .await
            .map_err(|error| wallet::map_rpc_error(error.into_error()))?;
        let ServerResponseKind::WalletSelections(response) = response else {
            return Err(WalletApiError::Protocol(
                "unexpected wallet selections RPC response".into(),
            ));
        };
        let mut value = WalletSelectionsResponse {
            selections: response
                .selections
                .into_iter()
                .map(wallet::decode_wallet_selection)
                .collect::<Result<Vec<_>, _>>()?,
            wallet_address_limit: response.wallet_address_limit as usize,
        };
        wallet::validate_selections(&value)?;
        wallet::sort_selections(&mut value.selections);
        Ok(value)
    }

    pub async fn add_wallet_selection(
        &self,
        address: &str,
    ) -> WalletMutationOutcome<WalletSelection> {
        if wallet::wallet_network(address).is_none() {
            return WalletMutationOutcome::Rejected(WalletApiError::InvalidAddress);
        }
        let response = match self
            .connector
            .call_rpc(
                Some(&self.profile),
                KytrixServiceCapability::Wallet,
                ClientRequestKind::WalletAddSelection(WalletAddSelection {
                    address: address.to_owned(),
                }),
            )
            .await
        {
            Ok(response) => response,
            Err(error) => return wallet::mutation_rpc_error(error),
        };
        let ServerResponseKind::WalletAddSelection(response) = response else {
            return WalletMutationOutcome::Unknown(WalletMutationUnknown::InvalidResponse);
        };
        let Some(selection) = response.selection else {
            return WalletMutationOutcome::Unknown(WalletMutationUnknown::InvalidResponse);
        };
        match wallet::decode_wallet_selection(selection) {
            Ok(selection) if selection.address == address => {
                WalletMutationOutcome::Confirmed(selection)
            }
            Ok(_) | Err(_) => {
                WalletMutationOutcome::Unknown(WalletMutationUnknown::InvalidResponse)
            }
        }
    }

    pub async fn delete_wallet_selection(&self, address: &str) -> WalletMutationOutcome<()> {
        if wallet::wallet_network(address).is_none() {
            return WalletMutationOutcome::Rejected(WalletApiError::InvalidAddress);
        }
        match self
            .connector
            .call_rpc(
                Some(&self.profile),
                KytrixServiceCapability::Wallet,
                ClientRequestKind::WalletDeleteSelection(WalletDeleteSelection {
                    address: address.to_owned(),
                }),
            )
            .await
        {
            Ok(ServerResponseKind::WalletDeleteSelection(_)) => {
                WalletMutationOutcome::Confirmed(())
            }
            Ok(_) => WalletMutationOutcome::Unknown(WalletMutationUnknown::InvalidResponse),
            Err(error) => wallet::mutation_rpc_error(error),
        }
    }

    pub async fn search_x_users(&self, query: &str) -> Result<Vec<XProfile>, XApiError> {
        let query = x::normalize_search_query(query).ok_or(XApiError::InvalidSearchQuery)?;
        let response = self
            .connector
            .call_rpc(
                Some(&self.profile),
                KytrixServiceCapability::X,
                ClientRequestKind::XSearch(XSearch {
                    query: query.to_owned(),
                }),
            )
            .await
            .map_err(|error| x::map_rpc_error(error.into_error()))?;
        let ServerResponseKind::XSearch(response) = response else {
            return Err(XApiError::Protocol(
                "unexpected X search RPC response".into(),
            ));
        };
        let profiles = response
            .profiles
            .into_iter()
            .map(x::decode_search_profile)
            .collect::<Result<Vec<_>, _>>()?;
        x::validate_search_profiles(&profiles)?;
        Ok(profiles)
    }

    pub async fn fetch_x_selections(&self) -> Result<XSelectionsResponse, XApiError> {
        let response = self
            .connector
            .call_rpc(
                Some(&self.profile),
                KytrixServiceCapability::X,
                ClientRequestKind::XSelections(XSelections {}),
            )
            .await
            .map_err(|error| x::map_rpc_error(error.into_error()))?;
        let ServerResponseKind::XSelections(response) = response else {
            return Err(XApiError::Protocol(
                "unexpected X selections RPC response".into(),
            ));
        };
        let selections = response
            .selections
            .into_iter()
            .map(x::decode_x_selection)
            .collect::<Result<Vec<_>, _>>()?;
        let value = XSelectionsResponse {
            selections,
            x_account_limit: response.x_account_limit as usize,
        };
        x::validate_selections(&value)?;
        Ok(value)
    }

    pub async fn fetch_x_tweets(&self) -> Result<TweetBatch, XApiError> {
        let response = self
            .connector
            .call_rpc(
                Some(&self.profile),
                KytrixServiceCapability::X,
                ClientRequestKind::XTweets(XTweets {}),
            )
            .await
            .map_err(|error| x::map_rpc_error(error.into_error()))?;
        let ServerResponseKind::XTweets(response) = response else {
            return Err(XApiError::Protocol(
                "unexpected X tweets RPC response".into(),
            ));
        };
        let batch = response
            .batch
            .ok_or_else(|| XApiError::Protocol("X tweets RPC response is missing data".into()))?;
        x::decode_tweets_response(TweetsResponse {
            tweets: batch.tweets,
            profiles: batch.profiles,
        })
    }

    pub async fn add_x_selection(&self, username: &str) -> XMutationOutcome<XSelection> {
        if !valid_x_username(username) {
            return XMutationOutcome::Rejected(XApiError::InvalidUsername);
        }
        let response = match self
            .connector
            .call_rpc(
                Some(&self.profile),
                KytrixServiceCapability::X,
                ClientRequestKind::XAddSelection(XAddSelection {
                    username: username.to_owned(),
                }),
            )
            .await
        {
            Ok(response) => response,
            Err(error) => {
                let (error, possibly_submitted) = error.into_parts();
                return x::mutation_rpc_error(error, possibly_submitted);
            }
        };
        let ServerResponseKind::XAddSelection(response) = response else {
            return XMutationOutcome::Unknown(XMutationUnknown::InvalidResponse);
        };
        let Some(selection) = response.selection else {
            return XMutationOutcome::Unknown(XMutationUnknown::InvalidResponse);
        };
        match x::decode_x_selection(selection) {
            // A profile is what the service sends once the account is active, so an answer
            // without one is the row a first-ever add gets back rather than a wrong account:
            // the username is only there to check against when the row carries a profile.
            Ok(selection)
                if selection
                    .profile
                    .as_ref()
                    .is_none_or(|profile| profile.username.eq_ignore_ascii_case(username)) =>
            {
                XMutationOutcome::Confirmed(selection)
            }
            Ok(_) | Err(_) => XMutationOutcome::Unknown(XMutationUnknown::InvalidResponse),
        }
    }

    pub async fn delete_x_selection(&self, x_user_id: &str) -> XMutationOutcome<()> {
        if !valid_x_id(x_user_id) {
            return XMutationOutcome::Rejected(XApiError::Protocol("X user ID is invalid".into()));
        }
        match self
            .connector
            .call_rpc(
                Some(&self.profile),
                KytrixServiceCapability::X,
                ClientRequestKind::XDeleteSelection(XDeleteSelection {
                    x_user_id: x_user_id.to_owned(),
                }),
            )
            .await
        {
            Ok(ServerResponseKind::XDeleteSelection(_)) => XMutationOutcome::Confirmed(()),
            Ok(_) => XMutationOutcome::Unknown(XMutationUnknown::InvalidResponse),
            Err(error) => {
                let (error, possibly_submitted) = error.into_parts();
                x::mutation_rpc_error(error, possibly_submitted)
            }
        }
    }
}

impl Drop for KytrixServiceConnectorInner {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.wake.notify_waiters();
        if let Some(task) = self
            .config_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
        if let Some(task) = self
            .supervisor_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }
}

impl KytrixServiceConnector {
    pub(super) fn inner(&self) -> &KytrixServiceConnectorInner {
        &self.inner
    }

    #[cfg(test)]
    pub(super) fn spawn_for_test(
        runtime: &Runtime,
        config: ConfigNotification<Config>,
        transport: Arc<TransportHub>,
    ) -> Self {
        let auth = AuthSession::for_test(Arc::clone(&transport));
        let credential_invalidations = auth.session_changes();
        Self::spawn_with_config(
            runtime,
            KYTRIX_SERVICE_WS_URL.clone(),
            config,
            credential_invalidations,
            transport,
            auth,
        )
    }

    #[cfg(test)]
    pub(super) fn spawn_with_urls(runtime: &Runtime, websocket_url: Url) -> Self {
        let transport = test_transport();
        let auth = AuthSession::for_test(Arc::clone(&transport));
        let credential_invalidations = auth.session_changes();
        Self::spawn_with_config(
            runtime,
            websocket_url,
            ConfigNotification::new(Arc::new(Config::default())),
            credential_invalidations,
            transport,
            auth,
        )
    }

    #[cfg(test)]
    pub(super) fn spawn_with_urls_config(
        runtime: &Runtime,
        websocket_url: Url,
        config: ConfigNotification<Config>,
        transport: Arc<TransportHub>,
        auth: AuthSession,
        credential_invalidations: watch::Receiver<SessionChange>,
    ) -> Self {
        Self::spawn_with_config(
            runtime,
            websocket_url,
            config,
            credential_invalidations,
            transport,
            auth,
        )
    }

    /// Connects the application to the Kytrix service.
    ///
    /// This is the owner's single install point: it creates the auth session (D1),
    /// the bridge, and the notification's reconfigure consumer.
    ///
    /// # Panics
    ///
    /// Panics when `ConfigManager` was not installed, because the bridge the connector
    /// routes with reads that manager's events. The composition root installs the
    /// manager before it calls this; see [`ConfigEventBridge::install`].
    pub fn install(
        cx: &mut App,
        runtime: &Runtime,
        config: ConfigNotification<Config>,
        transport: Arc<TransportHub>,
    ) -> Result<Self, AuthError> {
        let auth = AuthSession::spawn(config.clone(), Arc::clone(&transport), runtime.clone())?;
        Ok(Self::install_with_auth(
            cx, runtime, config, transport, auth,
        ))
    }

    /// Completes installation over a caller-supplied auth session.
    ///
    /// Exists so a test can install the connector without starting the real auth
    /// session; the only production caller is [`Self::install`].
    pub(crate) fn install_with_auth(
        cx: &mut App,
        runtime: &Runtime,
        config: ConfigNotification<Config>,
        transport: Arc<TransportHub>,
        auth: AuthSession,
    ) -> Self {
        let bridge = ConfigEventBridge::install(
            cx,
            ConfigTopics::NETWORK,
            config.clone(),
            Arc::new(
                |config: &app_config::settings::Config, _previous: &Config| config.network_config(),
            ),
        );
        cx.set_global(ServiceConfigBridge { _bridge: bridge });

        let credential_invalidations = auth.session_changes();
        Self::spawn_with_config(
            runtime,
            KYTRIX_SERVICE_WS_URL.clone(),
            config,
            credential_invalidations,
            transport,
            auth,
        )
    }

    pub(super) fn spawn_with_config(
        runtime: &Runtime,
        websocket_url: Url,
        config: ConfigNotification<Config>,
        credential_invalidations: watch::Receiver<SessionChange>,
        transport: Arc<TransportHub>,
        auth: AuthSession,
    ) -> Self {
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let lifecycle = transport.scope();
        let cancellation = lifecycle.cancellation().clone();
        let wake = Arc::new(Notify::new());
        let account_profile = Arc::new(RwLock::new(None));
        let session_state = SessionStateRegistry::new();
        let supervisor_lifecycle = lifecycle.clone();
        let supervisor_websocket_url = websocket_url.clone();
        let supervisor_config = config.clone();
        let supervisor_auth = auth.clone();
        let supervisor_account_profile = account_profile;
        let supervisor_session_state = session_state.clone();
        let supervisor_cancellation = cancellation.clone();
        let supervisor_wake = wake.clone();
        let supervisor_context = ServiceSessionContext {
            websocket_url: supervisor_websocket_url,
            config: supervisor_config,
            auth: supervisor_auth,
            account_profile: supervisor_account_profile,
            credential_invalidations,
            session_state: supervisor_session_state,
            cancellation: supervisor_cancellation,
            wake: supervisor_wake,
            transport_policy: supervisor_lifecycle.policy().clone(),
            runtime: runtime.clone(),
        };
        let supervisor_task = runtime.run(async move {
            let Ok(_operation) = supervisor_lifecycle.begin_operation() else {
                return;
            };
            run_service_supervisor(receiver, supervisor_context).await;
        });
        let config_task = {
            let notification = config.clone();
            let commands = commands.clone();
            runtime.run(async move {
                loop {
                    let _ = notification.wait().await;
                    if commands.send(SupervisorCommand::Reconfigure).await.is_err() {
                        return;
                    }
                }
            })
        };
        Self {
            inner: Arc::new(KytrixServiceConnectorInner {
                commands,
                auth,
                lifecycle,
                transport,
                websocket_url,
                config,
                cancellation,
                wake,
                supervisor_task: Mutex::new(Some(supervisor_task)),
                config_task: Mutex::new(Some(config_task)),
                next_subscriber_id: AtomicU64::new(1),
                session_state,
                runtime: runtime.clone(),
            }),
        }
    }

    /// Gracefully stops all shared Kytrix service sessions.
    pub async fn shutdown(&self) {
        self.shutdown_with_timeout(SHUTDOWN_TIMEOUT).await;
    }

    pub(super) async fn shutdown_with_timeout(&self, wait: Duration) {
        self.inner().lifecycle.cancel();
        self.inner().wake.notify_waiters();
        if let Some(task) = self
            .inner()
            .config_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
        let task = self
            .inner()
            .supervisor_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(mut task) = task
            && timeout(wait, &mut task).await.is_err()
        {
            task.abort();
            let _ = task.await;
        }
        self.inner().auth.shutdown().await;
        self.inner().lifecycle.shutdown().await;
    }

    pub fn subscribe_session_state(&self) -> watch::Receiver<Option<KytrixSessionState>> {
        self.inner().session_state.subscribe()
    }

    /// Returns a read-only view of the network projection this connector routes with.
    ///
    /// The clone observes the same owner state. A caller may read
    /// [`ConfigNotification::current`] at request time, but must never
    /// [`ConfigNotification::wait`]: this connector's own forwarding task is the
    /// single waiter, and a second one would consume the publication that task
    /// needs to reconfigure the sessions.
    pub fn network_notification(&self) -> ConfigNotification<Config> {
        self.inner().config.clone()
    }

    pub fn subscribe_account_session_changes(&self) -> watch::Receiver<SessionChange> {
        self.inner().auth.session_changes()
    }

    /// Inspects the local credential record without contacting the OAuth authority.
    pub async fn local_account_session_status(&self) -> Result<LocalSessionStatus, AuthError> {
        let _operation = self
            .inner()
            .lifecycle
            .begin_operation()
            .map_err(|_| AuthError::SessionClosed)?;
        self.inner().auth.local_session_status().await
    }

    /// Returns the plan cached in the active account session without loading credentials or
    /// contacting the OAuth authority.
    pub async fn current_account_plan(
        &self,
        expected_user_id: &str,
    ) -> Result<Option<PlanCode>, AuthError> {
        let _operation = self
            .inner()
            .lifecycle
            .begin_operation()
            .map_err(|_| AuthError::SessionClosed)?;
        self.inner().auth.current_plan(expected_user_id).await
    }

    /// Creates a one-time code that signs this account into the web dashboard.
    ///
    /// The request goes to the API host the access token was issued for, carrying the
    /// same DPoP proof every other authenticated call carries.
    pub async fn create_account_dashboard_handoff(&self) -> Result<String, AuthError> {
        let _operation = self
            .inner()
            .lifecycle
            .begin_operation()
            .map_err(|_| AuthError::SessionClosed)?;
        let endpoint = Url::parse(API_RESOURCE)
            .map_err(|_| AuthError::InvalidResponse)?
            .join(DASHBOARD_HANDOFF_PATH)
            .map_err(|_| AuthError::InvalidResponse)?;
        let (_, (access_token, proof)) = self
            .inner()
            .auth
            .authorization_headers_with_profile("POST", endpoint.as_str())
            .await?;
        let config = self.inner().config.current();
        request_dashboard_handoff(
            &config,
            &self.inner().transport,
            &self.inner().runtime,
            &endpoint,
            &access_token,
            &proof,
        )
        .await
    }

    pub fn prepare_account_login(&self) -> Result<DpopKey, AuthError> {
        let _operation = self
            .inner()
            .lifecycle
            .begin_operation()
            .map_err(|_| AuthError::SessionClosed)?;
        self.inner().auth.prepare_key()
    }

    pub async fn exchange_account_authorization_code(
        &self,
        key: &DpopKey,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<AccountIdentity, AuthError> {
        let _operation = self
            .inner()
            .lifecycle
            .begin_operation()
            .map_err(|_| AuthError::SessionClosed)?;
        self.inner()
            .auth
            .exchange_authorization_code(key, code, redirect_uri, code_verifier)
            .await
    }

    /// Ends the account session held on this machine and removes the credential.
    ///
    /// Synchronous on purpose: signing out is not a request to the service, and a
    /// sign-out that waited for the network — or for whatever the account session is in
    /// the middle of — would be one that can fail. What comes back is what
    /// [`Self::revoke_account_session`] needs to report it, when there is anything to
    /// report.
    pub fn discard_account_session(&self) -> Option<Revocation> {
        self.inner().auth.discard_local_session().into_revocation()
    }

    /// Tells the account authority to drop the refresh token of a finished sign-out.
    ///
    /// This is a report, not a step: the session is already gone, and the caller is
    /// expected to discard the result. It is still a request to the account service, so it
    /// has to be driven by a runtime that can make one — an executor without a reactor
    /// panics inside the request instead of sending it.
    pub async fn revoke_account_session(&self, revocation: Revocation) {
        self.inner().auth.revoke_session(revocation).await;
    }

    pub async fn subscribe_news(&self) -> Result<Subscription<NewsEvent>, KytrixServiceError> {
        let (publisher, subscription) =
            subscription_channel_with_wake(SUBSCRIBER_CAPACITY, self.inner().wake.clone())
                .map_err(subscription_setup_error)?;
        self.subscribe_topics(&[ServiceTopic::News], SubscriberSink::News(publisher))
            .await?;
        Ok(subscription)
    }

    pub async fn subscribe_indices(
        &self,
        topics: &[IndexTopic],
    ) -> Result<Subscription<IndexEvent>, KytrixServiceError> {
        let topics = topics
            .iter()
            .cloned()
            .map(ServiceTopic::Index)
            .collect::<Vec<_>>();
        let (publisher, subscription) =
            subscription_channel_with_wake(SUBSCRIBER_CAPACITY, self.inner().wake.clone())
                .map_err(subscription_setup_error)?;
        self.subscribe_topics(&topics, SubscriberSink::Indices(publisher))
            .await?;
        Ok(subscription)
    }

    pub(super) async fn subscribe_topics(
        &self,
        topics: &[ServiceTopic],
        sink: SubscriberSink,
    ) -> Result<(), KytrixServiceError> {
        self.subscribe_topics_inner(None, topics, sink).await
    }

    pub(super) async fn subscribe_topics_for_profile(
        &self,
        profile: &AccountProfile,
        topics: &[ServiceTopic],
        sink: SubscriberSink,
    ) -> Result<(), KytrixServiceError> {
        self.subscribe_topics_inner(Some(profile.clone()), topics, sink)
            .await
    }

    pub(super) async fn subscribe_topics_inner(
        &self,
        expected_profile: Option<AccountProfile>,
        topics: &[ServiceTopic],
        sink: SubscriberSink,
    ) -> Result<(), KytrixServiceError> {
        let _operation = self.begin_service_operation()?;
        let topics = normalize_topics(topics);
        let capability = sink.capability();
        let id = self
            .inner()
            .next_subscriber_id
            .fetch_add(1, Ordering::Relaxed);
        let cancellation = self.inner().cancellation.child_token();
        let mut cancel_on_drop = CancelOnDrop(cancellation.clone());
        let (respond_to, response) = oneshot::channel();
        let deadline = Instant::now() + OPERATION_TIMEOUT;
        let profile = match expected_profile {
            Some(profile) => profile,
            None => {
                let authorization_url = authorization_url(&self.inner().websocket_url)?;
                let (profile, _) = await_account_operation(
                    &self.inner().cancellation,
                    &cancellation,
                    deadline,
                    service_authorization_headers_with_profile(
                        &self.inner().auth,
                        "GET",
                        &authorization_url,
                    ),
                )
                .await?;
                profile
            }
        };
        let config = self.inner().config.current();
        let target = ServiceTarget::KytrixService { capability };
        let route_generation = config.service_network_route_generation(&target);
        let target = RouteTarget::Service(target);
        let resolved_route =
            await_account_operation(&self.inner().cancellation, &cancellation, deadline, async {
                network::proxy::resolve_route_async_cancellable(
                    &config,
                    &target,
                    &self.inner().websocket_url,
                    cancellation.clone(),
                    &self.inner().runtime,
                )
                .await
                .map_err(|error| {
                    KytrixServiceError::Connect(smol_str::format_smolstr!("{}", error))
                })
            })
            .await?;
        let key = WsSessionKey {
            endpoint: kytrix_service_endpoint(&self.inner().websocket_url)?,
            proxy: resolved_route.proxy,
            os_proxy_fingerprint: resolved_route.os_proxy_fingerprint,
        };
        let command = SupervisorCommand::Send {
            key,
            profile: profile.clone(),
            command: Box::new(Command::Subscribe {
                id,
                topics,
                expected_profile: Some(profile),
                route_generation,
                sink,
                cancellation: cancellation.clone(),
                deadline,
                respond_to,
            }),
        };

        let send_result = timeout_at(deadline, self.inner().commands.send(command)).await;
        match send_result {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                cancellation.cancel();
                return Err(KytrixServiceError::Closed);
            }
            Err(_) => {
                cancellation.cancel();
                return Err(KytrixServiceError::Timeout);
            }
        }

        let result = tokio::select! {
            biased;
            () = self.inner().cancellation.cancelled() => Err(KytrixServiceError::Closed),
            () = cancellation.cancelled() => Err(KytrixServiceError::Closed),
            result = timeout_at(deadline, response) => {
                match result {
                    Ok(Ok(result)) => result,
                    Ok(Err(_)) => Err(KytrixServiceError::Closed),
                    Err(_) => Err(KytrixServiceError::Timeout),
                }
            }
        };

        match result {
            Ok(()) => {
                cancel_on_drop.disarm();
                Ok(())
            }
            Err(error) => {
                cancellation.cancel();
                Err(error)
            }
        }
    }

    /// Resolves the current credential generation and idempotently installs it
    /// as the shared Kytrix service account session.
    pub async fn acquire_account_session(
        &self,
        expected_account_id: &str,
    ) -> Result<KytrixAccountSession, KytrixServiceError> {
        let _operation = self.begin_service_operation()?;
        let deadline = Instant::now() + OPERATION_TIMEOUT;
        let cancellation = self.inner().cancellation.child_token();
        let _cancel_on_drop = CancelOnDrop(cancellation.clone());
        let profile = await_account_operation(
            &self.inner().cancellation,
            &cancellation,
            deadline,
            service_account_profile(&self.inner().auth, expected_account_id),
        )
        .await?;
        self.acquire_account_profile_until(profile, cancellation, deadline)
            .await
    }

    pub(super) async fn acquire_account_profile_until(
        &self,
        profile: AccountProfile,
        cancellation: CancellationToken,
        deadline: Instant,
    ) -> Result<KytrixAccountSession, KytrixServiceError> {
        let (respond_to, response) = oneshot::channel();
        let command = SupervisorCommand::AcquireAccount {
            profile: profile.clone(),
            cancellation: cancellation.clone(),
            deadline,
            respond_to,
        };
        await_account_operation(&self.inner().cancellation, &cancellation, deadline, async {
            self.inner()
                .commands
                .send(command)
                .await
                .map_err(|_| KytrixServiceError::Closed)
        })
        .await?;
        let result =
            await_account_operation(&self.inner().cancellation, &cancellation, deadline, async {
                response.await.map_err(|_| KytrixServiceError::Closed)?
            })
            .await;
        result.map(|()| KytrixAccountSession {
            connector: self.clone(),
            profile,
        })
    }

    pub(super) async fn call_rpc(
        &self,
        expected_profile: Option<&AccountProfile>,
        capability: KytrixServiceCapability,
        request: ClientRequestKind,
    ) -> Result<proto_response::response::Response, RpcCallError> {
        let _operation = self
            .begin_service_operation()
            .map_err(RpcCallError::not_submitted)?;
        let deadline = Instant::now() + OPERATION_TIMEOUT;
        let cancellation = self.inner().cancellation.child_token();
        let _cancel_on_drop = CancelOnDrop(cancellation.clone());
        let possibly_submitted = Arc::new(AtomicBool::new(false));
        let profile = match expected_profile {
            Some(profile) => profile.clone(),
            None => {
                let authorization_url = authorization_url(&self.inner().websocket_url)
                    .map_err(RpcCallError::not_submitted)?;
                let (profile, _) = await_account_operation(
                    &self.inner().cancellation,
                    &cancellation,
                    deadline,
                    service_authorization_headers_with_profile(
                        &self.inner().auth,
                        "GET",
                        &authorization_url,
                    ),
                )
                .await
                .map_err(RpcCallError::not_submitted)?;
                profile
            }
        };
        let config = self.inner().config.current();
        let target = ServiceTarget::KytrixService { capability };
        let route_generation = config.service_network_route_generation(&target);
        let target = RouteTarget::Service(target);
        let resolved_route =
            await_account_operation(&self.inner().cancellation, &cancellation, deadline, async {
                network::proxy::resolve_route_async_cancellable(
                    &config,
                    &target,
                    &self.inner().websocket_url,
                    cancellation.clone(),
                    &self.inner().runtime,
                )
                .await
                .map_err(|error| {
                    KytrixServiceError::Connect(smol_str::format_smolstr!("{}", error))
                })
            })
            .await
            .map_err(RpcCallError::not_submitted)?;
        let key = WsSessionKey {
            endpoint: kytrix_service_endpoint(&self.inner().websocket_url)
                .map_err(RpcCallError::not_submitted)?,
            proxy: resolved_route.proxy,
            os_proxy_fingerprint: resolved_route.os_proxy_fingerprint,
        };
        let (respond_to, response) = oneshot::channel();
        await_account_operation(&self.inner().cancellation, &cancellation, deadline, async {
            self.inner()
                .commands
                .send(SupervisorCommand::Send {
                    key,
                    profile: profile.clone(),
                    command: Box::new(Command::Rpc {
                        request: Some(request),
                        expected_profile: profile,
                        capability,
                        route_generation,
                        cancellation: cancellation.clone(),
                        possibly_submitted: Arc::clone(&possibly_submitted),
                        deadline,
                        respond_to,
                    }),
                })
                .await
                .map_err(|_| KytrixServiceError::Closed)
        })
        .await
        .map_err(RpcCallError::not_submitted)?;
        let result = tokio::select! {
            biased;
            () = self.inner().cancellation.cancelled() => Err(KytrixServiceError::Closed),
            () = cancellation.cancelled() => Err(KytrixServiceError::Closed),
            result = timeout_at(deadline, response) => {
                result
                    .map_err(|_| KytrixServiceError::Timeout)
                    .and_then(|response| response.map_err(|_| KytrixServiceError::Closed))
                    .and_then(|response| response)
                    .and_then(validate_rpc_response)
                    .and_then(|response| {
                        response.response.ok_or_else(|| {
                            KytrixServiceError::Protocol(
                                "Kytrix service RPC response oneof is required".into(),
                            )
                        })
                    })
            },
        };
        result.map_err(|error| RpcCallError {
            error,
            possibly_submitted: possibly_submitted.load(Ordering::Acquire),
        })
    }

    /// Fetches the current ETF snapshot over WebSocket RPC.
    pub async fn fetch_etf(&self) -> Result<proto_types::Etf, KytrixServiceError> {
        let response = self
            .call_rpc(
                None,
                KytrixServiceCapability::Etf,
                ClientRequestKind::GetEtf(GetEtf {}),
            )
            .await
            .map_err(RpcCallError::into_error)?;
        let proto_response::response::Response::Etf(response) = response else {
            return Err(KytrixServiceError::Protocol(
                "unexpected ETF RPC response".into(),
            ));
        };
        response
            .etf
            .ok_or_else(|| KytrixServiceError::Protocol("ETF RPC response is missing data".into()))
    }

    /// Fetches the current index manifest over WebSocket RPC.
    pub async fn fetch_index_manifest(
        &self,
    ) -> Result<proto_types::IndexManifest, KytrixServiceError> {
        let response = self
            .call_rpc(
                None,
                KytrixServiceCapability::Indices,
                ClientRequestKind::GetIndexManifest(GetIndexManifest {}),
            )
            .await
            .map_err(RpcCallError::into_error)?;
        let proto_response::response::Response::IndexManifest(response) = response else {
            return Err(KytrixServiceError::Protocol(
                "unexpected index manifest RPC response".into(),
            ));
        };
        response.manifest.ok_or_else(|| {
            KytrixServiceError::Protocol("index manifest RPC response is missing data".into())
        })
    }

    /// Fetches the latest CMC fear and greed snapshot over WebSocket RPC.
    pub async fn fetch_cmc_fear_greed_index(
        &self,
    ) -> Result<proto_types::CmcFearGreedIndex, KytrixServiceError> {
        let response = self
            .call_rpc(
                None,
                KytrixServiceCapability::Cmc,
                ClientRequestKind::GetCmcFearGreedIndex(GetCmcFearGreedIndex {}),
            )
            .await
            .map_err(RpcCallError::into_error)?;
        let proto_response::response::Response::CmcFearGreedIndex(response) = response else {
            return Err(KytrixServiceError::Protocol(
                "unexpected fear and greed RPC response".into(),
            ));
        };
        response.cmc_fear_greed_index.ok_or_else(|| {
            KytrixServiceError::Protocol("fear and greed RPC response is missing data".into())
        })
    }

    /// Fetches the latest Pentagon Pizza Index snapshot over WebSocket RPC.
    pub async fn fetch_pentagon_pizza_index(
        &self,
    ) -> Result<proto_types::PentagonPizzaIndex, KytrixServiceError> {
        let response = self
            .call_rpc(
                None,
                KytrixServiceCapability::PentagonPizzaIndex,
                ClientRequestKind::GetPentagonPizzaIndex(GetPentagonPizzaIndex {}),
            )
            .await
            .map_err(RpcCallError::into_error)?;
        let proto_response::response::Response::PentagonPizzaIndex(response) = response else {
            return Err(KytrixServiceError::Protocol(
                "unexpected Pentagon Pizza Index RPC response".into(),
            ));
        };
        response.pentagon_pizza_index.ok_or_else(|| {
            KytrixServiceError::Protocol("Pentagon Pizza Index RPC response is missing data".into())
        })
    }

    pub(super) fn begin_service_operation(&self) -> Result<TransportPermit, KytrixServiceError> {
        self.inner()
            .lifecycle
            .begin_operation()
            .map_err(|_| KytrixServiceError::Closed)
    }
}

#[cfg(test)]
mod tests {
    use app_config::network::ProxyRoute;

    use super::{super::test_support::*, *};
    use crate::service::{
        wallet::{WalletMutationOutcome, WalletMutationUnknown},
        x::XSelectionStatus,
    };

    #[tokio::test]
    async fn snapshot_rpcs_use_the_service_websocket() {
        let mut server = TestServiceServer::spawn(false).await;
        let connector =
            KytrixServiceConnector::spawn_with_urls(&Runtime::for_test(), server.url.clone());

        assert_eq!(connector.fetch_etf().await.unwrap().timestamp, 42);
        assert_eq!(server.next_request().await.1, ObservedRequest::Rpc("etf"));
        assert_eq!(
            connector.fetch_index_manifest().await.unwrap(),
            index_manifest()
        );
        assert_eq!(
            server.next_request().await.1,
            ObservedRequest::Rpc("index_manifest")
        );
        assert_eq!(
            connector.fetch_cmc_fear_greed_index().await.unwrap(),
            cmc_fear_greed_index()
        );
        assert_eq!(
            server.next_request().await.1,
            ObservedRequest::Rpc("cmc_fear_greed_index")
        );
        assert_eq!(
            connector.fetch_pentagon_pizza_index().await.unwrap(),
            pentagon_pizza_index()
        );
        assert_eq!(
            server.next_request().await.1,
            ObservedRequest::Rpc("pentagon_pizza_index")
        );

        connector.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_pizza_index_answer_of_another_kind_is_a_protocol_error() {
        let mut server = TestServiceServer::spawn(false).await;
        server.answer_next_request_with(proto_response::response::Response::Etf(
            proto_response::EtfResponse { etf: None },
        ));
        let connector =
            KytrixServiceConnector::spawn_with_urls(&Runtime::for_test(), server.url.clone());

        assert_eq!(
            connector.fetch_pentagon_pizza_index().await,
            Err(KytrixServiceError::Protocol(
                "unexpected Pentagon Pizza Index RPC response".into()
            ))
        );
        assert_eq!(
            server.next_request().await.1,
            ObservedRequest::Rpc("pentagon_pizza_index")
        );

        connector.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_pizza_index_answer_without_a_snapshot_is_a_protocol_error() {
        let mut server = TestServiceServer::spawn(false).await;
        server.answer_next_request_with(proto_response::response::Response::PentagonPizzaIndex(
            proto_response::PentagonPizzaIndexResponse {
                pentagon_pizza_index: None,
            },
        ));
        let connector =
            KytrixServiceConnector::spawn_with_urls(&Runtime::for_test(), server.url.clone());

        assert_eq!(
            connector.fetch_pentagon_pizza_index().await,
            Err(KytrixServiceError::Protocol(
                "Pentagon Pizza Index RPC response is missing data".into()
            ))
        );
        assert_eq!(
            server.next_request().await.1,
            ObservedRequest::Rpc("pentagon_pizza_index")
        );

        connector.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_fear_and_greed_answer_of_another_kind_is_a_protocol_error() {
        let mut server = TestServiceServer::spawn(false).await;
        server.answer_next_request_with(proto_response::response::Response::Etf(
            proto_response::EtfResponse { etf: None },
        ));
        let connector =
            KytrixServiceConnector::spawn_with_urls(&Runtime::for_test(), server.url.clone());

        assert_eq!(
            connector.fetch_cmc_fear_greed_index().await,
            Err(KytrixServiceError::Protocol(
                "unexpected fear and greed RPC response".into()
            ))
        );
        assert_eq!(
            server.next_request().await.1,
            ObservedRequest::Rpc("cmc_fear_greed_index")
        );

        connector.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_fear_and_greed_answer_without_a_snapshot_is_a_protocol_error() {
        let mut server = TestServiceServer::spawn(false).await;
        server.answer_next_request_with(proto_response::response::Response::CmcFearGreedIndex(
            proto_response::CmcFearGreedIndexResponse {
                cmc_fear_greed_index: None,
            },
        ));
        let connector =
            KytrixServiceConnector::spawn_with_urls(&Runtime::for_test(), server.url.clone());

        assert_eq!(
            connector.fetch_cmc_fear_greed_index().await,
            Err(KytrixServiceError::Protocol(
                "fear and greed RPC response is missing data".into()
            ))
        );
        assert_eq!(
            server.next_request().await.1,
            ObservedRequest::Rpc("cmc_fear_greed_index")
        );

        connector.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn wallet_mutation_response_loss_is_unknown_and_is_not_retried() {
        let mut server = TestServiceServer::spawn(false).await;
        server.drop_next_response();
        let connector =
            KytrixServiceConnector::spawn_with_urls(&Runtime::for_test(), server.url.clone());
        let session = connector
            .acquire_account_session(TEST_ACCOUNT_ID)
            .await
            .unwrap();
        let address = "0x0000000000000000000000000000000000000001";

        assert_eq!(
            session.add_wallet_selection(address).await,
            WalletMutationOutcome::Unknown(WalletMutationUnknown::ResponseLost)
        );
        assert_eq!(
            server.next_request().await.1,
            ObservedRequest::Rpc("wallet_add_selection")
        );
        server.assert_no_request().await;

        connector.shutdown().await;
        server.shutdown().await;
    }

    /// A first-ever add is answered with the row that is waiting to be activated, and that row
    /// has no profile: the service sends one only once the account is active. The answer is
    /// still the account that was asked for, so it is a confirmation rather than an outcome the
    /// caller has to treat as unknown.
    #[tokio::test]
    async fn confirmed_x_add_without_a_profile_is_confirmed_as_pending() {
        let mut server = TestServiceServer::spawn(false).await;
        server.answer_x_add_with_pending_selection();
        let connector =
            KytrixServiceConnector::spawn_with_urls(&Runtime::for_test(), server.url.clone());
        let session = connector
            .acquire_account_session(TEST_ACCOUNT_ID)
            .await
            .unwrap();

        let XMutationOutcome::Confirmed(selection) = session.add_x_selection("kytrix").await else {
            panic!("an add the service confirmed should be confirmed");
        };
        assert_eq!(selection.status, XSelectionStatus::Pending);
        assert_eq!(selection.profile, None);
        assert_eq!(
            server.next_request().await.1,
            ObservedRequest::Rpc("x_add_selection")
        );

        connector.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn pending_rpc_responses_can_complete_out_of_order_and_cancel_independently() {
        let (_invalidations, invalidations) = credential_change_watch();
        let (mut actor, _subscription) = actor_with_wallet_subscriber(invalidations);
        let profile = actor
            .account_profile
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap();
        let (first_sender, first_receiver) = oneshot::channel();
        let (second_sender, second_receiver) = oneshot::channel();
        for (id, sender) in [(1, first_sender), (2, second_sender)] {
            actor.pending_rpcs.insert(
                id,
                PendingRpc {
                    expected_profile: profile.clone(),
                    capability: KytrixServiceCapability::Wallet,
                    route_generation: NetworkRouteGeneration::default(),
                    cancellation: CancellationToken::new(),
                    deadline: Instant::now() + Duration::from_secs(1),
                    respond_to: sender,
                },
            );
        }
        let response = |id| proto_response::Response {
            id,
            response: Some(proto_response::response::Response::WalletDeleteSelection(
                proto_response::WalletDeleteSelectionResponse {},
            )),
        };

        assert!(actor.complete_pending_rpc(response(2)));
        assert_eq!(second_receiver.await.unwrap().unwrap().id, 2);
        assert!(actor.complete_pending_rpc(response(1)));
        assert_eq!(first_receiver.await.unwrap().unwrap().id, 1);

        let cancellation = CancellationToken::new();
        let (cancelled_sender, cancelled_receiver) = oneshot::channel();
        actor.pending_rpcs.insert(
            3,
            PendingRpc {
                expected_profile: profile,
                capability: KytrixServiceCapability::Wallet,
                route_generation: NetworkRouteGeneration::default(),
                cancellation: cancellation.clone(),
                deadline: Instant::now() + Duration::from_secs(1),
                respond_to: cancelled_sender,
            },
        );
        cancellation.cancel();
        actor.prune_pending_rpcs();
        assert_eq!(
            cancelled_receiver.await.unwrap(),
            Err(KytrixServiceError::Closed)
        );
    }

    #[tokio::test]
    async fn connector_shutdown_completes_and_is_idempotent() {
        let config = ConfigNotification::new(Arc::new(Config::default()));
        let transport = test_transport();
        let connector = KytrixServiceConnector::spawn_for_test(
            &Runtime::for_test(),
            config,
            Arc::clone(&transport),
        );

        connector.shutdown().await;
        connector.shutdown().await;

        assert!(connector.inner().cancellation.is_cancelled());
        assert!(
            connector
                .inner()
                .supervisor_task
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none()
        );
        transport.shutdown().await;
    }

    /// Exercises [`KytrixServiceConnector::install_with_auth`], the test seam: the
    /// public [`KytrixServiceConnector::install`] is the thin wrapper that spawns the
    /// auth session before calling it.
    #[gpui_kit::test]
    fn install_with_auth_should_bridge_network_writes_and_take_its_config_task_on_shutdown(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use app_config::settings::Language;
        use app_config_provider::manager::ConfigManager;
        use gpui_kit::BorrowAppContext as _;

        let runtime = Runtime::for_test();
        let transport = test_transport();
        let notification = cx.update(|cx| {
            let manager = ConfigManager::in_memory(app_config::settings::Config::default(), cx);
            let notification = ConfigNotification::new(Arc::new(manager.read().network_config()));
            cx.set_global(manager);
            notification
        });
        let connector = cx.update(|cx| {
            KytrixServiceConnector::install_with_auth(
                cx,
                &runtime,
                notification.clone(),
                Arc::clone(&transport),
                AuthSession::for_test(Arc::clone(&transport)),
            )
        });

        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(
                    |config| {
                        config.proxies.insert(
                            SmolStr::new_static("install-bridge"),
                            app_config::network::ProxyConfig {
                                addr: SmolStr::new_static("127.0.0.1"),
                                port: 9,
                                authorization: app_config::network::ProxyAuthorizationConfig::None,
                                resolve_dns_at_remote: false,
                            },
                        );
                    },
                    cx,
                );
            });
        });
        assert!(
            connector
                .network_notification()
                .current()
                .proxies
                .contains_key(&SmolStr::new_static("install-bridge")),
            "a network write should reach the projection the connector routes with"
        );

        let before_language = connector.network_notification().current();
        cx.update(|cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(|config| config.language = Language::En, cx);
            });
        });
        assert!(
            Arc::ptr_eq(
                &before_language,
                &connector.network_notification().current()
            ),
            "a write outside the network domain must not publish a network projection"
        );

        assert!(
            connector
                .inner()
                .config_task
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some(),
            "install must start the configuration forwarding task"
        );

        runtime.handle().block_on(async {
            connector.shutdown().await;
            transport.shutdown().await;
        });
        assert!(
            connector
                .inner()
                .config_task
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none(),
            "shutdown must take the configuration forwarding task"
        );
    }

    #[tokio::test]
    async fn typed_service_subscriptions_should_not_receive_other_capability_events() {
        let mut server = TestServiceServer::spawn(false).await;
        let connector =
            KytrixServiceConnector::spawn_with_urls(&Runtime::for_test(), server.url.clone());
        let index = index_topic("SPX");

        let mut news = connector
            .subscribe_news()
            .await
            .expect("news should subscribe");
        assert_eq!(
            server.next_request().await,
            (
                1,
                ObservedRequest::SetSubscriptions(vec![ServiceTopic::News])
            )
        );
        let mut indices = connector
            .subscribe_indices(std::slice::from_ref(&index))
            .await
            .expect("indices should subscribe");
        assert_eq!(
            server.next_request().await,
            (
                1,
                ObservedRequest::SetSubscriptions(vec![
                    ServiceTopic::News,
                    ServiceTopic::Index(index.clone()),
                ])
            )
        );
        let session = connector
            .acquire_account_session(TEST_ACCOUNT_ID)
            .await
            .expect("test account should resolve");
        let mut x = session.subscribe_x().await.expect("X should subscribe");
        assert_eq!(
            server.next_request().await,
            (
                1,
                ObservedRequest::SetSubscriptions(vec![
                    ServiceTopic::News,
                    ServiceTopic::Index(index.clone()),
                    ServiceTopic::X,
                ])
            )
        );
        let mut wallet = session
            .subscribe_wallet()
            .await
            .expect("wallet should subscribe");
        assert_eq!(
            server.next_request().await,
            (
                1,
                ObservedRequest::SetSubscriptions(vec![
                    ServiceTopic::News,
                    ServiceTopic::Index(index),
                    ServiceTopic::X,
                    ServiceTopic::Wallet,
                ])
            )
        );
        let mut ais = session
            .subscribe_ais(AisArea::Hormuz)
            .await
            .expect("AIS should subscribe");
        assert_eq!(
            server.next_request().await,
            (
                1,
                ObservedRequest::SetSubscriptions(vec![
                    ServiceTopic::News,
                    ServiceTopic::Index(index_topic("SPX")),
                    ServiceTopic::X,
                    ServiceTopic::Wallet,
                    ServiceTopic::Ais(AisArea::Hormuz),
                ])
            )
        );
        assert_eq!(server.connection_count.load(Ordering::Acquire), 1);

        server.send_news("typed-news");
        server.send_index("SPX");
        server.send_x_profile();
        server.send_wallet_transfer("typed-wallet");
        server.send_ais_delta();

        assert_eq!(
            timeout(Duration::from_secs(2), news.recv())
                .await
                .expect("news event should arrive")
                .expect("news subscription should remain active")
                .content,
            "typed-news"
        );
        assert_eq!(
            timeout(Duration::from_secs(2), indices.recv())
                .await
                .expect("index event should arrive")
                .expect("index subscription should remain active")
                .topic,
            index_topic("SPX")
        );
        let x_event = timeout(Duration::from_secs(2), x.recv())
            .await
            .expect("X event should arrive")
            .expect("X subscription should remain active");
        assert!(matches!(
            x_event,
            XSubscriptionEvent::Profile(XProfileEvent { username, .. }) if username == "kytrix"
        ));
        assert_eq!(
            timeout(Duration::from_secs(2), wallet.recv())
                .await
                .expect("wallet event should arrive")
                .expect("wallet subscription should remain active")
                .event_id,
            "typed-wallet"
        );
        assert_eq!(
            timeout(Duration::from_secs(2), ais.recv())
                .await
                .expect("AIS event should arrive")
                .expect("AIS subscription should remain active"),
            AisUpdate::from_proto(ais_delta())
        );
        assert!(news.try_recv_for_test().is_err());
        assert!(indices.try_recv_for_test().is_err());
        assert!(x.try_recv_for_test().is_err());
        assert!(wallet.try_recv_for_test().is_err());
        assert!(ais.try_recv_for_test().is_err());

        drop((news, indices, x, wallet, ais));
        connector.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn ais_subscription_should_release_the_remote_topic_when_dropped() {
        let mut server = TestServiceServer::spawn(false).await;
        let connector =
            KytrixServiceConnector::spawn_with_urls(&Runtime::for_test(), server.url.clone());
        let session = connector
            .acquire_account_session(TEST_ACCOUNT_ID)
            .await
            .expect("test account should resolve");

        let ais = session
            .subscribe_ais(AisArea::Hormuz)
            .await
            .expect("AIS should subscribe");
        assert_eq!(
            server.next_request().await,
            (
                1,
                ObservedRequest::SetSubscriptions(vec![ServiceTopic::Ais(AisArea::Hormuz)])
            )
        );

        drop(ais);
        assert_eq!(
            server.next_request().await,
            (1, ObservedRequest::SetSubscriptions(Vec::new()))
        );

        connector.shutdown().await;
        server.shutdown().await;
    }

    /// A second pane of the same account joins a topic this session already follows.
    ///
    /// The server replays an area's keyframe only when a connection adds its topic, and
    /// both panes share this connection: without the session's own replay the second pane
    /// would receive only deltas it cannot apply until the server's fallback keyframe.
    #[tokio::test]
    async fn a_second_ais_subscription_should_start_from_the_keyframe_the_session_holds() {
        let mut server = TestServiceServer::spawn(false).await;
        let connector =
            KytrixServiceConnector::spawn_with_urls(&Runtime::for_test(), server.url.clone());
        let session = connector
            .acquire_account_session(TEST_ACCOUNT_ID)
            .await
            .expect("test account should resolve");

        let mut first = session
            .subscribe_ais(AisArea::Hormuz)
            .await
            .expect("AIS should subscribe");
        assert_eq!(
            server.next_request().await,
            (
                1,
                ObservedRequest::SetSubscriptions(vec![ServiceTopic::Ais(AisArea::Hormuz)])
            )
        );

        server.send_ais_keyframe();
        let keyframe = timeout(Duration::from_secs(2), first.recv())
            .await
            .expect("the keyframe should arrive")
            .expect("the first subscription should remain active");
        assert!(keyframe.keyframe);

        let mut second = session
            .subscribe_ais(AisArea::Hormuz)
            .await
            .expect("AIS should subscribe a second time");
        assert_eq!(
            timeout(Duration::from_secs(2), second.recv())
                .await
                .expect("the replayed keyframe should arrive")
                .expect("the second subscription should remain active"),
            keyframe,
            "the joining pane should start from the picture already held",
        );
        assert!(second.try_recv_for_test().is_err());

        drop((first, second));
        connector.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn replacing_typed_subscriptions_should_update_the_remote_topic_set() {
        let mut server = TestServiceServer::spawn(false).await;
        let connector =
            KytrixServiceConnector::spawn_with_urls(&Runtime::for_test(), server.url.clone());
        let news = vec![ServiceTopic::News];
        let dji = index_service_topic("DJI");
        let ndx = index_service_topic("NDX");
        let rut = index_service_topic("RUT");
        let spx = index_service_topic("SPX");

        let news_subscription = connector
            .subscribe_news()
            .await
            .expect("news batch should subscribe");
        assert_eq!(
            server.next_request().await,
            (1, ObservedRequest::SetSubscriptions(news.clone()))
        );

        let indices = connector
            .subscribe_indices(&[index_topic("SPX"), index_topic("DJI")])
            .await
            .expect("index batch should subscribe");
        assert_eq!(
            server.next_request().await,
            (
                1,
                ObservedRequest::SetSubscriptions(vec![news[0].clone(), dji.clone(), spx.clone(),])
            )
        );

        drop(indices);
        assert_eq!(
            server.next_request().await,
            (1, ObservedRequest::SetSubscriptions(news.clone()))
        );
        let indices = connector
            .subscribe_indices(&[index_topic("RUT"), index_topic("DJI"), index_topic("NDX")])
            .await
            .expect("replacement index selection should subscribe");
        assert_eq!(
            server.next_request().await,
            (
                1,
                ObservedRequest::SetSubscriptions(vec![
                    news[0].clone(),
                    dji.clone(),
                    ndx.clone(),
                    rut.clone(),
                ])
            )
        );

        drop(news_subscription);
        assert_eq!(
            server.next_request().await,
            (
                1,
                ObservedRequest::SetSubscriptions(vec![dji.clone(), ndx.clone(), rut.clone(),])
            )
        );
        drop(indices);
        assert_eq!(
            server.next_request().await,
            (1, ObservedRequest::SetSubscriptions(Vec::new()))
        );

        connector.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn stalled_empty_ack_should_not_lose_a_concurrent_news_resubscribe() {
        let mut server = TestServiceServer::spawn(false).await;
        let connector =
            KytrixServiceConnector::spawn_with_urls(&Runtime::for_test(), server.url.clone());

        let news = connector
            .subscribe_news()
            .await
            .expect("the initial News subscription should connect");
        assert_eq!(
            server.next_request().await,
            (
                1,
                ObservedRequest::SetSubscriptions(vec![ServiceTopic::News])
            )
        );

        server.stall_next_response();
        drop(news);
        assert_eq!(
            server.next_request().await,
            (1, ObservedRequest::SetSubscriptions(Vec::new()))
        );
        server.wait_for_stalled_response().await;

        let replacement_task = tokio::spawn({
            let connector = connector.clone();
            async move { connector.subscribe_news().await }
        });
        tokio::task::yield_now().await;
        server.resume_response();
        assert_eq!(
            server.next_request().await,
            (
                2,
                ObservedRequest::SetSubscriptions(vec![ServiceTopic::News])
            )
        );
        let mut replacement = timeout(Duration::from_secs(2), replacement_task)
            .await
            .expect("the queued News resubscribe should complete")
            .expect("the resubscribe task should not panic")
            .expect("the queued News resubscribe should remain owned by the actor");
        assert_eq!(server.connection_count.load(Ordering::Acquire), 2);

        server.send_news("resubscribed");
        assert_eq!(
            timeout(Duration::from_secs(2), replacement.recv())
                .await
                .expect("the replacement News event should arrive")
                .expect("the replacement News subscription should remain active")
                .content,
            "resubscribed"
        );

        let duplicate = connector
            .subscribe_news()
            .await
            .expect("a duplicate News subscriber should share the replacement topic");
        server.assert_no_request().await;
        drop(replacement);
        server.assert_no_request().await;
        drop(duplicate);
        assert_eq!(
            server.next_request().await,
            (2, ObservedRequest::SetSubscriptions(Vec::new()))
        );
        assert_eq!(server.connection_count.load(Ordering::Acquire), 2);

        connector.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn route_change_should_retire_only_the_affected_shared_session_capability() {
        let mut server = TestServiceServer::spawn(false).await;
        let notification = ConfigNotification::new(Arc::new(Config::default()));
        let transport = test_transport();
        let auth = AuthSession::for_test(Arc::clone(&transport));
        let credential_invalidations = auth.session_changes();
        let connector = KytrixServiceConnector::spawn_with_urls_config(
            &Runtime::for_test(),
            server.url.clone(),
            notification.clone(),
            Arc::clone(&transport),
            auth,
            credential_invalidations,
        );
        let mut news = connector
            .subscribe_news()
            .await
            .expect("news should subscribe");
        assert_eq!(
            server.next_request().await,
            (
                1,
                ObservedRequest::SetSubscriptions(vec![ServiceTopic::News])
            )
        );
        let index = index_topic("SPX");
        let index_service = ServiceTopic::Index(index.clone());
        let mut indices = connector
            .subscribe_indices(std::slice::from_ref(&index))
            .await
            .expect("indices should share the direct session");
        assert_eq!(
            server.next_request().await,
            (
                1,
                ObservedRequest::SetSubscriptions(vec![ServiceTopic::News, index_service.clone(),])
            )
        );

        let mut updated = Config::default();
        updated.proxies.insert(
            SmolStr::new_static("alternate"),
            app_config::network::ProxyConfig {
                addr: SmolStr::new_static("127.0.0.1"),
                port: 9,
                authorization: app_config::network::ProxyAuthorizationConfig::None,
                resolve_dns_at_remote: false,
            },
        );
        updated.proxy_applications.insert(
            ServiceTarget::KytrixService {
                capability: KytrixServiceCapability::News,
            },
            ProxyRoute::named("alternate"),
        );
        notification.publish_for_test(Arc::new(updated));

        assert_eq!(
            timeout(Duration::from_secs(2), news.recv())
                .await
                .expect("the route change should retire the News subscription"),
            Err(TransportError::RouteChanged)
        );
        server.send_news("stale-old-route");
        assert_eq!(
            server.next_request().await,
            (1, ObservedRequest::SetSubscriptions(vec![index_service]))
        );
        assert_eq!(
            indices.status(),
            network::transport::subscription::SubscriptionStatus::Active
        );
        assert_eq!(server.connection_count.load(Ordering::Acquire), 1);
        server.send_index("SPX");
        assert_eq!(
            timeout(Duration::from_secs(2), indices.recv())
                .await
                .expect("the unaffected Index event should arrive")
                .expect("the unaffected Index subscription should remain active")
                .topic,
            index
        );
        // The Index event is the FIFO barrier for the stale News frame: both
        // travel over the same socket and the retired frame was sent first, so
        // the actor has already processed it. `recv` cannot prove that (after a
        // terminal status it answers `Closed` immediately, whatever was
        // buffered), so inspect the queue directly: a delivered stale frame
        // would still be there.
        assert!(
            news.try_recv_for_test().is_err(),
            "a frame published after retirement must not reach the retired subscription"
        );
        assert_eq!(
            news.status(),
            network::transport::subscription::SubscriptionStatus::Failed(
                TransportError::RouteChanged
            )
        );

        drop(indices);
        connector.shutdown().await;
        server.shutdown().await;
    }

    /// A route change that lands while a subscription acknowledgement is still in
    /// flight is only visible to the ack fence: the queued reconfigure cannot run
    /// until the actor leaves `wait_for_ack`. The subscription must fail with
    /// `RouteChanged` instead of becoming active on the old route.
    #[tokio::test]
    async fn withheld_ack_should_fail_the_subscription_when_its_route_changed() {
        let mut server = TestServiceServer::spawn(false).await;
        let mut manager = TestConfigManager::in_memory(Config::default());
        let transport = test_transport();
        let auth = AuthSession::for_test(Arc::clone(&transport));
        let credential_invalidations = auth.session_changes();
        let connector = KytrixServiceConnector::spawn_with_urls_config(
            &Runtime::for_test(),
            server.url.clone(),
            manager.notification(),
            Arc::clone(&transport),
            auth,
            credential_invalidations,
        );

        server.stall_next_response();
        let subscribe = tokio::spawn({
            let connector = connector.clone();
            async move { connector.subscribe_news().await }
        });
        assert_eq!(
            server.next_request().await,
            (
                1,
                ObservedRequest::SetSubscriptions(vec![ServiceTopic::News])
            )
        );
        server.wait_for_stalled_response().await;

        let target = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::News,
        };
        manager.update(|config| {
            config.proxies.insert(
                "ack-fence".into(),
                app_config::network::ProxyConfig {
                    addr: "127.0.0.1".into(),
                    port: 9,
                    authorization: app_config::network::ProxyAuthorizationConfig::None,
                    resolve_dns_at_remote: false,
                },
            );
            config
                .proxy_applications
                .insert(target, ProxyRoute::named("ack-fence"));
        });
        server.resume_response();

        let result = timeout(Duration::from_secs(2), subscribe)
            .await
            .expect("the released acknowledgement should complete the subscription")
            .expect("the subscription task should not panic");
        assert!(
            matches!(result, Err(KytrixServiceError::RouteChanged)),
            "an acknowledgement that arrives after its route moved must not activate the sink"
        );

        connector.shutdown().await;
        transport.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn service_facade_should_reject_route_aba_before_actor_poll() {
        let mut server = TestServiceServer::spawn(false).await;
        let mut manager = TestConfigManager::in_memory(Config::default());
        let transport = test_transport();
        let auth = AuthSession::for_test(Arc::clone(&transport));
        let credential_invalidations = auth.session_changes();
        let connector = KytrixServiceConnector::spawn_with_urls_config(
            &Runtime::for_test(),
            server.url.clone(),
            manager.notification(),
            Arc::clone(&transport),
            auth,
            credential_invalidations,
        );
        let mut news = connector
            .subscribe_news()
            .await
            .expect("news should subscribe");
        assert_eq!(
            server.next_request().await,
            (
                1,
                ObservedRequest::SetSubscriptions(vec![ServiceTopic::News])
            )
        );
        let target = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::News,
        };
        let initial_generation = manager.read().service_network_route_generation(&target);

        manager.update(|config| {
            config.proxies.insert(
                "route-b".into(),
                app_config::network::ProxyConfig {
                    addr: "127.0.0.1".into(),
                    port: 9,
                    authorization: app_config::network::ProxyAuthorizationConfig::None,
                    resolve_dns_at_remote: true,
                },
            );
            config
                .proxy_applications
                .insert(target.clone(), ProxyRoute::named("route-b"));
        });
        manager.update(|config| {
            config.proxy_applications.remove(&target);
            config.proxies.remove("route-b");
        });

        assert_eq!(
            network::proxy::resolve(
                &manager.read(),
                &RouteTarget::Service(target.clone()),
                &Url::parse("https://example.test/").unwrap(),
            ),
            Ok(ResolvedProxy::NoProxy)
        );
        assert_ne!(
            manager.read().service_network_route_generation(&target),
            initial_generation
        );
        assert_eq!(
            timeout(Duration::from_secs(2), news.recv())
                .await
                .expect("the route ABA should retire the News subscription"),
            Err(TransportError::RouteChanged)
        );
        assert_eq!(
            server.next_request().await,
            (1, ObservedRequest::SetSubscriptions(Vec::new()))
        );
        assert_eq!(server.connection_count.load(Ordering::Acquire), 1);

        connector.shutdown().await;
        transport.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn rejected_last_subscription_should_not_leave_an_idle_connection() {
        let mut server = TestServiceServer::spawn(false).await;
        let connector =
            KytrixServiceConnector::spawn_with_urls(&Runtime::for_test(), server.url.clone());

        server.fail_next_request();
        let topic = ServiceTopic::News;
        assert!(matches!(
            connector.subscribe_news().await,
            Err(KytrixServiceError::Server(ErrorCode::RateLimit))
        ));
        assert_eq!(
            server.next_request().await,
            (1, ObservedRequest::SetSubscriptions(vec![topic.clone()]))
        );

        let subscription = connector
            .subscribe_news()
            .await
            .expect("a fresh connection should accept the next subscription");
        assert_eq!(
            server.next_request().await,
            (2, ObservedRequest::SetSubscriptions(vec![topic]))
        );
        assert_eq!(server.connection_count.load(Ordering::Acquire), 2);

        drop(subscription);
        connector.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn duplicate_topic_ack_should_be_rejected_through_the_websocket_facade() {
        let mut server = TestServiceServer::spawn(false).await;
        let connector =
            KytrixServiceConnector::spawn_with_urls(&Runtime::for_test(), server.url.clone());
        server.duplicate_topics_in_next_ack();

        assert!(matches!(
            connector.subscribe_news().await,
            Err(KytrixServiceError::Protocol(_))
        ));
        assert_eq!(
            server.next_request().await,
            (
                1,
                ObservedRequest::SetSubscriptions(vec![ServiceTopic::News])
            )
        );
        assert_eq!(server.connection_count.load(Ordering::Acquire), 1);

        connector.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_server_oneof_should_be_rejected_through_the_websocket_facade() {
        let mut server = TestServiceServer::spawn(false).await;
        let connector =
            KytrixServiceConnector::spawn_with_urls(&Runtime::for_test(), server.url.clone());
        server.send_unknown_oneof_instead_of_next_ack();

        assert!(matches!(
            connector.subscribe_news().await,
            Err(KytrixServiceError::Protocol(_))
        ));
        assert_eq!(
            server.next_request().await,
            (
                1,
                ObservedRequest::SetSubscriptions(vec![ServiceTopic::News])
            )
        );

        connector.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn index_facade_should_reference_count_and_expire_its_replay_cache() {
        let mut server = TestServiceServer::spawn(false).await;
        let connector =
            KytrixServiceConnector::spawn_with_urls(&Runtime::for_test(), server.url.clone());
        let index = index_topic("SPX");
        let service_topic = ServiceTopic::Index(index.clone());

        let mut first = connector
            .subscribe_indices(std::slice::from_ref(&index))
            .await
            .expect("first index subscriber should register");
        assert_eq!(
            server.next_request().await,
            (
                1,
                ObservedRequest::SetSubscriptions(vec![service_topic.clone()])
            )
        );
        server.send_index("SPX");
        let cached = timeout(Duration::from_secs(2), first.recv())
            .await
            .expect("first index event should arrive")
            .expect("first index subscriber should remain active");
        assert_eq!(cached.topic, index);
        assert_eq!(cached.last_price, 101);

        let mut second = connector
            .subscribe_indices(std::slice::from_ref(&index))
            .await
            .expect("second index subscriber should share the topic");
        assert_eq!(
            timeout(Duration::from_secs(2), second.recv())
                .await
                .expect("cached index event should replay")
                .expect("second index subscriber should remain active"),
            cached
        );
        server.assert_no_request().await;
        assert_eq!(server.connection_count.load(Ordering::Acquire), 1);

        first.close().await.expect("first close should complete");
        server.assert_no_request().await;
        assert_eq!(server.connection_count.load(Ordering::Acquire), 1);

        second.close().await.expect("last close should complete");
        assert_eq!(
            server.next_request().await,
            (1, ObservedRequest::SetSubscriptions(Vec::new()))
        );
        server.assert_no_request().await;
        assert_eq!(server.connection_count.load(Ordering::Acquire), 1);

        let mut fresh = connector
            .subscribe_indices(std::slice::from_ref(&index))
            .await
            .expect("fresh index subscriber should register");
        assert_eq!(
            server.next_request().await,
            (
                2,
                ObservedRequest::SetSubscriptions(vec![service_topic.clone()])
            )
        );
        assert!(
            timeout(Duration::from_millis(100), fresh.recv())
                .await
                .is_err(),
            "a topic with zero references must not retain its old cached value"
        );
        server.send_index("SPX");
        assert_eq!(
            timeout(Duration::from_secs(2), fresh.recv())
                .await
                .expect("new index frame should arrive")
                .expect("fresh index subscriber should remain active")
                .topic,
            index
        );
        fresh.close().await.expect("fresh close should complete");
        assert_eq!(
            server.next_request().await,
            (2, ObservedRequest::SetSubscriptions(Vec::new()))
        );

        connector.shutdown().await;
        server.shutdown().await;
    }
}
